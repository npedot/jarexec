//! Cache cleanup: `jarexec clean` and the automatic pass after an update.
//!
//! The markers (`<id>.ok`) decide what stays: for every lockfile the `KEEP` most
//! recent ones survive, and with them the dependencies and the JRE they use.
//! Full preparations hold `locks/cache.lock` shared; cleanup takes it exclusively,
//! so it never removes files a preparation is about to put on the classpath.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::fetch::open_lock_file;
use crate::launch::read_marker;

/// Markers kept per lockfile: the current version and the previous one (rollback).
pub const KEEP: usize = 2;
pub const REPO_DIR: &str = "repo";
pub const JRE_DIR: &str = "jre";
const LOCK_DIR: &str = "locks";
/// Temp files younger than this may belong to a process that does not take the cache lock.
const GRACE: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub files: u64,
    pub bytes: u64,
}

/// Lock shared by full preparations and taken exclusively by cleanup.
pub fn cache_lock(cache: &Path) -> Result<fd_lock::RwLock<File>> {
    let dir = cache.join(LOCK_DIR);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("cache.lock");
    let file = open_lock_file(&path).with_context(|| format!("creating {}", path.display()))?;
    Ok(fd_lock::RwLock::new(file))
}

/// Removes what no recent marker uses. `None` if a preparation is in progress.
/// Files that cannot be removed (e.g. in use on Windows) are skipped.
pub fn unused(cache: &Path) -> Result<Option<Report>> {
    if !cache.is_dir() {
        return Ok(Some(Report::default()));
    }
    let mut lock = cache_lock(cache)?;
    let Ok(_guard) = lock.try_write() else {
        return Ok(None);
    };
    let mut report = Report::default();

    // markers: invalid ones go, valid ones are grouped by lockfile
    let mut by_lock: HashMap<PathBuf, Vec<(SystemTime, PathBuf, Vec<PathBuf>)>> = HashMap::new();
    let mut stale_tmp = Vec::new();
    for path in entries(cache) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        if name.ends_with(".tmp") {
            stale_tmp.push(path);
            continue;
        }
        if !name.ends_with(".ok") {
            continue;
        }
        match read_marker(&path) {
            Some(m) if m.java.is_file() && m.app_jar.is_file() => {
                let mtime = modified(&path).unwrap_or(SystemTime::UNIX_EPOCH);
                by_lock
                    .entry(m.lock)
                    .or_default()
                    .push((mtime, path, m.uses));
            }
            _ => remove_marker(&path, &mut report),
        }
    }
    for path in stale_tmp {
        if is_stale(&path) {
            remove(&path, &mut report);
        }
    }

    let mut used = HashSet::new();
    let mut kept = HashSet::new();
    for mut markers in by_lock.into_values() {
        markers.sort_by_key(|(mtime, _, _)| std::cmp::Reverse(*mtime));
        for (i, (_, path, uses)) in markers.into_iter().enumerate() {
            if i < KEEP {
                used.extend(uses.into_iter().map(|u| cache.join(u)));
                kept.insert(path.with_extension(""));
            } else {
                remove_marker(&path, &mut report);
            }
        }
    }
    // argfiles left without their marker
    for path in entries(cache) {
        if path.extension().is_some_and(|e| e == "args") && !kept.contains(&path.with_extension(""))
        {
            remove(&path, &mut report);
        }
    }

    let repo = cache.join(REPO_DIR);
    let mut files = Vec::new();
    walk(&repo, &mut files);
    for file in files {
        let partial = file.to_string_lossy().ends_with(".part");
        if !used.contains(&file) && (!partial || is_stale(&file)) {
            remove(&file, &mut report);
        }
    }
    remove_empty_dirs(&repo);

    for dir in entries(&cache.join(JRE_DIR)) {
        let staging = dir
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with('.'));
        let unused = if staging {
            is_stale(&dir)
        } else {
            !used.contains(&dir)
        };
        if unused {
            remove(&dir, &mut report);
        }
    }
    Ok(Some(report))
}

/// Removes the whole cache content except `locks/` (held open by other launches).
/// Only names jarexec writes are touched, so a mistaken `--cache-dir` is harmless.
pub fn all(cache: &Path) -> Result<Option<Report>> {
    if !cache.is_dir() {
        return Ok(Some(Report::default()));
    }
    let mut lock = cache_lock(cache)?;
    let Ok(_guard) = lock.try_write() else {
        return Ok(None);
    };
    let mut report = Report::default();
    for path in entries(cache) {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let ours = name == REPO_DIR
            || name == JRE_DIR
            || [".ok", ".args", ".tmp"]
                .iter()
                .any(|ext| name.ends_with(ext));
        if ours {
            remove(&path, &mut report);
        }
    }
    Ok(Some(report))
}

fn remove_marker(path: &Path, report: &mut Report) {
    remove(path, report);
    remove(&path.with_extension("args"), report);
}

