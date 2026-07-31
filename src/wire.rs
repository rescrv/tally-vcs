//! §4 The wire protocol: the wire format is the packed format.
//!
//! The server is dumb (I10): it GETs and PUTs segments and manifests,
//! supports put-if-absent, and computes nothing an object store cannot.
//! There is no negotiation because there is nothing to negotiate.

use std::fs;
use std::path::PathBuf;

use crate::repo::Repository;
use crate::serve::{ServeManifest, pack, restore, unpack_segments};
use crate::{Error, Result, ioerr};

/// A dumb server: an object store and nothing else.  All intelligence is
/// client-side, including the maintainer.
pub trait ObjectStore {
    /// GET an object; `None` if absent.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>>;
    /// PUT an object; idempotent, since names are content hashes or
    /// sequence numbers guarded by put-if-absent.
    fn put(&self, name: &str, bytes: &[u8]) -> Result<()>;
    /// PUT unless the name exists.  Returns false on conflict.  This is the
    /// server-side linearization point (I8) — the same role the fsync'd
    /// append plays loose.
    fn put_if_absent(&self, name: &str, bytes: &[u8]) -> Result<bool>;
    /// List object names under a prefix.
    fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// An object store on a filesystem directory: the reference dumb server.
pub struct FsStore {
    root: PathBuf,
}

impl FsStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(ioerr("creating object store"))?;
        Ok(FsStore { root })
    }

    fn path_of(&self, name: &str) -> Result<PathBuf> {
        // Object names are relative paths with no traversal.
        if name.starts_with('/') || name.split('/').any(|c| c == ".." || c == "." || c.is_empty())
        {
            return Err(Error::Invalid(format!("bad object name: {name:?}")));
        }
        Ok(self.root.join(name))
    }
}

impl ObjectStore for FsStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path_of(name)?) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(ioerr(format!("GET {name}"))(err)),
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let path = self.path_of(name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(ioerr("creating store prefix"))?;
        }
        // Existing names are content-addressed no-ops.
        if path.exists() {
            return Ok(());
        }
        fs::write(&path, bytes).map_err(ioerr(format!("PUT {name}")))
    }

    fn put_if_absent(&self, name: &str, bytes: &[u8]) -> Result<bool> {
        let path = self.path_of(name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(ioerr("creating store prefix"))?;
        }
        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(bytes).map_err(ioerr(format!("PUT {name}")))?;
                file.sync_all().map_err(ioerr(format!("fsync {name}")))?;
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(err) => Err(ioerr(format!("put-if-absent {name}"))(err)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let dir = self.root.join(prefix);
        let mut names = Vec::new();
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(names),
            Err(err) => return Err(ioerr(format!("LIST {prefix}"))(err)),
        };
        for entry in entries {
            let entry = entry.map_err(ioerr("listing store"))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(format!("{prefix}/{name}"));
            }
        }
        names.sort();
        Ok(names)
    }
}

/////////////////////////////////////////////// ops ///////////////////////////////////////////////

/// GET the highest-`seq` manifest, if the store has any.  Fetched bytes are
/// hostile until proven otherwise: the parse is size-bounded and the id
/// verified (§4 step 1).
pub fn remote_latest(store: &dyn ObjectStore) -> Result<Option<ServeManifest>> {
    let mut best: Option<(u64, String)> = None;
    for name in store.list("manifest")? {
        let Some(seq) = name
            .strip_prefix("manifest/")
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| seq > *b) {
            best = Some((seq, name));
        }
    }
    let Some((_, name)) = best else {
        return Ok(None);
    };
    let bytes = store
        .get(&name)?
        .ok_or_else(|| Error::Corrupt(format!("listed manifest {name} vanished")))?;
    Ok(Some(ServeManifest::parse(&bytes)?))
}

/// Clone: GET the latest manifest, GET its segments (names are content
/// hashes, so caching is trivially safe), verify everything in the mandated
/// order, and materialize a loose repository at `dest`.
pub fn clone(store: &dyn ObjectStore, dest: impl Into<PathBuf>) -> Result<Repository> {
    let manifest = remote_latest(store)?
        .ok_or_else(|| Error::Invalid("nothing to clone: the store has no manifest".to_string()))?;
    let fetch = |name: &str| -> Result<Vec<u8>> {
        store
            .get(name)?
            .ok_or_else(|| Error::Corrupt(format!("manifest references absent object {name}")))
    };
    let unpacked = unpack_segments(&manifest, &fetch)?;
    restore(&unpacked, &dest.into())
}

