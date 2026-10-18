mod deps_list;
mod fetch;
mod hash;
mod http;
mod jar;
mod java;
mod launch;
mod lock;
mod lockfile;
mod paths;
mod platform;
#[cfg(test)]
mod test_util;

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use indicatif::HumanBytes;

use crate::lockfile::Lockfile;

/// Launcher for Java applications: fetches dependencies and JRE on first run.
#[derive(Parser)]
#[command(name = "jarexec", version, about)]
struct Cli {
    /// Cache directory (default: $JAREXEC_CACHE, then .jarexec/ next to the executable)
    #[arg(long, global = true)]
    cache_dir: Option<PathBuf>,
    /// Java to use unconditionally (default: $JAREXEC_JAVA_HOME, then auto-detection)
    #[arg(long, global = true)]
    java_home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate the lockfile from `mvn dependency:list` output (at build time).
    Lock {
        /// File written by dependency:list with outputAbsoluteArtifactFilename=true
        #[arg(long)]
        deps: PathBuf,
        /// Application JAR
        #[arg(long)]
        jar: PathBuf,
        /// Application name (default: JAR file name without version)
        #[arg(long)]
        name: Option<String>,
        /// Minimum Java version (default: inferred from the Main-Class bytecode)
        #[arg(long)]
        java: Option<String>,
        /// Maven repository, repeatable (default: Maven Central)
        #[arg(long = "repo")]
        repos: Vec<String>,
        /// Output file (default: <name>.lock next to the JAR)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Local Maven repository, used when paths in the deps file are unreachable
        /// (default: ~/.m2/repository)
        #[arg(long)]
        local_repo: Option<PathBuf>,
        /// Ship matching dependencies with the distribution (in lib/):
        /// `group` or `group:artifact`, with `*` wildcards (e.g. `com.example.*`). Repeatable
        #[arg(long)]
        bundle: Vec<String>,
        /// JVM argument, repeatable; use the `=` form
        /// (e.g. `--jvm-arg=--add-modules=jdk.incubator.vector`, `--jvm-arg=-Xmx2g`)
        #[arg(long = "jvm-arg", allow_hyphen_values = true)]
        jvm_args: Vec<String>,
        /// Maximum accepted version of a system Java
        #[arg(long)]
        java_max: Option<String>,
        /// Where Java comes from: auto (system, else download), system, download
        #[arg(long, value_enum)]
        java_source: Option<lockfile::JavaSource>,
    },
    /// Prepare dependencies and JRE, then launch the application.
    Run {
        /// Lockfile (default: <executable-name>.lock next to the binary)
        lock: Option<PathBuf>,
        /// Ignore the marker: redo Java discovery and cache verification
        #[arg(long)]
        refresh: bool,
        /// Arguments passed to the Java application (after `--`)
        #[arg(last = true)]
        args: Vec<OsString>,
    },
    /// Populate the cache without launching the application.
    Fetch {
        /// Lockfile (default: <executable-name>.lock next to the binary)
        lock: Option<PathBuf>,
    },
    /// Build per-platform distribution archives (not implemented yet).
    Package { lock: PathBuf },
    /// Show platform, selected Java and cache status.
    Doctor {
        /// Lockfile (default: <executable-name>.lock next to the binary)
        lock: Option<PathBuf>,
    },
    /// Clear the local cache (not implemented yet).
    Clean,
}

