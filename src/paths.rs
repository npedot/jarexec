//! Risoluzione di cache e lockfile rispetto alla posizione dell'eseguibile.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const CACHE_ENV: &str = "JAREXEC_CACHE";

/// Cartella che contiene l'eseguibile, con i symlink risolti.
pub fn exe_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("cannot determine the executable path")?;
    let exe = exe.canonicalize().unwrap_or(exe);
    Ok(exe.parent().unwrap_or(Path::new(".")).to_path_buf())
}

/// Priorità: `--cache-dir`, poi `JAREXEC_CACHE`, poi `.jarexec/` accanto all'eseguibile.
pub fn cache_dir(arg: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = arg {
        return Ok(dir);
    }
    if let Some(dir) = std::env::var_os(CACHE_ENV).filter(|v| !v.is_empty()) {
        return Ok(dir.into());
    }
    Ok(exe_dir()?.join(".jarexec"))
}

/// `<nome-eseguibile>.lock` accanto al binario, altrimenti l'unico `*.lock` presente.
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
