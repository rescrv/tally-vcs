//! §4 The wire protocol: the wire format is the packed format.
//!
//! The server is dumb (I10): it GETs and PUTs segments and manifests,
//! supports put-if-absent, and computes nothing an object store cannot.
//! There is no negotiation because there is nothing to negotiate.

use std::fs;
use std::path::PathBuf;

use futures_executor::{block_on, block_on_stream};
use object_store::ObjectStore as _;
use object_store::ObjectStoreExt as _;
use object_store::local::LocalFileSystem;

use crate::delta::{self, Policy};
use crate::remote::{RemoteCache, RemoteView, discover, manifest_name};
use crate::repo::Repository;
use crate::serve::{ServeManifest, restore, unpack_segments};
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
    /// True iff the object exists: a HEAD.  Push asks before it PUTs, so
    /// a segment the store already holds — a resumed upload, a race lost
    /// to identical bytes — costs a round trip, not its size.
    fn exists(&self, name: &str) -> Result<bool>;
    /// A stable name for this store; it keys the client-side remote cache.
    fn locator(&self) -> String;
}

/// Wrap an `object_store::Error` as a substrate error, annotated with what
/// was being attempted.
fn oserr(what: impl std::fmt::Display) -> impl FnOnce(object_store::Error) -> Error {
    let what = what.to_string();
    move |err| {
        Error::Io(handled::SError::new("object-store").with_message(&format!("{what}: {err}")))
    }
}

/// An object store on a filesystem directory: the reference dumb server,
/// backed by the object_store crate's [`LocalFileSystem`].  Its
/// `PutMode::Create` is the atomic put-if-absent the wire linearization
/// point (I8) requires.
pub struct FsStore {
    store: LocalFileSystem,
    locator: String,
}

impl FsStore {
    /// Open (creating if needed) a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(ioerr("creating object store"))?;
        let store = LocalFileSystem::new_with_prefix(&root)
            .map_err(oserr(format!("opening object store {}", root.display())))?
            .with_automatic_cleanup(true);
        let canonical = fs::canonicalize(&root).map_err(ioerr("resolving object store root"))?;
        Ok(FsStore {
            store,
            locator: format!("file://{}", canonical.display()),
        })
    }

    fn path_of(&self, name: &str) -> Result<object_store::path::Path> {
        // Object names are relative paths with no traversal.
        if name.starts_with('/')
            || name
                .split('/')
                .any(|c| c == ".." || c == "." || c.is_empty())
        {
            return Err(Error::Invalid(format!("bad object name: {name:?}")));
        }
        object_store::path::Path::parse(name)
            .map_err(|err| Error::Invalid(format!("bad object name {name:?}: {err}")))
    }
}

