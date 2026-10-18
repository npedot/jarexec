//! Application launch: prepares Java, dependencies and argfile, then starts the JVM.
//!
//! Fast path: if the cache marker `<id>.ok` still describes the current state
//! (same lock, same JAR, Java and dependencies still present), launch
//! right away with no network, no Java discovery and no rehashing.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::fetch::{self, Fetcher};
use crate::java::{JavaResolver, Origin, Requirement};
use crate::lockfile::{Dep, Lockfile};
use crate::{hash, jar};

/// Ready command: `java <args> <user arguments>`.
#[derive(Debug)]
pub struct Prepared {
    pub java: PathBuf,
    pub args: Vec<String>,
    /// `true` if prepared from the marker, with no network or Java discovery.
    pub fast: bool,
}

/// State saved after a successful full preparation.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Marker {
    java: PathBuf,
    override_home: Option<PathBuf>,
    app_jar: PathBuf,
    cache: PathBuf,
    args: Vec<String>,
}

pub fn prepare(
    lock_path: &Path,
    cache: &Path,
    override_home: Option<PathBuf>,
    refresh: bool,
) -> Result<Prepared> {
    let text = std::fs::read_to_string(lock_path)
        .with_context(|| format!("reading {}", lock_path.display()))?;
    let lock = Lockfile::parse(&text)
        .with_context(|| format!("invalid lockfile {}", lock_path.display()))?;
    let (lock_hash, _) = hash::sha256_reader(text.as_bytes())?;
    let marker_path = cache.join(format!("{}.ok", &lock_hash[..16]));

    let lock_dir = absolute(lock_path.parent().unwrap_or(Path::new(".")))?;
    let app_jar = lock_dir.join(&lock.app.jar);
    ensure!(
        app_jar.is_file(),
        "application JAR not found: {} (expected next to the lockfile)",
        app_jar.display()
    );
    let override_home = override_home.or_else(|| {
        std::env::var_os(crate::java::JAVA_HOME_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    });

    if !refresh
        && let Some(marker) = read_marker(&marker_path)
        && marker.override_home == override_home
        && marker.app_jar == app_jar
        && marker.cache == cache
        && marker.java.is_file()
        && Fetcher::cached_paths(cache, &lock.remote_deps()).is_some()
        && lock
            .deps
            .iter()
            .all(|d| bundled_path(&lock_dir, d, false).is_ok())
    {
        return Ok(Prepared {
            java: marker.java,
            args: marker.args,
            fast: true,
        });
    }

    let req = Requirement::from_lock(&lock.java)?;
    let java = JavaResolver::new(cache, override_home.clone())?.resolve(&req)?;
    if java.origin == Origin::Downloaded {
        eprintln!(
            "jarexec: installed Java {} in {}",
            java.install.version,
            java.install.home.display()
        );
    }
    let (remote_paths, report) = Fetcher::new(cache, &lock.repositories)?
        .fetch_all(&lock.remote_deps(), fetch::DEFAULT_THREADS)?;
    if report.downloaded > 0 {
        eprintln!("jarexec: downloaded {} dependencies", report.downloaded);
    }

    let main = match lock.app.main.clone() {
        Some(m) => m,
        None => jar::inspect(&app_jar)?.main_class.with_context(|| {
            format!(
                "no main class: set [app].main in {} or Main-Class in the manifest",
                lock_path.display()
            )
        })?,
    };

    // same order as the lock, with bundled dependencies in place
    let mut remote_paths = remote_paths.into_iter();
    let mut classpath = vec![app_jar.clone()];
    for dep in &lock.deps {
        classpath.push(match bundled_path(&lock_dir, dep, true)? {
            Some(path) => path,
            None => remote_paths.next().expect("one path per remote dependency"),
        });
    }
    let classpath = std::env::join_paths(&classpath)
        .context("a classpath entry contains the classpath separator")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("classpath entry is not UTF-8"))?;

    let mut jvm_args = lock.app.jvm_args.clone();
    jvm_args.extend(["-cp".to_string(), classpath, main]);

    // argfiles (`java @file`) exist since Java 9; for Java 8 pass arguments directly
    let args = if java.install.version.feature() >= 9 {
        let argfile = cache.join(format!("{}.args", &lock_hash[..16]));
        write_atomic(&argfile, &argfile_content(&jvm_args))?;
        vec![format!("@{}", argfile.display())]
    } else {
        jvm_args
    };

    let marker = Marker {
        // real path: if a symlink (e.g. sdkman `current`) changes target, the
        // fast path stays on the verified Java instead of following it blindly
        java: absolute(&java.install.java_bin())?,
        override_home,
        app_jar,
        cache: cache.to_path_buf(),
        args,
    };
    write_atomic(&marker_path, &toml::to_string(&marker)?)?;
    Ok(Prepared {
        java: marker.java,
        args: marker.args,
        fast: false,
    })
}

