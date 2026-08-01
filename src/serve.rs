//! §3.2–§3.3 The serve-manifest, packing, and unpacking.
//!
//! The packed repository is described by a chained sequence of manifests.
//! Loose is truth; pack is an encoding; unpack is total, deterministic,
//! model-free, and equivalence-preserving (I2).  Compaction correctness is
//! an arithmetic proof: across any manifest swap, the image-setsum delta
//! equals the setsum of items genuinely added — zero for a pure compaction.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::fork::ForkFile;
use crate::ident::{Sum, canonical_json, record_id, verify_record_id};
use crate::log::{LogLine, parse_log_strict};
use crate::manifest::Manifest;
use crate::patch::{apply_realized_to_manifest, apply_realized_to_sum};
use crate::repo::Repository;
use crate::segment::{BuiltSegment, Segment, SegmentInput, build_segment, image_setsum};
use crate::{Error, Result, ioerr};

/// Root-of-trust parse bound for a serve-manifest (§5): 16 MiB.
pub const MANIFEST_SIZE_BOUND: usize = 16 << 20;

/// A fork's head as the serve-manifest records it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkHead {
    /// The fork's anchor sum.
    pub anchor: String,
    /// The id of the last log line; `""` for an empty log.
    pub head_id: String,
    /// The fork's current state sum.
    pub head_sum: String,
    /// The segments holding this fork's log, in log order.
    pub log_segments: Vec<String>,
}

/// A segment's metadata: two integrity values answering different
/// questions.  `pk_sha3` authenticates the particular encoded artifact and
/// MUST be checked before any byte reaches a decompressor; `image_setsum`
/// identifies the logical content regardless of encoding.  Equal image,
/// unequal pk: a re-encoding.  Unequal image: content forgery.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentMeta {
    /// SHA3-256 of the compressed `.pk` bytes; equals the segid.
    pub pk_sha3: String,
    /// SHA3-256 of the `.idx` bytes.
    pub idx_sha3: String,
    /// Size of the `.pk` in bytes.
    pub bytes: u64,
    /// Number of idx entries.
    pub entries: u64,
    /// Setsum over the segment's image items.
    pub image_setsum: String,
    /// First and last log line ids in the segment, if it holds log spans.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_span: Option<(String, String)>,
}

/// `manifest/<seq>.json`, canonical JSON, id per §1.3, put-if-absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServeManifest {
    /// Record id per §1.3.
    pub id: String,
    /// Format version.
    pub v: u64,
    /// Sequence number; the put-if-absent on seq N+1 is the wire
    /// linearization point (I8).
    pub seq: u64,
    /// Id of the manifest at seq−1; `""` for the first.  The history of
    /// swaps is itself an auditable log.
    pub prev: String,
    /// Fork heads.
    pub forks: BTreeMap<String, ForkHead>,
    /// Segment metadata by segid.
    pub segments: BTreeMap<String, SegmentMeta>,
    /// Anchor sums the forks reference.
    pub anchors: Vec<String>,
    /// Segments retired by this swap; retained until the retention window
    /// ages them out (§3.3) — the only thing abelian ever deletes, and they
    /// are re-encodings, not information.
    pub retire: Vec<String>,
}

