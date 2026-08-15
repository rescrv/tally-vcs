//! §6 Union: bring a fork's log into a target state, stratified so that
//! each stratum is strictly cheaper than the next and almost all work stops
//! early.
//!
//! 1. Arithmetic — equal sums, done, O(1).
//! 2. Realized replay — membership plus addition, no content inspection.
//! 3. Intent replay — re-validate span preconditions; disjoint-span edits
//!    to the same file sail through here.
//! 4. Re-enactment — the only stratum that costs tokens; never automatic.

use std::collections::{BTreeMap, BTreeSet};

use crate::blobs::BlobStore;
use crate::log::{Annotation, Fuse, LogLine, Origin, Provenance, last_state_position};
use crate::manifest::Manifest;
use crate::patch::{apply_intent, apply_realized_to_manifest, apply_realized_to_sum};
use crate::repo::Repository;
use crate::{Error, Result};

/// Which stratum landed a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stratum {
    /// Stratum 2: the realized delta applied directly.
    RealizedReplay,
    /// Stratum 3: the intent re-validated against the drifted target.
    IntentReplay,
}

/// One landed line.
#[derive(Debug)]
pub struct Landed {
    /// The new line on the target: new id, new linkage, union provenance.
    pub line: LogLine,
    /// The source line it re-enacts mechanically.
    pub origin_id: String,
    /// How it landed.
    pub stratum: Stratum,
}

/// Which side of a read/write conflict read and which wrote (§6).  Both are
/// `W₁∩R₂ ≠ ∅`; the direction says whose reasoning the merge would strand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictDirection {
    /// The incoming source line read state a concurrent target write changed:
    /// the incoming patch's own reasoning is stale.  `W_target ∩ R_source`.
    IncomingReadStale,
    /// The incoming source line's write changed state a concurrent target line
    /// read: landing the merge would strand the target patch's reasoning.
    /// `W_source ∩ R_target`.
    IncomingWriteHitsTargetRead,
}

/// A semantic conflict the span strata cannot see: a patch that landed
/// mechanically — every span precondition still held — but where one side's
/// observed read set (§5) names state the other side wrote.  The bytes
/// applied; the reasoning may be stale.  This is exactly the conflict class
/// that distinguishes tally from a purely textual merge, and it is
/// reported, never silently swallowed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticConflict {
    /// The incoming source line involved.
    pub source_id: String,
    /// The concurrent target line involved.
    pub target_id: String,
    /// The paths one side read and the other wrote.
    pub paths: BTreeSet<String>,
    /// Which side read and which wrote.
    pub direction: ConflictDirection,
}

/// What a union did, and what it declined to do.
#[derive(Debug, Default)]
pub struct UnionOutcome {
    /// Stratum 1 fired: the states were already identical.
    pub already_identical: bool,
    /// Lines landed, in order.
    pub landed: Vec<Landed>,
    /// Stratum 4 gate: the source line whose assumptions are genuinely
    /// dead, with the conflict evidence.  Union stops here unless a model
    /// is invited; it never proceeds on its own.
    pub needs_reenactment: Option<(String, String)>,
    /// Read/write conflicts (§6): lines that landed but read state a
    /// concurrent write has since changed.  The bytes merged; a human or a
    /// model must reconcile the reasoning.
    pub semantic_conflicts: Vec<SemanticConflict>,
}

impl UnionOutcome {
    /// True iff every source line landed mechanically and none of them read
    /// state a concurrent write invalidated.  A `W₁∩R₂` hit leaves the merge
    /// incomplete even when every span precondition still held.
    pub fn complete(&self) -> bool {
        self.needs_reenactment.is_none() && self.semantic_conflicts.is_empty()
    }
}