/// Starts the JVM. On Unix it replaces this process (`exec`): signals and exit code
/// go directly to/from the JVM. Returns only on error.
pub fn exec(prepared: Prepared, user_args: &[OsString]) -> Result<std::convert::Infallible> {
    let mut cmd = Command::new(&prepared.java);
    cmd.args(&prepared.args).args(user_args);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        Err(err).with_context(|| format!("launching {}", prepared.java.display()))
    }

    #[cfg(not(unix))]
    {
        // Ctrl+C reaches every process on the console: ignore it here
        // so the JVM decides how to exit, and its code is forwarded
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(None, 1);
        }
        let status = cmd
            .status()
            .with_context(|| format!("launching {}", prepared.java.display()))?;
        std::process::exit(status.code().unwrap_or(1));
    }
}

/// Path of a dependency shipped with the distribution (`None` if it must be downloaded).
/// Always checks the size; with `verify` also the hash (full preparation).
fn bundled_path(lock_dir: &Path, dep: &Dep, verify: bool) -> Result<Option<PathBuf>> {
    let Some(rel) = &dep.bundled else {
        return Ok(None);
    };
    // the lockfile must not be able to point outside the app directory
    ensure!(
        Path::new(rel)
            .components()
            .all(|c| matches!(c, Component::Normal(_))),
        "{}: invalid path for a bundled dependency: {rel}",
        dep.coord
    );
    let path = lock_dir.join(rel);
    let meta = std::fs::metadata(&path).with_context(|| {
        format!(
            "missing bundled dependency: {} (must ship next to the lockfile)",
            path.display()
        )
    })?;
    ensure!(
        meta.len() == dep.size,
        "{}: size differs from the lockfile (lib/ not updated together with the lock?)",
        path.display()
    );
    if verify {
        let (sha, _) = hash::sha256_file(&path)?;
        ensure!(
            sha == dep.sha256,
            "{}: SHA-256 differs from the lockfile",
            path.display()
        );
    }
    Ok(Some(path))
}

/// One argument per line. In an argfile `\` is an escape character inside
/// quotes, so Windows paths (`C:\...`) must be doubled.
fn argfile_content(args: &[String]) -> String {
    args.iter().map(|a| quote_arg(a) + "\n").collect()
}

