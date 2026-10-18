//! Modello del file `app.lock` (formato v1, vedi PLAN.md).
#![allow(dead_code)] // usato dalle Fasi 1-4

use serde::{Deserialize, Serialize};

pub const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lockfile {
    pub version: u32,
    pub app: App,
    pub java: Java,
    #[serde(default, rename = "repository")]
    pub repositories: Vec<Repository>,
    #[serde(default, rename = "dep")]
    pub deps: Vec<Dep>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct App {
    pub name: String,
    /// Percorso del JAR, relativo al lockfile.
    pub jar: String,
    /// Se assente si usa `Main-Class` dal manifest.
    pub main: Option<String>,
    #[serde(default)]
    pub jvm_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Java {
    /// Versione minima (feature version, es. "21").
    pub version: String,
    /// Versione massima accettata per un Java di sistema.
    pub max: Option<String>,
    #[serde(default)]
    pub source: JavaSource,
    #[serde(default = "default_vendor")]
    pub vendor: String,
    #[serde(default)]
    pub image: JavaImage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum JavaSource {
    #[default]
    Auto,
    System,
    Download,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JavaImage {
    #[default]
    Jre,
    Jdk,
}

impl JavaImage {
    pub fn as_str(self) -> &'static str {
        match self {
            JavaImage::Jre => "jre",
            JavaImage::Jdk => "jdk",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dep {
    /// `group:artifact:type[:classifier]:version`
    pub coord: String,
    pub sha256: String,
    pub size: u64,
    /// Path relativo al lockfile (es. `lib/x.jar`) se il JAR viaggia con l'app
    /// invece di essere scaricato da un repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundled: Option<String>,
}

fn default_vendor() -> String {
    "temurin".into()
}

impl Lockfile {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let lock: Self = toml::from_str(text)?;
        anyhow::ensure!(
            lock.version == FORMAT_VERSION,
            "unsupported lockfile version: {} (expected {FORMAT_VERSION})",
            lock.version
        );
        Ok(lock)
    }

    /// Dipendenze da scaricare (escluse quelle incluse nella distribuzione).
    pub fn remote_deps(&self) -> Vec<Dep> {
        self.deps
            .iter()
            .filter(|d| d.bundled.is_none())
            .cloned()
            .collect()
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid lockfile {}", path.display()))
    }

    pub fn to_toml(&self) -> anyhow::Result<String> {
        Ok(toml::to_string(self)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version = 1

[app]
name = "hello"
jar = "hello-1.0.0.jar"

[java]
version = "21"

[[repository]]
id = "central"
url = "https://repo.maven.apache.org/maven2"

[[dep]]
coord = "org.slf4j:slf4j-api:jar:2.0.13"
sha256 = "e7c2a48e8515ba1a49fd637d3c5bd5b6"
size = 68518
"#;

    #[test]
    fn parses_minimal_lock_with_defaults() {
        let lock = Lockfile::parse(SAMPLE).unwrap();
        assert_eq!(lock.java.source, JavaSource::Auto);
        assert_eq!(lock.java.vendor, "temurin");
        assert_eq!(lock.java.image, JavaImage::Jre);
        assert_eq!(lock.app.main, None);
        assert_eq!(lock.deps.len(), 1);
    }

    #[test]
    fn roundtrips_through_toml() {
        let lock = Lockfile::parse(SAMPLE).unwrap();
        assert_eq!(Lockfile::parse(&lock.to_toml().unwrap()).unwrap(), lock);
    }

    #[test]
    fn rejects_unknown_format_version() {
        let text = SAMPLE.replace("version = 1", "version = 2");
        assert!(Lockfile::parse(&text).is_err());
    }
}
