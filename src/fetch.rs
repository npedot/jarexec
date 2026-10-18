//! Download and caching of lockfile dependencies.
//!
//! Invariant: a file in `repo/` exists only if its SHA-256 has already been verified.
//! Writes always go to a `.part` file, renamed only after verification, under a
//! per-artifact file lock, so concurrent instances or interrupted downloads never
//! leave corrupt files in the cache.

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};
use indicatif::ProgressBar;
use reqwest::StatusCode;
use reqwest::blocking::Client;

use crate::lockfile::{Dep, Repository};
use crate::{hash, http};

pub const DEFAULT_THREADS: usize = 6;

/// Cache path and origin of a dependency.
type Fetched = Result<(PathBuf, Source)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Cache,
    M2,
    Download,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub cached: usize,
    pub from_m2: usize,
    pub downloaded: usize,
    /// Bytes copied or downloaded in this run.
    pub bytes: u64,
}

pub struct Fetcher {
    client: Client,
    repo_dir: PathBuf,
    lock_dir: PathBuf,
    m2: Option<PathBuf>,
    repos: Vec<Repository>,
}

impl Fetcher {
    pub fn new(cache_root: &Path, repos: &[Repository]) -> Result<Self> {
        Ok(Self::with_client(
            http::client()?,
            cache_root,
            repos,
            default_m2(),
        ))
    }

    fn with_client(
        client: Client,
        cache_root: &Path,
        repos: &[Repository],
        m2: Option<PathBuf>,
    ) -> Self {
        Self {
            client,
            repo_dir: cache_root.join("repo"),
            lock_dir: cache_root.join("locks"),
            m2,
            repos: repos.to_vec(),
        }
    }

    pub fn local_path(&self, dep: &Dep) -> Result<PathBuf> {
        Ok(self.repo_dir.join(repo_path(&dep.coord)?))
    }

    /// Cache paths if *all* dependencies are already there, with no network or HTTP client.
    /// Size is enough: only verified files ever enter the cache.
    pub fn cached_paths(cache_root: &Path, deps: &[Dep]) -> Option<Vec<PathBuf>> {
        let repo_dir = cache_root.join("repo");
        deps.iter()
            .map(|d| {
                let path = repo_dir.join(repo_path(&d.coord).ok()?);
                is_complete(&path, d).then_some(path)
            })
            .collect()
    }

    /// Brings every dependency into the cache and returns their paths in lock order.
    /// Keeps going when some fail, then reports all errors together.
    pub fn fetch_all(&self, deps: &[Dep], threads: usize) -> Result<(Vec<PathBuf>, Report)> {
        fs::create_dir_all(&self.lock_dir)
            .with_context(|| format!("creating {}", self.lock_dir.display()))?;

        let missing_bytes = deps
            .iter()
            .filter(|d| !self.local_path(d).is_ok_and(|p| is_complete(&p, d)))
            .map(|d| d.size)
            .sum();
        let progress = http::progress_bar(missing_bytes, "dependencies");

        let next = AtomicUsize::new(0);
        let results: Vec<Mutex<Option<Fetched>>> = deps.iter().map(|_| Mutex::new(None)).collect();
        std::thread::scope(|s| {
            for _ in 0..threads.clamp(1, deps.len().max(1)) {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(dep) = deps.get(i) else { break };
                        let result = self.ensure(dep, &progress);
                        *results[i].lock().unwrap() = Some(result);
                    }
                });
            }
        });
        progress.finish_and_clear();

        let mut paths = Vec::with_capacity(deps.len());
        let mut report = Report::default();
        let mut errors = Vec::new();
        for (dep, slot) in deps.iter().zip(results) {
            match slot
                .into_inner()
                .unwrap()
                .expect("every dependency is processed")
            {
                Ok((path, source)) => {
                    match source {
                        Source::Cache => report.cached += 1,
                        Source::M2 => report.from_m2 += 1,
                        Source::Download => report.downloaded += 1,
                    }
                    if source != Source::Cache {
                        report.bytes += dep.size;
                    }
                    paths.push(path);
                }
                Err(e) => errors.push(format!("{}: {e:#}", dep.coord)),
            }
        }
        if !errors.is_empty() {
            bail!(
                "{} dependencies unavailable:\n  {}",
                errors.len(),
                errors.join("\n  ")
            );
        }
        Ok((paths, report))
    }

    fn ensure(&self, dep: &Dep, progress: &ProgressBar) -> Fetched {
        ensure!(
            dep.sha256.len() == 64 && dep.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid sha256 in lockfile"
        );
        let target = self.local_path(dep)?;
        if is_complete(&target, dep) {
            return Ok((target, Source::Cache));
        }

        let lock_path = self.lock_dir.join(format!("{}.lock", dep.sha256));
        let mut lock = fd_lock::RwLock::new(
            open_lock_file(&lock_path)
                .with_context(|| format!("creating {}", lock_path.display()))?,
        );
        let _guard = lock.write().context("acquiring the file lock")?;
        // another instance may have completed it while we waited for the lock
        if is_complete(&target, dep) {
            return Ok((target, Source::Cache));
        }

        let dir = target.parent().expect("cache path always has a parent");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let part = part_path(&target);

        if self.copy_from_m2(dep, &part, progress)? {
            fs::rename(&part, &target)?;
            return Ok((target, Source::M2));
        }

        let rel = repo_path(&dep.coord)?;
        let mut failures = Vec::new();
        for repo in &self.repos {
            let url = format!("{}/{rel}", repo.url.trim_end_matches('/'));
            match self.download(&url, dep, &part, progress) {
                Ok(true) => {
                    fs::rename(&part, &target)?;
                    return Ok((target, Source::Download));
                }
                Ok(false) => failures.push(format!("{}: not found", repo.id)),
                Err(e) => failures.push(format!("{}: {e:#}", repo.id)),
            }
            let _ = fs::remove_file(&part);
        }
        if failures.is_empty() {
            bail!("no repository configured in the lockfile");
        }
        bail!("{}", failures.join("; "))
    }

    /// Copies from `~/.m2` if present with the same hash. `false` if not usable.
    fn copy_from_m2(&self, dep: &Dep, part: &Path, progress: &ProgressBar) -> Result<bool> {
        let Some(m2) = &self.m2 else { return Ok(false) };
        let Ok(src) = File::open(m2.join(repo_path(&dep.coord)?)) else {
            return Ok(false);
        };
        let out = File::create(part)?;
        let (sha, _) = hash::copy_hashed(src, &out, |_| {})?;
        if sha != dep.sha256 {
            // e.g. a SNAPSHOT rebuilt locally: ignore it and download from the repository
            drop(out);
            fs::remove_file(part)?;
            return Ok(false);
        }
        out.sync_all()?;
        progress.inc(dep.size);
        Ok(true)
    }

    /// `Ok(false)` if the repository answers 404.
    fn download(&self, url: &str, dep: &Dep, part: &Path, progress: &ProgressBar) -> Result<bool> {
        let resp = self.client.get(url).send()?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let out = File::create(part)?;
        let (sha, _) = hash::copy_hashed(resp, &out, |n| progress.inc(n))?;
        if sha != dep.sha256 {
            bail!("SHA-256 mismatch: expected {}, got {sha}", dep.sha256);
        }
        out.sync_all()?;
        Ok(true)
    }
}

