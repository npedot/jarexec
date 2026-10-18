//! Scelta del Java: un'installazione di sistema compatibile, altrimenti un JRE
//! Temurin scaricato da Adoptium nella cache (`jre/`).
//!
//! Come per le dipendenze, un JRE compare in `jre/<nome>` solo a installazione
//! completa e verificata: si estrae in una cartella temporanea e poi si rinomina.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::fetch::open_lock_file;
use crate::lockfile::{Java, JavaImage, JavaSource};
use crate::platform::{Os, Platform};
use crate::{hash, http};

pub const JAVA_HOME_ENV: &str = "JAREXEC_JAVA_HOME";
const ADOPTIUM_API: &str = "https://api.adoptium.net";

/// Versione Java normalizzata: `1.8.0_402` → `8.0.402`, `21.0.10` → `21.0.10`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u32>);

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let mut nums: Vec<u32> = s
            .split(|c: char| !c.is_ascii_digit())
            .filter(|p| !p.is_empty())
            .map_while(|p| p.parse().ok())
            .collect();
        if nums.len() > 1 && nums[0] == 1 {
            nums.remove(0); // schema pre-Java 9
        }
        (!nums.is_empty()).then_some(Self(nums))
    }

    pub fn feature(&self) -> u32 {
        self.0[0]
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u32::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// Requisiti dal blocco `[java]` del lockfile.
#[derive(Debug, Clone)]
pub struct Requirement {
    pub min: u32,
    pub max: Option<u32>,
    pub source: JavaSource,
    pub image: JavaImage,
    pub vendor: String,
}

impl Requirement {
    pub fn from_lock(java: &Java) -> Result<Self> {
        let feature = |s: &str| {
            Version::parse(s)
                .map(|v| v.feature())
                .with_context(|| format!("invalid Java version in lockfile: {s}"))
        };
        Ok(Self {
            min: feature(&java.version)?,
            max: java.max.as_deref().map(feature).transpose()?,
            source: java.source,
            image: java.image,
            vendor: java.vendor.clone(),
        })
    }

    pub fn describe(&self) -> String {
        match self.max {
            Some(max) => format!("Java {}-{max}", self.min),
            None => format!("Java {}+", self.min),
        }
    }
}

/// Da dove arriva un candidato. L'ordine delle varianti è la priorità di scelta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    Override,
    Cache,
    JavaHome,
    Path,
    System,
    Downloaded,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Override => "override",
            Origin::Cache => "cache",
            Origin::JavaHome => "JAVA_HOME",
            Origin::Path => "PATH",
            Origin::System => "system",
            Origin::Downloaded => "downloaded",
        })
    }
}

#[derive(Debug, Clone)]
pub struct JavaInstall {
    pub home: PathBuf,
    pub version: Version,
    pub arch: Option<String>,
}

impl JavaInstall {
    pub fn java_bin(&self) -> PathBuf {
        bin(&self.home, "java")
    }
}

#[derive(Debug, Clone)]
pub struct Selection {
    pub origin: Origin,
    pub install: JavaInstall,
}

/// Un'installazione valutata: compatibile, oppure scartata con il motivo.
#[derive(Debug)]
pub struct Candidate {
    pub origin: Origin,
    pub home: PathBuf,
    pub verdict: Result<JavaInstall, String>,
}

#[derive(Debug)]
pub struct Evaluation {
    pub candidates: Vec<Candidate>,
    pub chosen: Option<usize>,
}

impl Evaluation {
    pub fn selection(&self) -> Option<Selection> {
        let c = &self.candidates[self.chosen?];
        Some(Selection {
            origin: c.origin,
            install: c.verdict.clone().ok()?,
        })
    }
}

pub struct JavaResolver {
    client: Client,
    api: String,
    jre_dir: PathBuf,
    lock_dir: PathBuf,
    platform: Platform,
    override_home: Option<PathBuf>,
    /// Installazioni di sistema note (JAVA_HOME, PATH, cartelle tipiche, registro).
    system: Vec<(Origin, PathBuf)>,
}