/// Run union of `source` into `target`.  Strata 1–3 only: stratum 4 is
/// reported, never performed.
///
/// Union is atomic (I8).  The whole merge is replayed against an in-memory
/// manifest under a single fork lock; the sealed lines are committed in one
/// fsync at the end, or — if any line's span precondition is dead (a stratum-4
/// gate) or any landed line's read set was invalidated by a concurrent write
/// (a semantic conflict, §6) — nothing is written and the target is left
/// exactly as it was.  A union either lands in full or not at all: there is no
/// partial prefix to reconcile, and a crash mid-replay commits nothing because
/// the only durable write is the final batch append.
pub fn union(repo: &Repository, source: &str, target: &str, author: &str) -> Result<UnionOutcome> {
    // Hold the target lock across the entire replay-and-commit so the manifest
    // we validate against cannot drift between decision and durability.
    let _lock = repo.lock_fork(target)?;
    let source_state = repo.current_state(source)?;
    let mut outcome = UnionOutcome::default();

    // Stratum 1: arithmetic.  Equal sums mean the states are already
    // identical, but fuses are arithmetic identities the sum cannot see:
    // uncarried fuse lines still land below.
    let target_state = repo.current_state(target)?;
    if source_state.sum == target_state.sum {
        outcome.already_identical = true;
    }

    // The other operand of the read/write tripwire (§6): everything the
    // target wrote after the source diverged from it.  The source's anchor
    // names the divergence point in the target's log; lines past it are
    // writes the source never witnessed.  If the anchor is not found (the
    // fork sits below the target's current log, or predates it), we fall
    // back to treating the whole target log as concurrent — conservative,
    // never unsound.
    let source_anchor = repo.read_fork(source)?.anchor;
    let concurrent_start = last_state_position(&target_state.lines, &source_anchor)
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut concurrent: Vec<(String, BTreeSet<String>)> = Vec::new();
    for line in &target_state.lines[concurrent_start..] {
        let writes = write_paths(line)?;
        if !writes.is_empty() {
            concurrent.push((line.id.clone(), writes));
        }
    }
    // The pristine target — before this union lands anything — is the
    // reference the span-precise staleness check diffs each read against.
    let blobs = repo.blobs();
    let target_before = &target_state.manifest;

    // What the target already carries, and the re-key map for fuse spans:
    // union re-seals ids, so a landed fuse's `from`/`to` follow the
    // `origin_id -> landed id` correspondence.  The name travels untouched.
    let mut carried = BTreeSet::new();
    let mut rekey: BTreeMap<String, String> = BTreeMap::new();
    for line in &target_state.lines {
        carried.insert(line.id.clone());
        if let Some(origin) = &line.annotation.origin
            && origin.fork == source
        {
            carried.insert(origin.id.clone());
            rekey.insert(origin.id.clone(), line.id.clone());
        }
    }

    // In-memory replay state.  `scratch`/`sum`/`prev` evolve as lines land;
    // `bytes` accumulates their sealed encodings; `staged` holds the landings
    // we will publish iff the whole union is clean.  Nothing here touches the
    // durable log.
    let mut scratch = target_state.manifest.clone();
    let mut sum = target_state.sum.clone();
    let mut prev = target_state.head_id.clone();
    let mut bytes: Vec<u8> = Vec::new();
    let mut staged: Vec<Landed> = Vec::new();
    // Which source line wrote each path, for the symmetric tripwire below.
    let mut union_writers: BTreeMap<String, String> = BTreeMap::new();

    for line in &source_state.lines {
        if carried.contains(&line.id) {
            continue;
        }
        if outcome.already_identical && line.annotation.fuse.is_none() {
            // The arithmetic already accounts for every patch line; only
            // fuses still travel.
            continue;
        }
        let fuse = line.annotation.fuse.as_ref().map(|f| Fuse {
            name: f.name.clone(),
            from: rekey.get(&f.from).cloned().unwrap_or_else(|| f.from.clone()),
            to: rekey.get(&f.to).cloned().unwrap_or_else(|| f.to.clone()),
        });
        let annotation = Annotation {
            author: author.to_string(),
            provenance: Provenance::Union,
            reason: None,
            sig: None,
            session: line.annotation.session.clone(),
            prose: line.annotation.prose.clone(),
            reads: line.annotation.reads.clone(),
            origin: Some(Origin { fork: source.to_string(), id: line.id.clone() }),
            fuse,
            import: line.annotation.import.clone(),
        };

        // Stratum 2: realized replay.  If the incoming realized delta applies
        // to the scratch manifest — all removed records present — adopt it
        // directly.  We try on a clone so a miss leaves the scratch pristine
        // for the stratum-3 attempt.
        let mut trial = scratch.clone();
        let (realized, st) = if apply_realized_to_manifest(&mut trial, &line.realized).is_ok() {
            scratch = trial;
            (line.realized.clone(), Stratum::RealizedReplay)
        } else {
            // Stratum 3: intent replay.  A consumed record is missing because
            // the target drifted; re-validate the span precondition against
            // the scratch blob and realize fresh deltas.
            match apply_intent(&line.intent, &mut scratch, &blobs) {
                Ok(realization) => {
                    // Write the re-realized blobs now: content-addressed and
                    // idempotent, they are safe to leave even if the union
                    // aborts (gc-blobs reclaims the unreferenced ones).
                    // Unsynced: the committing append syncs the device
                    // before the log fsync.
                    for (_, content) in &realization.new_blobs {
                        blobs.put_unsynced(content)?;
                    }
                    (realization.realized, Stratum::IntentReplay)
                }
                Err(Error::Precondition(evidence)) => {
                    // Stratum 4 gate: the patch's assumptions are genuinely
                    // dead.  Abort the whole union; nothing is written.
                    outcome.needs_reenactment = Some((line.id.clone(), evidence));
                    return Ok(outcome);
                }
                Err(other) => return Err(other),
            }
        };

        // Fold the sum and seal the line in memory, chaining from the running
        // head.  `seal` stamps the id and commit time.
        sum = apply_realized_to_sum(&sum, &realized)?;
        let mut landed = LogLine {
            id: String::new(),
            prev: prev.clone(),
            intent: line.intent.clone(),
            realized,
            sum_after: sum.hexdigest(),
            committed_ms: 0,
            annotation,
        };
        bytes.extend_from_slice(&landed.seal(&blobs)?);
        prev = landed.id.clone();
        rekey.insert(line.id.clone(), landed.id.clone());
        note_stale_reads(&mut outcome, line, &concurrent, target_before, &blobs)?;
        for path in write_paths(&landed)? {
            union_writers.insert(path, line.id.clone());
        }
        staged.push(Landed { line: landed, origin_id: line.id.clone(), stratum: st });
    }

    // The symmetric tripwire (§6): W_source ∩ R_target.  The reads above ask
    // whether the incoming patches read stale state; these ask whether landing
    // them would strand a concurrent target line's reasoning — a target patch
    // that read state this merge overwrites.  Only paths the union wrote are
    // considered, and staleness is span-precise against the post-union
    // manifest, so a target read the merge did not disturb is left alone.
    for target_line in &target_state.lines[concurrent_start..] {
        note_stranded_reads(&mut outcome, target_line, &union_writers, &scratch, &blobs);
    }

    // The commit point.  A semantic conflict aborts exactly like a stratum-4
    // gate: a merge whose reasoning is known-stale does not land on its own.
    // Either way the target is untouched and `landed` stays empty.
    if !outcome.semantic_conflicts.is_empty() {
        return Ok(outcome);
    }
    repo.append_sealed_locked(target, &staged_lines(&staged), &bytes)?;
    outcome.landed = staged;
    Ok(outcome)
}

