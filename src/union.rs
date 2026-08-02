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

use crate::log::{Annotation, LogLine, Origin, Provenance, ViewSpan};
use crate::patch::apply_realized_to_manifest;
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
}

impl UnionOutcome {
    /// True iff every source line landed mechanically.
    pub fn complete(&self) -> bool {
        self.needs_reenactment.is_none()
    }
}

/// Run union of `source` into `target`.  Strata 1–3 only: stratum 4 is
/// reported, never performed.
pub fn union(repo: &Repository, source: &str, target: &str, author: &str) -> Result<UnionOutcome> {
    let source_state = repo.current_state(source)?;
    let mut outcome = UnionOutcome::default();

    // Stratum 1: arithmetic.  Equal sums mean the states are already
    // identical, but views are arithmetic identities the sum cannot see:
    // uncarried view lines still land below.
    let target_state = repo.current_state(target)?;
    if source_state.sum == target_state.sum {
        outcome.already_identical = true;
    }

    // What the target already carries, and the re-key map for view spans:
    // union re-seals ids, so a landed view's `from`/`to` follow the
    // `origin_id -> landed id` correspondence.
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

    for line in &source_state.lines {
        if carried.contains(&line.id) {
            continue;
        }
        if outcome.already_identical && line.annotation.view.is_none() {
            // The arithmetic already accounts for every patch line; only
            // views still travel.
            continue;
        }
        let view = line.annotation.view.as_ref().map(|v| ViewSpan {
            from: rekey.get(&v.from).cloned().unwrap_or_else(|| v.from.clone()),
            to: rekey.get(&v.to).cloned().unwrap_or_else(|| v.to.clone()),
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
            view,
        };

        // Stratum 2: realized replay.  Check the incoming applied patch's
        // removed records against the target manifest; all present, apply
        // the realized delta directly.
        let target_now = repo.current_state(target)?;
        let mut scratch = target_now.manifest.clone();
        let replayable = apply_realized_to_manifest(&mut scratch, &line.realized).is_ok();
        if replayable {
            let landed = repo.apply_realized(
                target,
                line.intent.clone(),
                line.realized.clone(),
                annotation,
            )?;
            rekey.insert(line.id.clone(), landed.id.clone());
            outcome.landed.push(Landed {
                line: landed,
                origin_id: line.id.clone(),
                stratum: Stratum::RealizedReplay,
            });
            continue;
        }

        // Stratum 3: intent replay.  A consumed record is missing because
        // the target drifted; re-validate the span precondition against the
        // target's current blob and realize fresh deltas.
        match repo.apply(target, line.intent.clone(), annotation) {
            Ok(landed) => {
                rekey.insert(line.id.clone(), landed.id.clone());
                outcome.landed.push(Landed {
                    line: landed,
                    origin_id: line.id.clone(),
                    stratum: Stratum::IntentReplay,
                });
            }
            Err(Error::Precondition(evidence)) => {
                // Stratum 4 gate: the patch's assumptions are genuinely
                // dead.  Stop; hand the intent, its prose, and the evidence
                // to a model only when invited.
                outcome.needs_reenactment = Some((line.id.clone(), evidence));
                break;
            }
            Err(other) => return Err(other),
        }
    }
    Ok(outcome)
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
            std::env::temp_dir().join(format!("abelian-union-{name}-{}", std::process::id()));
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
    fn views_travel_through_union_rekeyed() {
        // The ISSUE reproduction: fork, two patches, fuse, union — the
        // fused beat must be visible on main, re-keyed to main's ids.
        let repo = temp_repo("views");
        repo.create_fork("session-1", "main").unwrap();
        let l1 = repo.apply("session-1", create("/a", b"one\n"), note("t")).unwrap();
        let l2 = repo.apply("session-1", edit("/a", "one", "two"), note("t")).unwrap();
        repo.fuse("session-1", &l1.id, &l2.id, Some("one narrative beat".to_string()), "sid")
            .unwrap();
        let outcome = union(&repo, "session-1", "main", "maintainer").unwrap();
        assert!(outcome.complete());
        assert_eq!(outcome.landed.len(), 3, "two patches plus the view line");
        let state = repo.current_state("main").unwrap();
        let beats = crate::views::fused_beats(&state.lines);
        assert_eq!(beats.len(), 1, "main renders one fused beat, not raw lines");
        let crate::views::Beat::Fused { view, lines } = &beats[0] else {
            panic!("expected the fused beat on main");
        };
        assert_eq!(lines.len(), 2);
        assert_eq!(view.annotation.prose.as_deref(), Some("one narrative beat"));
        // Re-keyed through the landed origin map: the span names main's ids.
        let span = view.annotation.view.as_ref().unwrap();
        assert_eq!(span.from, outcome.landed[0].line.id);
        assert_eq!(span.to, outcome.landed[1].line.id);
        assert_ne!(span.from, l1.id);
        assert_ne!(span.to, l2.id);
        // The fork is now fully carried: remove-fork succeeds without force.
        repo.remove_fork("session-1", false).unwrap();
    }

    #[test]
    fn views_land_even_when_sums_are_already_identical() {
        // A view is an arithmetic identity, so stratum 1 cannot see it;
        // union must land it anyway.
        let repo = temp_repo("views-s1");
        repo.create_fork("s", "main").unwrap();
        let l1 = repo.apply("s", create("/a", b"a\n"), note("t")).unwrap();
        let first = union(&repo, "s", "main", "maintainer").unwrap();
        assert_eq!(first.landed.len(), 1);
        // Fuse after the fact: the sums are already equal.
        repo.fuse("s", &l1.id, &l1.id, Some("beat".to_string()), "sid").unwrap();
        let outcome = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(outcome.already_identical, "stratum 1 fired on the arithmetic");
        assert_eq!(outcome.landed.len(), 1, "and the view still landed");
        let span =
            outcome.landed[0].line.annotation.view.as_ref().unwrap();
        assert_eq!(span.from, first.landed[0].line.id, "re-keyed to main's line");
        // Idempotent: a third union carries nothing new.
        let again = union(&repo, "s", "main", "maintainer").unwrap();
        assert!(again.already_identical);
        assert!(again.landed.is_empty());
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
