//! Content-addressed blob store on disk.
//!
//! Layout:
//!
//! ```text
//! data_dir/blobs/<sha256 hex>
//! ```
//!
//! There is no subdirectory sharding.  If the store ever grows large
//! enough for `ls` to struggle, switch to a `aa/bb/<sha256>` layout —
//! this is a v2 concern.
//!
//! Writes are atomic: we write to a tempfile in the same directory and
//! then `rename` it onto the final path.

use std::io;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BlobError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("sha256 mismatch: expected {expected}, got {got}")]
    HashMismatch { expected: String, got: String },
    #[error("blob not found: {0}")]
    NotFound(String),
    #[error("invalid sha256: {0}")]
    InvalidHash(String),
}

/// A content-addressed blob store rooted at a directory.
#[derive(Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (or create) a blob store at the given root directory.
    pub fn new(root: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The on-disk path for a given hash.
    fn path_for(&self, sha256: &str) -> PathBuf {
        self.root.join(sha256)
    }

    /// Store bytes at the given hash.  Returns `HashMismatch` if the
    /// caller-provided hash disagrees with the content.
    pub async fn put(&self, sha256: &str, bytes: &[u8]) -> Result<(), BlobError> {
        validate_hex_sha256(sha256)?;

        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let got = format!("{:x}", hasher.finalize());
        if got != sha256 {
            return Err(BlobError::HashMismatch {
                expected: sha256.to_string(),
                got,
            });
        }

        let final_path = self.path_for(sha256);
        let tmp_path = self.root.join(format!("{sha256}.tmp"));
        tokio::fs::write(&tmp_path, bytes).await?;
        tokio::fs::rename(&tmp_path, &final_path).await?;
        Ok(())
    }

    /// Read the bytes for a given hash.
    pub async fn get(&self, sha256: &str) -> Result<Vec<u8>, BlobError> {
        validate_hex_sha256(sha256)?;
        let path = self.path_for(sha256);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                // Canary: verify the hash at read time and log loudly on mismatch.
                let mut hasher = Sha256::new();
                hasher.update(&bytes);
                let got = format!("{:x}", hasher.finalize());
                if got != sha256 {
                    tracing::error!(
                        expected = sha256,
                        got = %got,
                        path = %path.display(),
                        "DISK CORRUPTION: blob hash mismatch on read"
                    );
                    return Err(BlobError::HashMismatch {
                        expected: sha256.to_string(),
                        got,
                    });
                }
                Ok(bytes)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Err(BlobError::NotFound(sha256.to_string()))
            }
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    /// Return `true` if the store already has the given hash.
    pub fn contains(&self, sha256: &str) -> bool {
        self.path_for(sha256).is_file()
    }

    /// Expose the root path for diagnostics.
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Sanity-check a user-supplied sha256 hex string.
///
/// Prevents directory traversal via `../` and bogus hashes.
fn validate_hex_sha256(s: &str) -> Result<(), BlobError> {
    if s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(BlobError::InvalidHash(s.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_sha256(bytes: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    #[tokio::test]
    async fn put_then_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();

        let data = b"hello blob";
        let hash = hex_sha256(data);

        store.put(&hash, data).await.unwrap();
        assert!(store.contains(&hash));

        let back = store.get(&hash).await.unwrap();
        assert_eq!(back, data);
    }

    #[tokio::test]
    async fn put_rejects_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();

        let data = b"hello";
        // A valid-looking but wrong hash.
        let wrong = "0".repeat(64);
        let err = store.put(&wrong, data).await.unwrap_err();
        assert!(matches!(err, BlobError::HashMismatch { .. }));
    }

    #[tokio::test]
    async fn get_missing_returns_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();

        let hash = "a".repeat(64);
        let err = store.get(&hash).await.unwrap_err();
        assert!(matches!(err, BlobError::NotFound(_)));
    }

    #[tokio::test]
    async fn rejects_invalid_hash_format() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs")).unwrap();

        let err = store.get("not-a-hash").await.unwrap_err();
        assert!(matches!(err, BlobError::InvalidHash(_)));

        let err = store.put("../evil", b"x").await.unwrap_err();
        assert!(matches!(err, BlobError::InvalidHash(_)));
    }
}
