use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// SHA-256 (lowercase hex) and size in bytes of the file, read as a stream.
pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    sha256_reader(file).with_context(|| format!("reading {}", path.display()))
}

pub fn sha256_reader(reader: impl Read) -> io::Result<(String, u64)> {
    copy_hashed(reader, io::sink(), |_| {})
}

/// Copies `reader` into `writer`, computing the hash in a single pass.
/// `on_chunk` receives the byte count of each chunk (for the progress bar).
pub fn copy_hashed(
    mut reader: impl Read,
    mut writer: impl Write,
    mut on_chunk: impl FnMut(u64),
) -> io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
        size += n as u64;
        on_chunk(n as u64);
    }
    writer.flush()?;
    Ok((hex::encode(hasher.finalize()), size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_value() {
        let (hash, size) = sha256_reader(&b"abc"[..]).unwrap();
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(size, 3);
    }

    #[test]
    fn copy_hashed_writes_everything() {
        let mut out = Vec::new();
        let mut chunks = 0;
        let (_, size) = copy_hashed(&b"hello"[..], &mut out, |_| chunks += 1).unwrap();
        assert_eq!((out.as_slice(), size, chunks), (&b"hello"[..], 5, 1));
    }
}
