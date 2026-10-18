//! `jarexec lock` command: from Maven output to the `<app>.lock` file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::lockfile::{
    App, Dep, FORMAT_VERSION, Java, JavaImage, JavaSource, Lockfile, Repository,
};
use crate::{deps_list, fetch, hash, jar, java};

pub const MAVEN_CENTRAL: &str = "https://repo.maven.apache.org/maven2";

#[derive(clap::Args)]
pub struct LockOptions {
    /// File written by dependency:list with outputAbsoluteArtifactFilename=true
    #[arg(long)]
    pub deps: PathBuf,
    /// Application JAR
    #[arg(long)]
    pub jar: PathBuf,
    /// Application name (default: JAR file name without version)
    #[arg(long)]
    pub name: Option<String>,
    /// Minimum Java version (default: inferred from the Main-Class bytecode)
    #[arg(long)]
    pub java: Option<String>,
    /// Maven repository, repeatable (default: Maven Central)
    #[arg(long = "repo")]
    pub repos: Vec<String>,
    /// Output file (default: <name>.lock next to the JAR)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
    /// Local Maven repository, used when paths in the deps file are unreachable
    /// (default: ~/.m2/repository)
    #[arg(long)]
    pub local_repo: Option<PathBuf>,
    /// Ship matching dependencies with the distribution (in lib/):
    /// `group` or `group:artifact`, with `*` wildcards (e.g. `com.example.*`). Repeatable
    #[arg(long)]
    pub bundle: Vec<String>,
    /// JVM argument, repeatable; use the `=` form
    /// (e.g. `--jvm-arg=--add-modules=jdk.incubator.vector`, `--jvm-arg=-Xmx2g`)
    #[arg(long = "jvm-arg", allow_hyphen_values = true)]
    pub jvm_args: Vec<String>,
    /// Maximum accepted version of a system Java
    #[arg(long)]
    pub java_max: Option<String>,
    /// Where Java comes from: auto (system, else download), system, download
    #[arg(long, value_enum)]
    pub java_source: Option<JavaSource>,
}

/// Directory, relative to the lockfile, holding the bundled JARs.
pub const BUNDLE_DIR: &str = "lib";

