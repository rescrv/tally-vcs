//! §4 Remote tracking: what a client knows about a store, kept so that a
//! push costs work proportional to what changed, not to what exists.
//!
//! Everything here lives under `.tally/index/remote/<store>/` and is
//! derived (I3): the cached manifest and `.idx` tables are reconstructible
//! from the store, and deleting the directory only costs re-fetching them.
//! The one non-derived record is the synced-heads table — the fork heads at
//! the last clone or push — and losing it is safe in the conservative
//! direction: a fork you are merely behind on is refused as if it had
//! diverged, exactly as it would be with no table at all.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::ident::sha3_hex;
use crate::repo::Repository;
use crate::segment::{EMPTY_BLOB, Enc, IdxEntry, parse_idx};
use crate::serve::{SegmentMeta, ServeManifest};
use crate::wire::{ObjectStore, remote_latest};
use crate::{Error, Result, ioerr};

/// The name of the manifest at `seq`.
pub fn manifest_name(seq: u64) -> String {
    format!("manifest/{seq}.json")
}

/// Write `bytes` to `path` atomically: a temp file beside it, then
/// `rename(2)`.  The cache is derived, so durability is not required; a
/// reader must never observe a partial file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("no parent directory for {}", path.display())))?;
    fs::create_dir_all(dir).map_err(ioerr(format!("creating {}", dir.display())))?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::write(&tmp, bytes).map_err(ioerr(format!("writing {}", tmp.display())))?;
    fs::rename(&tmp, path).map_err(ioerr(format!("renaming into {}", path.display())))
}

/// Read a cache file, treating absence as a miss.
fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(ioerr(format!("reading {}", path.display()))(err)),
    }
}

/////////////////////////////////////////// RemoteCache ///////////////////////////////////////////

/// The per-store cache under `.tally/index/remote/`.
pub struct RemoteCache {
    dir: PathBuf,
}

impl RemoteCache {
    /// The cache for `store` in `repo`, keyed by the store's locator.
    pub fn for_store(repo: &Repository, store: &dyn ObjectStore) -> Self {
        let key = sha3_hex(store.locator().as_bytes());
        RemoteCache {
            dir: repo.index_dir().join("remote").join(&key[..32]),
        }
    }

