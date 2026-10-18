//! Reads application JAR metadata: manifest and bytecode version.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use zip::ZipArchive;

pub struct JarInfo {
    pub main_class: Option<String>,
    /// Java version required by the main class bytecode (e.g. 21).
    pub java_release: Option<u16>,
}

pub fn inspect(path: &Path) -> Result<JarInfo> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip =
        ZipArchive::new(file).with_context(|| format!("{} is not a valid JAR", path.display()))?;

    let manifest = match zip.by_name("META-INF/MANIFEST.MF") {
        Ok(mut entry) => {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            parse_manifest(&text)
        }
        Err(_) => HashMap::new(),
    };
    let main_class = manifest.get("Main-Class").cloned();

    let java_release = match &main_class {
        Some(main) => {
            let entry_name = format!("{}.class", main.replace('.', "/"));
            let mut entry = zip
                .by_name(&entry_name)
                .with_context(|| format!("Main-Class {main} not found in the JAR"))?;
            let mut header = [0u8; 8];
            entry.read_exact(&mut header)?;
            Some(class_file_release(&header)?)
        }
        None => None,
    };

    Ok(JarInfo {
        main_class,
        java_release,
    })
}

/// Attributes of the main section. Lines starting with a space
/// continue the previous one (the manifest wraps every 72 bytes).
fn parse_manifest(text: &str) -> HashMap<String, String> {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            break; // end of the main section
        }
        match (line.strip_prefix(' '), lines.last_mut()) {
            (Some(cont), Some(last)) => last.push_str(cont),
            _ => lines.push(line.to_string()),
        }
    }
    lines
        .iter()
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

/// Highest Java version required by the JAR's classes. Ignores classes
/// in `META-INF/versions/` (multi-release: loaded only by Javas that support them)
/// and `module-info.class` (often compiled separately for Java 9+).
pub fn max_class_release(path: &Path) -> Result<Option<u16>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut zip = ZipArchive::new(file)?;
    let mut best = None;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let skip = match entry.name() {
            Ok(name) => {
                !name.ends_with(".class")
                    || name.starts_with("META-INF/versions/")
                    || name.ends_with("module-info.class")
            }
            Err(_) => true,
        };
        if skip {
            continue;
        }
        let mut header = [0u8; 8];
        if entry.read_exact(&mut header).is_ok()
            && let Ok(release) = class_file_release(&header)
        {
            best = best.max(Some(release));
        }
    }
    Ok(best)
}

/// Header of a .class: magic `CAFEBABE`, minor (u16), major (u16). Java N = major - 44.
fn class_file_release(header: &[u8; 8]) -> Result<u16> {
    if header[..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        bail!("invalid main class (bad magic number)");
    }
    let major = u16::from_be_bytes([header[6], header[7]]);
    if major < 45 {
        bail!("invalid class file version: {major}");
    }
    Ok(major - 44)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    /// Creates a minimal JAR with a manifest and a main class of version `major`.
    pub fn write_test_jar(path: &Path, main: Option<&str>, major: u16) {
        let mut mf = "Manifest-Version: 1.0\r\n".to_string();
        if let Some(main) = main {
            mf.push_str(&format!("Main-Class: {main}\r\n"));
        }
        mf.push_str("\r\n");

        let opts = SimpleFileOptions::default();
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        zip.start_file("META-INF/MANIFEST.MF", opts).unwrap();
        zip.write_all(mf.as_bytes()).unwrap();
        if let Some(main) = main {
            zip.start_file(format!("{}.class", main.replace('.', "/")), opts)
                .unwrap();
            let mut class = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0];
            class.extend_from_slice(&major.to_be_bytes());
            zip.write_all(&class).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn reads_main_class_and_release() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("app.jar");
        write_test_jar(&jar, Some("com.example.Main"), 65);
        let info = inspect(&jar).unwrap();
        assert_eq!(info.main_class.as_deref(), Some("com.example.Main"));
        assert_eq!(info.java_release, Some(21));
    }

    #[test]
    fn jar_without_main_class() {
        let dir = tempfile::tempdir().unwrap();
        let jar = dir.path().join("lib.jar");
        write_test_jar(&jar, None, 0);
        let info = inspect(&jar).unwrap();
        assert!(info.main_class.is_none() && info.java_release.is_none());
    }

    #[test]
    fn max_release_ignores_multi_release_and_module_info() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.jar");
        let mut zip = zip::ZipWriter::new(File::create(&path).unwrap());
        let opts = SimpleFileOptions::default();
        for (name, major) in [
            ("a/A.class", 52u16),
            ("a/B.class", 61),
            ("module-info.class", 65),
            ("META-INF/versions/21/a/A.class", 65),
        ] {
            zip.start_file(name, opts).unwrap();
            let mut class = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0];
            class.extend_from_slice(&major.to_be_bytes());
            zip.write_all(&class).unwrap();
        }
        zip.finish().unwrap();
        assert_eq!(max_class_release(&path).unwrap(), Some(17));
    }

    #[test]
    fn manifest_continuation_lines() {
        let mf = "Manifest-Version: 1.0\r\nMain-Class: com.example.very.long.packa\r\n ge.Main\r\n\r\nName: x\r\nMain-Class: ignored\r\n";
        assert_eq!(
            parse_manifest(mf)["Main-Class"],
            "com.example.very.long.package.Main"
        );
    }
}
