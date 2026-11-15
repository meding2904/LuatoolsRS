//! Streaming file hashing used by manifest verification and cache keys.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use sha1::Sha1;
use sha2::Sha256;
// Both hash crates re-export the same `digest` traits; import them once under clear names.
use sha1::Digest as _;

use crate::error::{Error, Result};

/// Size of the read buffer used while hashing.
const CHUNK: usize = 64 * 1024;

/// A hash digest in lowercase hexadecimal form.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HashDigest(String);

impl HashDigest {
    /// Wraps an already-hexadecimal digest.
    #[must_use]
    pub fn from_hex(hex: impl Into<String>) -> Self {
        Self(hex.into().to_ascii_lowercase())
    }

    /// Returns the hexadecimal representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for HashDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn hash_stream<D: sha2::Digest + Default>(reader: &mut impl Read) -> Result<HashDigest> {
    let mut hasher = D::default();
    let mut buf = vec![0_u8; CHUNK];
    loop {
        let read = reader
            .read(&mut buf)
            .map_err(|e| Error::io("reading while hashing", e))?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(HashDigest(hex::encode(hasher.finalize())))
}

/// Computes the SHA-256 of a slice.
#[must_use]
pub fn sha256_bytes(bytes: &[u8]) -> HashDigest {
    HashDigest(hex::encode(Sha256::digest(bytes)))
}

/// Computes the SHA-1 of a slice, the digest Steam manifests use.
#[must_use]
pub fn sha1_bytes(bytes: &[u8]) -> HashDigest {
    HashDigest(hex::encode(Sha1::digest(bytes)))
}

/// Streams a file through SHA-256.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be opened or read.
pub fn sha256_file(path: &Path) -> Result<HashDigest> {
    let file = File::open(path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
    hash_stream::<Sha256>(&mut BufReader::with_capacity(CHUNK * 4, file))
}

/// Streams a file through SHA-1.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be opened or read.
pub fn sha1_file(path: &Path) -> Result<HashDigest> {
    let file = File::open(path).map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
    hash_stream::<Sha1>(&mut BufReader::with_capacity(CHUNK * 4, file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn digests_match_known_vectors() {
        assert_eq!(
            sha256_bytes(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha1_bytes(b"abc").as_str(),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }

    #[test]
    fn file_hashing_matches_in_memory_hashing() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let payload = vec![0x5a_u8; 200_000];
        file.write_all(&payload).unwrap();
        assert_eq!(sha256_file(file.path()).unwrap(), sha256_bytes(&payload));
        assert_eq!(sha1_file(file.path()).unwrap(), sha1_bytes(&payload));
    }

    #[test]
    fn missing_files_report_a_contextual_error() {
        let err = sha256_file(Path::new("/definitely/not/here")).unwrap_err();
        assert!(err.to_string().starts_with("opening /definitely/not/here"));
    }
}