fn main() -> ExitCode {
    // renamed binary (e.g. `myapp`): acts only as a launcher and all
    // arguments go to the application; options come from environment variables
    let result = if launcher_mode() {
        run_app(
            None,
            std::env::args_os().skip(1).collect(),
            None,
            None,
            false,
        )
    } else {
        run(Cli::parse())
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("jarexec: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Doctor { lock } => {
            doctor(lock, cli.cache_dir, cli.java_home)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Lock {
            deps,
            jar,
            name,
            java,
            repos,
            output,
            local_repo,
            bundle,
            jvm_args,
            java_max,
            java_source,
        } => {
            let path = lock::run(lock::LockOptions {
                deps,
                jar,
                name,
                java,
                repos,
                output,
                local_repo,
                bundle,
                jvm_args,
                java_max,
                java_source,
            })?;
            println!("wrote {}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        Command::Fetch { lock } => {
            let lock_path = match lock {
                Some(p) => p,
                None => paths::default_lock()?,
            };
            let lock = Lockfile::load(&lock_path)?;
            let cache = paths::cache_dir(cli.cache_dir)?;
            let req = java::Requirement::from_lock(&lock.java)?;
            let java = java::JavaResolver::new(&cache, cli.java_home)?.resolve(&req)?;
            println!(
                "Java {} ({}): {}",
                java.install.version,
                java.origin,
                java.install.home.display()
            );
            let remote = lock.remote_deps();
            let fetcher = fetch::Fetcher::new(&cache, &lock.repositories)?;
            let (_, r) = fetcher.fetch_all(&remote, fetch::DEFAULT_THREADS)?;
            println!(
                "{} dependencies ready in {}: {} already cached, {} from ~/.m2, {} downloaded ({})",
                remote.len(),
                cache.display(),
                r.cached,
                r.from_m2,
                r.downloaded,
                HumanBytes(r.bytes)
            );
            let bundled = lock.deps.len() - remote.len();
            if bundled > 0 {
                println!(
                    "{bundled} dependencies shipped with the distribution (lib/), not downloaded"
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Run {
            lock,
            refresh,
            args,
        } => run_app(lock, args, cli.cache_dir, cli.java_home, refresh),
        Command::Package { .. } => bail!("`package` is not implemented yet"),
        Command::Clean => bail!("`clean` is not implemented yet"),
    }
}

fn run_app(
    lock: Option<PathBuf>,
    args: Vec<OsString>,
    cache_dir: Option<PathBuf>,
    java_home: Option<PathBuf>,
    refresh: bool,
) -> Result<ExitCode> {
    let lock = match lock {
        Some(p) => p,
        None => paths::default_lock()?,
    };
    let cache = paths::cache_dir(cache_dir)?;
    let prepared = launch::prepare(&lock, &cache, java_home, refresh)?;
    if std::env::var_os("JAREXEC_DEBUG").is_some() {
        eprintln!(
            "jarexec: {} preparation → {} {}",
            if prepared.fast {
                "fast (marker)"
            } else {
                "full"
            },
            prepared.java.display(),
            prepared.args.join(" ")
        );
    }
    match launch::exec(prepared, &args)? {}
}

fn launcher_mode() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .is_some_and(|stem| is_launcher_name(&stem))
}

/// `jarexec`, `jarexec-0.1.0`, `jarexec-<hash>` (test binaries) stay the CLI.
fn is_launcher_name(stem: &str) -> bool {
    let stem = stem.to_ascii_lowercase();
    stem != "jarexec" && !stem.starts_with("jarexec-")
}

fn doctor(
    lock: Option<PathBuf>,
    cache_dir: Option<PathBuf>,
    java_home: Option<PathBuf>,
) -> Result<()> {
    let cache = paths::cache_dir(cache_dir)?;
    let resolver = java::JavaResolver::new(&cache, java_home)?;
    let p = resolver.platform();
    println!("platform:    {}-{}", p.os, p.arch);
    println!("cache:       {}", cache.display());

    let lock_path = lock.map(Ok).unwrap_or_else(paths::default_lock);
    let req = match lock_path.as_ref().map(|p| Lockfile::load(p)) {
        Ok(Ok(lock)) => {
            println!("lockfile:    {}", lock_path.as_ref().unwrap().display());
            java::Requirement::from_lock(&lock.java)?
        }
        Ok(Err(e)) => return Err(e),
        Err(e) => {
            println!("lockfile:    none ({e:#})");
            java::Requirement {
                min: 8,
                max: None,
                source: lockfile::JavaSource::Auto,
                image: lockfile::JavaImage::Jre,
                vendor: "temurin".into(),
            }
        }
    };
    println!(
        "requirement: {} {}, source = {:?}",
        req.describe(),
        req.image.as_str(),
        req.source
    );

    let eval = resolver.find(&req);
    println!("candidates:");
    if eval.candidates.is_empty() {
        println!("  (none)");
    }
    for (i, c) in eval.candidates.iter().enumerate() {
        let mark = if Some(i) == eval.chosen { "→" } else { " " };
        let detail = match &c.verdict {
            Ok(inst) => format!(
                "Java {} {}",
                inst.version,
                inst.arch.as_deref().unwrap_or("?")
            ),
            Err(reason) => format!("rejected: {reason}"),
        };
        println!(
            "{mark} {:<9} {}  {detail}",
            c.origin.to_string(),
            c.home.display()
        );
    }
    match eval.selection() {
        Some(sel) => println!("selected:    {}", sel.install.home.display()),
        None if req.source == lockfile::JavaSource::System => {
            println!("selected:    none: launch will fail (source = \"system\")")
        }
        None => println!(
            "selected:    none: Temurin {} {} ({}-{}) will be downloaded on first run",
            req.min,
            req.image.as_str(),
            p.os,
            p.arch
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::is_launcher_name;

    #[test]
    fn renamed_binary_is_a_launcher() {
        assert!(is_launcher_name("myapp"));
        assert!(is_launcher_name("hello"));
        assert!(!is_launcher_name("jarexec"));
        assert!(!is_launcher_name("JarExec"));
        assert!(!is_launcher_name("jarexec-3f2a9c1d"));
    }
}