/// Generates the lockfile and returns the path it was written to.
pub fn run(opts: LockOptions) -> Result<PathBuf> {
    let text = std::fs::read_to_string(&opts.deps)
        .with_context(|| format!("reading {}", opts.deps.display()))?;
    let listed =
        deps_list::parse(&text).with_context(|| format!("parsing {}", opts.deps.display()))?;

    let jar_file = file_name(&opts.jar)?;
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| default_app_name(&jar_file));
    let output = opts.output.clone().unwrap_or_else(|| {
        opts.jar
            .parent()
            .unwrap_or(Path::new("."))
            .join(format!("{name}.lock"))
    });
    let bundle_dir = output.parent().unwrap_or(Path::new(".")).join(BUNDLE_DIR);

    let local_repo = opts
        .local_repo
        .clone()
        .or_else(|| dirs::home_dir().map(|h| h.join(".m2").join("repository")));
    let mut deps = Vec::new();
    let mut requires: Vec<(u16, String)> = Vec::new();
    let mut used_patterns = vec![false; opts.bundle.len()];
    for d in listed {
        match d.scope.as_str() {
            "test" | "provided" => continue,
            "system" => bail!(
                "{}: scope `system` not supported (the JAR cannot be downloaded from a repository)",
                d.coord()
            ),
            _ => {}
        }
        if d.kind == "pom" {
            continue; // no JAR to put on the classpath
        }
        let path = locate(&d.path, &d.coord(), local_repo.as_deref())?;
        let (sha256, size) =
            hash::sha256_file(&path).with_context(|| format!("hashing {}", d.coord()))?;
        if let Ok(Some(release)) = jar::max_class_release(&path) {
            requires.push((release, d.coord()));
        }

        let matching: Vec<usize> = (0..opts.bundle.len())
            .filter(|&i| bundle_matches(&opts.bundle[i], &d.group, &d.artifact))
            .collect();
        let bundled = if matching.is_empty() {
            None
        } else {
            matching.iter().for_each(|&i| used_patterns[i] = true);
            let file = Path::new(&fetch::repo_path(&d.coord())?)
                .file_name()
                .expect("repo_path ends with the file name")
                .to_owned();
            std::fs::create_dir_all(&bundle_dir)?;
            std::fs::copy(&path, bundle_dir.join(&file)).with_context(|| {
                format!("copying {} to {}", path.display(), bundle_dir.display())
            })?;
            Some(format!("{BUNDLE_DIR}/{}", file.to_string_lossy()))
        };
        deps.push(Dep {
            coord: d.coord(),
            sha256,
            size,
            bundled,
        });
    }
    for (pattern, used) in opts.bundle.iter().zip(&used_patterns) {
        if !used {
            eprintln!("warning: --bundle {pattern} matches no dependency");
        }
    }

    let info = jar::inspect(&opts.jar)?;
    if let Ok(Some(release)) = jar::max_class_release(&opts.jar) {
        requires.push((release, format!("{jar_file} (JAR applicativo)")));
    }
    let java_version = match (opts.java, info.java_release) {
        (Some(v), _) => v,
        (None, Some(release)) => release.to_string(),
        (None, None) => bail!("cannot determine Java version from the JAR: pass --java"),
    };
    let java_feature = java::Version::parse(&java_version)
        .with_context(|| format!("invalid Java version: {java_version}"))?
        .feature();
    if let Some(warning) = java_warning(java_feature, &mut requires) {
        eprintln!("{warning}");
    }
    if let Some(max) = &opts.java_max {
        let max_feature = java::Version::parse(max)
            .with_context(|| format!("invalid Java version: {max}"))?
            .feature();
        ensure!(
            max_feature >= java_feature,
            "--java-max {max} is lower than the minimum version {java_version}"
        );
    }

    let repos = if opts.repos.is_empty() {
        vec![MAVEN_CENTRAL.to_string()]
    } else {
        opts.repos
    };

    let lock = Lockfile {
        version: FORMAT_VERSION,
        app: App {
            name: name.clone(),
            jar: jar_file,
            main: info.main_class,
            jvm_args: opts.jvm_args,
        },
        java: Java {
            version: java_version,
            max: opts.java_max,
            source: opts.java_source.unwrap_or_default(),
            vendor: "temurin".into(),
            image: JavaImage::Jre,
        },
        repositories: repos
            .into_iter()
            .enumerate()
            .map(|(i, url)| Repository {
                id: if url == MAVEN_CENTRAL {
                    "central".into()
                } else {
                    format!("repo{i}")
                },
                url: url.trim_end_matches('/').into(),
            })
            .collect(),
        deps,
    };

    if lock.app.main.is_none() {
        eprintln!("warning: no Main-Class in the manifest; set [app].main in the lockfile");
    }

    std::fs::write(&output, lock.to_toml()?)
        .with_context(|| format!("writing {}", output.display()))?;
    Ok(output)
}

/// Warning if some JAR contains classes for a newer Java than the required one.
/// Only a warning: some libraries include classes loaded only when available
/// (e.g. jline with FFM support), so the maximum may overestimate.
fn java_warning(java: u32, requires: &mut [(u16, String)]) -> Option<String> {
    requires.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let above: Vec<&(u16, String)> = requires
        .iter()
        .filter(|(r, _)| u32::from(*r) > java)
        .collect();
    let max = above.first()?.0;
    let examples: Vec<String> = above
        .iter()
        .take(3)
        .map(|(r, c)| format!("\n    Java {r}  {c}"))
        .collect();
    let more = match above.len() {
        n if n > 3 => format!("\n    ... and {} more", n - 3),
        _ => String::new(),
    };
    Some(format!(
        "warning: the lock requires Java {java}, but {} JARs contain classes for newer Java versions:{}{more}\n  \
         if they are needed at runtime, regenerate with --java {max}",
        above.len(),
        examples.concat()
    ))
}

/// `com.example.*` matches the group, `group:artifact` both; `*` = any text.
fn bundle_matches(pattern: &str, group: &str, artifact: &str) -> bool {
    match pattern.split_once(':') {
        Some((g, a)) => glob(g, group) && glob(a, artifact),
        None => glob(pattern, group),
    }
}