impl JavaResolver {
    /// `override_home`: `--java-home`; se assente si legge `JAREXEC_JAVA_HOME`.
    pub fn new(cache_root: &Path, override_home: Option<PathBuf>) -> Result<Self> {
        let platform = Platform::current()?;
        let override_home = override_home.or_else(|| {
            std::env::var_os(JAVA_HOME_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        });
        Ok(Self {
            client: http::client()?,
            api: ADOPTIUM_API.into(),
            jre_dir: cache_root.join("jre"),
            lock_dir: cache_root.join("locks"),
            platform,
            override_home,
            system: system_candidates(platform.os),
        })
    }

    pub fn platform(&self) -> Platform {
        self.platform
    }

    /// Valuta tutti i candidati senza scaricare nulla.
    pub fn find(&self, req: &Requirement) -> Evaluation {
        let mut homes: Vec<(Origin, PathBuf)> = Vec::new();
        if let Some(home) = &self.override_home {
            homes.push((Origin::Override, home.clone()));
        } else {
            if req.source != JavaSource::System {
                homes.extend(self.cached_homes().into_iter().map(|h| (Origin::Cache, h)));
            }
            if req.source != JavaSource::Download {
                homes.extend(self.system.iter().cloned());
            }
        }

        let mut seen = HashSet::new();
        let candidates: Vec<Candidate> = homes
            .into_iter()
            .filter(|(_, h)| seen.insert(h.canonicalize().unwrap_or_else(|_| h.clone())))
            .map(|(origin, home)| {
                let verdict = inspect(&home).and_then(|i| {
                    check(&i, req, self.platform)?;
                    Ok(i)
                });
                Candidate {
                    origin,
                    home,
                    verdict,
                }
            })
            .collect();

        // priorità per origine; a parità, la feature più vicina alla minima
        // (quella con cui l'app è stata compilata), poi l'aggiornamento più recente
        let chosen = candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.verdict.as_ref().ok().map(|inst| (i, c.origin, inst)))
            .min_by_key(|(_, origin, inst)| {
                (
                    *origin,
                    inst.version.feature(),
                    Reverse(inst.version.clone()),
                )
            })
            .map(|(i, _, _)| i);
        Evaluation { candidates, chosen }
    }

    /// Java da usare: un candidato compatibile, oppure (se la politica lo consente) un download.
    pub fn resolve(&self, req: &Requirement) -> Result<Selection> {
        let eval = self.find(req);
        if let Some(sel) = eval.selection() {
            return Ok(sel);
        }
        let reasons = || {
            eval.candidates
                .iter()
                .filter_map(|c| {
                    let reason = c.verdict.as_ref().err()?;
                    Some(format!("\n  {} {}: {reason}", c.origin, c.home.display()))
                })
                .collect::<String>()
        };
        if self.override_home.is_some() {
            bail!(
                "the specified Java is not usable for {}:{}",
                req.describe(),
                reasons()
            );
        }
        if req.source == JavaSource::System {
            bail!(
                "no system Java compatible with {} (source = \"system\"){}",
                req.describe(),
                reasons()
            );
        }
        Ok(Selection {
            origin: Origin::Downloaded,
            install: self.download(req)?,
        })
    }

    fn cached_homes(&self) -> Vec<PathBuf> {
        subdirs(&self.jre_dir)
            .into_iter()
            .filter(|p| {
                !p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            })
            .map(|p| mac_home(&p))
            .collect()
    }

    fn download(&self, req: &Requirement) -> Result<JavaInstall> {
        ensure!(
            req.vendor == "temurin",
            "vendor `{}` not supported for download (only `temurin`)",
            req.vendor
        );
        let Platform { os, arch } = self.platform;
        let image = req.image.as_str();
        fs::create_dir_all(&self.jre_dir)?;
        fs::create_dir_all(&self.lock_dir)?;

        let lock_path = self
            .lock_dir
            .join(format!("jre-{}-{image}-{os}-{arch}.lock", req.min));
        let mut lock = fd_lock::RwLock::new(open_lock_file(&lock_path)?);
        let _guard = lock.write().context("acquiring the file lock")?;
        // un'altra istanza può averlo installato mentre aspettavamo
        if let Some(sel) = self.find(req).selection() {
            return Ok(sel.install);
        }

        let url = format!(
            "{}/v3/assets/latest/{}/hotspot?architecture={arch}&image_type={image}&os={os}&vendor=eclipse",
            self.api, req.min
        );
        let assets: Vec<Asset> = self
            .client
            .get(&url)
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.json())
            .with_context(|| format!("querying Adoptium ({url})"))?;
        let Some(asset) = assets.into_iter().next() else {
            bail!(
                "Adoptium has no Temurin {} {image} for {os}-{arch}{}",
                req.min,
                if req.image == JavaImage::Jre {
                    " (try image = \"jdk\")"
                } else {
                    ""
                }
            );
        };
        let pkg = &asset.binary.package;
        let pid = std::process::id();