/// Borrow the sealed [`LogLine`]s out of the staged landings, in order, for
/// the single batch append.
fn staged_lines(staged: &[Landed]) -> Vec<LogLine> {
    staged.iter().map(|l| l.line.clone()).collect()
}

//////////////////////////////////////////// commutation ///////////////////////////////////////////

/// The paths a line writes: every path its realized delta touches.
pub fn write_paths(line: &LogLine) -> Result<BTreeSet<String>> {
    let mut paths = BTreeSet::new();
    for entry in &line.realized {
        if let Some(removed) = entry.removed()? {
            paths.insert(removed.path);
        }
        if let Some(added) = entry.added()? {
            paths.insert(added.path);
        }
    }
    Ok(paths)
}

/// The paths a line's observed read set covers.  `None` means the read set
/// is universally quantified (a grep's negative covers the whole state) or
/// unobserved (andon), and nothing commutes with it.
pub fn read_paths(line: &LogLine) -> Option<BTreeSet<String>> {
    let reads = line.annotation.reads.as_ref()?;
    let array = reads.as_array()?;
    let mut paths = BTreeSet::new();
    for read in array {
        if let Some(path) = read.get("path").and_then(|p| p.as_str()) {
            paths.insert(path.to_string());
        } else {
            // A grep record: three lines read plus one universally
            // quantified negative — the author may have acted on the
            // absence.
            return None;
        }
    }
    Some(paths)
}