impl ServeManifest {
    /// Canonical bytes as stored.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    /// Size-bounded parse plus id verification (§4 step 1): the manifest is
    /// the residual root-of-trust surface, so the bound is a security
    /// parameter, not a convenience.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MANIFEST_SIZE_BOUND {
            return Err(Error::Corrupt(format!(
                "serve-manifest of {} bytes exceeds the {MANIFEST_SIZE_BOUND}-byte parse bound",
                bytes.len()
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        verify_record_id(&value)?;
        let manifest: ServeManifest = serde_json::from_value(value)?;
        if manifest.v != 0 {
            return Err(Error::Corrupt(format!("unsupported manifest version {}", manifest.v)));
        }
        Ok(manifest)
    }

    /// The sum of all segments' image setsums: the manifest's logical
    /// content, one addition per segment.
    pub fn total_image(&self) -> Result<Sum> {
        let mut total = Sum::zero();
        for meta in self.segments.values() {
            total = total + Sum::from_hexdigest(&meta.image_setsum)?;
        }
        Ok(total)
    }
}

/// §3.3: across a swap from `prev` to `next`, the image delta equals the
/// setsum of items genuinely added.  Returns that delta; a pure compaction
/// yields zero.  A re-encoding that lost or invented so much as one entry
/// fails this group equation, checkable without touching a single `.pk`.
pub fn swap_delta(prev: &ServeManifest, next: &ServeManifest) -> Result<Sum> {
    Ok(next.total_image()? - prev.total_image()?)
}

////////////////////////////////////////////// pack ///////////////////////////////////////////////

/// Atomically put `bytes` at `path` unless the name already exists: write a
/// temp file beside it, `link(2)` it into place — creation of the link is
/// the atomic put-if-absent — and unlink the temp.  No reader ever observes
/// a partial file, and no writer ever overwrites a committed one.  Returns
/// false when the name was already present.
fn put_if_absent(path: &Path, bytes: &[u8]) -> Result<bool> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::Invalid(format!("no parent directory for {}", path.display())))?;
    let tmp = dir.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    fs::write(&tmp, bytes).map_err(ioerr(format!("writing {}", tmp.display())))?;
    let outcome = match fs::hard_link(&tmp, path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(ioerr(format!("linking {}", path.display()))(err)),
    };
    let _ = fs::remove_file(&tmp);
    outcome
}

/// Pack a loose repository into `out_dir` as segments plus a
/// serve-manifest at `seq` chaining to `prev_id`.  `level` is packing
/// policy (§5).
pub fn pack(
    repo: &Repository,
    out_dir: &Path,
    seq: u64,
    prev_id: &str,
    level: i32,
) -> Result<ServeManifest> {
    fs::create_dir_all(out_dir.join("seg")).map_err(ioerr("creating seg/"))?;
    fs::create_dir_all(out_dir.join("manifest")).map_err(ioerr("creating manifest/"))?;

    let blobs = repo.blobs();
    let mut inputs = Vec::new();
    for hash in blobs.list()? {
        inputs.push(SegmentInput::Blob(blobs.get(&hash)?));
    }
    for id in repo.claim_ids()? {
        inputs.push(SegmentInput::Claim { bytes: repo.claim_bytes(&id)?, id });
    }
    let mut forks = BTreeMap::new();
    let mut anchors = Vec::new();
    for fork in repo.fork_names()? {
        let fork_file = repo.read_fork(&fork)?;
        let state = repo.current_state(&fork)?;
        let log_bytes = repo.log_bytes(&fork)?;
        let line_ids: Vec<String> = state.lines.iter().map(|l| l.id.clone()).collect();
        if !line_ids.is_empty() {
            inputs.push(SegmentInput::LogSpan {
                fork: fork.clone(),
                bytes: log_bytes,
                line_ids,
            });
        }
        if !anchors.contains(&fork_file.anchor) {
            anchors.push(fork_file.anchor.clone());
        }
        forks.insert(
            fork.clone(),
            ForkHead {
                anchor: fork_file.anchor,
                head_id: state.head_id,
                head_sum: state.sum.hexdigest(),
                log_segments: Vec::new(), // filled once the segid is known
            },
        );
    }

    let built: BuiltSegment = build_segment(&inputs, level)?;
    for head in forks.values_mut() {
        head.log_segments = vec![built.segid.clone()];
    }
    // Segments are content-addressed: an existing name is the same bytes,
    // so a lost put-if-absent race is a no-op.
    put_if_absent(&out_dir.join("seg").join(format!("{}.pk", built.segid)), &built.pk)?;
    put_if_absent(&out_dir.join("seg").join(format!("{}.idx", built.segid)), &built.idx)?;

    let mut segments = BTreeMap::new();
    segments.insert(
        built.segid.clone(),
        SegmentMeta {
            pk_sha3: built.segid.clone(),
            idx_sha3: built.idx_sha3.clone(),
            bytes: built.pk.len() as u64,
            entries: built.entries as u64,
            image_setsum: built.image_setsum.hexdigest(),
            log_span: built.log_span.clone(),
        },
    );
    let mut manifest = ServeManifest {
        id: String::new(),
        v: 0,
        seq,
        prev: prev_id.to_string(),
        forks,
        segments,
        anchors,
        retire: Vec::new(),
    };
    manifest.id = record_id(&serde_json::to_value(&manifest)?)?;
    // Manifests are seq-addressed: losing the put-if-absent means another
    // packer took this seq, and silently dropping ours would lie.
    if !put_if_absent(&out_dir.join("manifest").join(format!("{seq}.json")), &manifest.to_bytes()?)?
    {
        return Err(Error::Invalid(format!(
            "manifest {seq}.json already exists in {}; pack lost the put-if-absent",
            out_dir.display()
        )));
    }
    Ok(manifest)
}