impl ObjectStore for FsStore {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let path = self.path_of(name)?;
        match block_on(async { self.store.get(&path).await?.bytes().await }) {
            Ok(bytes) => Ok(Some(bytes.to_vec())),
            Err(object_store::Error::NotFound { .. }) => Ok(None),
            Err(err) => Err(oserr(format!("GET {name}"))(err)),
        }
    }

    fn put(&self, name: &str, bytes: &[u8]) -> Result<()> {
        // Existing names are content-addressed no-ops, so put-if-absent
        // losing the race is success.
        self.put_if_absent(name, bytes).map(|_| ())
    }

    fn put_if_absent(&self, name: &str, bytes: &[u8]) -> Result<bool> {
        let path = self.path_of(name)?;
        let payload = object_store::PutPayload::from(bytes.to_vec());
        match block_on(
            self.store
                .put_opts(&path, payload, object_store::PutMode::Create.into()),
        ) {
            Ok(_) => Ok(true),
            Err(object_store::Error::AlreadyExists { .. }) => Ok(false),
            Err(err) => Err(oserr(format!("put-if-absent {name}"))(err)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let path = self.path_of(prefix)?;
        let mut names = Vec::new();
        for meta in block_on_stream(self.store.list(Some(&path))) {
            let meta = meta.map_err(|err| {
                Error::Io(
                    handled::SError::new("object-store")
                        .with_message(&format!("LIST {prefix}: {err}")),
                )
            })?;
            names.push(meta.location.to_string());
        }
        names.sort();
        Ok(names)
    }

    fn exists(&self, name: &str) -> Result<bool> {
        let path = self.path_of(name)?;
        match block_on(self.store.head(&path)) {
            Ok(_) => Ok(true),
            Err(object_store::Error::NotFound { .. }) => Ok(false),
            Err(err) => Err(oserr(format!("HEAD {name}"))(err)),
        }
    }

    fn locator(&self) -> String {
        self.locator.clone()
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
/// order, and materialize a loose repository at `dest`.  The clone seeds
/// its remote cache — the manifest, every `.idx`, the small payloads, and
/// every fork head as synced — so its first push is already incremental.
pub fn clone(store: &dyn ObjectStore, dest: impl Into<PathBuf>) -> Result<Repository> {
    let manifest = remote_latest(store)?
        .ok_or_else(|| Error::Invalid("nothing to clone: the store has no manifest".to_string()))?;
    let fetched: std::cell::RefCell<Vec<(String, Vec<u8>)>> = std::cell::RefCell::new(Vec::new());
    let fetch = |name: &str| -> Result<Vec<u8>> {
        let bytes = store
            .get(name)?
            .ok_or_else(|| Error::Corrupt(format!("manifest references absent object {name}")))?;
        fetched.borrow_mut().push((name.to_string(), bytes.clone()));
        Ok(bytes)
    };
    let unpacked = unpack_segments(&manifest, &fetch)?;
    let repo = restore(&unpacked, &dest.into())?;
    // Everything fetched was authenticated by unpack before use.
    let cache = RemoteCache::for_store(&repo, store);
    let policy = Policy::default();
    for (name, bytes) in fetched.into_inner() {
        let Some(rest) = name.strip_prefix("seg/") else {
            continue;
        };
        if let Some(segid) = rest.strip_suffix(".idx") {
            cache.keep_idx(&manifest.segments[segid].idx_sha3, &bytes)?;
        } else if let Some(segid) = rest.strip_suffix(".pk")
            && manifest.segments[segid].bytes < policy.small_segment
        {
            cache.keep_pk(segid, &bytes)?;
        }
    }
    cache.set_manifest(&manifest)?;
    cache.set_synced_heads(
        &manifest
            .forks
            .iter()
            .map(|(fork, head)| (fork.clone(), head.head_id.clone()))
            .collect(),
    )?;
    Ok(repo)
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
        cache
            .join("manifest")
            .join(format!("{}.json", manifest.seq)),
        manifest.to_bytes()?,
    )
    .map_err(ioerr("writing cache manifest"))?;
    Ok(Some(manifest))
}

/// What a push did.
#[derive(Clone, Debug)]
pub struct PushOutcome {
    /// The store's manifest after the push: the one this push published,
    /// or the current one when there was nothing to publish.
    pub manifest: ServeManifest,
    /// Whether this push published a manifest.
    pub published: bool,
    /// Segments this push built.
    pub segments: usize,
    /// Bytes PUT to the store: segments the store lacked, plus the
    /// manifest.
    pub uploaded: u64,
    /// New blobs that travelled as constructs (no bytes).
    pub constructs: usize,
    /// New blobs that travelled as keyframes.
    pub keyframes: usize,
    /// Small segments folded by this swap.
    pub compacted: usize,
    /// Put-if-absent attempts, including the one that won.
    pub attempts: usize,
}

/// How many times push re-plans after losing the manifest race before it
/// gives up.
pub const PUSH_ATTEMPTS: usize = 16;

/// Push: carry what the store lacks, and nothing else (§4).
///
/// 1. discover the latest manifest by probing forward from the cached one;
/// 2. plan the delta: lines past each fork's remote head, blobs no live
///    segment holds (I12), edits and inline creates as constructs;
/// 3. build segments, folding small ones when enough have accumulated;
/// 4. PUT each segment the store lacks (HEAD first);
/// 5. put-if-absent manifest seq N+1 with prev = id of N ← LINEARIZATION
///    POINT (wire);
/// 6. on conflict, re-plan from step 1 against the winner.  Segments that
///    come out byte-identical are already uploaded and cost a HEAD; a
///    winner that stored some of the same blobs changes the plan, which is
///    what keeps the image a set.
///
/// Two pushes advancing the same fork violate single-writer (I8): the
/// loser MUST NOT silently splice, and gets an error telling it to union.
pub fn push(repo: &Repository, store: &dyn ObjectStore, policy: &Policy) -> Result<PushOutcome> {
    let cache = RemoteCache::for_store(repo, store);
    let mut uploaded = 0u64;
    for attempt in 1..=PUSH_ATTEMPTS {
        // Discovery first: it invalidates synced heads that describe a
        // chain the store no longer holds.
        let base = discover(store, &cache)?;
        let synced = cache.synced_heads()?;
        let view = RemoteView::load(store, &cache, base.as_ref())?;
        let plan = delta::plan(repo, &view, &synced, policy)?;
        let (constructs, keyframes) = (plan.constructs, plan.keyframes);
        let synced_now = plan.synced.clone();
        let Some(assembled) = delta::assemble(store, &cache, repo, &view, plan, policy)? else {
            cache.set_synced_heads(&synced_now)?;
            let manifest = base.ok_or_else(|| {
                Error::Invalid("nothing to push: the repository has no forks".to_string())
            })?;
            return Ok(PushOutcome {
                manifest,
                published: false,
                segments: 0,
                uploaded,
                constructs,
                keyframes,
                compacted: 0,
                attempts: attempt,
            });
        };
        for segment in &assembled.segments {
            for (ext, bytes) in [("pk", &segment.pk), ("idx", &segment.idx)] {
                let name = format!("seg/{}.{ext}", segment.segid);
                if !store.exists(&name)? {
                    store.put(&name, bytes)?;
                    uploaded += bytes.len() as u64;
                }
            }
            cache.keep_idx(&segment.idx_sha3, &segment.idx)?;
            if (segment.pk.len() as u64) < policy.small_segment {
                cache.keep_pk(&segment.segid, &segment.pk)?;
            }
        }
        let bytes = assembled.manifest.to_bytes()?;
        if store.put_if_absent(&manifest_name(assembled.manifest.seq), &bytes)? {
            uploaded += bytes.len() as u64;
            cache.set_manifest(&assembled.manifest)?;
            cache.set_synced_heads(&synced_now)?;
            cache.prune_pk(&assembled.manifest)?;
            return Ok(PushOutcome {
                manifest: assembled.manifest,
                published: true,
                segments: assembled.segments.len(),
                uploaded,
                constructs,
                keyframes,
                compacted: assembled.compacted,
                attempts: attempt,
            });
        }
    }
    Err(Error::Invalid(format!(
        "lost the manifest race {PUSH_ATTEMPTS} times; the store is too contended to push"
    )))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::ident::Sum;
    use crate::log::{Annotation, parse_log_strict};
    use crate::patch::{Intent, Op};
    use crate::segment::{ImageItem, image_setsum};

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tally-wire-{name}-{}", std::process::id()));
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

    fn create_by_ref(repo: &Repository, path: &str, content: &[u8]) -> Intent {
        let blob = repo.blobs().put(content).unwrap();
        Intent {
            ops: vec![Op::Create {
                path: path.to_string(),
                mode: "100644".to_string(),
                blob: Some(blob),
                content_b64: None,
            }],
        }
    }

    fn edit(path: &str, old: &str, new: &str) -> Intent {
        Intent {
            ops: vec![Op::Edit {
                path: path.to_string(),
                old_str: old.to_string(),
                new_str: new.to_string(),
            }],
        }
    }

    fn note() -> Annotation {
        Annotation {
            author: "t".to_string(),
            ..Annotation::default()
        }
    }

    fn policy() -> Policy {
        Policy::default()
    }

    /// The setsum of the distinct items a repository holds: its blob pool
    /// and every fork's lines.  A store whose summed segment images equal
    /// this for its own clone holds every item exactly once (I12).
    fn distinct_image(repo: &Repository) -> Sum {
        let mut items = Vec::new();
        for hash in repo.blobs().list().unwrap() {
            items.push(ImageItem::Blob(hash));
        }
        for fork in repo.fork_names().unwrap() {
            let pool = repo.blobs();
            let fetch = |h: &str| -> Result<Vec<u8>> { pool.get(h) };
            for line in parse_log_strict(&repo.log_bytes(&fork).unwrap(), &fetch).unwrap() {
                items.push(ImageItem::Line {
                    fork: fork.clone(),
                    id: line.id,
                });
            }
        }
        image_setsum(&items)
    }

    fn assert_clone_matches(store: &dyn ObjectStore, name: &str, forks: &[(&Repository, &str)]) {
        let cloned = clone(store, temp_dir(name)).unwrap();
        for (repo, fork) in forks {
            assert_eq!(
                repo.current_state(fork).unwrap().sum,
                cloned.current_state(fork).unwrap().sum,
                "fork {fork}"
            );
            assert_eq!(
                repo.log_bytes(fork).unwrap(),
                cloned.log_bytes(fork).unwrap(),
                "log bytes of fork {fork}"
            );
        }
        let manifest = remote_latest(store).unwrap().unwrap();
        assert_eq!(
            manifest.total_image().unwrap(),
            distinct_image(&cloned),
            "the store's image is exactly the set of items it holds (I12)"
        );
    }

    /// An instrumented store: counts what crosses the wire, can fail PUTs
    /// past a budget, and can run a competing push just before a manifest
    /// put-if-absent.
    struct Wire<'a> {
        inner: &'a FsStore,
        lists: Cell<usize>,
        put_bytes: Cell<u64>,
        puts: RefCell<Vec<String>>,
        fail_after_puts: Cell<Option<usize>>,
        race: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
    }

    impl<'a> Wire<'a> {
        fn new(inner: &'a FsStore) -> Self {
            Wire {
                inner,
                lists: Cell::new(0),
                put_bytes: Cell::new(0),
                puts: RefCell::new(Vec::new()),
                fail_after_puts: Cell::new(None),
                race: RefCell::new(None),
            }
        }

        fn record(&self, name: &str, bytes: &[u8]) -> Result<()> {
            if let Some(budget) = self.fail_after_puts.get() {
                if budget == 0 {
                    return Err(Error::Invalid("injected PUT failure".to_string()));
                }
                self.fail_after_puts.set(Some(budget - 1));
            }
            self.puts.borrow_mut().push(name.to_string());
            self.put_bytes
                .set(self.put_bytes.get() + bytes.len() as u64);
            Ok(())
        }
    }

    impl ObjectStore for Wire<'_> {
        fn get(&self, name: &str) -> Result<Option<Vec<u8>>> {
            self.inner.get(name)
        }
        fn put(&self, name: &str, bytes: &[u8]) -> Result<()> {
            self.record(name, bytes)?;
            self.inner.put(name, bytes)
        }
        fn put_if_absent(&self, name: &str, bytes: &[u8]) -> Result<bool> {
            if name.starts_with("manifest/")
                && let Some(race) = self.race.borrow_mut().take()
            {
                race();
            }
            self.record(name, bytes)?;
            self.inner.put_if_absent(name, bytes)
        }
        fn list(&self, prefix: &str) -> Result<Vec<String>> {
            self.lists.set(self.lists.get() + 1);
            self.inner.list(prefix)
        }
        fn exists(&self, name: &str) -> Result<bool> {
            self.inner.exists(name)
        }
        fn locator(&self) -> String {
            self.inner.locator()
        }
    }

    #[test]
    fn push_clone_round_trip() {
        let repo = Repository::init(temp_dir("pcrt-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let store = FsStore::open(temp_dir("pcrt-store")).unwrap();
        let manifest = push(&repo, &store, &policy()).unwrap().manifest;
        assert_eq!(manifest.seq, 1);
        assert_eq!(manifest.prev, "");
        assert_clone_matches(&store, "pcrt-dst", &[(&repo, "main")]);
    }

    #[test]
    fn sequential_pushes_chain() {
        let repo = Repository::init(temp_dir("chain-src")).unwrap();
        let store = FsStore::open(temp_dir("chain-store")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let m1 = push(&repo, &store, &policy()).unwrap().manifest;
        repo.apply("main", create("/b.rs", b"b\n"), note()).unwrap();
        let m2 = push(&repo, &store, &policy()).unwrap().manifest;
        assert_eq!(m2.seq, 2);
        assert_eq!(m2.prev, m1.id);
    }

    #[test]
    fn a_push_appends_and_retires_nothing_and_an_idle_push_publishes_nothing() {
        let store = FsStore::open(temp_dir("append-store")).unwrap();
        let repo = Repository::init(temp_dir("append-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let m1 = push(&repo, &store, &policy()).unwrap().manifest;
        assert_eq!(m1.segments.len(), 1);
        repo.apply("main", create("/b.rs", b"b\n"), note()).unwrap();
        let m2 = push(&repo, &store, &policy()).unwrap().manifest;
        assert_eq!(m2.segments.len(), 2, "a push appends a segment");
        assert!(m2.retire.is_empty(), "and replaces nothing");
        for segid in m1.segments.keys() {
            assert!(
                m2.segments.contains_key(segid),
                "the first segment stays live"
            );
        }
        assert_eq!(
            m2.forks["main"].log_segments.len(),
            2,
            "main's log spans both"
        );
        // The swap's image delta is exactly the new segment's image.
        let added: Vec<_> = m2
            .segments
            .keys()
            .filter(|s| !m1.segments.contains_key(*s))
            .collect();
        assert_eq!(
            crate::serve::swap_delta(&m1, &m2).unwrap(),
            Sum::from_hexdigest(&m2.segments[added[0]].image_setsum).unwrap()
        );
        // Nothing new: nothing is published.
        let idle = push(&repo, &store, &policy()).unwrap();
        assert!(!idle.published);
        assert_eq!(idle.manifest.id, m2.id);
        assert!(store.get(&manifest_name(3)).unwrap().is_none());
        assert_clone_matches(&store, "append-dst", &[(&repo, "main")]);
    }

    #[test]
    fn edits_travel_as_constructs() {
        let store = FsStore::open(temp_dir("edits-store")).unwrap();
        let repo = Repository::init(temp_dir("edits-src")).unwrap();
        // Hex noise per line, so zstd cannot make the keyframe small.
        let big: String = (0..2_000)
            .map(|i: u32| format!("line {i} {}\n", crate::ident::sha3_hex(&i.to_le_bytes())))
            .collect();
        repo.apply(
            "main",
            create_by_ref(&repo, "/big.txt", big.as_bytes()),
            note(),
        )
        .unwrap();
        repo.apply("main", create("/small.rs", b"fn main() {}\n"), note())
            .unwrap();
        let first = push(&repo, &store, &policy()).unwrap();
        assert_eq!(
            first.keyframes, 1,
            "a create by blob reference is a keyframe"
        );
        assert_eq!(first.constructs, 1, "an inline create replays from nothing");
        assert!(first.uploaded > 64 << 10, "the keyframe travels once");

        for k in [3, 141, 592, 1_800] {
            repo.apply(
                "main",
                edit(
                    "/big.txt",
                    &format!("line {k} "),
                    &format!("line {k} edited "),
                ),
                note(),
            )
            .unwrap();
        }
        let second = push(&repo, &store, &policy()).unwrap();
        assert_eq!(second.keyframes, 0, "no edited version ships its bytes");
        assert_eq!(second.constructs, 4);
        assert!(
            second.uploaded < 8 << 10,
            "an edit push costs its log lines, not the file: {} bytes",
            second.uploaded
        );
        assert_clone_matches(&store, "edits-dst", &[(&repo, "main")]);
    }

    #[test]
    fn construct_chains_bottom_out_at_keyframes() {
        let store = FsStore::open(temp_dir("chain-k-store")).unwrap();
        let repo = Repository::init(temp_dir("chain-k-src")).unwrap();
        let text: String = (0..64).map(|i| format!("row {i}\n")).collect();
        repo.apply(
            "main",
            create_by_ref(&repo, "/f.txt", text.as_bytes()),
            note(),
        )
        .unwrap();
        let k3 = Policy {
            keyframe_interval: 3,
            ..policy()
        };
        push(&repo, &store, &k3).unwrap();
        // Ten edits in one push: depths 1,2,3,K,1,2,3,K,1,2 over the
        // keyframe the first push stored.
        for i in 0..10 {
            repo.apply(
                "main",
                edit("/f.txt", &format!("row {i}\n"), &format!("row {i}!\n")),
                note(),
            )
            .unwrap();
        }
        let outcome = push(&repo, &store, &k3).unwrap();
        assert_eq!(outcome.keyframes, 2);
        assert_eq!(outcome.constructs, 8);
        // Constructs are packed in hash order, not chain order; unpack
        // resolves chains to a fixpoint.
        assert_clone_matches(&store, "chain-k-dst", &[(&repo, "main")]);
    }

    #[test]
    fn construct_chains_span_segments() {
        let store = FsStore::open(temp_dir("xseg-store")).unwrap();
        let repo = Repository::init(temp_dir("xseg-src")).unwrap();
        repo.apply("main", create("/f.rs", b"let x = 0;\n"), note())
            .unwrap();
        push(&repo, &store, &policy()).unwrap();
        for i in 0..5 {
            repo.apply(
                "main",
                edit("/f.rs", &format!("x = {i};"), &format!("x = {};", i + 1)),
                note(),
            )
            .unwrap();
            let outcome = push(&repo, &store, &policy()).unwrap();
            assert_eq!((outcome.constructs, outcome.keyframes), (1, 0));
        }
        // Six segments, one chain of six constructs from the empty blob.
        assert_clone_matches(&store, "xseg-dst", &[(&repo, "main")]);
    }

    #[test]
    fn pushes_to_different_forks_merge_trivially() {
        let store = FsStore::open(temp_dir("merge-store")).unwrap();
        let alice = Repository::init(temp_dir("merge-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &store, &policy()).unwrap();
        // Bob clones and works on his own fork.
        let bob = clone(&store, temp_dir("merge-bob")).unwrap();
        bob.create_fork("bob", "main").unwrap();
        bob.apply("bob", create("/b.rs", b"b\n"), note()).unwrap();
        push(&bob, &store, &policy()).unwrap();
        // Alice advances main concurrently; her push must merge bob's fork
        // forward, not lose it.
        alice
            .apply("main", create("/c.rs", b"c\n"), note())
            .unwrap();
        let m = push(&alice, &store, &policy()).unwrap().manifest;
        assert!(
            m.forks.contains_key("bob"),
            "different forks merge trivially"
        );
        assert!(m.forks.contains_key("main"));
        let carol = clone(&store, temp_dir("merge-carol")).unwrap();
        assert_eq!(carol.current_state("main").unwrap().manifest.len(), 2);
        assert_eq!(carol.current_state("bob").unwrap().manifest.len(), 2);
    }

    #[test]
    fn being_behind_on_a_fork_is_not_divergence() {
        let store = FsStore::open(temp_dir("behind-store")).unwrap();
        let alice = Repository::init(temp_dir("behind-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &store, &policy()).unwrap();
        let bob = clone(&store, temp_dir("behind-bob")).unwrap();
        alice
            .apply("main", create("/x.rs", b"x\n"), note())
            .unwrap();
        let alices = push(&alice, &store, &policy()).unwrap().manifest;
        // Bob never touches main; his main is merely behind.
        bob.create_fork("bob", "main").unwrap();
        bob.apply("bob", create("/b.rs", b"b\n"), note()).unwrap();
        let m = push(&bob, &store, &policy()).unwrap().manifest;
        assert_eq!(
            m.forks["main"], alices.forks["main"],
            "main is left exactly as alice pushed it"
        );
        assert!(m.forks.contains_key("bob"));
    }

    #[test]
    fn same_fork_advance_refuses_to_splice() {
        let store = FsStore::open(temp_dir("splice-store")).unwrap();
        let alice = Repository::init(temp_dir("splice-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &store, &policy()).unwrap();
        let bob = clone(&store, temp_dir("splice-bob")).unwrap();
        // Both advance main.
        alice
            .apply("main", create("/x.rs", b"x\n"), note())
            .unwrap();
        push(&alice, &store, &policy()).unwrap();
        bob.apply("main", create("/y.rs", b"y\n"), note()).unwrap();
        let err = push(&bob, &store, &policy());
        assert!(
            matches!(err, Err(Error::Invalid(_))),
            "loser must not silently splice"
        );
    }

    #[test]
    fn the_race_loser_replans_and_the_image_stays_a_set() {
        let fs_store = FsStore::open(temp_dir("race-store")).unwrap();
        let alice = Repository::init(temp_dir("race-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &fs_store, &policy()).unwrap();
        let bob = clone(&fs_store, temp_dir("race-bob")).unwrap();
        // Both produce the same blob on their own forks.
        alice.create_fork("alice-f", "main").unwrap();
        alice
            .apply("alice-f", create("/shared.rs", b"same bytes\n"), note())
            .unwrap();
        bob.create_fork("bob-f", "main").unwrap();
        bob.apply("bob-f", create("/shared.rs", b"same bytes\n"), note())
            .unwrap();
        // Alice's push lands between Bob's segment upload and his manifest
        // put-if-absent.
        let wire = Wire::new(&fs_store);
        *wire.race.borrow_mut() = Some(Box::new(|| {
            push(&alice, &fs_store, &policy()).unwrap();
        }));
        let outcome = push(&bob, &wire, &policy()).unwrap();
        assert_eq!(outcome.attempts, 2, "bob lost once, re-planned, won");
        assert_eq!(outcome.manifest.seq, 3);
        // The re-plan dropped the blob alice stored: no live duplicate.
        let cache = RemoteCache::at(temp_dir("race-view-cache"));
        RemoteView::load(&fs_store, &cache, Some(&outcome.manifest)).unwrap();
        // Bob's first-attempt segment is orphaned: uploaded, never named.
        let orphans: Vec<String> = wire
            .puts
            .borrow()
            .iter()
            .filter_map(|n| {
                n.strip_prefix("seg/")?
                    .strip_suffix(".pk")
                    .map(String::from)
            })
            .filter(|s| !outcome.manifest.segments.contains_key(s))
            .collect();
        assert_eq!(orphans.len(), 1, "the loser's first segment is garbage");
        assert_clone_matches(
            &fs_store,
            "race-dst",
            &[(&alice, "main"), (&alice, "alice-f"), (&bob, "bob-f")],
        );
    }

    #[test]
    fn an_interrupted_push_resumes_without_resending() {
        let fs_store = FsStore::open(temp_dir("resume-store")).unwrap();
        let repo = Repository::init(temp_dir("resume-src")).unwrap();
        // Exhaust: blobs no line produces travel as keyframes.
        let mut seed = 0x9e3779b97f4a7c15u64;
        for _ in 0..40 {
            let bytes: Vec<u8> = (0..2048)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed as u8
                })
                .collect();
            repo.blobs().put(&bytes).unwrap();
        }
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let small = Policy {
            segment_target: 8 << 10,
            ..policy()
        };
        let wire = Wire::new(&fs_store);
        wire.fail_after_puts.set(Some(6));
        assert!(push(&repo, &wire, &small).is_err(), "the upload is cut off");
        let landed = wire.puts.borrow().clone();
        assert_eq!(landed.len(), 6);
        let wire = Wire::new(&fs_store);
        let outcome = push(&repo, &wire, &small).unwrap();
        assert!(
            outcome.segments > 3,
            "the pack was cut into several segments"
        );
        for name in wire.puts.borrow().iter() {
            assert!(!landed.contains(name), "{name} was sent twice");
        }
        assert_clone_matches(&fs_store, "resume-dst", &[(&repo, "main")]);
    }

    #[test]
    fn a_warm_push_never_lists() {
        let fs_store = FsStore::open(temp_dir("warm-store")).unwrap();
        let repo = Repository::init(temp_dir("warm-src")).unwrap();
        let wire = Wire::new(&fs_store);
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        push(&repo, &wire, &policy()).unwrap();
        assert_eq!(wire.lists.get(), 1, "a cold cache lists once");
        for i in 0..5 {
            repo.apply("main", create(&format!("/f{i}.rs"), b"f\n"), note())
                .unwrap();
            push(&repo, &wire, &policy()).unwrap();
        }
        assert_eq!(wire.lists.get(), 1, "a warm cache probes forward instead");
        // A clone is warm from the start.
        let bob = clone(&fs_store, temp_dir("warm-bob")).unwrap();
        bob.create_fork("bob", "main").unwrap();
        bob.apply("bob", create("/b.rs", b"b\n"), note()).unwrap();
        let wire = Wire::new(&fs_store);
        push(&bob, &wire, &policy()).unwrap();
        assert_eq!(wire.lists.get(), 0);
    }

    #[test]
    fn a_wiped_store_is_not_mistaken_for_the_cached_one() {
        let store_dir = temp_dir("wipe-store");
        let repo = Repository::init(temp_dir("wipe-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        push(&repo, &FsStore::open(&store_dir).unwrap(), &policy()).unwrap();
        // The store is wiped and someone else pushes seq 1.
        fs::remove_dir_all(&store_dir).unwrap();
        let other = Repository::init(temp_dir("wipe-other")).unwrap();
        other
            .apply("main", create("/z.rs", b"z\n"), note())
            .unwrap();
        let store = FsStore::open(&store_dir).unwrap();
        push(&other, &store, &policy()).unwrap();
        // The stale cache names seq 1 with a different id: discovery falls
        // back to LIST and the push refuses to splice main.
        assert!(matches!(
            push(&repo, &store, &policy()),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn small_segments_fold_and_the_manifest_stays_bounded() {
        let store = FsStore::open(temp_dir("absurd-store")).unwrap();
        let alice = Repository::init(temp_dir("absurd-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &store, &policy()).unwrap();
        // Bob's fork exists only remotely; alice's folds must carry it.
        let bob = clone(&store, temp_dir("absurd-bob")).unwrap();
        bob.create_fork("bob", "main").unwrap();
        bob.apply("bob", create("/b.rs", b"b\n"), note()).unwrap();
        let mut prev = push(&bob, &store, &policy()).unwrap().manifest;

        const ABSURD: usize = 200;
        let trigger = policy().compact_trigger;
        let mut steady: Vec<u64> = Vec::new();
        let mut folds = 0;
        for i in 0..ABSURD {
            alice
                .apply(
                    "main",
                    create(&format!("/f{i:04}.rs"), format!("{i}\n").as_bytes()),
                    note(),
                )
                .unwrap();
            let outcome = push(&alice, &store, &policy()).unwrap();
            let m = &outcome.manifest;
            assert!(
                m.segments.len() <= trigger,
                "push {i}: {} live segments",
                m.segments.len()
            );
            assert_eq!(m.anchors.len(), 2, "push {i}: anchors track live forks");
            if outcome.compacted > 0 {
                folds += 1;
                assert_eq!(m.retire.len(), outcome.compacted);
                // A fold is a pure re-encoding plus this push's items: the
                // folded segments' image moves whole into the new one.
                let mut retired = Sum::zero();
                for segid in &m.retire {
                    retired =
                        retired + Sum::from_hexdigest(&prev.segments[segid].image_setsum).unwrap();
                }
                let mut fresh = Sum::zero();
                for (segid, meta) in &m.segments {
                    if !prev.segments.contains_key(segid) {
                        fresh = fresh + Sum::from_hexdigest(&meta.image_setsum).unwrap();
                    }
                }
                assert_eq!(crate::serve::swap_delta(&prev, m).unwrap(), fresh - retired,);
            } else {
                assert!(m.retire.is_empty());
                steady.push(outcome.uploaded);
            }
            prev = m.clone();
        }
        assert!(folds >= ABSURD / trigger - 1, "{folds} folds");
        // Per-push cost is flat in history: the last pushes cost what the
        // first ones did.
        let head: u64 = steady[..20].iter().sum();
        let tail: u64 = steady[steady.len() - 20..].iter().sum();
        assert!(
            tail <= head * 3 / 2,
            "steady-state push cost grew from {head} to {tail} bytes per 20 pushes"
        );
        // Every manifest ever published chains, and the clone restores
        // every fork exactly, with each item stored once.
        for seq in 2..=prev.seq {
            let m =
                ServeManifest::parse(&store.get(&manifest_name(seq)).unwrap().unwrap()).unwrap();
            let p = ServeManifest::parse(&store.get(&manifest_name(seq - 1)).unwrap().unwrap())
                .unwrap();
            assert_eq!(m.prev, p.id);
        }
        assert_clone_matches(&store, "absurd-carol", &[(&alice, "main"), (&bob, "bob")]);
    }

    /// Prose too large for a log line even after spilling, and too noisy
    /// for zstd to shrink: the line overflows into a blob that stays large
    /// once packed.
    fn overflowing_note() -> Annotation {
        let noise: String = (0..(2 * crate::log::SPILL_THRESHOLD / 64))
            .map(|i: usize| crate::ident::sha3_hex(&i.to_le_bytes()))
            .collect();
        Annotation {
            prose: Some(noise),
            ..note()
        }
    }

    #[test]
    fn an_overflow_blob_is_never_a_construct() {
        let store = FsStore::open(temp_dir("ovc-store")).unwrap();
        let repo = Repository::init(temp_dir("ovc-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), overflowing_note())
            .unwrap();
        let refs = crate::log::line_blob_refs(&repo.log_bytes("main").unwrap());
        assert_eq!(refs.len(), 1, "the line overflowed");
        let overflow = refs.into_iter().next().unwrap();
        // A file whose bytes are that line's canonical JSON — the same blob,
        // added by a line in the same push.  Packed as a construct, it would
        // wait on a line that waits on it.
        let canonical = repo.blobs().get(&overflow).unwrap();
        repo.apply("main", create("/line.json", &canonical), note())
            .unwrap();
        let outcome = push(&repo, &store, &policy()).unwrap();
        let view = RemoteView::load(
            &store,
            &RemoteCache::at(temp_dir("ovc-view")),
            Some(&outcome.manifest),
        )
        .unwrap();
        let stored_as = view.entries.values().flatten().find(|e| e.sha3 == overflow);
        assert!(
            matches!(stored_as.map(|e| &e.enc), Some(crate::segment::Enc::Raw)),
            "the overflow blob is stored whole: {stored_as:?}"
        );
        assert_clone_matches(&store, "ovc-dst", &[(&repo, "main")]);
    }

    #[test]
    fn a_fold_resolves_overflowed_lines_it_never_had() {
        let store = FsStore::open(temp_dir("ovf-store")).unwrap();
        let sized = Policy {
            segment_target: 32 << 10,
            small_segment: 20 << 10,
            compact_trigger: 100,
            ..policy()
        };
        let alice = Repository::init(temp_dir("ovf-alice")).unwrap();
        alice
            .apply("main", create("/a.rs", b"a\n"), note())
            .unwrap();
        push(&alice, &store, &sized).unwrap();
        // Carol clones before the overflowing line exists anywhere.
        let carol = clone(&store, temp_dir("ovf-carol")).unwrap();
        alice.create_fork("side", "main").unwrap();
        alice
            .apply("side", create("/s.rs", b"s\n"), overflowing_note())
            .unwrap();
        push(&alice, &store, &sized).unwrap();
        let overflow = crate::log::line_blob_refs(&alice.log_bytes("side").unwrap())
            .into_iter()
            .next()
            .unwrap();
        assert!(carol.blobs().get(&overflow).is_err(), "carol never had it");
        // Carol's push folds every small segment — including the one holding
        // side's span — while the overflow blob's segment is too large to
        // fold.  Imaging the folded span needs the blob: from the store.
        carol.create_fork("carol", "main").unwrap();
        carol
            .apply("carol", create("/c.rs", b"c\n"), note())
            .unwrap();
        let folding = Policy {
            compact_trigger: 1,
            ..sized
        };
        let outcome = push(&carol, &store, &folding).unwrap();
        assert!(outcome.compacted >= 2, "folded {}", outcome.compacted);
        assert_clone_matches(
            &store,
            "ovf-dst",
            &[(&alice, "main"), (&alice, "side"), (&carol, "carol")],
        );
    }

    #[test]
    fn a_duplicated_live_blob_is_refused() {
        use crate::segment::{SegmentInput, build_segment};
        use crate::serve::SegmentMeta;
        let store = FsStore::open(temp_dir("dup-store")).unwrap();
        let one = build_segment(&[SegmentInput::Blob(b"x".to_vec())], 3).unwrap();
        let two = build_segment(
            &[
                SegmentInput::Blob(b"x".to_vec()),
                SegmentInput::Blob(b"y".to_vec()),
            ],
            3,
        )
        .unwrap();
        let mut segments = std::collections::BTreeMap::new();
        for built in [&one, &two] {
            store
                .put(&format!("seg/{}.pk", built.segid), &built.pk)
                .unwrap();
            store
                .put(&format!("seg/{}.idx", built.segid), &built.idx)
                .unwrap();
            segments.insert(built.segid.clone(), SegmentMeta::of(built));
        }
        let mut manifest = ServeManifest {
            id: String::new(),
            v: 0,
            seq: 1,
            prev: String::new(),
            forks: Default::default(),
            segments,
            anchors: Vec::new(),
            retire: Vec::new(),
        };
        manifest.id = crate::ident::record_id(&serde_json::to_value(&manifest).unwrap()).unwrap();
        store
            .put_if_absent("manifest/1.json", &manifest.to_bytes().unwrap())
            .unwrap();
        let repo = Repository::init(temp_dir("dup-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        match push(&repo, &store, &policy()) {
            Err(Error::Corrupt(msg)) => assert!(msg.contains("I12"), "{msg}"),
            other => panic!("a duplicated live blob must be refused, got {other:?}"),
        }
    }

    #[test]
    fn fetch_populates_a_cache() {
        let repo = Repository::init(temp_dir("fetch-src")).unwrap();
        repo.apply("main", create("/a.rs", b"a\n"), note()).unwrap();
        let store = FsStore::open(temp_dir("fetch-store")).unwrap();
        let pushed = push(&repo, &store, &policy()).unwrap().manifest;
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
        store
            .put("manifest/1.json", br#"{"id":"lies","v":0,"seq":1}"#)
            .unwrap();
        assert!(remote_latest(&store).is_err());
    }
}