/// The paths a landed source line read that a set of concurrent writes has
/// since changed (§6), at span granularity.  Absent reads (unobserved/andon)
/// invalidate nothing — absence of evidence is not evidence of a conflict;
/// the conservatism there lives in [`commutes`], which certifies reordering
/// rather than reports staleness.  A universally-quantified read (a grep
/// negative, or reads spilled to a blob we did not inline) conflicts with
/// *any* concurrent write, because the author may have acted on the observed
/// absence.  A path-scoped read conflicts only when one of its observed spans
/// covers a line the concurrent write actually touched: a read of one region
/// survives a disjoint edit to the same file, exactly as §6 promises.
fn stale_reads(
    line: &LogLine,
    concurrent_writes: &BTreeSet<String>,
    target: &Manifest,
    blobs: &BlobStore,
) -> BTreeSet<String> {
    let mut stale = BTreeSet::new();
    if concurrent_writes.is_empty() {
        return stale;
    }
    let Some(reads) = line.annotation.reads.as_ref() else {
        return stale;
    };
    let Some(array) = reads.as_array() else {
        // Spilled to a blob: substantial reads we did not inline.  Assume any
        // concurrent write may have invalidated one.
        return concurrent_writes.clone();
    };
    for read in array {
        let Some(path) = read.get("path").and_then(|p| p.as_str()) else {
            // A universally-quantified read: the observed absence spans the
            // whole state, so any concurrent write may have voided it.
            return concurrent_writes.clone();
        };
        if concurrent_writes.contains(path) && read_is_stale(read, path, target, blobs) {
            stale.insert(path.to_string());
        }
    }
    stale
}

/// Whether one path-scoped read is stale against the target's current blob.
/// Compares the exact bytes the author read — named by the read's recorded
/// `blob` hash — with the target's blob for the path, and asks whether any
/// observed span covers a line the concurrent write changed.  Anything we
/// cannot resolve precisely — a missing/opaque blob hash, a non-UTF-8 blob, a
/// span not uniquely locatable, a vanished path, or a read that named no
/// spans (a whole-file read) — is treated as stale: conservative, never a
/// missed conflict.
fn read_is_stale(read: &serde_json::Value, path: &str, target: &Manifest, blobs: &BlobStore) -> bool {
    let Some(read_hash) = read.get("blob").and_then(|b| b.as_str()) else {
        return true;
    };
    // The path the author read is gone from the target: certainly stale.
    let Some(record) = target.get(path) else {
        return true;
    };
    // Net-identical to what was read (e.g. a concurrent write-then-revert):
    // the read is still valid regardless of the churn in between.
    if record.blob == read_hash {
        return false;
    }
    let (Ok(read_bytes), Ok(now_bytes)) = (blobs.get(read_hash), blobs.get(&record.blob)) else {
        return true;
    };
    let Some(changed) = crate::diff::changed_old_lines(&read_bytes, &now_bytes) else {
        return true; // non-UTF-8: cannot localize.
    };
    let Some(spans) = read.get("spans").and_then(|s| s.as_array()).filter(|s| !s.is_empty()) else {
        return true; // no named span: a whole-file read.
    };
    for span in spans {
        let Some(text) = span.as_str() else {
            return true;
        };
        match span_line_range(&read_bytes, text.as_bytes()) {
            Some((lo, hi)) => {
                if (lo..=hi).any(|l| changed.contains(&l)) {
                    return true;
                }
            }
            None => return true, // not uniquely locatable: conservative.
        }
    }
    false
}

/// The inclusive range of 0-based line indices a `needle` covers in `blob`,
/// iff it occurs exactly once.  A line index is the count of `\n` bytes
/// preceding an offset, matching the diff's line splitting.
fn span_line_range(blob: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() || crate::patch::count_occurrences(blob, needle) != 1 {
        return None;
    }
    let start = blob.windows(needle.len()).position(|w| w == needle)?;
    let last = start + needle.len() - 1;
    let line_of = |offset: usize| blob[..offset].iter().filter(|&&b| b == b'\n').count();
    Some((line_of(start), line_of(last)))
}

/// The symmetric direction: record a conflict for each read of a concurrent
/// target line that the union's write would strand.  Only paths the union
/// actually wrote (`union_writers`, path → the source line that wrote it) are
/// considered, and each read is checked span-precisely against the post-union
/// manifest.  A universally-quantified target read (a grep negative) or one
/// spilled to a blob is stranded conservatively by any union write.  Conflicts
/// are grouped one per (writing source line, this target line).
fn note_stranded_reads(
    outcome: &mut UnionOutcome,
    target_line: &LogLine,
    union_writers: &BTreeMap<String, String>,
    after: &Manifest,
    blobs: &BlobStore,
) {
    let Some(reads) = target_line.annotation.reads.as_ref() else {
        return;
    };
    // Accumulate stranded paths per source writer, so each conflict names one
    // (source line, target line) pair, matching the forward direction.
    let mut by_source: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let strand_all = |by_source: &mut BTreeMap<String, BTreeSet<String>>| {
        for (path, source_id) in union_writers {
            by_source.entry(source_id.clone()).or_default().insert(path.clone());
        }
    };
    match reads.as_array() {
        // Spilled to a blob: substantial reads we did not inline; any union
        // write may have stranded one.
        None => strand_all(&mut by_source),
        Some(array) => {
            for read in array {
                match read.get("path").and_then(|p| p.as_str()) {
                    // A universally-quantified read spans the whole state.
                    None => strand_all(&mut by_source),
                    Some(path) => {
                        if let Some(source_id) = union_writers.get(path)
                            && read_is_stale(read, path, after, blobs)
                        {
                            by_source
                                .entry(source_id.clone())
                                .or_default()
                                .insert(path.to_string());
                        }
                    }
                }
            }
        }
    }
    for (source_id, paths) in by_source {
        outcome.semantic_conflicts.push(SemanticConflict {
            source_id,
            target_id: target_line.id.clone(),
            paths,
            direction: ConflictDirection::IncomingWriteHitsTargetRead,
        });
    }
}

