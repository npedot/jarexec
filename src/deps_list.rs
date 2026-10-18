//! Parser dell'output di `mvn dependency:list` con `outputAbsoluteArtifactFilename=true`.
//!
//! Formato di ogni riga (dopo l'intestazione):
//! `group:artifact:type[:classifier]:version:scope:path[ -- module name [auto]]`
//! Il path può contenere `:` (es. `C:\...` su Windows), quindi si individua il campo
//! `scope` e tutto ciò che segue è il path.

use std::path::PathBuf;

use anyhow::{Result, bail};

const SCOPES: [&str; 5] = ["compile", "runtime", "provided", "test", "system"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedDep {
    pub group: String,
    pub artifact: String,
    pub kind: String,
    pub classifier: Option<String>,
    pub version: String,
    pub scope: String,
    pub path: PathBuf,
}

impl ListedDep {
    /// Coordinate nel formato del lockfile: `group:artifact:type[:classifier]:version`.
    pub fn coord(&self) -> String {
        match &self.classifier {
            Some(c) => format!(
                "{}:{}:{}:{c}:{}",
                self.group, self.artifact, self.kind, self.version
            ),
            None => format!(
                "{}:{}:{}:{}",
                self.group, self.artifact, self.kind, self.version
            ),
        }
    }
}

pub fn parse(text: &str) -> Result<Vec<ListedDep>> {
    let mut deps = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let clean = strip_ansi(raw);
        let line = clean.trim();
        if let Some(dep) = parse_line(line).map_err(|e| e.context(format!("line {}", n + 1)))? {
            deps.push(dep);
        }
    }
    Ok(deps)
}

/// Toglie le sequenze colore ANSI (`ESC [ ... m`): con l'output colorato attivo
/// Maven le scrive anche nel file, attaccate al path (`...x.jar\x1b[36m -- module`).
fn strip_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains('\x1b') {
        return line.into();
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        if chars.next() == Some('[') {
            // parametri e intermedi fino al byte finale nell'intervallo '@'..='~'
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    out.into()
}

/// `Ok(None)` per righe che non sono dipendenze (intestazione, righe vuote, `none`).
fn parse_line(line: &str) -> Result<Option<ListedDep>> {
    let line = match line.find(" -- ") {
        Some(i) => &line[..i],
        None => line,
    };
    let parts: Vec<&str> = line.split(':').collect();
    // scope alla posizione 4 (senza classifier) o 5 (con classifier)
    let Some(scope_at) = [4, 5]
        .into_iter()
        .find(|&i| parts.get(i).is_some_and(|p| SCOPES.contains(p)))
    else {
        return Ok(None);
    };
    if parts.len() <= scope_at + 1 {
        bail!(
            "missing file path in `{line}`: \
             configure dependency:list with outputAbsoluteArtifactFilename=true"
        );
    }
    let classifier = (scope_at == 5).then(|| parts[3].to_string());
    Ok(Some(ListedDep {
        group: parts[0].into(),
        artifact: parts[1].into(),
        kind: parts[2].into(),
        classifier,
        version: parts[scope_at - 1].into(),
        scope: parts[scope_at].into(),
        path: PathBuf::from(parts[scope_at + 1..].join(":")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINUX: &str = include_str!("../tests/fixtures/deps-linux.txt");
    const WINDOWS: &str = include_str!("../tests/fixtures/deps-windows.txt");

    #[test]
    fn strips_ansi_colors_written_by_maven() {
        // formato reale osservato con Maven su Windows e colori attivi
        let text = "\r\nThe following files have been resolved:\r\n   \
            at.yawk.lz4:lz4-java:jar:1.11.0:compile:C:\\m2\\lz4-java-1.11.0.jar\x1b[36m -- module org.lz4.java\x1b[0;1m [auto]\x1b[m\r\n";
        let deps = parse(text).unwrap();
        assert_eq!(deps[0].path, PathBuf::from(r"C:\m2\lz4-java-1.11.0.jar"));
        assert_eq!(strip_ansi("a\x1b[0;1mb\x1b[mc"), "abc");
    }

    #[test]
    fn parses_real_maven_output() {
        let deps = parse(LINUX).unwrap();
        let coords: Vec<_> = deps.iter().map(ListedDep::coord).collect();
        assert_eq!(
            coords,
            [
                "org.apache.commons:commons-lang3:jar:3.14.0",
                "org.apache.commons:commons-text:jar:1.12.0",
                "org.slf4j:slf4j-api:jar:2.0.16",
                "org.slf4j:slf4j-simple:jar:2.0.16",
            ]
        );
        assert!(deps[0].path.ends_with("commons-lang3-3.14.0.jar"));
    }

    #[test]
    fn handles_windows_paths_crlf_and_classifier() {
        let deps = parse(WINDOWS).unwrap();
        assert_eq!(deps.len(), 3);
        assert_eq!(
            deps[0].path,
            PathBuf::from(
                r"C:\Users\dev\.m2\repository\org\apache\commons\commons-lang3\3.14.0\commons-lang3-3.14.0.jar"
            )
        );
        let fx = &deps[1];
        assert_eq!(fx.classifier.as_deref(), Some("linux"));
        assert_eq!(fx.scope, "runtime");
        assert_eq!(fx.coord(), "org.openjfx:javafx-base:jar:linux:21.0.4");
        assert!(
            fx.path
                .to_string_lossy()
                .ends_with("javafx-base-21.0.4-linux.jar")
        );
    }

    #[test]
    fn ignores_header_and_none() {
        assert!(
            parse("The following files have been resolved:\n   none\n")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn explains_missing_absolute_path() {
        let err = parse("   org.slf4j:slf4j-api:jar:2.0.16:compile").unwrap_err();
        assert!(format!("{err:#}").contains("outputAbsoluteArtifactFilename"));
    }
}