        let archive = self.jre_dir.join(format!(".{}.{pid}.part", pkg.name));
        let result = self.download_archive(pkg, &archive);
        let staging = self.jre_dir.join(format!(".tmp-{pid}"));
        let result = result.and_then(|()| {
            let _ = fs::remove_dir_all(&staging);
            fs::create_dir_all(&staging)?;
            extract(&archive, pkg.name.ends_with(".zip"), &staging)
        });
        let _ = fs::remove_file(&archive);

        let result = result.and_then(|()| {
            let top = single_root(&staging)?;
            let name = format!(
                "{}-{}-{image}-{os}-{arch}",
                req.vendor,
                asset.release_name.trim_start_matches("jdk-")
            );
            let dest = self.jre_dir.join(name);
            if dest.exists() {
                fs::remove_dir_all(&dest)?; // resto incompleto di una versione precedente
            }
            fs::rename(&top, &dest)?;
            Ok(dest)
        });
        let _ = fs::remove_dir_all(&staging);
        let dest = result?;

        let install = inspect(&mac_home(&dest))
            .and_then(|i| {
                check(&i, req, self.platform)?;
                Ok(i)
            })
            .map_err(|e| anyhow::anyhow!("downloaded JRE is not usable: {e}"))?;
        Ok(install)
    }

    fn download_archive(&self, pkg: &Package, part: &Path) -> Result<()> {
        let resp = self
            .client
            .get(&pkg.link)
            .send()
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("downloading {}", pkg.link))?;
        let progress = http::progress_bar(pkg.size, "JRE");
        let out = File::create(part)?;
        let copied = hash::copy_hashed(resp, &out, |n| progress.inc(n));
        progress.finish_and_clear();
        let (sha, _) = copied.with_context(|| format!("downloading {}", pkg.link))?;
        ensure!(
            sha.eq_ignore_ascii_case(&pkg.checksum),
            "JRE SHA-256 mismatch: expected {}, got {sha}",
            pkg.checksum
        );
        out.sync_all()?;
        Ok(())
    }
}

#[derive(Deserialize)]
struct Asset {
    binary: Binary,
    release_name: String,
}

#[derive(Deserialize)]
struct Binary {
    package: Package,
}

#[derive(Deserialize)]
struct Package {
    checksum: String,
    link: String,
    name: String,
    size: u64,
}

/// Versione e architettura dal file `release`; se manca, chiedendole alla JVM.
fn inspect(home: &Path) -> Result<JavaInstall, String> {
    if !bin(home, "java").is_file() {
        return Err("bin/java not found".into());
    }
    if let Ok(text) = fs::read_to_string(home.join("release")) {
        let props = parse_release(&text);
        if let Some(version) = props.get("JAVA_VERSION").and_then(|v| Version::parse(v)) {
            return Ok(JavaInstall {
                home: home.into(),
                version,
                arch: props.get("OS_ARCH").cloned(),
            });
        }
    }
    probe(home)
}

/// Righe `CHIAVE="valore"` del file `$JAVA_HOME/release`.
fn parse_release(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

/// Fallback lento (avvia una JVM): proprietà stampate da `-XshowSettings:properties`.
fn probe(home: &Path) -> Result<JavaInstall, String> {
    let out = Command::new(bin(home, "java"))
        .args(["-XshowSettings:properties", "-version"])
        .output()
        .map_err(|e| format!("running java failed: {e}"))?;
    let text = String::from_utf8_lossy(&out.stderr);
    let prop = |key: &str| {
        text.lines().find_map(|l| {
            let v = l.trim().strip_prefix(key)?.trim_start().strip_prefix('=')?;
            Some(v.trim().to_string())
        })
    };
    let version = prop("java.version")
        .and_then(|v| Version::parse(&v))
        .ok_or("cannot determine version")?;
    Ok(JavaInstall {
        home: home.into(),
        version,
        arch: prop("os.arch"),
    })
}

fn check(install: &JavaInstall, req: &Requirement, platform: Platform) -> Result<(), String> {
    let feature = install.version.feature();
    if feature < req.min {
        return Err(format!("Java {} < {} required", install.version, req.min));
    }
    if let Some(max) = req.max
        && feature > max
    {
        return Err(format!("Java {} > {max} maximum", install.version));
    }
    if let Some(arch) = &install.arch
        && !platform.arch.matches(arch)
    {
        return Err(format!("architecture {arch}, need {}", platform.arch));
    }
    if req.image == JavaImage::Jdk && !bin(&install.home, "javac").is_file() {
        return Err("is a JRE, a JDK is required".into());
    }
    Ok(())
}

fn bin(home: &Path, tool: &str) -> PathBuf {
    let name = if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    };
    home.join("bin").join(name)
}