///////////////////////////////////////////// unpack //////////////////////////////////////////////

/// Everything a verified pack yields, in memory, before it touches a loose
/// repository.
pub struct Unpacked {
    /// The manifest that described the pack.
    pub manifest: ServeManifest,
    /// Blob contents by hash (dictionaries included).
    pub blobs: BTreeMap<String, Vec<u8>>,
    /// Exact log bytes per fork.
    pub logs: BTreeMap<String, Vec<u8>>,
    /// Exact claim bytes by id.
    pub claims: BTreeMap<String, Vec<u8>>,
}

/// Verify and unpack segments named by `manifest`, reading `.pk`/`.idx`
/// via `fetch`.  The order of proof is mandated (§4, I11); entries verify
/// their logical identity post-decode, and each segment's image_setsum is
/// recomputed and compared.
pub fn unpack_segments(
    manifest: &ServeManifest,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>>,
) -> Result<Unpacked> {
    let mut blobs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut spans: BTreeMap<(String, String), Vec<u8>> = BTreeMap::new();
    let mut claims: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut lines_by_id: BTreeMap<String, LogLine> = BTreeMap::new();

    // Two passes so construct entries can reference bases and lines packed
    // in any segment: first everything with stored bytes, then constructs.
    let mut opened = Vec::new();
    for (segid, meta) in &manifest.segments {
        if meta.pk_sha3 != *segid {
            return Err(Error::Corrupt(format!("segment {segid}: pk_sha3 differs from segid")));
        }
        let pk = fetch(&format!("seg/{segid}.pk"))?;
        let idx = fetch(&format!("seg/{segid}.idx"))?;
        let segment = Segment::open(&pk, &idx, &meta.pk_sha3, &meta.idx_sha3)?;
        opened.push((segid.clone(), segment));
    }
    let no_blob = |h: &str| -> Result<Vec<u8>> {
        Err(Error::Corrupt(format!("first pass cannot fetch blob {h}")))
    };
    let no_line = |id: &str| -> Result<LogLine> {
        Err(Error::Corrupt(format!("first pass cannot fetch line {id}")))
    };
    let mut constructs = Vec::new();
    for (segid, segment) in &opened {
        for entry in &segment.entries {
            match &entry.enc {
                crate::segment::Enc::Construct { .. } => {
                    constructs.push((segid.clone(), entry.clone()));
                }
                crate::segment::Enc::Lines { fork, .. } => {
                    let bytes = segment.materialize(entry, &no_blob, &no_line)?;
                    for line in parse_log_strict(&bytes)? {
                        lines_by_id.insert(line.id.clone(), line);
                    }
                    spans
                        .entry((segid.clone(), fork.clone()))
                        .or_default()
                        .extend_from_slice(&bytes);
                }
                crate::segment::Enc::Claim => {
                    let bytes = segment.materialize(entry, &no_blob, &no_line)?;
                    let claim = crate::claims::Claim::parse(&bytes)?;
                    claims.insert(claim.id, bytes);
                }
                crate::segment::Enc::Raw
                | crate::segment::Enc::Zstd
                | crate::segment::Enc::Zstdd { .. } => {
                    let bytes = segment.materialize(entry, &no_blob, &no_line)?;
                    blobs.insert(entry.sha3.clone(), bytes);
                }
            }
        }
    }
    // Second pass: constructs apply only verified inputs.
    let fetch_blob = |h: &str| -> Result<Vec<u8>> {
        blobs
            .get(h)
            .cloned()
            .ok_or_else(|| Error::Corrupt(format!("construct references absent blob {h}")))
    };
    let fetch_line = |id: &str| -> Result<LogLine> {
        lines_by_id
            .get(id)
            .cloned()
            .ok_or_else(|| Error::Corrupt(format!("construct references absent line {id}")))
    };
    let mut constructed = Vec::new();
    for (segid, entry) in &constructs {
        let segment = &opened.iter().find(|(s, _)| s == segid).expect("opened above").1;
        let bytes = segment.materialize(entry, &fetch_blob, &fetch_line)?;
        constructed.push((entry.sha3.clone(), bytes));
    }
    for (hash, bytes) in constructed {
        blobs.insert(hash, bytes);
    }
    // Verify each segment's image against the manifest's claim.
    let fetch_blob = |h: &str| -> Result<Vec<u8>> {
        blobs
            .get(h)
            .cloned()
            .ok_or_else(|| Error::Corrupt(format!("image pass: absent blob {h}")))
    };
    for (segid, segment) in &opened {
        let items = segment.image_items(&fetch_blob, &fetch_line)?;
        let actual = image_setsum(&items).hexdigest();
        let claimed = &manifest.segments[segid].image_setsum;
        if actual != *claimed {
            return Err(Error::Corrupt(format!(
                "segment {segid}: image setsum {actual} disagrees with manifest {claimed} \
                 — content forgery"
            )));
        }
    }
    // Assemble each fork's log from the segments its head names, in order:
    // a merged manifest may carry spans of the same fork from several
    // pushes, and only the head's list is authoritative.
    let mut logs: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (fork, head) in &manifest.forks {
        let mut bytes = Vec::new();
        for segid in &head.log_segments {
            if let Some(span) = spans.get(&(segid.clone(), fork.clone())) {
                bytes.extend_from_slice(span);
            }
        }
        logs.insert(fork.clone(), bytes);
    }
    Ok(Unpacked { manifest: manifest.clone(), blobs, logs, claims })
}

