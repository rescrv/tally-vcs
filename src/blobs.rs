//! §2.2 Blobs: raw bytes, no framing, no compression, no type tag.
//!
//! Everything content-shaped shares this pool: file contents, claim
//! transcripts, PR prose, spilled read sets, zstd dictionaries.  The pool is
//! append-only (I3): nothing in it is ever rewritten.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ident::{is_hex64, sha3_hex};
use crate::{Error, Result, ioerr};

/// A content-addressed blob store rooted at `blobs/`.
///
/// The path of a blob is `blobs/<first 2 hex>/<remaining 62 hex>`.  Write
/// protocol: stream to `blobs/tmp/<random>` hashing as you go; fsync;
/// `rename(2)` into place.  A collision on rename is a deduplication hit.
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (without creating) a blob store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Self {
        BlobStore { root: root.into() }
    }

    /// Create the blob store's directories if absent.
    pub fn init(root: impl Into<PathBuf>) -> Result<Self> {
        let store = BlobStore::open(root);
        fs::create_dir_all(store.root.join("tmp")).map_err(ioerr("creating blobs/tmp"))?;
        Ok(store)
    }

    /// The filesystem path a blob hash names.
    pub fn path_for(&self, blob: &str) -> Result<PathBuf> {
        if !is_hex64(blob) {
            return Err(Error::Invalid(format!("blob names are 64 lowercase hex: {blob:?}")));
        }
        Ok(self.root.join(&blob[..2]).join(&blob[2..]))
    }

    /// True iff the blob is present.
    pub fn has(&self, blob: &str) -> Result<bool> {
        Ok(self.path_for(blob)?.exists())
    }

    /// Read a blob's bytes, verifying nothing: the name was the hash when it
    /// was written, and the pool is immutable.
    pub fn get(&self, blob: &str) -> Result<Vec<u8>> {
        let path = self.path_for(blob)?;
        fs::read(&path).map_err(ioerr(format!("reading blob {blob}")))
    }

    /// List every blob hash in the pool, sorted.
    pub fn list(&self) -> Result<Vec<String>> {
        let mut hashes = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(hashes),
            Err(err) => return Err(ioerr("listing blob pool")(err)),
        };
        for entry in entries {
            let entry = entry.map_err(ioerr("listing blob pool"))?;
            let Some(prefix) = entry.file_name().to_str().map(String::from) else {
                continue;
            };
            if prefix.len() != 2 || !entry.path().is_dir() {
                continue;
            }
            let inner = fs::read_dir(entry.path()).map_err(ioerr("listing blob fan-out"))?;
            for file in inner {
                let file = file.map_err(ioerr("listing blob fan-out"))?;
                if let Some(rest) = file.file_name().to_str() {
                    let hash = format!("{prefix}{rest}");
                    if is_hex64(&hash) {
                        hashes.push(hash);
                    }
                }
            }
        }
        hashes.sort();
        Ok(hashes)
    }

    /// Write `content` to the pool; returns its hash.  Idempotent: an
    /// existing blob of the same name is a deduplication hit.
    pub fn put(&self, content: &[u8]) -> Result<String> {
        let hash = sha3_hex(content);
        let dst = self.path_for(&hash)?;
        if dst.exists() {
            return Ok(hash);
        }
        let tmp_dir = self.root.join("tmp");
        fs::create_dir_all(&tmp_dir).map_err(ioerr("creating blobs/tmp"))?;
        let tmp = tmp_dir.join(format!("{}.{}", hash, std::process::id()));
        {
            let mut f = fs::File::create(&tmp).map_err(ioerr("creating blob temp file"))?;
            f.write_all(content).map_err(ioerr("writing blob temp file"))?;
            f.sync_all().map_err(ioerr("fsyncing blob temp file"))?;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(ioerr("creating blob fan-out directory"))?;
        }
        fs::rename(&tmp, &dst).map_err(ioerr(format!("renaming blob {hash} into place")))?;
        fsync_dir(dst.parent().unwrap_or(&self.root))?;
        Ok(hash)
    }

    /// Remove a blob from the pool.  The pool is otherwise append-only (I3);
    /// the sole sanctioned caller is garbage collection, which removes only
    /// content no fork or active claim reaches.  An absent blob is a no-op,
    /// so removal is idempotent.
    pub fn remove(&self, blob: &str) -> Result<()> {
        let path = self.path_for(blob)?;
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(ioerr(format!("removing blob {blob}"))(err)),
        }
        if let Some(parent) = path.parent() {
            fsync_dir(parent)?;
        }
        Ok(())
    }
}

/// fsync a directory so a rename into it is durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    let f = fs::File::open(dir).map_err(ioerr(format!("opening directory {}", dir.display())))?;
    f.sync_all().map_err(ioerr(format!("fsyncing directory {}", dir.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abelian-blobs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn put_get_round_trip() {
        let store = BlobStore::init(tempdir("roundtrip")).unwrap();
        let hash = store.put(b"hello, abelian\n").unwrap();
        assert_eq!(hash, sha3_hex(b"hello, abelian\n"));
        assert!(store.has(&hash).unwrap());
        assert_eq!(store.get(&hash).unwrap(), b"hello, abelian\n");
    }

    #[test]
    fn put_is_idempotent() {
        let store = BlobStore::init(tempdir("idempotent")).unwrap();
        let a = store.put(b"same bytes").unwrap();
        let b = store.put(b"same bytes").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn fan_out_layout() {
        let store = BlobStore::init(tempdir("fanout")).unwrap();
        let hash = store.put(b"x").unwrap();
        let path = store.path_for(&hash).unwrap();
        assert!(path.ends_with(format!("{}/{}", &hash[..2], &hash[2..])));
    }

    #[test]
    fn remove_is_idempotent() {
        let store = BlobStore::init(tempdir("remove")).unwrap();
        let hash = store.put(b"collectible").unwrap();
        assert!(store.has(&hash).unwrap());
        store.remove(&hash).unwrap();
        assert!(!store.has(&hash).unwrap());
        // Removing an absent blob is a no-op, not an error.
        store.remove(&hash).unwrap();
    }

    #[test]
    fn bad_names_rejected() {
        let store = BlobStore::open(tempdir("badnames"));
        assert!(store.path_for("nothex").is_err());
        assert!(store.path_for(&"A".repeat(64)).is_err());
    }
}
