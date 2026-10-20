use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use indicatif::HumanBytes;

use jarexec::lockfile::Lockfile;
use jarexec::{clean, fetch, java, launch, lock, lockfile, paths};

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
    // boxed: the options are much larger than the other variants (PathBuf is 32 bytes on Windows)
    Lock(Box<lock::LockOptions>),
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
    /// Remove cached dependencies and JREs.
    Clean {
        /// Remove only what the last two versions of each app do not use
        #[arg(long)]
        unused: bool,
    },
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
        Command::Lock(opts) => {
            let path = lock::run(*opts)?;
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
            let cache_lock = clean::cache_lock(&cache)?;
            let _in_use = cache_lock.read().context("acquiring the cache lock")?;
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
        Command::Clean { unused } => {
            let cache = paths::cache_dir(cli.cache_dir)?;
            let report = if unused {
                clean::unused(&cache)?
            } else {
                clean::all(&cache)?
            };
            let Some(r) = report else {
                bail!("an application is starting with this cache: retry when it is running");
            };
            println!(
                "removed {} files ({}) from {}",
                r.files,
                HumanBytes(r.bytes),
                cache.display()
            );
            Ok(ExitCode::SUCCESS)
        }
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
    let private_cache = paths::is_default_cache(cache_dir.as_deref());
    let cache = paths::cache_dir(cache_dir)?;
    let prepared = launch::prepare(&lock, &cache, java_home, refresh)?;
    // a full preparation usually follows an update: drop what older versions left behind.
    // Only in the app's own cache: a shared one may serve apps this launch knows nothing of
    if !prepared.fast && private_cache {
        match clean::unused(&cache) {
            Ok(Some(r)) if r.files > 0 => eprintln!(
                "jarexec: removed {} unused files ({}) from the cache",
                r.files,
                HumanBytes(r.bytes)
            ),
            Ok(_) => {}
            Err(e) => eprintln!("jarexec: cache cleanup skipped: {e:#}"),
        }
    }
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