/// Materialize an unpacked repository as a loose one at `dest`.  Anchor
/// manifests are derived by replaying each fork's log from the empty state
/// to the anchor sum — snapshots are compactions, so this is total.
pub fn restore(unpacked: &Unpacked, dest: &Path) -> Result<Repository> {
    let repo = Repository::init_bare(dest)?;
    let blobs = repo.blobs();
    for content in unpacked.blobs.values() {
        blobs.put(content)?;
    }
    for bytes in unpacked.claims.values() {
        repo.put_claim_bytes(bytes)?;
    }
    // Anchors are derived, never primary: every anchor sum is a state on
    // some fork's history, so derive manifests to a fixpoint.  Start from
    // the empty state; a fork whose anchor is known contributes every state
    // its log passes through.
    let empty: Vec<u8> = Vec::new();
    let mut known: BTreeMap<String, Manifest> = BTreeMap::new();
    known.insert(Sum::zero().hexdigest(), Manifest::new());
    let mut parsed: BTreeMap<&String, Vec<LogLine>> = BTreeMap::new();
    for fork in unpacked.manifest.forks.keys() {
        parsed.insert(fork, parse_log_strict(unpacked.logs.get(fork).unwrap_or(&empty))?);
    }
    // Every state a log passes through is derivable by replay, guarded by
    // arithmetic: a derived manifest is recorded only when its running sum
    // agrees with the line's sum_after, so a replay in the wrong context
    // (a fork anchored elsewhere) records nothing wrong — it simply stops.
    // The state a log begins at never appears among its sum_afters, but
    // undo is the inverse: pre-state of line 0 = sum_after(line 0) plus the
    // inverse of its realized delta.
    let initial_sum = |lines: &[LogLine]| -> Result<Option<String>> {
        let Some(first) = lines.first() else {
            return Ok(None);
        };
        let mut sum = Sum::from_hexdigest(&first.sum_after)?;
        for entry in &first.realized {
            if let Some(added) = entry.added()? {
                sum.remove(&added.to_bytes());
            }
            if let Some(removed) = entry.removed()? {
                sum.insert(&removed.to_bytes());
            }
        }
        Ok(Some(sum.hexdigest()))
    };
    let mut initials: BTreeMap<&String, Option<String>> = BTreeMap::new();
    for (fork, lines) in &parsed {
        initials.insert(fork, initial_sum(lines)?);
    }
    let derive = |from: &Manifest,
                      from_sum: &str,
                      lines: &[LogLine],
                      initial: &Option<String>,
                      known: &mut BTreeMap<String, Manifest>| {
        let start = if initial.as_deref() == Some(from_sum) {
            0
        } else {
            match crate::log::last_state_position(lines, from_sum) {
                Some(i) => i + 1,
                None => return, // this starting state is not on this log
            }
        };
        let mut manifest = from.clone();
        let Ok(mut sum) = Sum::from_hexdigest(from_sum) else {
            return;
        };
        for line in &lines[start..] {
            if apply_realized_to_manifest(&mut manifest, &line.realized).is_err() {
                return;
            }
            let Ok(next) = apply_realized_to_sum(&sum, &line.realized) else {
                return;
            };
            sum = next;
            if sum.hexdigest() != line.sum_after {
                return; // wrong context; record nothing further
            }
            known.entry(line.sum_after.clone()).or_insert_with(|| manifest.clone());
        }
    };
    let mut unresolved: Vec<&String> = unpacked.manifest.forks.keys().collect();
    while !unresolved.is_empty() {
        let mut progressed = false;
        // Grow `known` from every log reachable from every known state.
        let snapshot: Vec<(String, Manifest)> =
            known.iter().map(|(s, m)| (s.clone(), m.clone())).collect();
        for (fork, lines) in &parsed {
            for (sum_hex, manifest) in &snapshot {
                derive(manifest, sum_hex, lines, &initials[fork], &mut known);
            }
        }
        unresolved.retain(|fork| {
            let head = &unpacked.manifest.forks[*fork];
            if known.contains_key(&head.anchor) {
                progressed = true;
                false
            } else {
                true
            }
        });
        if !unresolved.is_empty() && !progressed {
            return Err(Error::Corrupt(format!(
                "anchors not derivable from any packed log: {:?}",
                unresolved
                    .iter()
                    .map(|f| unpacked.manifest.forks[*f].anchor.clone())
                    .collect::<Vec<_>>()
            )));
        }
    }
    for (fork, head) in &unpacked.manifest.forks {
        let anchor_manifest = known
            .get(&head.anchor)
            .expect("fixpoint resolved every fork");
        repo.write_anchor_manifest(anchor_manifest)?;
        let fork_file = ForkFile { anchor: head.anchor.clone(), manifest: head.anchor.clone() };
        repo.restore_fork(fork, &fork_file, unpacked.logs.get(fork).unwrap_or(&empty))?;
        let state = repo.current_state(fork)?;
        if state.sum.hexdigest() != head.head_sum || state.head_id != head.head_id {
            return Err(Error::Corrupt(format!(
                "fork {fork}: restored head disagrees with the manifest"
            )));
        }
    }
    // One working tree: materialize main if present, else the first fork.
    let tree_fork = if unpacked.manifest.forks.contains_key("main") {
        Some("main".to_string())
    } else {
        unpacked.manifest.forks.keys().next().cloned()
    };
    if let Some(fork) = tree_fork {
        repo.materialize(&repo.current_state(&fork)?.manifest)?;
    }
    Ok(repo)
}