fn glob(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => text.strip_prefix(head).is_some_and(|tail| {
            (0..=tail.len())
                .filter(|&i| tail.is_char_boundary(i))
                .any(|i| glob(rest, &tail[i..]))
        }),
    }
}

/// The path written by Maven, then the same path translated for WSL (Maven on Windows,
/// jarexec in WSL), then the standard layout in the local repository.
fn locate(listed: &Path, coord: &str, local_repo: Option<&Path>) -> Result<PathBuf> {
    let mut candidates = vec![listed.to_path_buf()];
    if cfg!(unix)
        && let Some(wsl) = listed.to_str().and_then(windows_to_wsl)
    {
        candidates.push(wsl);
    }
    if let Some(repo) = local_repo {
        candidates.push(repo.join(fetch::repo_path(coord)?));
    }
    if let Some(found) = candidates.iter().find(|p| p.is_file()) {
        return Ok(found.clone());
    }
    let tried: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    bail!(
        "{coord}: JAR not found; searched:\n  {}\n(use --local-repo if the Maven repository is elsewhere)",
        tried.join("\n  ")
    )
}

/// `C:\Users\x\a.jar` → `/mnt/c/Users/x/a.jar`
fn windows_to_wsl(path: &str) -> Option<PathBuf> {
    let b = path.as_bytes();
    let is_drive =
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    is_drive.then(|| {
        PathBuf::from(format!(
            "/mnt/{}/{}",
            (b[0] as char).to_ascii_lowercase(),
            path[3..].replace('\\', "/")
        ))
    })
}

fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .with_context(|| format!("invalid JAR path: {}", path.display()))
}