/// Fetch: GET the latest manifest, diff its segment set against a local
/// packed cache directory, GET the difference.
pub fn fetch(store: &dyn ObjectStore, cache: &std::path::Path) -> Result<Option<ServeManifest>> {
    let Some(manifest) = remote_latest(store)? else {
        return Ok(None);
    };
    fs::create_dir_all(cache.join("seg")).map_err(ioerr("creating cache seg/"))?;
    fs::create_dir_all(cache.join("manifest")).map_err(ioerr("creating cache manifest/"))?;
    for segid in manifest.segments.keys() {
        for ext in ["pk", "idx"] {
            let name = format!("seg/{segid}.{ext}");
            let path = cache.join(&name);
            if path.exists() {
                continue; // content-addressed: caches never invalidate
            }
            let bytes = store.get(&name)?.ok_or_else(|| {
                Error::Corrupt(format!("manifest references absent object {name}"))
            })?;
            fs::write(&path, bytes).map_err(ioerr("writing cache object"))?;
        }
    }
    fs::write(
        cache.join("manifest").join(format!("{}.json", manifest.seq)),
        manifest.to_bytes()?,
    )
    .map_err(ioerr("writing cache manifest"))?;
    Ok(Some(manifest))
}

/// Push: pack locally, PUT segments (idempotent), build manifest seq N+1
/// with prev = id of N, and put-if-absent it — the wire linearization
/// point.  On conflict, rebuild against the winner and retry; uploaded
/// segments are content-addressed and reusable as-is.  Two pushes advancing
/// the same fork violate single-writer (I8): the loser MUST NOT silently
/// splice, and gets an error telling it to union.
pub fn push(repo: &Repository, store: &dyn ObjectStore, level: i32) -> Result<ServeManifest> {
    let staging = std::env::temp_dir().join(format!(
        "abelian-push-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::create_dir_all(&staging).map_err(ioerr("creating push staging"))?;
    let base = remote_latest(store)?;
    let (seq, prev_id) = match &base {
        Some(m) => (m.seq + 1, m.id.clone()),
        None => (1, String::new()),
    };
    // 1. pack new content into segments locally.
    let mut manifest = pack(repo, &staging, seq, &prev_id, level)?;
    // 2. PUT segments (idempotent; existing names are no-ops).
    for segid in manifest.segments.keys() {
        for ext in ["pk", "idx"] {
            let name = format!("seg/{segid}.{ext}");
            let bytes =
                fs::read(staging.join(&name)).map_err(ioerr("reading staged segment"))?;
            store.put(&name, &bytes)?;
        }
    }
    let _ = fs::remove_dir_all(&staging);
    // Carry forward the winner's segments and any forks we do not carry:
    // manifest rebuild is a union of segment sets plus per-fork heads.
    let mut base = base;
    loop {
        if let Some(winner) = &base {
            for (segid, meta) in &winner.segments {
                manifest.segments.entry(segid.clone()).or_insert_with(|| meta.clone());
            }
            for (fork, head) in &winner.forks {
                match manifest.forks.get(fork) {
                    None => {
                        manifest.forks.insert(fork.clone(), head.clone());
                    }
                    Some(ours) if ours.head_id == head.head_id => {}
                    Some(_) => {
                        // The winner advanced a fork we are also advancing.
                        let advanced_remotely = repo
                            .current_state(fork)
                            .ok()
                            .map(|s| s.lines.iter().all(|l| l.id != head.head_id))
                            .unwrap_or(true);
                        if advanced_remotely && !head.head_id.is_empty() {
                            return Err(Error::Invalid(format!(
                                "fork {fork} advanced remotely to {}; refusing to splice — \
                                 run union at the log level and push the result",
                                head.head_id
                            )));
                        }
                    }
                }
            }
            for anchor in &winner.anchors {
                if !manifest.anchors.contains(anchor) {
                    manifest.anchors.push(anchor.clone());
                }
            }
        }
        manifest.seq = base.as_ref().map(|m| m.seq + 1).unwrap_or(1);
        manifest.prev = base.as_ref().map(|m| m.id.clone()).unwrap_or_default();
        manifest.id = crate::ident::record_id(&serde_json::to_value(&manifest)?)?;
        // 4. put-if-absent ← LINEARIZATION POINT (wire).
        let name = format!("manifest/{}.json", manifest.seq);
        if store.put_if_absent(&name, &manifest.to_bytes()?)? {
            return Ok(manifest);
        }
        // 5. on conflict: GET the winner, rebuild against it, retry.
        base = remote_latest(store)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Annotation;
    use crate::patch::{Intent, Op};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abelian-wire-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn create(path: &str, content: &[u8]) -> Intent {
        Intent {
            ops: vec![Op::Create {
                path: path.to_string(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(crate::b64::encode(content)),
            }],
        }
    }

    fn note() -> Annotation {
        Annotation { author: "t".to_string(), ..Annotation::default() }
    }

    #[test]
    fn push_clone_round_trip() {
        let repo = Repository::init(temp_dir("pcrt-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let store = FsStore::open(temp_dir("pcrt-store")).unwrap();
        let manifest = push(&repo, &store, 3).unwrap();
        assert_eq!(manifest.seq, 1);
        assert_eq!(manifest.prev, "");
        let cloned = clone(&store, temp_dir("pcrt-dst")).unwrap();
        assert_eq!(
            repo.current_state("main").unwrap().sum,
            cloned.current_state("main").unwrap().sum,
        );
        assert_eq!(repo.log_bytes("main").unwrap(), cloned.log_bytes("main").unwrap());
    }

    #[test]
    fn sequential_pushes_chain() {
        let repo = Repository::init(temp_dir("chain-src")).unwrap();
        let store = FsStore::open(temp_dir("chain-store")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let m1 = push(&repo, &store, 3).unwrap();
        repo.apply("main", create("/b.rs", b"b\n"), note()).unwrap();
        let m2 = push(&repo, &store, 3).unwrap();
        assert_eq!(m2.seq, 2);
        assert_eq!(m2.prev, m1.id);
    }

    #[test]
    fn pushes_to_different_forks_merge_trivially() {
        let store = FsStore::open(temp_dir("merge-store")).unwrap();
        let alice = Repository::init(temp_dir("merge-alice")).unwrap();
        alice.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        push(&alice, &store, 3).unwrap();
        // Bob clones and works on his own fork.
        let bob = clone(&store, temp_dir("merge-bob")).unwrap();
        bob.create_fork("bob", "main").unwrap();
        bob.apply("bob", create("/b.rs", b"b\n"), note()).unwrap();
        push(&bob, &store, 3).unwrap();
        // Alice advances main concurrently; her push must merge bob's fork
        // forward, not lose it.
        alice.apply("main", create("/c.rs", b"c\n"), note()).unwrap();
        let m = push(&alice, &store, 3).unwrap();
        assert!(m.forks.contains_key("bob"), "different forks merge trivially");
        assert!(m.forks.contains_key("main"));
        let carol = clone(&store, temp_dir("merge-carol")).unwrap();
        assert_eq!(carol.current_state("main").unwrap().manifest.len(), 2);
        assert_eq!(carol.current_state("bob").unwrap().manifest.len(), 2);
    }

    #[test]
    fn same_fork_advance_refuses_to_splice() {
        let store = FsStore::open(temp_dir("splice-store")).unwrap();
        let alice = Repository::init(temp_dir("splice-alice")).unwrap();
        alice.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        push(&alice, &store, 3).unwrap();
        let bob = clone(&store, temp_dir("splice-bob")).unwrap();
        // Both advance main.
        alice.apply("main", create("/x.rs", b"x\n"), note()).unwrap();
        push(&alice, &store, 3).unwrap();
        bob.apply("main", create("/y.rs", b"y\n"), note()).unwrap();
        let err = push(&bob, &store, 3);
        assert!(matches!(err, Err(Error::Invalid(_))), "loser must not silently splice");
    }

    #[test]
    fn fetch_populates_a_cache() {
        let repo = Repository::init(temp_dir("fetch-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let store = FsStore::open(temp_dir("fetch-store")).unwrap();
        let pushed = push(&repo, &store, 3).unwrap();
        let cache = temp_dir("fetch-cache");
        let fetched = fetch(&store, &cache).unwrap().unwrap();
        assert_eq!(fetched.id, pushed.id);
        for segid in fetched.segments.keys() {
            assert!(cache.join("seg").join(format!("{segid}.pk")).exists());
            assert!(cache.join("seg").join(format!("{segid}.idx")).exists());
        }
        // The cache is a packed repository: unpack works offline.
        let offline = crate::serve::unpack_dir(&cache, &temp_dir("fetch-offline")).unwrap();
        assert_eq!(
            offline.current_state("main").unwrap().sum,
            repo.current_state("main").unwrap().sum,
        );
    }

    #[test]
    fn hostile_manifest_is_rejected() {
        let store = FsStore::open(temp_dir("hostile-store")).unwrap();
        store.put("manifest/1.json", br#"{"id":"lies","v":0,"seq":1}"#).unwrap();
        assert!(remote_latest(&store).is_err());
    }
}