/// Su macOS la JAVA_HOME è `<bundle>/Contents/Home`.
fn mac_home(dir: &Path) -> PathBuf {
    let home = dir.join("Contents").join("Home");
    if home.is_dir() { home } else { dir.into() }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn single_root(dir: &Path) -> Result<PathBuf> {
    let entries: Vec<PathBuf> = fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    match entries.as_slice() {
        [root] if root.is_dir() => Ok(root.clone()),
        _ => bail!("unexpected JRE archive: expected a single root directory"),
    }
}

/// `tar` e `zip` scartano da soli i path con `..` o assoluti.
fn extract(archive: &Path, is_zip: bool, dest: &Path) -> Result<()> {
    let file = File::open(archive)?;
    if is_zip {
        zip::ZipArchive::new(file)?.extract(dest)?;
    } else {
        tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(dest)?;
    }
    Ok(())
}

/// JAVA_HOME, `java` nel PATH, cartelle tipiche della piattaforma e (Windows) registro.
fn system_candidates(os: Os) -> Vec<(Origin, PathBuf)> {
    let mut found = Vec::new();
    if let Some(home) = std::env::var_os("JAVA_HOME").filter(|v| !v.is_empty()) {
        found.push((Origin::JavaHome, PathBuf::from(home)));
    }
    if let Some(home) = java_on_path() {
        found.push((Origin::Path, home));
    }
    for root in platform_roots(os) {
        found.extend(subdirs(&root).iter().map(|d| (Origin::System, mac_home(d))));
    }
    #[cfg(windows)]
    found.extend(registry_homes().into_iter().map(|h| (Origin::System, h)));
    found
}

/// Risale dal `java` nel PATH alla sua home, risolvendo i symlink
/// (es. `/usr/bin/java` → `/etc/alternatives/java` → `/usr/lib/jvm/.../bin/java`).
fn java_on_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| Some(p.parent()?.parent()?.to_path_buf()))
}

fn platform_roots(os: Os) -> Vec<PathBuf> {
    let user = dirs::home_dir();
    let mut roots: Vec<PathBuf> = match os {
        Os::Linux => ["/usr/lib/jvm", "/usr/java", "/opt/java", "/opt/jdk"]
            .map(PathBuf::from)
            .into(),
        Os::Mac => {
            let mut r = vec![PathBuf::from("/Library/Java/JavaVirtualMachines")];
            r.extend(
                user.iter()
                    .map(|h| h.join("Library/Java/JavaVirtualMachines")),
            );
            r
        }
        Os::Windows => ["ProgramFiles", "ProgramW6432"]
            .iter()
            .filter_map(std::env::var_os)
            .flat_map(|pf| {
                let pf = PathBuf::from(pf);
                [
                    "Eclipse Adoptium",
                    "Java",
                    "Microsoft",
                    "Zulu",
                    "Amazon Corretto",
                    "BellSoft",
                    "Semeru",
                ]
                .map(|vendor| pf.join(vendor))
            })
            .collect(),
    };
    if os != Os::Windows
        && let Some(h) = &user
    {
        roots.push(h.join(".sdkman/candidates/java"));
    }
    roots
}

