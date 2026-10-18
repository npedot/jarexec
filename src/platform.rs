use std::fmt;

use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
    Mac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X64,
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// Piattaforma per cui è stato compilato questo binario.
    pub fn current() -> Result<Self> {
        Self::from_rust(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn from_rust(os: &str, arch: &str) -> Result<Self> {
        let os = match os {
            "linux" => Os::Linux,
            "windows" => Os::Windows,
            "macos" => Os::Mac,
            other => bail!("unsupported operating system: {other}"),
        };
        let arch = match arch {
            "x86_64" => Arch::X64,
            "aarch64" => Arch::Aarch64,
            other => bail!("unsupported architecture: {other}"),
        };
        Ok(Self { os, arch })
    }
}

impl Arch {
    /// Confronta con `OS_ARCH` del file `release` o con la proprietà `os.arch` della JVM.
    pub fn matches(self, os_arch: &str) -> bool {
        match self {
            Arch::X64 => matches!(os_arch, "x86_64" | "amd64" | "x64"),
            Arch::Aarch64 => matches!(os_arch, "aarch64" | "arm64"),
        }
    }
}

/// Nomi usati dall'API Adoptium (`os=`, `architecture=`).
impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
            Os::Mac => "mac",
        })
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::X64 => "x64",
            Arch::Aarch64 => "aarch64",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_rust_names_to_adoptium_names() {
        let p = Platform::from_rust("macos", "aarch64").unwrap();
        assert_eq!(format!("{}-{}", p.os, p.arch), "mac-aarch64");
    }

    #[test]
    fn rejects_unsupported_arch() {
        assert!(Platform::from_rust("linux", "x86").is_err());
    }

    #[test]
    fn current_platform_is_supported() {
        Platform::current().unwrap();
    }
}