/// Record a semantic conflict for each concurrent target line whose writes
/// invalidated a read of the just-landed source line.
fn note_stale_reads(
    outcome: &mut UnionOutcome,
    source: &LogLine,
    concurrent: &[(String, BTreeSet<String>)],
    target: &Manifest,
    blobs: &BlobStore,
) -> Result<()> {
    for (target_id, writes) in concurrent {
        let paths = stale_reads(source, writes, target, blobs);
        if !paths.is_empty() {
            outcome.semantic_conflicts.push(SemanticConflict {
                source_id: source.id.clone(),
                target_id: target_id.clone(),
                paths,
                direction: ConflictDirection::IncomingReadStale,
            });
        }
    }
    Ok(())
}

/// Two patches commute iff neither's write spans intersect the other's read
/// set and their write spans are pairwise disjoint (§6).  Path granularity:
/// conservative, never unsound.  Conflict is not textual overlap; conflict
/// is `W₁∩R₂ ≠ ∅`, checked against observed reads.
pub fn commutes(a: &LogLine, b: &LogLine) -> Result<bool> {
    let wa = write_paths(a)?;
    let wb = write_paths(b)?;
    if wa.intersection(&wb).next().is_some() {
        return Ok(false);
    }
    let (Some(ra), Some(rb)) = (read_paths(a), read_paths(b)) else {
        // Unobserved or universally quantified reads: refuse to certify.
        return Ok(false);
    };
    Ok(wa.intersection(&rb).next().is_none() && wb.intersection(&ra).next().is_none())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{Intent, Op};
    use crate::repo::Repository;

    fn temp_repo(name: &str) -> Repository {
        let dir =
            std::env::temp_dir().join(format!("tally-union-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Repository::init(&dir).unwrap()
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

    fn edit(path: &str, old: &str, new: &str) -> Intent {
        Intent {
            ops: vec![Op::Edit {
                path: path.to_string(),
                old_str: old.to_string(),
                new_str: new.to_string(),
            }],
        }
    }

    fn note(author: &str) -> Annotation {
        Annotation { author: author.to_string(), ..Annotation::default() }
    }

    #[test]
    fn stratum_1_identical_states() {
        let repo = temp_repo("s1");
        repo.apply("main", create("/a", b"a\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.already_identical);
        assert!(outcome.landed.is_empty());
    }

    #[test]
    fn stratum_2_realized_replay() {
        let repo = temp_repo("s2");
        repo.apply("main", create("/a", b"hello\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        repo.apply("s", edit("/a", "hello", "goodbye"), note("t")).unwrap();
        // Target has not drifted: membership plus addition.
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.complete());
        assert_eq!(outcome.landed.len(), 1);
        assert_eq!(outcome.landed[0].stratum, Stratum::RealizedReplay);
        let landed = &outcome.landed[0].line;
        assert_eq!(landed.annotation.provenance, Provenance::Union);
        assert_eq!(landed.annotation.origin.as_ref().unwrap().fork, "s");
        // Landed lines are new lines with new ids.
        assert_ne!(landed.id, outcome.landed[0].origin_id);
        // States converge.
        assert_eq!(
            repo.current_state("main").unwrap().sum,
            repo.current_state("s").unwrap().sum,
        );
    }

    #[test]
    fn stratum_3_disjoint_spans_sail_through() {
        let repo = temp_repo("s3");
        repo.apply("main", create("/a", b"alpha\nbeta\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        // Fork edits one span; main drifts in a disjoint span of the same file.
        repo.apply("s", edit("/a", "beta", "BETA"), note("t")).unwrap();
        repo.apply("main", edit("/a", "alpha", "ALPHA"), note("t")).unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.complete());
        assert_eq!(outcome.landed.len(), 1);
        assert_eq!(outcome.landed[0].stratum, Stratum::IntentReplay);
        let state = repo.current_state("main").unwrap();
        let blob = repo.blobs().get(&state.manifest.get("/a").unwrap().blob).unwrap();
        assert_eq!(blob, b"ALPHA\nBETA\n");
    }

    #[test]
    fn stratum_4_is_gated_not_performed() {
        let repo = temp_repo("s4");
        repo.apply("main", create("/a", b"needle\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        repo.apply("s", edit("/a", "needle", "thread"), note("t")).unwrap();
        // Main consumes the same span: the fork's assumptions die.
        repo.apply("main", edit("/a", "needle", "nail"), note("t")).unwrap();
        let before = repo.current_state("main").unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(!outcome.complete());
        assert!(outcome.landed.is_empty());
        let (_, evidence) = outcome.needs_reenactment.unwrap();
        assert!(evidence.contains("0 times"), "conflict evidence: {evidence}");
        // Nothing landed: the target is untouched.
        assert_eq!(repo.current_state("main").unwrap().sum, before.sum);
    }

    #[test]
    fn fuses_travel_through_union_rekeyed() {
        // The ISSUE reproduction: fork, two patches, fuse, union — the
        // fused beat must be visible on main, re-keyed to main's ids.
        let repo = temp_repo("fuses");
        repo.create_fork("session-1", "main").unwrap();
        let l1 = repo.apply("session-1", create("/a", b"one\n"), note("t")).unwrap();
        let l2 = repo.apply("session-1", edit("/a", "one", "two"), note("t")).unwrap();
        repo.fuse("session-1", "beat", &l1.id, &l2.id, Some("one narrative beat".to_string()), "sid")
            .unwrap();
        let outcome = union(&repo, "session-1", "main", "maintainer").unwrap();
        assert!(outcome.complete());
        assert_eq!(outcome.landed.len(), 3, "two patches plus the fuse line");
        let state = repo.current_state("main").unwrap();
        let beats = crate::views::fused_beats(&state.lines, None);
        assert_eq!(beats.len(), 1, "main renders one fused beat, not raw lines");
        let crate::views::Beat::Fused { fuse, lines } = &beats[0] else {
            panic!("expected the fused beat on main");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(fuse.annotation.prose.as_deref(), Some("one narrative beat"));
        // Re-keyed through the landed origin map: the span names main's ids;
        // the name travels untouched.
        let span = fuse.annotation.fuse.as_ref().unwrap();
        assert_eq!(span.name, "beat");
        assert_eq!(span.from, outcome.landed[0].line.id);
        assert_eq!(span.to, outcome.landed[1].line.id);
        assert_ne!(span.from, l1.id);
        assert_ne!(span.to, l2.id);
        // The fork is now fully carried: remove-fork succeeds without force.
        repo.remove_fork("session-1", false).unwrap();
    }

    #[test]
    fn fuses_land_even_when_sums_are_already_identical() {
        // A fuse is an arithmetic identity, so stratum 1 cannot see it;
        // union must land it anyway.
        let repo = temp_repo("fuses-s1");
        repo.create_fork("s", "main").unwrap();
        let l1 = repo.apply("s", create("/a", b"a\n"), note("t")).unwrap();
        let first = union(&repo, "s", "main", "maintainer").unwrap();
        assert_eq!(first.landed.len(), 1);
        // Fuse after the fact: the sums are already equal.
        repo.fuse("s", "beat", &l1.id, &l1.id, Some("beat".to_string()), "sid").unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.already_identical, "stratum 1 fired on the arithmetic");
        assert_eq!(outcome.landed.len(), 1, "and the fuse still landed");
        let span =
            outcome.landed[0].line.annotation.fuse.as_ref().unwrap();
        assert_eq!(span.name, "beat");
        assert_eq!(span.from, first.landed[0].line.id, "re-keyed to main's line");
        // Idempotent: a third union carries nothing new.
        let again = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(again.already_identical);
        assert!(again.landed.is_empty());
    }

    #[test]
    fn stale_read_is_a_semantic_conflict_even_when_spans_apply() {
        // The conflict class the span strata cannot see: the fork reads
        // /config, then edits /handler on the strength of it; main
        // concurrently rewrites /config.  The /handler edit still applies
        // cleanly — its own precondition never depended on /config — so it
        // lands mechanically, but its reasoning is now stale.  Union must
        // report that, not swallow it.
        let repo = temp_repo("stale-read");
        repo.apply("main", create("/config", b"MAX=10\n"), note("t")).unwrap();
        repo.apply("main", create("/handler", b"limit = default\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        // The fork's edit witnesses /config as a read.
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": "x"}]));
        repo.apply("s", edit("/handler", "default", "10"), n).unwrap();
        // Main concurrently rewrites the very file the fork read.
        repo.apply("main", edit("/config", "MAX=10", "MAX=1000"), note("t")).unwrap();

        let before = repo.current_state("main").unwrap().sum;
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        // Atomic: the /handler edit would apply mechanically, but the read
        // set was invalidated, so the whole union aborts and nothing lands.
        assert!(!outcome.complete());
        assert!(outcome.landed.is_empty());
        assert_eq!(repo.current_state("main").unwrap().sum, before, "target untouched");
        assert_eq!(outcome.semantic_conflicts.len(), 1);
        let conflict = &outcome.semantic_conflicts[0];
        assert!(conflict.paths.contains("/config"));
    }

    #[test]
    fn union_is_atomic_a_clean_line_does_not_land_before_a_dead_one() {
        // The fork writes a clean, unrelated file /ok, then edits /a's
        // "needle" span; main concurrently consumes that span.  The second
        // line is a stratum-4 gate.  Atomicity demands the first line does
        // *not* land: the target must be byte-for-byte unchanged.
        let repo = temp_repo("atomic-union");
        repo.apply("main", create("/a", b"needle\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        repo.apply("s", create("/ok", b"clean\n"), note("t")).unwrap();
        repo.apply("s", edit("/a", "needle", "thread"), note("t")).unwrap();
        repo.apply("main", edit("/a", "needle", "nail"), note("t")).unwrap();

        let before = repo.current_state("main").unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(!outcome.complete());
        assert!(outcome.landed.is_empty(), "no prefix lands");
        assert!(outcome.needs_reenactment.is_some());
        let after = repo.current_state("main").unwrap();
        assert_eq!(after.sum, before.sum, "target untouched");
        assert_eq!(after.lines.len(), before.lines.len(), "no /ok line leaked onto main");
    }

    #[test]
    fn disjoint_spans_in_same_file_do_not_conflict() {
        // The span-granularity payoff: the fork reads the MIN line of
        // /config, then edits /handler; main concurrently changes the MAX
        // line of the *same* /config file.  Path granularity would cry
        // conflict; span granularity sees the read line is untouched.
        let repo = temp_repo("span-disjoint");
        repo.apply("main", create("/config", b"MAX=10\nMIN=1\n"), note("t")).unwrap();
        repo.apply("main", create("/handler", b"x = default\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let cfg = repo.current_state("s").unwrap().manifest.get("/config").unwrap().blob.clone();
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": cfg, "spans": ["MIN=1"]}]));
        repo.apply("s", edit("/handler", "default", "1"), n).unwrap();
        // Main edits a disjoint line of the same file.
        repo.apply("main", edit("/config", "MAX=10", "MAX=1000"), note("t")).unwrap();

        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert_eq!(outcome.landed.len(), 1);
        assert!(outcome.complete(), "reading MIN survives a concurrent edit to MAX");
        assert!(outcome.semantic_conflicts.is_empty());
    }

    #[test]
    fn a_read_of_the_edited_span_still_conflicts() {
        // Same setup, but now the fork read the very line main changed:
        // span granularity must still catch it, and must not be fooled by
        // "MAX=10" being a substring of the new "MAX=1000".
        let repo = temp_repo("span-conflict");
        repo.apply("main", create("/config", b"MAX=10\nMIN=1\n"), note("t")).unwrap();
        repo.apply("main", create("/handler", b"x = default\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let cfg = repo.current_state("s").unwrap().manifest.get("/config").unwrap().blob.clone();
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": cfg, "spans": ["MAX=10"]}]));
        repo.apply("s", edit("/handler", "default", "10"), n).unwrap();
        repo.apply("main", edit("/config", "MAX=10", "MAX=1000"), note("t")).unwrap();

        let before = repo.current_state("main").unwrap().sum;
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(!outcome.complete(), "reading the edited line is a conflict");
        assert!(outcome.landed.is_empty(), "atomic: nothing lands on conflict");
        assert_eq!(repo.current_state("main").unwrap().sum, before, "target untouched");
        assert_eq!(outcome.semantic_conflicts.len(), 1);
        assert!(outcome.semantic_conflicts[0].paths.contains("/config"));
    }

    #[test]
    fn incoming_write_strands_a_concurrent_target_read() {
        // The symmetric direction: main's own patch read /config's MAX line
        // (a pure dependency), then wrote /report on the strength of it.  The
        // fork concurrently rewrites that MAX line.  Landing the fork would
        // strand main's reasoning: W_source ∩ R_target.
        let repo = temp_repo("strand");
        repo.apply("main", create("/config", b"MAX=10\nMIN=1\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let cfg = repo.current_state("s").unwrap().manifest.get("/config").unwrap().blob.clone();
        // Main reads the MAX line, writes an unrelated file on the strength of it.
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": cfg, "spans": ["MAX=10"]}]));
        repo.apply("main", create("/report", b"limit is 10\n"), n).unwrap();
        // The fork rewrites the very line main read.
        repo.apply("s", edit("/config", "MAX=10", "MAX=1000"), note("t")).unwrap();

        let before = repo.current_state("main").unwrap().sum;
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(!outcome.complete(), "landing the fork strands main's read of MAX");
        assert!(outcome.landed.is_empty(), "atomic: nothing lands");
        assert_eq!(repo.current_state("main").unwrap().sum, before, "target untouched");
        assert_eq!(outcome.semantic_conflicts.len(), 1);
        let c = &outcome.semantic_conflicts[0];
        assert_eq!(c.direction, ConflictDirection::IncomingWriteHitsTargetRead);
        assert!(c.paths.contains("/config"));
    }

    #[test]
    fn incoming_write_to_a_disjoint_line_leaves_a_target_read_alone() {
        // The fork rewrites the MAX line; main had read the disjoint MIN line.
        // Span granularity must not manufacture a stranded-read conflict.
        let repo = temp_repo("strand-disjoint");
        repo.apply("main", create("/config", b"MAX=10\nMIN=1\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let cfg = repo.current_state("s").unwrap().manifest.get("/config").unwrap().blob.clone();
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": cfg, "spans": ["MIN=1"]}]));
        repo.apply("main", create("/report", b"min is 1\n"), n).unwrap();
        repo.apply("s", edit("/config", "MAX=10", "MAX=1000"), note("t")).unwrap();

        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.complete(), "the fork edited MAX; main only read MIN");
        assert!(outcome.semantic_conflicts.is_empty());
        assert_eq!(outcome.landed.len(), 1, "the fork's edit lands");
    }

    #[test]
    fn disjoint_writes_do_not_manufacture_a_semantic_conflict() {
        // A read of a path nobody concurrently wrote is not a conflict.
        let repo = temp_repo("no-stale");
        repo.apply("main", create("/config", b"MAX=10\n"), note("t")).unwrap();
        repo.apply("main", create("/handler", b"limit = default\n"), note("t")).unwrap();
        repo.create_fork("s", "main").unwrap();
        let mut n = note("t");
        n.reads = Some(serde_json::json!([{"path": "/config", "blob": "x"}]));
        repo.apply("s", edit("/handler", "default", "10"), n).unwrap();
        // Main drifts a file the fork never read.
        repo.apply("main", create("/unrelated", b"z\n"), note("t")).unwrap();

        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.complete(), "reading /config while main writes /unrelated is no conflict");
        assert!(outcome.semantic_conflicts.is_empty());
    }

    #[test]
    fn commutation_is_read_set_based() {
        let repo = temp_repo("commute");
        let mut n1 = note("t");
        n1.reads = Some(serde_json::json!([{"path": "/a", "blob": "x"}]));
        let l1 = repo.apply("main", create("/a", b"a\n"), n1).unwrap();
        let mut n2 = note("t");
        n2.reads = Some(serde_json::json!([{"path": "/b", "blob": "x"}]));
        let l2 = repo.apply("main", create("/b", b"b\n"), n2).unwrap();
        assert!(commutes(&l1, &l2).unwrap(), "disjoint writes and reads commute");

        // Reading what the other writes: conflict is W1∩R2, not text.
        let mut n3 = note("t");
        n3.reads = Some(serde_json::json!([{"path": "/a", "blob": "x"}]));
        let l3 = repo.apply("main", create("/c", b"c\n"), n3).unwrap();
        assert!(!commutes(&l1, &l3).unwrap());

        // A grep negative is universally quantified: refuse to certify.
        let mut n4 = note("t");
        n4.reads =
            Some(serde_json::json!([{"grep": "unwrap\\(\\)", "matches": [], "over_sum": "x"}]));
        let l4 = repo.apply("main", create("/d", b"d\n"), n4).unwrap();
        assert!(!commutes(&l2, &l4).unwrap());

        // Unobserved reads (andon) refuse likewise.
        let l5 = repo.apply("main", create("/e", b"e\n"), note("t")).unwrap();
        assert!(!commutes(&l2, &l5).unwrap());
    }
}