#[cfg(windows)]
fn registry_homes() -> Vec<PathBuf> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let mut homes = Vec::new();
    for base in [
        r"SOFTWARE\JavaSoft\JDK",
        r"SOFTWARE\JavaSoft\JRE",
        r"SOFTWARE\JavaSoft\Java Development Kit",
        r"SOFTWARE\JavaSoft\Java Runtime Environment",
    ] {
        let Ok(key) = hklm.open_subkey(base) else {
            continue;
        };
        for version in key.enum_keys().flatten() {
            if let Ok(sub) = key.open_subkey(&version)
                && let Ok(home) = sub.get_value::<String, _>("JavaHome")
            {
                homes.push(PathBuf::from(home));
            }
        }
    }
    homes
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;
    use crate::test_util::{client, serve};

    fn platform() -> Platform {
        Platform::current().unwrap()
    }

    pub fn os_arch() -> &'static str {
        match platform().arch {
            crate::platform::Arch::X64 => "x86_64",
            crate::platform::Arch::Aarch64 => "aarch64",
        }
    }

    fn req(min: u32, max: Option<u32>, source: JavaSource) -> Requirement {
        Requirement {
            min,
            max,
            source,
            image: JavaImage::Jre,
            vendor: "temurin".into(),
        }
    }

    /// Finta installazione: `bin/java` (vuoto) + file `release`.
    pub fn fake_home(root: &Path, name: &str, version: &str, arch: &str) -> PathBuf {
        let home = root.join(name);
        fs::create_dir_all(home.join("bin")).unwrap();
        fs::write(bin(&home, "java"), "").unwrap();
        fs::write(
            home.join("release"),
            format!("JAVA_VERSION=\"{version}\"\nOS_ARCH=\"{arch}\"\n"),
        )
        .unwrap();
        home
    }

    fn resolver(cache: &Path, api: &str, system: Vec<(Origin, PathBuf)>) -> JavaResolver {
        JavaResolver {
            client: client(),
            api: api.into(),
            jre_dir: cache.join("jre"),
            lock_dir: cache.join("locks"),
            platform: platform(),
            override_home: None,
            system,
        }
    }

    #[test]
    fn parses_versions() {
        assert_eq!(Version::parse("21.0.10").unwrap().to_string(), "21.0.10");
        assert_eq!(Version::parse("1.8.0_402").unwrap().feature(), 8);
        assert_eq!(Version::parse("22-ea").unwrap().feature(), 22);
        assert!(Version::parse("").is_none());
        assert!(Version::parse("21.0.10").unwrap() > Version::parse("21.0.9").unwrap());
    }

    #[test]
    fn reads_release_file() {
        let props = parse_release(
            "JAVA_VERSION=\"21.0.4\"\nOS_ARCH=\"aarch64\"\nIMPLEMENTOR=\"Eclipse Adoptium\"\n",
        );
        assert_eq!(props["JAVA_VERSION"], "21.0.4");
        assert_eq!(props["IMPLEMENTOR"], "Eclipse Adoptium");
    }

    #[test]
    fn prefers_closest_feature_then_latest_update() {
        let root = tempfile::tempdir().unwrap();
        let a = os_arch();
        let system = ["17.0.12", "21.0.3", "21.0.10", "25.0.1"]
            .iter()
            .map(|v| (Origin::System, fake_home(root.path(), v, v, a)))
            .collect();
        let r = resolver(root.path(), "http://unused", system);

        let sel = r
            .find(&req(21, None, JavaSource::Auto))
            .selection()
            .unwrap();
        assert_eq!(sel.install.version.to_string(), "21.0.10");
        let sel = r
            .find(&req(22, None, JavaSource::Auto))
            .selection()
            .unwrap();
        assert_eq!(sel.install.version.to_string(), "25.0.1");
    }

    #[test]
    fn java_home_wins_over_scanned_installations() {
        let root = tempfile::tempdir().unwrap();
        let a = os_arch();
        let system = vec![
            (Origin::System, fake_home(root.path(), "s", "21.0.10", a)),
            (Origin::JavaHome, fake_home(root.path(), "jh", "25.0.1", a)),
        ];
        let r = resolver(root.path(), "http://unused", system);
        let sel = r
            .find(&req(21, None, JavaSource::Auto))
            .selection()
            .unwrap();
        assert_eq!(sel.origin, Origin::JavaHome);
    }

    #[test]
    fn rejects_wrong_arch_and_too_new_with_reasons() {
        let root = tempfile::tempdir().unwrap();
        let system = vec![
            (
                Origin::System,
                fake_home(root.path(), "x86", "21.0.1", "i386"),
            ),
            (
                Origin::System,
                fake_home(root.path(), "new", "26", os_arch()),
            ),
        ];
        let r = resolver(root.path(), "http://unused", system);
        let err = r
            .resolve(&req(21, Some(25), JavaSource::System))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("architecture i386"), "{msg}");
        assert!(msg.contains("Java 26 > 25 maximum"), "{msg}");
    }

    fn tar_gz_jre(top: &str, version: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::fast(),
        ));
        let mut add = |path: String, body: &[u8], mode: u32| {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(mode);
            header.set_cksum();
            builder.append_data(&mut header, path, body).unwrap();
        };
        let release = format!("JAVA_VERSION=\"{version}\"\nOS_ARCH=\"{}\"\n", os_arch());
        add(format!("{top}/release"), release.as_bytes(), 0o644);
        let java = if cfg!(windows) { "java.exe" } else { "java" };
        add(format!("{top}/bin/{java}"), b"#!/bin/sh\n", 0o755);
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// Finta API Adoptium per Temurin 21 JRE sulla piattaforma corrente + l'archivio.
    fn serve_adoptium(archive: &[u8], checksum: &str) -> crate::test_util::TestServer {
        let p = platform();
        let api_path = format!(
            "/v3/assets/latest/21/hotspot?architecture={}&image_type=jre&os={}&vendor=eclipse",
            p.arch, p.os
        );
        let json = format!(
            r#"[{{"release_name":"jdk-21.0.4+7","binary":{{"package":{{"checksum":"{checksum}","link":"{{BASE}}/jre.tar.gz","name":"jre.tar.gz","size":{}}}}}}}]"#,
            archive.len()
        );
        serve(&[(&api_path, json.as_bytes()), ("/jre.tar.gz", archive)])
    }

    #[test]
    fn downloads_installs_and_then_reuses_cached_jre() {
        let archive = tar_gz_jre("jdk-21.0.4+7-jre", "21.0.4");
        let (checksum, _) = hash::sha256_reader(archive.as_slice()).unwrap();
        let server = serve_adoptium(&archive, &checksum);
        let cache = tempfile::tempdir().unwrap();
        // un Java di sistema compatibile esiste, ma source = download lo ignora
        let sys = fake_home(cache.path(), "sys", "21.0.10", os_arch());
        let r = resolver(cache.path(), &server.url, vec![(Origin::System, sys)]);
        let need = req(21, None, JavaSource::Download);

        let sel = r.resolve(&need).unwrap();
        assert_eq!(sel.origin, Origin::Downloaded);
        assert_eq!(sel.install.version.to_string(), "21.0.4");
        assert!(sel.install.java_bin().is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(sel.install.java_bin())
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "bin/java deve restare eseguibile");
        }
        let p = platform();
        let expected = format!("temurin-21.0.4+7-jre-{}-{}", p.os, p.arch);
        assert!(
            sel.install.home.ends_with(&expected),
            "{:?}",
            sel.install.home
        );

        let again = r.resolve(&need).unwrap();
        assert_eq!(again.origin, Origin::Cache);
        assert_eq!(
            server.hits(),
            2,
            "una chiamata API + un download, poi solo cache"
        );
    }

    #[test]
    fn checksum_mismatch_installs_nothing() {
        let archive = tar_gz_jre("jdk-21.0.4+7-jre", "21.0.4");
        let server = serve_adoptium(&archive, &"0".repeat(64));
        let cache = tempfile::tempdir().unwrap();
        let r = resolver(cache.path(), &server.url, vec![]);

        let err = format!(
            "{:#}",
            r.resolve(&req(21, None, JavaSource::Auto)).unwrap_err()
        );
        assert!(err.contains("JRE SHA-256 mismatch"), "{err}");
        let leftovers: Vec<_> = fs::read_dir(cache.path().join("jre"))
            .unwrap()
            .flatten()
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn extracts_zip_archives() {
        let dir = tempfile::tempdir().unwrap();
        let zip_path = dir.path().join("jre.zip");
        let mut zip = zip::ZipWriter::new(File::create(&zip_path).unwrap());
        zip.start_file(
            "jdk-21-jre/release",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(b"JAVA_VERSION=\"21\"").unwrap();
        zip.finish().unwrap();

        let out = dir.path().join("out");
        fs::create_dir(&out).unwrap();
        extract(&zip_path, true, &out).unwrap();
        assert!(single_root(&out).unwrap().join("release").is_file());
    }
}