    /// A cache rooted at an explicit directory.
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        RemoteCache { dir: dir.into() }
    }

    /// The last manifest this client verified, if any.  The cache is
    /// derived, so an unparseable entry is a miss, never an error.
    pub fn manifest(&self) -> Result<Option<ServeManifest>> {
        let Some(bytes) = read_optional(&self.dir.join("manifest.json"))? else {
            return Ok(None);
        };
        Ok(ServeManifest::parse(&bytes).ok())
    }

    /// Forget everything tied to one manifest chain: the cached manifest
    /// and the synced heads.  Called when the store no longer holds the
    /// cached manifest — it was wiped, or replaced — so "this was the
    /// remote head" no longer means anything.  Content-addressed `.idx`
    /// tables stay valid and stay.
    pub fn invalidate(&self) -> Result<()> {
        for name in ["manifest.json", "synced.json"] {
            match fs::remove_file(self.dir.join(name)) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(ioerr(format!("invalidating {name}"))(err)),
            }
        }
        Ok(())
    }

    /// Record the last verified manifest.
    pub fn set_manifest(&self, manifest: &ServeManifest) -> Result<()> {
        write_atomic(&self.dir.join("manifest.json"), &manifest.to_bytes()?)
    }

    /// A segment's `.idx`, authenticated against the manifest's `idx_sha3`
    /// (I11).  Served from the cache when present and intact; otherwise
    /// fetched, verified, and cached.  Content-addressed, so a cached
    /// table never invalidates.
    pub fn idx(&self, store: &dyn ObjectStore, segid: &str, meta: &SegmentMeta) -> Result<Vec<u8>> {
        let path = self.dir.join("idx").join(&meta.idx_sha3);
        if let Some(bytes) = read_optional(&path)?
            && sha3_hex(&bytes) == meta.idx_sha3
        {
            return Ok(bytes);
        }
        let name = format!("seg/{segid}.idx");
        let bytes = store
            .get(&name)?
            .ok_or_else(|| Error::Corrupt(format!("manifest references absent object {name}")))?;
        if sha3_hex(&bytes) != meta.idx_sha3 {
            return Err(Error::Corrupt(format!("{name}: idx hash mismatch")));
        }
        write_atomic(&path, &bytes)?;
        Ok(bytes)
    }

    /// Cache an `.idx` this client built (and so already trusts).
    pub fn keep_idx(&self, idx_sha3: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.dir.join("idx").join(idx_sha3), bytes)
    }

    /// A segment's `.pk`, authenticated against the manifest's `pk_sha3`
    /// before anything else touches it (I11).  Served from the cache when
    /// present and intact, otherwise fetched; fetched payloads are not
    /// cached.
    pub fn pk(&self, store: &dyn ObjectStore, segid: &str, meta: &SegmentMeta) -> Result<Vec<u8>> {
        let path = self.dir.join("pk").join(segid);
        if let Some(bytes) = read_optional(&path)?
            && sha3_hex(&bytes) == meta.pk_sha3
        {
            return Ok(bytes);
        }
        let name = format!("seg/{segid}.pk");
        let bytes = store
            .get(&name)?
            .ok_or_else(|| Error::Corrupt(format!("manifest references absent object {name}")))?;
        if sha3_hex(&bytes) != meta.pk_sha3 {
            return Err(Error::Corrupt(format!("{name}: pk hash mismatch")));
        }
        Ok(bytes)
    }

    /// Cache a `.pk` this client built, so a later compaction can fold it
    /// without fetching it back.
    pub fn keep_pk(&self, segid: &str, bytes: &[u8]) -> Result<()> {
        write_atomic(&self.dir.join("pk").join(segid), bytes)
    }

    /// Drop cached payloads no longer live in `manifest`.  Payloads are
    /// kept only to feed compaction, and compaction only reads live
    /// segments.
    pub fn prune_pk(&self, manifest: &ServeManifest) -> Result<()> {
        let dir = self.dir.join("pk");
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(ioerr("listing cached payloads")(err)),
        };
        for entry in entries {
            let entry = entry.map_err(ioerr("listing cached payloads"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !manifest.segments.contains_key(&name) {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(())
    }

    /// The fork heads as of this client's last clone or push of each fork.
    pub fn synced_heads(&self) -> Result<BTreeMap<String, String>> {
        let Some(bytes) = read_optional(&self.dir.join("synced.json"))? else {
            return Ok(BTreeMap::new());
        };
        Ok(serde_json::from_slice(&bytes).unwrap_or_default())
    }

    /// Record fork heads as synced, merging over what is recorded.
    pub fn set_synced_heads(&self, heads: &BTreeMap<String, String>) -> Result<()> {
        let mut all = self.synced_heads()?;
        for (fork, head) in heads {
            all.insert(fork.clone(), head.clone());
        }
        let bytes = crate::ident::canonical_json(&serde_json::to_value(&all)?)?;
        write_atomic(&self.dir.join("synced.json"), bytes.as_bytes())
    }
}

//////////////////////////////////////////// discovery ////////////////////////////////////////////

/// Find the store's latest manifest without listing it.  Start from the
/// cached manifest (confirming the store still holds it, byte for byte by
/// id) and probe `manifest/<seq+1>.json` forward until a miss; every step
/// must chain (`seq` and `prev`), so what comes back is an authenticated
/// extension of what this client already verified.  Only a cold cache
/// falls back to LIST — and a cache whose manifest the store no longer
/// holds is invalidated first, synced heads included, because they
/// describe a chain that no longer exists.  The result is recorded in the
/// cache.
pub fn discover(store: &dyn ObjectStore, cache: &RemoteCache) -> Result<Option<ServeManifest>> {
    let warm = match cache.manifest()? {
        Some(cached) => {
            let confirmed = match store.get(&manifest_name(cached.seq))? {
                Some(bytes) => match ServeManifest::parse(&bytes) {
                    Ok(remote) if remote.id == cached.id => Some(remote),
                    _ => None,
                },
                None => None,
            };
            if confirmed.is_none() {
                // The chain this client verified is gone from the store.
                cache.invalidate()?;
            }
            confirmed
        }
        None => None,
    };
    let mut current = match warm {
        Some(m) => m,
        None => match remote_latest(store)? {
            Some(m) => m,
            None => return Ok(None),
        },
    };
    while let Some(bytes) = store.get(&manifest_name(current.seq + 1))? {
        let next = ServeManifest::parse(&bytes)?;
        if next.seq != current.seq + 1 || next.prev != current.id {
            return Err(Error::Corrupt(format!(
                "manifest {} does not chain to manifest {} ({})",
                next.seq, current.seq, current.id
            )));
        }
        current = next;
    }
    cache.set_manifest(&current)?;
    Ok(Some(current))
}

//////////////////////////////////////////// RemoteView ///////////////////////////////////////////

/// What a store holds, as far as a writer needs to know: every blob item
/// in a live segment, how it is stored, and how deep its construct chain
/// runs.  Built from `.idx` tables alone; no payload is fetched.
#[derive(Clone, Debug, Default)]
pub struct RemoteView {
    /// The manifest this view describes; `None` for an empty store.
    pub manifest: Option<ServeManifest>,
    /// Parsed `.idx` entries of every live segment.
    pub entries: BTreeMap<String, Vec<IdxEntry>>,
    /// Every blob (and dictionary) item in a live segment → its segid.
    pub blobs: BTreeMap<String, String>,
    /// Construct chain depth of every blob item: 0 for a keyframe.
    depth: BTreeMap<String, usize>,
}

impl RemoteView {
    /// The view of an empty store.
    pub fn empty() -> Self {
        RemoteView::default()
    }

    /// Build the view of `manifest`, reading `.idx` tables through the
    /// cache.  Enforces I12 on the way: a blob item stored in two live
    /// segments, a construct whose base is not live, or a construct cycle
    /// is corruption.
    pub fn load(
        store: &dyn ObjectStore,
        cache: &RemoteCache,
        manifest: Option<&ServeManifest>,
    ) -> Result<Self> {
        let Some(manifest) = manifest else {
            return Ok(RemoteView::empty());
        };
        let mut view = RemoteView {
            manifest: Some(manifest.clone()),
            ..RemoteView::default()
        };
        let mut bases: BTreeMap<String, String> = BTreeMap::new();
        for (segid, meta) in &manifest.segments {
            let idx = cache.idx(store, segid, meta)?;
            let entries = parse_idx(&idx)?;
            for entry in &entries {
                if matches!(entry.enc, Enc::Lines { .. }) {
                    continue;
                }
                if let Some(other) = view.blobs.insert(entry.sha3.clone(), segid.clone()) {
                    return Err(Error::Corrupt(format!(
                        "blob {} is stored in live segments {other} and {segid}; \
                         the image must be a set (I12)",
                        entry.sha3
                    )));
                }
                if let Enc::Construct { base, .. } = &entry.enc {
                    bases.insert(entry.sha3.clone(), base.clone());
                }
            }
            view.entries.insert(segid.clone(), entries);
        }
        // Chain depths, iteratively: walk each chain down to a known depth
        // or a keyframe, then unwind.
        for start in view.blobs.keys() {
            let mut chain: Vec<String> = Vec::new();
            let mut seen: BTreeSet<String> = BTreeSet::new();
            let mut cursor = start.clone();
            let bottom = loop {
                if let Some(d) = view.depth.get(&cursor) {
                    break *d;
                }
                let Some(base) = bases.get(&cursor) else {
                    // A keyframe.
                    view.depth.insert(cursor.clone(), 0);
                    break 0;
                };
                if !seen.insert(cursor.clone()) {
                    return Err(Error::Corrupt(format!(
                        "construct cycle through blob {cursor}"
                    )));
                }
                chain.push(cursor.clone());
                if base == EMPTY_BLOB {
                    break 0;
                }
                if !view.blobs.contains_key(base) {
                    return Err(Error::Corrupt(format!(
                        "construct of {cursor} references base {base}, which no live segment holds"
                    )));
                }
                cursor = base.clone();
            };
            let mut depth = bottom;
            for hash in chain.into_iter().rev() {
                depth += 1;
                view.depth.insert(hash, depth);
            }
        }
        Ok(view)
    }

    /// True iff a live segment holds the blob.
    pub fn has(&self, blob: &str) -> bool {
        self.blobs.contains_key(blob)
    }

    /// The construct chain depth of a live blob: 0 for a keyframe.  The
    /// empty blob is implicitly present at depth 0.
    pub fn depth(&self, blob: &str) -> Option<usize> {
        if blob == EMPTY_BLOB {
            return Some(0);
        }
        self.depth.get(blob).copied()
    }
}
