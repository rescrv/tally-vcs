//! §2.2 Blobs: raw bytes, no framing, no compression, no type tag.
//!
//! Everything content-shaped shares this pool: file contents, PR prose,
//! spilled read sets, zstd dictionaries.  The pool is append-only (I3):
//! nothing in it is ever rewritten.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::ident::{is_hex64, sha3_hex};
use crate::{Error, Result, ioerr};

/// A content-addressed blob store rooted at `blobs/`.
///
/// The path of a blob is `blobs/<first 2 hex>/<remaining 62 hex>`.  Write
/// protocol: stream to `blobs/tmp/<random>` hashing as you go; `rename(2)`
/// into place.  Single writes fsync the temp file and the fan-out directory;
/// bulk writers ([`BlobStore::put_unsynced`]) skip both fsyncs and call
/// [`BlobStore::sync`] once, before the commit that references the blobs.
/// A collision on rename is a deduplication hit.
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
    /// existing blob of the same name is a deduplication hit.  The blob is
    /// fsync'd before this returns, so it is durable before anything can
    /// reference it.
    pub fn put(&self, content: &[u8]) -> Result<String> {
        self.write(content, Durability::Immediate)
    }

    /// Write `content` to the pool like [`BlobStore::put`], but skip every
    /// fsync: the rename is visible, the bytes are not yet durable.  Bulk
    /// writers (git import, working-tree ingest, union replay, unpack) use
    /// this and then call [`BlobStore::sync`] once before any log line
    /// referencing the blobs is appended, preserving I8 write-ahead ordering
    /// with a single device sync instead of two fsyncs per blob.
    pub fn put_unsynced(&self, content: &[u8]) -> Result<String> {
        self.write(content, Durability::Deferred)
    }

    /// Make every deferred ([`BlobStore::put_unsynced`]) write durable in
    /// one device-level sync of the filesystem holding the pool.
    pub fn sync(&self) -> Result<()> {
        sync_device(&self.root)
    }

    fn write(&self, content: &[u8], durability: Durability) -> Result<String> {
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
            if durability == Durability::Immediate {
                f.sync_all().map_err(ioerr("fsyncing blob temp file"))?;
            }
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(ioerr("creating blob fan-out directory"))?;
        }
        fs::rename(&tmp, &dst).map_err(ioerr(format!("renaming blob {hash} into place")))?;
        if durability == Durability::Immediate {
            fsync_dir(dst.parent().unwrap_or(&self.root))?;
        }
        Ok(hash)
    }

    /// Remove a blob from the pool.  The pool is otherwise append-only (I3);
    /// the sole sanctioned caller is garbage collection, which removes only
    /// content no fork reaches.  An absent blob is a no-op,
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

/// Whether a blob write fsyncs before returning.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Durability {
    /// fsync the temp file and the fan-out directory: durable on return.
    Immediate,
    /// No fsync at all: [`BlobStore::sync`] must run before any commit that
    /// references the blob.
    Deferred,
}

/// fsync a directory so a rename into it is durable.
pub fn fsync_dir(dir: &Path) -> Result<()> {
    let f = fs::File::open(dir).map_err(ioerr(format!("opening directory {}", dir.display())))?;
    f.sync_all().map_err(ioerr(format!("fsyncing directory {}", dir.display())))
}

/// Sync the device holding `dir`, making every deferred write on that
/// filesystem durable in one call: `syncfs(2)` on Linux, `sync(2)` on other
/// unix.  Bulk blob writers use this in place of per-blob fsyncs: write
/// every file unsynced, sync once, then commit.
pub fn sync_device(dir: &Path) -> Result<()> {
    sync_device_impl(dir)
}

#[cfg(target_os = "linux")]
fn sync_device_impl(dir: &Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let f = fs::File::open(dir).map_err(ioerr(format!("opening directory {}", dir.display())))?;
    if unsafe { libc::syncfs(f.as_raw_fd()) } != 0 {
        return Err(ioerr(format!("syncfs on {}", dir.display()))(
            std::io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "linux")))]
fn sync_device_impl(_dir: &Path) -> Result<()> {
    // No per-filesystem sync outside Linux; sync(2) flushes every device.
    unsafe { libc::sync() }
    Ok(())
}

#[cfg(not(unix))]
fn sync_device_impl(_dir: &Path) -> Result<()> {
    // The durability protocol is unix-shaped (fsync_dir opens directories);
    // there is nothing to sync here.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tally-blobs-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn put_get_round_trip() {
        let store = BlobStore::init(tempdir("roundtrip")).unwrap();
        let hash = store.put(b"hello, tally\n").unwrap();
        assert_eq!(hash, sha3_hex(b"hello, tally\n"));
        assert!(store.has(&hash).unwrap());
        assert_eq!(store.get(&hash).unwrap(), b"hello, tally\n");
    }

    #[test]
    fn put_is_idempotent() {
        let store = BlobStore::init(tempdir("idempotent")).unwrap();
        let a = store.put(b"same bytes").unwrap();
        let b = store.put(b"same bytes").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn put_unsynced_then_sync_round_trip() {
        let store = BlobStore::init(tempdir("deferred")).unwrap();
        let hash = store.put_unsynced(b"deferred durability\n").unwrap();
        assert_eq!(hash, sha3_hex(b"deferred durability\n"));
        store.sync().unwrap();
        assert!(store.has(&hash).unwrap());
        assert_eq!(store.get(&hash).unwrap(), b"deferred durability\n");
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
