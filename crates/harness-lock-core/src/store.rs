//! Content-addressed blob store.

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::hash::sha256_bytes;

/// Write-once, read-by-hash blob store rooted at a directory.
///
/// `put` returns a `sha256:<hex>` reference; `get` resolves it. Blobs are
/// addressed by their hex digest on disk.
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Store { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store bytes; returns `sha256:<hex>`.
    pub fn put_bytes(&self, data: &[u8]) -> Result<String> {
        fs::create_dir_all(&self.root).map_err(|e| crate::error::io_at(&self.root, e))?;
        let digest = sha256_bytes(data);
        let target = self.path_for(&digest);
        if !target.exists() {
            // by rename, so a crash can't leave a partial blob under the digest's name
            crate::fsutil::write_atomic(&target, data).map_err(|e| crate::error::at(&target, e))?;
        }
        Ok(digest)
    }

    pub fn put_text(&self, text: &str) -> Result<String> {
        self.put_bytes(text.as_bytes())
    }

    pub fn put_value(&self, v: &serde_json::Value) -> Result<String> {
        self.put_text(&crate::hash::canon_json(v))
    }

    pub fn get_bytes(&self, reference: &str) -> Result<Vec<u8>> {
        let p = self.path_for(reference);
        if !p.exists() {
            return Err(Error::Store(format!("blob not in store: {reference}")));
        }
        fs::read(&p).map_err(|e| crate::error::io_at(&p, e))
    }

    pub fn get_text(&self, reference: &str) -> Result<String> {
        Ok(String::from_utf8_lossy(&self.get_bytes(reference)?).into_owned())
    }

    pub fn exists(&self, reference: &str) -> bool {
        self.path_for(reference).exists()
    }

    pub fn count(&self) -> usize {
        match fs::read_dir(&self.root) {
            Ok(rd) => rd.filter_map(|e| e.ok()).filter(|e| e.path().is_file()).count(),
            Err(_) => 0,
        }
    }

    fn path_for(&self, reference: &str) -> PathBuf {
        let hex = if let Some(idx) = reference.find(':') {
            &reference[idx + 1..]
        } else {
            reference
        };
        self.root.join(hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let dir = std::env::temp_dir().join(format!("hlock-store-{}", std::process::id()));
        let s = Store::new(&dir);
        let ref_ = s.put_text("hello").unwrap();
        assert!(ref_.starts_with("sha256:"));
        assert_eq!(s.get_text(&ref_).unwrap(), "hello");
        assert!(s.exists(&ref_));
        assert_eq!(s.count(), 1);
        s.put_text("hello").unwrap();
        assert_eq!(s.count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }
}
