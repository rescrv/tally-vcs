//! §4 Delta push: plan what a store lacks, and nothing else.
//!
//! A push carries the log lines past each fork's remote head and the blobs
//! no live segment holds — the image is a set (I12), so nothing already
//! stored is stored again.  A new blob that one of the pushed lines
//! produces by editing (or creating from inline content) is packed as a
//! `construct`: no bytes, just the reference, because the op bytes already
//! travel in the line.  Constructs chain at most K deep before a keyframe.
//!
//! Small segments accumulate one per push; once enough of them form a
//! suffix of every fork's log, the push folds them and its own content into
//! fresh segments in the same swap.  Folding disjoint segments is a pure
//! re-encoding, so the writer checks the §3.3 equation before publishing.

use std::collections::{BTreeMap, BTreeSet};

use crate::ident::Sum;
use crate::log::{LogLine, line_blob_refs, parse_log_strict};
use crate::remote::{RemoteCache, RemoteView};
use crate::repo::Repository;
use crate::segment::{
    BuiltSegment, EMPTY_BLOB, Enc, Segment, SegmentInput, build_segments, construct_blob,
    image_setsum,
};
use crate::serve::{ForkHead, SegmentMeta, ServeManifest};
use crate::wire::ObjectStore;
use crate::{Error, Result};

/// Packing policy for a push: all encoding-side (I1), none in any hash.
#[derive(Clone, Debug)]
pub struct Policy {
    /// zstd level for pack-for-push (§5: 3).
    pub level: i32,
    /// Decompressed bytes per segment before a cut (§5: 64 MiB).
    pub segment_target: usize,
    /// Keyframe interval K: the deepest construct chain (§5: 32).
    pub keyframe_interval: usize,
    /// Fold small segments once this many form a suffix of every fork's
    /// log (§5: 8).
    pub compact_trigger: usize,
    /// A segment whose `.pk` is smaller than this is small (§5: 4 MiB).
    pub small_segment: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            level: 3,
            segment_target: 64 << 20,
            keyframe_interval: 32,
            compact_trigger: 8,
            small_segment: 4 << 20,
        }
    }
}

/// What a push must carry, before any segment is built.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    /// New items: blobs and constructs sorted by hash, then one log span
    /// per advancing fork, forks in name order.
    pub inputs: Vec<SegmentInput>,
    /// Heads to publish for the forks this push creates or advances.  Their
    /// `log_segments` are the remote's; assembly appends.
    pub heads: BTreeMap<String, ForkHead>,
    /// Fork heads this push brings into sync, recorded on success.
    pub synced: BTreeMap<String, String>,
    /// How many new blobs travel as constructs.
    pub constructs: usize,
    /// How many new blobs travel as keyframes.
    pub keyframes: usize,
}

/// One fork's log, split into lines with their exact bytes.
struct ForkLog {
    lines: Vec<LogLine>,
    chunks: Vec<Vec<u8>>,
}