/// Unpack a packed directory (as `pack` lays it out) into a loose
/// repository at `dest`, choosing the highest-`seq` manifest.
pub fn unpack_dir(packed: &Path, dest: &Path) -> Result<Repository> {
    let manifest = latest_manifest(packed)?;
    let fetch = |name: &str| -> Result<Vec<u8>> {
        fs::read(packed.join(name)).map_err(ioerr(format!("reading {name}")))
    };
    let unpacked = unpack_segments(&manifest, &fetch)?;
    restore(&unpacked, dest)
}

/// The highest-`seq` manifest in a packed directory.
pub fn latest_manifest(packed: &Path) -> Result<ServeManifest> {
    let dir = packed.join("manifest");
    let entries = fs::read_dir(&dir).map_err(ioerr(format!("listing {}", dir.display())))?;
    let mut best: Option<(u64, std::path::PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(ioerr("listing manifests"))?;
        let name = entry.file_name();
        let Some(seq) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok())
        else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| seq > *b) {
            best = Some((seq, entry.path()));
        }
    }
    let (_, path) = best.ok_or_else(|| Error::Corrupt("no serve-manifest found".to_string()))?;
    let bytes = fs::read(&path).map_err(ioerr("reading serve-manifest"))?;
    ServeManifest::parse(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Annotation;
    use crate::patch::{Intent, Op};

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("abelian-serve-{name}-{}", std::process::id()));
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

    fn populated(name: &str) -> Repository {
        let repo = Repository::init(temp_dir(&format!("{name}-loose"))).unwrap();
        repo.apply("main", create("/src/main.rs", b"fn main() {}\n"), note()).unwrap();
        repo.apply(
            "main",
            Intent {
                ops: vec![Op::Edit {
                    path: "/src/main.rs".to_string(),
                    old_str: "main()".to_string(),
                    new_str: "main() /* v2 */".to_string(),
                }],
            },
            note(),
        )
        .unwrap();
        repo.create_fork("session-1", "main").unwrap();
        repo.apply("session-1", create("/notes.md", b"# notes\n"), note()).unwrap();
        let state = repo.current_state("main").unwrap();
        let inputs: Vec<_> = state.manifest.records().cloned().collect();
        let claim = crate::claims::Claim::new(
            &state.sum,
            "cargo test",
            inputs,
            0,
            &crate::ident::sha3_hex(b"ok"),
        )
        .unwrap();
        repo.put_claim(&claim).unwrap();
        repo
    }

    #[test]
    fn pack_unpack_equivalence() {
        // I2: unpack-before equals unpack-after, bit for bit at the loose
        // level.
        let repo = populated("equiv");
        let packed = temp_dir("equiv-packed");
        let manifest = pack(&repo, &packed, 1, "", 3).unwrap();
        assert_eq!(manifest.seq, 1);
        let restored = unpack_dir(&packed, &temp_dir("equiv-restored")).unwrap();

        // Logs are byte-preserved.
        for fork in ["main", "session-1"] {
            assert_eq!(
                repo.log_bytes(fork).unwrap(),
                restored.log_bytes(fork).unwrap(),
                "log bytes of {fork}"
            );
        }
        // Claims are byte-preserved.
        assert_eq!(repo.claim_ids().unwrap(), restored.claim_ids().unwrap());
        for id in repo.claim_ids().unwrap() {
            assert_eq!(repo.claim_bytes(&id).unwrap(), restored.claim_bytes(&id).unwrap());
        }
        // The blob pool survives.
        assert_eq!(repo.blobs().list().unwrap(), restored.blobs().list().unwrap());
        // States agree.
        for fork in ["main", "session-1"] {
            assert_eq!(
                repo.current_state(fork).unwrap().sum,
                restored.current_state(fork).unwrap().sum,
            );
        }
        // And re-packing the restored repo yields the same logical image.
        let repacked_dir = temp_dir("equiv-repacked");
        let repacked = pack(&restored, &repacked_dir, 1, "", 19).unwrap();
        assert_eq!(
            manifest.total_image().unwrap(),
            repacked.total_image().unwrap(),
            "re-encoding preserves the image"
        );
    }

    #[test]
    fn snapshotted_anchors_survive_pack_unpack() {
        // A snapshot repoints a fork's anchor at a mid-log state; unpack
        // must re-derive that anchor manifest by replay from genesis.
        let repo = populated("snap");
        repo.snapshot("main").unwrap();
        repo.snapshot("session-1").unwrap();
        repo.apply("main", create("/after-snap.rs", b"a\n"), note()).unwrap();
        let packed = temp_dir("snap-packed");
        pack(&repo, &packed, 1, "", 3).unwrap();
        let restored = unpack_dir(&packed, &temp_dir("snap-restored")).unwrap();
        for fork in ["main", "session-1"] {
            assert_eq!(
                repo.current_state(fork).unwrap().sum,
                restored.current_state(fork).unwrap().sum,
                "fork {fork}"
            );
            assert_eq!(repo.log_bytes(fork).unwrap(), restored.log_bytes(fork).unwrap());
        }
    }

    #[test]
    fn manifest_ids_verify_and_bound() {
        let repo = populated("ids");
        let packed = temp_dir("ids-packed");
        let manifest = pack(&repo, &packed, 1, "", 3).unwrap();
        let bytes = manifest.to_bytes().unwrap();
        let parsed = ServeManifest::parse(&bytes).unwrap();
        assert_eq!(parsed, manifest);
        // Tampering breaks the id.
        let tampered = String::from_utf8(bytes).unwrap().replace("\"seq\":1", "\"seq\":2");
        assert!(ServeManifest::parse(tampered.as_bytes()).is_err());
    }

    #[test]
    fn compaction_correctness_is_arithmetic() {
        let repo = populated("compact");
        let packed_a = temp_dir("compact-a");
        let packed_b = temp_dir("compact-b");
        // The same content packed at different levels: a pure re-encoding.
        let a = pack(&repo, &packed_a, 1, "", 3).unwrap();
        let b = pack(&repo, &packed_b, 2, &a.id, 19).unwrap();
        let delta = swap_delta(&a, &b).unwrap();
        assert_eq!(delta, Sum::zero(), "pure compaction: image delta is zero");
        // Different pk bytes, same image: a re-encoding, not forgery.
        if a.segments.keys().next() != b.segments.keys().next() {
            let ia = &a.segments.values().next().unwrap().image_setsum;
            let ib = &b.segments.values().next().unwrap().image_setsum;
            assert_eq!(ia, ib);
        }
        // New content moves the delta off zero.
        repo.apply("main", create("/new.rs", b"n\n"), note()).unwrap();
        let packed_c = temp_dir("compact-c");
        let c = pack(&repo, &packed_c, 3, &b.id, 3).unwrap();
        assert_ne!(swap_delta(&b, &c).unwrap(), Sum::zero());
    }

    #[test]
    fn forged_segment_is_rejected() {
        let repo = populated("forge");
        let packed = temp_dir("forge-packed");
        let manifest = pack(&repo, &packed, 1, "", 3).unwrap();
        let segid = manifest.segments.keys().next().unwrap().clone();
        // Truncate the .pk: fetch-time hash check must reject before decode.
        let pk_path = packed.join("seg").join(format!("{segid}.pk"));
        let mut pk = fs::read(&pk_path).unwrap();
        pk.pop();
        fs::write(&pk_path, &pk).unwrap();
        assert!(unpack_dir(&packed, &temp_dir("forge-dest")).is_err());
    }
}
