//! Cache and lockfile resolution relative to the executable's location.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const CACHE_ENV: &str = "JAREXEC_CACHE";

/// Directory containing the executable, with symlinks resolved.
pub fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine the executable path")?;
    let exe = exe.canonicalize().map(plain).unwrap_or(exe);
    Ok(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
}

/// On Windows `canonicalize` yields verbatim paths (`\\?\C:\...`), which the JVM
/// cannot use on the classpath: turns them back into the plain form.
pub fn plain(path: PathBuf) -> PathBuf {
    if cfg!(windows)
        && let Some(s) = path.to_str().and_then(strip_verbatim)
    {
        return s.into();
    }
    path
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` → `\\server\share\x`; `None` otherwise.
fn strip_verbatim(s: &str) -> Option<String> {
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    let rest = s.strip_prefix(r"\\?\")?;
    let b = rest.as_bytes();
    (b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':').then(|| rest.to_string())
}

/// Priority: `--cache-dir`, then `JAREXEC_CACHE`, then `.jarexec/` next to the executable.
pub fn cache_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = arg {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os(CACHE_ENV).filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    Ok(exe_dir()?.join(".jarexec"))
}

/// `<executable-name>.lock` next to the binary, otherwise the only `*.lock` present.
pub fn default_lock() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let stem = exe.file_stem().unwrap_or_default().to_owned();
    find_lock(&exe_dir()?, &stem.to_string_lossy())
}

fn find_lock(dir: &Path, stem: &str) -> Result<PathBuf> {
    let named = dir.join(format!("{stem}.lock"));
    if named.is_file() {
        return Ok(named);
    }
    let mut locks: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "lock") && p.is_file())
        .collect();
    match locks.len() {
        1 => Ok(locks.remove(0)),
        0 => bail!(
            "no lockfile found in {} (expected {stem}.lock): pass it as an argument",
            dir.display()
        ),
        n => bail!(
            "{n} lockfiles in {} and none is named {stem}.lock: pass it as an argument",
            dir.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_windows_verbatim_prefix() {
        assert_eq!(
            strip_verbatim(r"\\?\C:\Users\x\.jarexec").as_deref(),
            Some(r"C:\Users\x\.jarexec")
        );
        assert_eq!(
            strip_verbatim(r"\\?\UNC\server\share\app").as_deref(),
            Some(r"\\server\share\app")
        );
        // verbatim forms with no plain equivalent stay as they are
        assert_eq!(strip_verbatim(r"\\?\Volume{abc}\x"), None);
        assert_eq!(strip_verbatim(r"C:\Users\x"), None);
    }

    #[test]
    fn prefers_lock_named_like_executable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("other.lock"), "").unwrap();
        std::fs::write(dir.path().join("myapp.lock"), "").unwrap();
        let found = find_lock(dir.path(), "myapp").unwrap();
        assert!(found.ends_with("myapp.lock"));
    }

    #[test]
    fn falls_back_to_single_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("hello.lock"), "").unwrap();
        assert!(
            find_lock(dir.path(), "jarexec")
                .unwrap()
                .ends_with("hello.lock")
        );
    }

    #[test]
    fn ambiguous_or_missing_lock_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_lock(dir.path(), "x").is_err());
        std::fs::write(dir.path().join("a.lock"), "").unwrap();
        std::fs::write(dir.path().join("b.lock"), "").unwrap();
        assert!(find_lock(dir.path(), "x").is_err());
    }
}