fn quote_arg(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for c in arg.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn read_marker(path: &Path) -> Option<Marker> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

/// Writes to a temp file and renames: readers see the old or the new one, never half.
fn write_atomic(path: &Path, content: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    std::fs::write(&tmp, content).with_context(|| format!("writing {}", path.display()))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    match path.canonicalize() {
        // on Windows canonicalize yields `\\?\C:\...`, which some Java programs cannot handle
        Ok(p) if cfg!(windows) => Ok(crate::paths::plain(std::path::absolute(path).unwrap_or(p))),
        Ok(p) => Ok(p),
        Err(e) => bail!("{}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::jar::tests::write_test_jar;
    use crate::java::tests::{fake_home, os_arch};

    #[test]
    fn quotes_argfile_arguments() {
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(
            quote_arg(r"C:\Program Files\x.jar"),
            r#""C:\\Program Files\\x.jar""#
        );
        assert_eq!(quote_arg(r#"-Dmsg="ciao""#), r#""-Dmsg=\"ciao\"""#);
    }

    /// App with one dependency already cached and a fake Java: no network needed.
    struct Fixture {
        _dir: tempfile::TempDir,
        lock: PathBuf,
        cache: PathBuf,
        java: PathBuf,
    }

    fn fixture(java_version: &str, main_in_lock: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let cache = dir.path().join("cache");
        fs::create_dir_all(&app).unwrap();
        write_test_jar(&app.join("hello-1.0.jar"), Some("com.acme.Main"), 65);

        let lib = cache.join("repo/com/acme/lib/1.0/lib-1.0.jar");
        fs::create_dir_all(lib.parent().unwrap()).unwrap();
        fs::write(&lib, b"lib").unwrap();
        let (sha, size) = hash::sha256_file(&lib).unwrap();

        let main = if main_in_lock {
            "main = \"com.acme.Main\"\n"
        } else {
            ""
        };
        let lock = app.join("hello.lock");
        fs::write(
            &lock,
            format!(
                "version = 1\n[app]\nname = \"hello\"\njar = \"hello-1.0.jar\"\n{main}\
                 jvm_args = [\"-Xmx64m\"]\n[java]\nversion = \"8\"\n\
                 [[dep]]\ncoord = \"com.acme:lib:jar:1.0\"\nsha256 = \"{sha}\"\nsize = {size}\n"
            ),
        )
        .unwrap();
        let java = fake_home(dir.path(), "jdk", java_version, os_arch());
        Fixture {
            _dir: dir,
            lock,
            cache,
            java,
        }
    }

    #[test]
    fn writes_argfile_then_uses_fast_path() {
        let f = fixture("21.0.4", true);
        let first = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        assert!(!first.fast);
        assert_eq!(first.args.len(), 1);
        let argfile = first.args[0].strip_prefix('@').unwrap();
        let content = fs::read_to_string(argfile).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines[0], "\"-Xmx64m\"");
        assert_eq!(lines[1], "\"-cp\"");
        assert!(lines[2].contains("hello-1.0.jar") && lines[2].contains("lib-1.0.jar"));
        assert_eq!(lines[3], "\"com.acme.Main\"");

        let second = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        assert!(second.fast);
        assert_eq!(second.args, first.args);

        let refreshed = prepare(&f.lock, &f.cache, Some(f.java.clone()), true).unwrap();
        assert!(!refreshed.fast);
    }

    #[test]
    fn changed_lock_or_java_invalidates_marker() {
        let f = fixture("21.0.4", true);
        prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();

        let other_java = fake_home(f.java.parent().unwrap(), "jdk2", "25", os_arch());
        assert!(
            !prepare(&f.lock, &f.cache, Some(other_java), false)
                .unwrap()
                .fast
        );

        let text = fs::read_to_string(&f.lock).unwrap();
        fs::write(&f.lock, text.replace("-Xmx64m", "-Xmx128m")).unwrap();
        assert!(
            !prepare(&f.lock, &f.cache, Some(f.java.clone()), false)
                .unwrap()
                .fast
        );
    }

    #[test]
    fn java8_gets_arguments_without_argfile() {
        let f = fixture("1.8.0_402", true);
        let p = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        assert_eq!(p.args[0], "-Xmx64m");
        assert_eq!(p.args[1], "-cp");
        assert_eq!(p.args[3], "com.acme.Main");
    }

    #[test]
    fn main_class_falls_back_to_manifest() {
        let f = fixture("21.0.4", false);
        let p = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        let content = fs::read_to_string(p.args[0].strip_prefix('@').unwrap()).unwrap();
        assert!(content.ends_with("\"com.acme.Main\"\n"));
    }

    #[test]
    fn bundled_dependency_comes_from_lib_next_to_the_lock() {
        let f = fixture("21.0.4", true);
        let app = f.lock.parent().unwrap().to_path_buf();
        fs::create_dir(app.join("lib")).unwrap();
        fs::write(app.join("lib/core-1.0-SNAPSHOT.jar"), b"internal").unwrap();
        let (sha, size) = hash::sha256_file(&app.join("lib/core-1.0-SNAPSHOT.jar")).unwrap();
        let mut text = fs::read_to_string(&f.lock).unwrap();
        text.push_str(&format!(
            "[[dep]]\ncoord = \"com.acme:core:jar:1.0-SNAPSHOT\"\nsha256 = \"{sha}\"\n\
             size = {size}\nbundled = \"lib/core-1.0-SNAPSHOT.jar\"\n"
        ));
        fs::write(&f.lock, text).unwrap();

        let p = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        let content = fs::read_to_string(p.args[0].strip_prefix('@').unwrap()).unwrap();
        let cp = content.lines().nth(2).unwrap();
        assert!(cp.find("lib-1.0.jar").unwrap() < cp.find("core-1.0-SNAPSHOT.jar").unwrap());
        assert!(
            prepare(&f.lock, &f.cache, Some(f.java.clone()), false)
                .unwrap()
                .fast
        );

        // same size but different content: the fast path does not rehash,
        // the full preparation does
        fs::write(app.join("lib/core-1.0-SNAPSHOT.jar"), b"INTERNAL").unwrap();
        let err = prepare(&f.lock, &f.cache, Some(f.java.clone()), true).unwrap_err();
        assert!(format!("{err:#}").contains("SHA-256 differs"), "{err:#}");

        fs::remove_file(app.join("lib/core-1.0-SNAPSHOT.jar")).unwrap();
        let err = prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing bundled dependency"),
            "{err:#}"
        );
    }

    #[test]
    fn bundled_path_cannot_escape_the_app_folder() {
        let dep = Dep {
            coord: "a:b:jar:1".into(),
            sha256: "0".repeat(64),
            size: 1,
            bundled: Some("../../etc/passwd".into()),
        };
        assert!(bundled_path(Path::new("/app"), &dep, false).is_err());
    }

    #[test]
    fn missing_app_jar_is_reported() {
        let f = fixture("21.0.4", true);
        fs::remove_file(f.lock.parent().unwrap().join("hello-1.0.jar")).unwrap();
        let err = format!(
            "{:#}",
            prepare(&f.lock, &f.cache, Some(f.java), false).unwrap_err()
        );
        assert!(err.contains("application JAR not found"), "{err}");
    }
}