/// Coordinates `group:artifact:type[:classifier]:version` → relative path in the Maven repository.
pub fn repo_path(coord: &str) -> Result<String> {
    let parts: Vec<&str> = coord.split(':').collect();
    let (group, artifact, kind, classifier, version) = match parts[..] {
        [g, a, t, v] => (g, a, t, None, v),
        [g, a, t, c, v] => (g, a, t, Some(c), v),
        _ => bail!("invalid coordinates: {coord}"),
    };
    // the lockfile ships with the app, but must not be able to write outside the cache
    ensure!(
        parts
            .iter()
            .all(|p| !p.is_empty() && *p != ".." && !p.contains(['/', '\\'])),
        "invalid coordinates: {coord}"
    );
    let ext = match kind {
        "jar" | "bundle" | "maven-plugin" | "ejb" | "test-jar" => "jar",
        other => other,
    };
    let classifier = match (classifier, kind) {
        (Some(c), _) => format!("-{c}"),
        (None, "test-jar") => "-tests".into(),
        _ => String::new(),
    };
    Ok(format!(
        "{}/{artifact}/{version}/{artifact}-{version}{classifier}.{ext}",
        group.replace('.', "/")
    ))
}

/// Opens (or creates) a file used only as a lock, without truncating it: on Windows
/// truncating a file locked by another instance can fail.
pub(crate) fn open_lock_file(path: &Path) -> std::io::Result<File> {
    fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

fn is_complete(path: &Path, dep: &Dep) -> bool {
    path.metadata().is_ok_and(|m| m.len() == dep.size)
}

/// `x.jar` → `x.jar.<pid>.part`, in the same directory so the rename is atomic.
fn part_path(target: &Path) -> PathBuf {
    let mut name = OsString::from(target.as_os_str());
    name.push(format!(".{}.part", std::process::id()));
    name.into()
}

fn default_m2() -> Option<PathBuf> {
    dirs::home_dir()
        .map(|h| h.join(".m2").join("repository"))
        .filter(|p| p.is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{client, serve};

    fn dep(coord: &str, content: &[u8]) -> Dep {
        let (sha256, size) = hash::sha256_reader(content).unwrap();
        Dep {
            coord: coord.into(),
            sha256,
            size,
            bundled: None,
        }
    }

    fn repo(id: &str, url: &str) -> Repository {
        Repository {
            id: id.into(),
            url: url.into(),
        }
    }

    fn fetcher(cache: &Path, repos: &[Repository], m2: Option<PathBuf>) -> Fetcher {
        Fetcher::with_client(client(), cache, repos, m2)
    }

    const LIB: &[u8] = b"jar contents";
    const LIB_PATH: &str = "/com/acme/lib/1.0/lib-1.0.jar";

    #[test]
    fn maps_coordinates_to_repository_paths() {
        assert_eq!(
            repo_path("org.slf4j:slf4j-api:jar:2.0.16").unwrap(),
            "org/slf4j/slf4j-api/2.0.16/slf4j-api-2.0.16.jar"
        );
        assert_eq!(
            repo_path("org.openjfx:javafx-base:jar:linux:21.0.4").unwrap(),
            "org/openjfx/javafx-base/21.0.4/javafx-base-21.0.4-linux.jar"
        );
        assert_eq!(
            repo_path("com.acme:core:test-jar:tests:1.0").unwrap(),
            "com/acme/core/1.0/core-1.0-tests.jar"
        );
    }

    #[test]
    fn rejects_path_traversal_in_coordinates() {
        assert!(repo_path("com.acme:..:jar:1.0").is_err());
        assert!(repo_path("com.acme:lib:jar:../../x").is_err());
        assert!(repo_path("com.acme:lib").is_err());
    }

    #[test]
    fn downloads_once_then_uses_cache() {
        let server = serve(&[(LIB_PATH, LIB)]);
        let cache = tempfile::tempdir().unwrap();
        let f = fetcher(cache.path(), &[repo("r", &server.url)], None);
        let deps = [dep("com.acme:lib:jar:1.0", LIB)];

        let (paths, report) = f.fetch_all(&deps, 2).unwrap();
        assert_eq!(fs::read(&paths[0]).unwrap(), LIB);
        assert_eq!((report.downloaded, report.bytes), (1, LIB.len() as u64));

        let (_, report) = f.fetch_all(&deps, 2).unwrap();
        assert_eq!(report.cached, 1);
        assert_eq!(server.hits(), 1);
    }

    #[test]
    fn hash_mismatch_leaves_nothing_in_cache() {
        let server = serve(&[(LIB_PATH, b"jar manomesso")]);
        let cache = tempfile::tempdir().unwrap();
        let f = fetcher(cache.path(), &[repo("r", &server.url)], None);
        let deps = [dep("com.acme:lib:jar:1.0", LIB)];

        let err = format!("{:#}", f.fetch_all(&deps, 1).unwrap_err());
        assert!(err.contains("SHA-256 mismatch"), "{err}");
        let dir = f
            .local_path(&deps[0])
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        assert_eq!(
            fs::read_dir(dir).unwrap().count(),
            0,
            "no .jar or .part left"
        );
    }

    #[test]
    fn falls_back_to_next_repository_on_404() {
        let empty = serve(&[]);
        let full = serve(&[(LIB_PATH, LIB)]);
        let cache = tempfile::tempdir().unwrap();
        let repos = [repo("primo", &empty.url), repo("secondo", &full.url)];
        let f = fetcher(cache.path(), &repos, None);

        let (_, report) = f.fetch_all(&[dep("com.acme:lib:jar:1.0", LIB)], 1).unwrap();
        assert_eq!(report.downloaded, 1);
        assert_eq!(empty.hits(), 1);
    }

    #[test]
    fn reports_every_missing_dependency() {
        let server = serve(&[]);
        let cache = tempfile::tempdir().unwrap();
        let f = fetcher(cache.path(), &[repo("central", &server.url)], None);
        let deps = [
            dep("com.acme:a:jar:1.0", b"a"),
            dep("com.acme:b:jar:1.0", b"b"),
        ];

        let err = format!("{:#}", f.fetch_all(&deps, 2).unwrap_err());
        assert!(err.contains("2 dependencies unavailable"), "{err}");
        assert!(
            err.contains("com.acme:a:jar:1.0: central: not found"),
            "{err}"
        );
    }

    #[test]
    fn reuses_m2_only_when_hash_matches() {
        let server = serve(&[("/com/acme/b/1.0/b-1.0.jar", b"b originale")]);
        let m2 = tempfile::tempdir().unwrap();
        for (name, content) in [("a", &b"a"[..]), ("b", b"b ricompilato")] {
            let dir = m2.path().join(format!("com/acme/{name}/1.0"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{name}-1.0.jar")), content).unwrap();
        }
        let cache = tempfile::tempdir().unwrap();
        let f = fetcher(
            cache.path(),
            &[repo("r", &server.url)],
            Some(m2.path().into()),
        );
        let deps = [
            dep("com.acme:a:jar:1.0", b"a"),
            dep("com.acme:b:jar:1.0", b"b originale"),
        ];

        let (paths, report) = f.fetch_all(&deps, 1).unwrap();
        assert_eq!((report.from_m2, report.downloaded), (1, 1));
        assert_eq!(fs::read(&paths[1]).unwrap(), b"b originale");
    }

    #[test]
    fn concurrent_fetchers_download_once() {
        let server = serve(&[(LIB_PATH, LIB)]);
        let cache = tempfile::tempdir().unwrap();
        let deps = [dep("com.acme:lib:jar:1.0", LIB)];
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let f = fetcher(cache.path(), &[repo("r", &server.url)], None);
                    f.fetch_all(&deps, 1).unwrap();
                });
            }
        });
        assert_eq!(server.hits(), 1);
    }
}