fn read_fork_log(repo: &Repository, fork: &str) -> Result<ForkLog> {
    let bytes = repo.log_bytes(fork)?;
    let blobs = repo.blobs();
    let fetch = |h: &str| -> Result<Vec<u8>> { blobs.get(h) };
    let lines = parse_log_strict(&bytes, &fetch)?;
    let chunks: Vec<Vec<u8>> = bytes
        .split_inclusive(|&b| b == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if chunks.len() != lines.len() {
        return Err(Error::Corrupt(format!(
            "fork {fork}: {} log lines in {} LF-terminated records",
            lines.len(),
            chunks.len()
        )));
    }
    Ok(ForkLog { lines, chunks })
}

/// How a new blob will travel.
#[derive(Clone, Debug)]
enum Class {
    Keyframe,
    Construct {
        base: String,
        line_id: String,
        depth: usize,
    },
}

/// Plan a push of every local fork against `view`.
///
/// For each fork: a remote head the local log contains fast-forwards (the
/// lines after it travel); a fork the store lacks travels whole; a fork
/// whose remote head the local log lacks is either behind — its local head
/// is the one this client last synced, so there is nothing to send — or
/// has diverged, and the push refuses to splice (I8).
pub fn plan(
    repo: &Repository,
    view: &RemoteView,
    synced: &BTreeMap<String, String>,
    policy: &Policy,
) -> Result<Plan> {
    let mut plan = Plan::default();
    let remote_forks = view
        .manifest
        .as_ref()
        .map(|m| m.forks.clone())
        .unwrap_or_default();
    // New lines per fork, forks in name order, lines in log order.
    let mut spans: Vec<(String, Vec<LogLine>, Vec<u8>)> = Vec::new();
    for fork in repo.fork_names()? {
        let fork_file = repo.read_fork(&fork)?;
        let state = repo.current_state(&fork)?;
        let log = read_fork_log(repo, &fork)?;
        let head = ForkHead {
            anchor: fork_file.anchor.clone(),
            head_id: state.head_id.clone(),
            head_sum: state.sum.hexdigest(),
            log_segments: Vec::new(),
        };
        let new_from = match remote_forks.get(&fork) {
            None => 0,
            Some(remote) if remote.head_id == state.head_id => {
                plan.synced.insert(fork.clone(), state.head_id.clone());
                if remote.anchor != head.anchor || remote.head_sum != head.head_sum {
                    plan.heads.insert(
                        fork.clone(),
                        ForkHead {
                            log_segments: remote.log_segments.clone(),
                            ..head
                        },
                    );
                }
                continue;
            }
            Some(remote) if remote.head_id.is_empty() => 0,
            Some(remote) => match log.lines.iter().position(|l| l.id == remote.head_id) {
                Some(p) => p + 1,
                None if synced.get(&fork) == Some(&state.head_id) => {
                    // Behind, with no local work: the remote extends what
                    // this client last synced, and logs are append-only.
                    continue;
                }
                None => {
                    return Err(Error::Invalid(format!(
                        "fork {fork} advanced remotely to {}; refusing to splice — \
                         run union at the log level and push the result",
                        remote.head_id
                    )));
                }
            },
        };
        let log_segments = remote_forks
            .get(&fork)
            .map(|r| r.log_segments.clone())
            .unwrap_or_default();
        plan.heads.insert(
            fork.clone(),
            ForkHead {
                log_segments,
                ..head
            },
        );
        plan.synced.insert(fork.clone(), state.head_id.clone());
        if new_from < log.lines.len() {
            let bytes = log.chunks[new_from..].concat();
            spans.push((fork, log.lines[new_from..].to_vec(), bytes));
        }
    }

    // Blobs the store lacks.  The pool is the whole repository, exhaust
    // included; the store holds a set, so the difference is exact.
    let blobs = repo.blobs();
    let to_send: BTreeSet<String> = blobs.list()?.into_iter().filter(|h| !view.has(h)).collect();

    // Construct candidates: a blob a pushed line adds, replayable from the
    // blob at its path before the line (or from nothing, for a path the
    // line creates).  The first candidate per blob wins.  A blob a pushed
    // line overflows into is never a candidate, whatever else produces it:
    // readers parse lines before they resolve constructs, so a line must
    // never wait on one.
    let mut lines_by_id: BTreeMap<String, &LogLine> = BTreeMap::new();
    let mut candidates: Vec<(String, String, String)> = Vec::new(); // (target, base, line)
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (_, _, bytes) in &spans {
        seen.extend(line_blob_refs(bytes));
    }
    for (_, lines, _) in &spans {
        for line in lines {
            lines_by_id.insert(line.id.clone(), line);
            for entry in &line.realized {
                let Some(added) = entry.added()? else {
                    continue;
                };
                if !to_send.contains(&added.blob) || seen.contains(&added.blob) {
                    continue;
                }
                let mut base = EMPTY_BLOB.to_string();
                for prior in &line.realized {
                    if let Some(removed) = prior.removed()?
                        && removed.path == added.path
                    {
                        base = removed.blob;
                        break;
                    }
                    if let Some(earlier) = prior.added()?
                        && earlier.path == added.path
                    {
                        break; // created by this line: replay from nothing
                    }
                }
                seen.insert(added.blob.clone());
                candidates.push((added.blob, base, line.id.clone()));
            }
        }
    }

    // Classify to a fixpoint: a candidate is decided once its base's depth
    // is known.  Log order resolves a fork's chains in one round; a base
    // produced on another fork may take another.
    let mut classes: BTreeMap<String, Class> = BTreeMap::new();
    let depth_of = |hash: &str, classes: &BTreeMap<String, Class>| -> Option<usize> {
        if let Some(d) = view.depth(hash) {
            return Some(d);
        }
        match classes.get(hash)? {
            Class::Keyframe => Some(0),
            Class::Construct { depth, .. } => Some(*depth),
        }
    };
    let mut pending = candidates;
    while !pending.is_empty() {
        let before = pending.len();
        let mut deferred = Vec::new();
        for (target, base, line_id) in pending {
            let depth = match depth_of(&base, &classes) {
                Some(d) => d,
                None if to_send.contains(&base) => {
                    deferred.push((target, base, line_id));
                    continue;
                }
                None => {
                    // The base is neither stored nor sent: not replayable.
                    classes.insert(target, Class::Keyframe);
                    continue;
                }
            };
            let replays = depth < policy.keyframe_interval && {
                let base_bytes = if base == EMPTY_BLOB {
                    Vec::new()
                } else {
                    blobs.get(&base)?
                };
                construct_blob(&base_bytes, lines_by_id[&line_id], &target).is_ok()
            };
            let class = if replays {
                Class::Construct {
                    base,
                    line_id,
                    depth: depth + 1,
                }
            } else {
                Class::Keyframe
            };
            classes.insert(target, class);
        }
        if deferred.len() == before {
            // No base resolved this round: settle the rest as keyframes.
            for (target, _, _) in deferred {
                classes.insert(target, Class::Keyframe);
            }
            break;
        }
        pending = deferred;
    }

    for hash in &to_send {
        match classes.get(hash).cloned().unwrap_or(Class::Keyframe) {
            Class::Keyframe => {
                plan.keyframes += 1;
                plan.inputs.push(SegmentInput::Blob(blobs.get(hash)?));
            }
            Class::Construct { base, line_id, .. } => {
                plan.constructs += 1;
                plan.inputs.push(SegmentInput::Construct {
                    sha3: hash.clone(),
                    base,
                    line_id,
                });
            }
        }
    }
    for (fork, lines, bytes) in spans {
        plan.inputs.push(SegmentInput::LogSpan {
            fork,
            bytes,
            line_ids: lines.into_iter().map(|l| l.id).collect(),
        });
    }
    Ok(plan)
}

//////////////////////////////////////////// assembly /////////////////////////////////////////////

/// A push ready to upload: its segments and the manifest that names them.
#[derive(Clone, Debug)]
pub struct Assembled {
    /// The segments to upload.
    pub segments: Vec<BuiltSegment>,
    /// The manifest to put-if-absent at `seq`.
    pub manifest: ServeManifest,
    /// How many small segments this swap folds.
    pub compacted: usize,
}

/// The small segments a swap may fold: those whose `.pk` is under the
/// small threshold and which, in every fork's `log_segments`, form a
/// suffix — folding then rewrites each fork's log tail in place and
/// preserves log order.
pub fn compaction_candidates(base: &ServeManifest, policy: &Policy) -> BTreeSet<String> {
    let mut candidates: BTreeSet<String> = base
        .segments
        .iter()
        .filter(|(_, meta)| meta.bytes < policy.small_segment)
        .map(|(segid, _)| segid.clone())
        .collect();
    loop {
        let mut changed = false;
        for head in base.forks.values() {
            if let Some(last) = head
                .log_segments
                .iter()
                .rposition(|s| !candidates.contains(s))
            {
                for segid in &head.log_segments[..last] {
                    changed |= candidates.remove(segid);
                }
            }
        }
        if !changed {
            return candidates;
        }
    }
}

/// Folded content: blobs and constructs, plus each fork's spans in log
/// order.
type Folded = (Vec<SegmentInput>, BTreeMap<String, Vec<SegmentInput>>);

/// Fetch one stored blob from whichever live segment holds it.
fn fetch_live(
    store: &dyn ObjectStore,
    cache: &RemoteCache,
    view: &RemoteView,
    base: &ServeManifest,
    hash: &str,
) -> Result<Vec<u8>> {
    let segid = view.blobs.get(hash).ok_or_else(|| {
        Error::Corrupt(format!(
            "log references blob {hash}, which no live segment holds"
        ))
    })?;
    let meta = &base.segments[segid];
    let pk = cache.pk(store, segid, meta)?;
    let idx = cache.idx(store, segid, meta)?;
    let segment = Segment::open(&pk, &idx, &meta.pk_sha3, &meta.idx_sha3)?;
    let entry = segment
        .entries
        .iter()
        .find(|e| e.sha3 == hash)
        .ok_or_else(|| Error::Corrupt(format!("segment {segid} does not hold blob {hash}")))?;
    let refuse_blob = |h: &str| -> Result<Vec<u8>> {
        Err(Error::Corrupt(format!(
            "blob {hash} a log line overflows into is not stored whole (needs {h})"
        )))
    };
    let refuse_line = |id: &str| -> Result<LogLine> {
        Err(Error::Corrupt(format!(
            "blob {hash} a log line overflows into is a construct (of line {id})"
        )))
    };
    segment.materialize(entry, &refuse_blob, &refuse_line)
}

/// Read the folded segments back into inputs: blobs and constructs as
/// stored, log spans grouped by fork in log order.  A span's overflowed
/// lines resolve through the folded segments, then the local pool, then
/// the live segment that holds the blob — the folder may be a writer whose
/// pool never had it, and the blob may sit in a segment too large to fold.
fn fold_inputs(
    store: &dyn ObjectStore,
    cache: &RemoteCache,
    repo: &Repository,
    view: &RemoteView,
    base: &ServeManifest,
    folded: &BTreeSet<String>,
) -> Result<Folded> {
    let mut items = Vec::new();
    let mut stored: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut raw_spans: BTreeMap<(String, String), Vec<u8>> = BTreeMap::new();
    let no_blob = |h: &str| -> Result<Vec<u8>> {
        Err(Error::Invalid(format!(
            "compaction does not re-encode dictionary-compressed entries (dictionary {h})"
        )))
    };
    let no_line = |id: &str| -> Result<LogLine> {
        Err(Error::Corrupt(format!("unexpected line fetch {id}")))
    };
    for segid in folded {
        let meta: &SegmentMeta = &base.segments[segid];
        let pk = cache.pk(store, segid, meta)?;
        let idx = cache.idx(store, segid, meta)?;
        let segment = Segment::open(&pk, &idx, &meta.pk_sha3, &meta.idx_sha3)?;
        for entry in &segment.entries {
            match &entry.enc {
                Enc::Construct { base, line_id } => items.push(SegmentInput::Construct {
                    sha3: entry.sha3.clone(),
                    base: base.clone(),
                    line_id: line_id.clone(),
                }),
                Enc::Lines { fork, .. } => {
                    let bytes = segment.materialize(entry, &no_blob, &no_line)?;
                    if raw_spans
                        .insert((segid.clone(), fork.clone()), bytes)
                        .is_some()
                    {
                        return Err(Error::Corrupt(format!(
                            "segment {segid} holds two spans of fork {fork}"
                        )));
                    }
                }
                Enc::Raw | Enc::Zstd | Enc::Zstdd { .. } => {
                    let bytes = segment.materialize(entry, &no_blob, &no_line)?;
                    stored.insert(entry.sha3.clone(), bytes.clone());
                    items.push(SegmentInput::Blob(bytes));
                }
            }
        }
    }
    // Spans parse once every folded blob is gathered (as in unpack).
    let pool = repo.blobs();
    let fetch = |h: &str| -> Result<Vec<u8>> {
        if let Some(bytes) = stored.get(h) {
            return Ok(bytes.clone());
        }
        if let Ok(bytes) = pool.get(h) {
            return Ok(bytes);
        }
        fetch_live(store, cache, view, base, h)
    };
    let mut spans: BTreeMap<(String, String), SegmentInput> = BTreeMap::new();
    for ((segid, fork), bytes) in raw_spans {
        let line_ids = parse_log_strict(&bytes, &fetch)?
            .into_iter()
            .map(|l| l.id)
            .collect();
        spans.insert(
            (segid, fork.clone()),
            SegmentInput::LogSpan {
                fork,
                bytes,
                line_ids,
            },
        );
    }
    // Each folded span must belong to a fork that lists its segment; the
    // fork's list orders the spans.
    let mut by_fork: BTreeMap<String, Vec<SegmentInput>> = BTreeMap::new();
    for (fork, head) in &base.forks {
        for segid in &head.log_segments {
            if let Some(span) = spans.remove(&(segid.clone(), fork.clone())) {
                by_fork.entry(fork.clone()).or_default().push(span);
            }
        }
    }
    if let Some((segid, fork)) = spans.keys().next() {
        return Err(Error::Corrupt(format!(
            "segment {segid} holds a span of fork {fork}, which does not list it"
        )));
    }
    Ok((items, by_fork))
}

/// Coalesce one fork's spans, in order, into a single span.
fn coalesce(fork: &str, spans: Vec<SegmentInput>) -> SegmentInput {
    let mut bytes = Vec::new();
    let mut line_ids = Vec::new();
    for span in spans {
        if let SegmentInput::LogSpan {
            bytes: b,
            line_ids: ids,
            ..
        } = span
        {
            bytes.extend_from_slice(&b);
            line_ids.extend(ids);
        }
    }
    SegmentInput::LogSpan {
        fork: fork.to_string(),
        bytes,
        line_ids,
    }
}

fn input_hash(input: &SegmentInput) -> Option<String> {
    match input {
        SegmentInput::LogSpan { .. } => None,
        other => other
            .image_items()
            .into_iter()
            .next()
            .map(|item| match item {
                crate::segment::ImageItem::Blob(h) | crate::segment::ImageItem::Dict(h) => h,
                crate::segment::ImageItem::Line { id, .. } => id,
            }),
    }
}

/// Turn a plan into segments and the next manifest over `view`'s.  Folds
/// small segments when enough have accumulated.  Returns `None` when the
/// push would change nothing.
pub fn assemble(
    store: &dyn ObjectStore,
    cache: &RemoteCache,
    repo: &Repository,
    view: &RemoteView,
    plan: Plan,
    policy: &Policy,
) -> Result<Option<Assembled>> {
    if plan.inputs.is_empty() && plan.heads.is_empty() {
        return Ok(None);
    }
    let empty = ServeManifest {
        id: String::new(),
        v: 0,
        seq: 0,
        prev: String::new(),
        forks: BTreeMap::new(),
        segments: BTreeMap::new(),
        anchors: Vec::new(),
        retire: Vec::new(),
    };
    let base = view.manifest.as_ref().unwrap_or(&empty);
    let pushed_items: Vec<_> = plan.inputs.iter().flat_map(|i| i.image_items()).collect();
    let added: Sum = image_setsum(&pushed_items);

    let candidates = compaction_candidates(base, policy);
    let folded = if candidates.len() >= policy.compact_trigger {
        candidates
    } else {
        BTreeSet::new()
    };
    let inputs = if folded.is_empty() {
        plan.inputs
    } else {
        let (mut items, mut by_fork) = fold_inputs(store, cache, repo, view, base, &folded)?;
        for input in plan.inputs {
            match input {
                SegmentInput::LogSpan { ref fork, .. } => {
                    by_fork.entry(fork.clone()).or_default().push(input);
                }
                other => items.push(other),
            }
        }
        items.sort_by_key(input_hash);
        for pair in items.windows(2) {
            if input_hash(&pair[0]) == input_hash(&pair[1]) {
                return Err(Error::Corrupt(format!(
                    "blob {:?} would be stored twice; the image must be a set (I12)",
                    input_hash(&pair[0])
                )));
            }
        }
        for (fork, spans) in by_fork {
            items.push(coalesce(&fork, spans));
        }
        items
    };
    let segments = build_segments(&inputs, policy.segment_target, policy.level)?;

    // §3.3, checked by the writer before anything is published: the new
    // segments' image is the folded segments' image plus exactly what this
    // push adds.  Disjointness (I12) is what makes this an equation.
    let mut folded_image = Sum::zero();
    for segid in &folded {
        folded_image = folded_image + Sum::from_hexdigest(&base.segments[segid].image_setsum)?;
    }
    let mut built_image = Sum::zero();
    for segment in &segments {
        built_image = built_image + segment.image_setsum.clone();
    }
    if built_image != folded_image + added {
        return Err(Error::Corrupt(
            "segment image disagrees with folded image plus pushed items (§3.3)".to_string(),
        ));
    }

    let mut forks = base.forks.clone();
    for (fork, head) in plan.heads {
        forks.insert(fork, head);
    }
    for (fork, head) in forks.iter_mut() {
        head.log_segments.retain(|s| !folded.contains(s));
        for segment in &segments {
            if segment.forks.contains(fork) {
                head.log_segments.push(segment.segid.clone());
            }
        }
    }
    let mut live = base.segments.clone();
    live.retain(|segid, _| !folded.contains(segid));
    for segment in &segments {
        live.insert(segment.segid.clone(), SegmentMeta::of(segment));
    }
    let mut anchors: Vec<String> = Vec::new();
    for head in forks.values() {
        if !anchors.contains(&head.anchor) {
            anchors.push(head.anchor.clone());
        }
    }
    let mut manifest = ServeManifest {
        id: String::new(),
        v: 0,
        seq: view.manifest.as_ref().map(|m| m.seq + 1).unwrap_or(1),
        prev: view
            .manifest
            .as_ref()
            .map(|m| m.id.clone())
            .unwrap_or_default(),
        forks,
        segments: live,
        anchors,
        retire: folded.iter().cloned().collect(),
    };
    manifest.id = crate::ident::record_id(&serde_json::to_value(&manifest)?)?;
    Ok(Some(Assembled {
        segments,
        manifest,
        compacted: folded.len(),
    }))
}