/// Removes a file or a directory tree, counting only what was actually removed.
fn remove(path: &Path, report: &mut Report) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_dir() {
        let mut files = Vec::new();
        walk(path, &mut files);
        for file in &files {
            remove(file, report);
        }
        remove_empty_dirs(path);
        let _ = fs::remove_dir(path);
    } else if fs::remove_file(path).is_ok() {
        report.files += 1;
        report.bytes += meta.len();
    }
}

/// Every file under `dir`, without following symlinks.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for path in entries(dir) {
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => walk(&path, out),
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

/// Removes the empty directories under `dir` (not `dir` itself).
fn remove_empty_dirs(dir: &Path) {
    for path in entries(dir) {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
            remove_empty_dirs(&path);
            let _ = fs::remove_dir(&path); // fails if not empty: fine
        }
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

fn is_stale(path: &Path) -> bool {
    modified(path)
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{self, tests::fixture};

    fn set_age(path: &Path, secs: u64) {
        let t = SystemTime::now() - Duration::from_secs(secs);
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    fn markers(cache: &Path) -> usize {
        entries(cache)
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "ok"))
            .count()
    }

    #[test]
    fn keeps_what_recent_markers_use() {
        let f = fixture("21.0.4", true);
        let used_lib = f.cache.join("repo/com/acme/lib/1.0/lib-1.0.jar");
        launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();

        // dependency of a removed version, an old JRE, a fresh and a stale download
        let old_lib = f.cache.join("repo/com/acme/old/0.9/old-0.9.jar");
        fs::create_dir_all(old_lib.parent().unwrap()).unwrap();
        fs::write(&old_lib, b"old!").unwrap();
        let old_jre = f.cache.join("jre/temurin-17.0.1-jre-linux-x64");
        fs::create_dir_all(old_jre.join("bin")).unwrap();
        fs::write(old_jre.join("bin/java"), b"").unwrap();
        let fresh_part = f.cache.join("repo/com/acme/lib/1.0/x.jar.1.part");
        fs::write(&fresh_part, b"").unwrap();
        let stale_part = f.cache.join("repo/com/acme/lib/1.0/y.jar.2.part");
        fs::write(&stale_part, b"").unwrap();
        set_age(&stale_part, 2 * 24 * 3600);

        let report = unused(&f.cache).unwrap().unwrap();
        assert!(used_lib.is_file());
        assert!(fresh_part.is_file());
        assert!(!old_lib.exists() && !old_lib.parent().unwrap().exists());
        assert!(!old_jre.exists());
        assert!(!stale_part.exists());
        assert_eq!(report.files, 3);
        assert_eq!(report.bytes, 4);
        // the launch is still on the fast path
        assert!(
            launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false)
                .unwrap()
                .fast
        );
    }

    #[test]
    fn keeps_the_latest_markers_per_lock() {
        let f = fixture("21.0.4", true);
        let text = fs::read_to_string(&f.lock).unwrap();
        for (age, heap) in [(300, "-Xmx1m"), (200, "-Xmx2m"), (100, "-Xmx3m")] {
            fs::write(&f.lock, text.replace("-Xmx64m", heap)).unwrap();
            let p = launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
            let argfile = PathBuf::from(p.args[0].strip_prefix('@').unwrap());
            set_age(&argfile.with_extension("ok"), age);
        }
        fs::write(f.cache.join("deadbeef.ok"), "not a marker").unwrap();
        fs::write(f.cache.join("deadbeef.args"), "").unwrap();
        assert_eq!(markers(&f.cache), 4);

        unused(&f.cache).unwrap().unwrap();
        assert_eq!(markers(&f.cache), KEEP);
        assert!(!f.cache.join("deadbeef.args").exists());
        // the current version stays on the fast path, the oldest one does not
        assert!(
            launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false)
                .unwrap()
                .fast
        );
        fs::write(&f.lock, text.replace("-Xmx64m", "-Xmx1m")).unwrap();
        assert!(
            !launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false)
                .unwrap()
                .fast
        );
    }

    #[test]
    fn skips_while_a_preparation_holds_the_lock() {
        let f = fixture("21.0.4", true);
        launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        let lock = cache_lock(&f.cache).unwrap();
        let _shared = lock.read().unwrap();
        assert_eq!(unused(&f.cache).unwrap(), None);
        assert_eq!(all(&f.cache).unwrap(), None);
    }

    #[test]
    fn all_removes_only_jarexec_entries() {
        let f = fixture("21.0.4", true);
        launch::prepare(&f.lock, &f.cache, Some(f.java.clone()), false).unwrap();
        fs::write(f.cache.join("notes.txt"), b"not ours").unwrap();

        let report = all(&f.cache).unwrap().unwrap();
        assert!(report.files >= 3, "{report:?}");
        assert!(!f.cache.join(REPO_DIR).exists());
        assert_eq!(markers(&f.cache), 0);
        assert!(f.cache.join("notes.txt").is_file());
        assert!(f.cache.join(LOCK_DIR).is_dir());
    }
}