/// `myapp-1.4.2.jar` → `myapp`: cut at the first `-` followed by a digit.
fn default_app_name(jar_file: &str) -> String {
    let stem = jar_file.strip_suffix(".jar").unwrap_or(jar_file);
    let cut = stem
        .match_indices('-')
        .find(|(i, _)| stem[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .map_or(stem.len(), |(i, _)| i);
    stem[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jar::tests::write_test_jar;

    #[test]
    fn translates_windows_paths_for_wsl() {
        assert_eq!(
            windows_to_wsl(r"C:\Users\dev\.m2\repository\a.jar").unwrap(),
            PathBuf::from("/mnt/c/Users/dev/.m2/repository/a.jar")
        );
        assert!(windows_to_wsl("/home/x/a.jar").is_none());
    }

    #[test]
    fn locates_jar_in_local_repo_when_listed_path_is_missing() {
        let repo = tempfile::tempdir().unwrap();
        let jar = repo.path().join("com/acme/lib/1.0/lib-1.0.jar");
        std::fs::create_dir_all(jar.parent().unwrap()).unwrap();
        std::fs::write(&jar, b"x").unwrap();
        let found = locate(
            Path::new("/does/not/exist/lib-1.0.jar"),
            "com.acme:lib:jar:1.0",
            Some(repo.path()),
        )
        .unwrap();
        assert_eq!(found, jar);

        let err = locate(
            Path::new("/does/not/exist/x.jar"),
            "com.acme:x:jar:1.0",
            Some(repo.path()),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("/does/not/exist/x.jar") && err.contains("--local-repo"),
            "{err}"
        );
    }

    #[test]
    fn bundle_patterns() {
        assert!(bundle_matches(
            "com.example.*",
            "com.example.internal",
            "commons"
        ));
        assert!(!bundle_matches("com.example.*", "com.example", "x"));
        assert!(bundle_matches("com.example*", "com.example", "x"));
        assert!(bundle_matches("com.acme:core-*", "com.acme", "core-api"));
        assert!(!bundle_matches("com.acme:core-*", "com.acme", "web"));
        assert!(bundle_matches("org.slf4j", "org.slf4j", "slf4j-api"));
        assert!(!bundle_matches("org.slf4j", "org.slf4j.ext", "x"));
    }

    #[test]
    fn warns_only_when_jars_need_newer_java() {
        let mut reqs = vec![
            (8, "a".to_string()),
            (21, "lucene".into()),
            (17, "b".into()),
        ];
        let w = java_warning(8, &mut reqs).unwrap();
        assert!(w.contains("2 JAR") && w.contains("--java 21"), "{w}");
        assert!(
            w.find("lucene").unwrap() < w.find("Java 17").unwrap(),
            "newest first: {w}"
        );
        assert!(java_warning(21, &mut reqs).is_none());
    }

    #[test]
    fn bundles_matching_jars_next_to_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let internal = dir.path().join("commons-1.0.0-SNAPSHOT.jar");
        std::fs::write(&internal, b"internal").unwrap();
        let public = dir.path().join("lib-1.0.jar");
        std::fs::write(&public, b"public").unwrap();
        let deps = dir.path().join("deps.txt");
        std::fs::write(
            &deps,
            format!(
                "   com.example.internal:commons:jar:1.0.0-SNAPSHOT:compile:{}\n   \
                 com.acme:lib:jar:1.0:compile:{}\n",
                internal.display(),
                public.display()
            ),
        )
        .unwrap();
        let out_dir = dir.path().join("target");
        std::fs::create_dir(&out_dir).unwrap();
        let app = out_dir.join("myapp-1.0.jar");
        write_test_jar(&app, Some("com.acme.Main"), 65);

        let out = run(LockOptions {
            deps,
            jar: app,
            name: None,
            java: None,
            repos: vec![],
            output: None,
            local_repo: None,
            bundle: vec!["com.example.*".into()],
            jvm_args: vec![],
            java_max: None,
            java_source: None,
        })
        .unwrap();

        let lock = Lockfile::parse(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(
            lock.deps[0].bundled.as_deref(),
            Some("lib/commons-1.0.0-SNAPSHOT.jar")
        );
        assert_eq!(lock.deps[1].bundled, None);
        assert_eq!(
            std::fs::read(out_dir.join("lib/commons-1.0.0-SNAPSHOT.jar")).unwrap(),
            b"internal"
        );
        assert!(!out_dir.join("lib/lib-1.0.jar").exists());
    }

    #[test]
    fn app_name_from_jar_file() {
        assert_eq!(default_app_name("myapp-1.4.2.jar"), "myapp");
        assert_eq!(default_app_name("my-app-2.0-SNAPSHOT.jar"), "my-app");
        assert_eq!(default_app_name("tool.jar"), "tool");
    }

    #[test]
    fn generates_lock_from_maven_list() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("lib-1.0.jar");
        std::fs::write(&lib, b"abc").unwrap();
        let test_lib = dir.path().join("junit.jar"); // must not be read
        let deps = dir.path().join("deps.txt");
        std::fs::write(
            &deps,
            format!(
                "The following files have been resolved:\n   \
                 com.acme:lib:jar:1.0:compile:{} -- module lib\n   \
                 org.junit:junit:jar:5.0:test:{}\n",
                lib.display(),
                test_lib.display()
            ),
        )
        .unwrap();
        let app = dir.path().join("hello-1.0.0.jar");
        write_test_jar(&app, Some("com.acme.Main"), 65);

        let out = run(LockOptions {
            deps,
            jar: app,
            name: None,
            java: None,
            repos: vec![],
            output: None,
            local_repo: None,
            bundle: vec![],
            jvm_args: vec!["--add-modules=jdk.incubator.vector".into()],
            java_max: Some("25".into()),
            java_source: Some(JavaSource::Download),
        })
        .unwrap();

        assert_eq!(out, dir.path().join("hello.lock"));
        let lock = Lockfile::parse(&std::fs::read_to_string(out).unwrap()).unwrap();
        assert_eq!(lock.app.name, "hello");
        assert_eq!(lock.app.jar, "hello-1.0.0.jar");
        assert_eq!(lock.app.main.as_deref(), Some("com.acme.Main"));
        assert_eq!(lock.java.version, "21");
        assert_eq!(lock.java.max.as_deref(), Some("25"));
        assert_eq!(lock.java.source, JavaSource::Download);
        assert_eq!(lock.app.jvm_args, ["--add-modules=jdk.incubator.vector"]);
        assert_eq!(lock.repositories[0].url, MAVEN_CENTRAL);
        assert_eq!(lock.deps.len(), 1);
        assert_eq!(lock.deps[0].coord, "com.acme:lib:jar:1.0");
        assert_eq!(
            lock.deps[0].sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
