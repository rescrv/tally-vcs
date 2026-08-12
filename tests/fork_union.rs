//! Integration: generated deltas driven through the fork/union paths.
//!
//! Two claims, one file.  Correctness: randomly generated deltas (create,
//! edit, delete, chmod) fan out across forks and union back into targets,
//! and the resulting sums equal an independently maintained model — in any
//! union order, idempotently, through every stratum, with stratum 4 gated
//! and the target untouched.  Speed: the same generator drives a benchmark
//! that measures commits/s on the interactive apply path, the batched
//! fast-forward path, and the union carry path, asserting floors and
//! printing measured rates (`--nocapture` to see them).

use std::collections::BTreeMap;
use std::time::Instant;

use abelian::b64;
use abelian::ident::{ElementRecord, Sum, sha3_hex};
use abelian::log::Annotation;
use abelian::manifest::Manifest;
use abelian::patch::{Intent, Op, RealizedEntry};
use abelian::repo::Repository;
use abelian::Error;
use abelian::union::{Stratum, union};

/////////////////////////////////////////// scaffolding ///////////////////////////////////////////

fn temp_repo(name: &str) -> Repository {
    let dir = std::env::temp_dir().join(format!(
        "abelian-fork-union-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Repository::init(&dir).unwrap()
}

fn note(author: &str) -> Annotation {
    Annotation { author: author.to_string(), ..Annotation::default() }
}

/// xorshift64*: deterministic, seedable, no dev-dependencies.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

//////////////////////////////////////////// the model ////////////////////////////////////////////

/// An independent model of the state: path -> (mode, content).  The model
/// never touches the repository; agreement between the two at the end is
/// the correctness claim.
#[derive(Default)]
struct Model {
    files: BTreeMap<String, (String, Vec<u8>)>,
    /// Monotonic counter so every generated line is unique within its file
    /// (edit spans must occur exactly once).
    counter: u64,
}

impl Model {
    fn unique_line(&mut self, path: &str) -> String {
        self.counter += 1;
        format!("L{:06} {} salt{:016x}\n", self.counter, path, self.counter.wrapping_mul(0x9E3779B97F4A7C15))
    }

    /// The sum the repository must reach if it applied the same deltas.
    fn expected_sum(&self) -> Sum {
        let records = self.files.iter().map(|(path, (mode, content))| {
            ElementRecord::new(mode, path, &sha3_hex(content)).unwrap()
        });
        Manifest::from_records(records).unwrap().sum()
    }

    /// Generate one delta under `prefix`, mutating the model to match.
    /// Weights favor edits; deletes never empty the namespace.
    fn gen_delta(&mut self, rng: &mut Rng, prefix: &str) -> Intent {
        let mine: Vec<String> = self
            .files
            .keys()
            .filter(|p| p.starts_with(prefix))
            .cloned()
            .collect();
        let roll = rng.below(100);
        if mine.is_empty() || roll < 20 {
            // Create.
            let path = format!("{prefix}/f{:06x}.txt", rng.next() & 0xFFFFFF);
            if self.files.contains_key(&path) {
                return self.gen_delta(rng, prefix);
            }
            let mut content = Vec::new();
            for _ in 0..1 + rng.below(4) {
                content.extend_from_slice(self.unique_line(&path).as_bytes());
            }
            self.files.insert(path.clone(), ("100644".to_string(), content.clone()));
            Intent {
                ops: vec![Op::Create {
                    path,
                    mode: "100644".to_string(),
                    blob: None,
                    content_b64: Some(b64::encode(&content)),
                }],
            }
        } else if roll < 75 || mine.len() == 1 {
            // Edit: replace one whole existing line (unique by construction).
            let path = mine[rng.below(mine.len())].clone();
            let (_, content) = self.files.get(&path).unwrap().clone();
            let text = String::from_utf8(content).unwrap();
            let lines: Vec<&str> = text.lines().collect();
            let old = format!("{}\n", lines[rng.below(lines.len())]);
            let new = self.unique_line(&path);
            let new_text = text.replacen(&old, &new, 1);
            self.files.get_mut(&path).unwrap().1 = new_text.into_bytes();
            Intent {
                ops: vec![Op::Edit {
                    path,
                    old_str: old.trim_end_matches('\n').to_string(),
                    new_str: new.trim_end_matches('\n').to_string(),
                }],
            }
        } else if roll < 90 {
            // Delete: consume the whole element by blob hash.
            let path = mine[rng.below(mine.len())].clone();
            let (_, content) = self.files.remove(&path).unwrap();
            Intent { ops: vec![Op::Delete { path, blob: sha3_hex(&content) }] }
        } else {
            // Chmod: flip 100644 <-> 100755.
            let path = mine[rng.below(mine.len())].clone();
            let (mode, _) = self.files.get(&path).unwrap().clone();
            let new_mode =
                if mode == "100644" { "100755".to_string() } else { "100644".to_string() };
            self.files.get_mut(&path).unwrap().0 = new_mode.clone();
            Intent { ops: vec![Op::Chmod { path, old_mode: mode, new_mode }] }
        }
    }
}

fn create(path: &str, content: &[u8]) -> Intent {
    Intent {
        ops: vec![Op::Create {
            path: path.to_string(),
            mode: "100644".to_string(),
            blob: None,
            content_b64: Some(b64::encode(content)),
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

/////////////////////////////////////// correctness: fan-in ///////////////////////////////////////

/// Generated deltas fan out across forks, union back in, and the repository
/// agrees with the model — in every union order, idempotently, and the
/// working tree agrees with the log.
#[test]
fn generated_deltas_converge_across_forks() {
    const FORKS: usize = 4;
    const DELTAS: usize = 25;
    let repo = temp_repo("converge");
    let mut model = Model::default();
    let mut rng = Rng::new(0xABE11A);

    // Seed main, then snapshot so replay starts from a repointed anchor —
    // the fork/union paths must work across an anchor boundary (§2.4).
    for _ in 0..5 {
        let intent = model.gen_delta(&mut rng, "/seed");
        repo.apply("main", intent, note("seeder")).unwrap();
    }
    repo.snapshot("main").unwrap();

    // Order-independence targets, forked at the seed.
    repo.create_fork("target-a", "main").unwrap();
    repo.create_fork("target-b", "main").unwrap();

    // Fan out: each fork works its own namespace, so every line must land
    // mechanically.
    let names: Vec<String> = (0..FORKS).map(|i| format!("fork-{i}")).collect();
    for (i, name) in names.iter().enumerate() {
        repo.create_fork(name, "main").unwrap();
        let prefix = format!("/ns{i}");
        for _ in 0..DELTAS {
            let intent = model.gen_delta(&mut rng, &prefix);
            repo.apply(name, intent, note(name)).unwrap();
        }
    }

    // Fan in to main; every stratum-2/3 line lands, nothing gates.
    for name in &names {
        let outcome = union(&repo, name, "main", "maintainer").unwrap();
        assert!(outcome.complete(), "union of {name} must land mechanically");
        assert_eq!(outcome.landed.len(), DELTAS);
    }

    // The repository agrees with the model: states are sums.
    let state = repo.current_state("main").unwrap();
    assert_eq!(state.sum.hexdigest(), model.expected_sum().hexdigest());

    // The working tree agrees with the log.
    let (expected, actual) = repo.check("main").unwrap();
    assert_eq!(expected.hexdigest(), actual.hexdigest());

    // Order never matters: forward order into target-a, a scrambled order
    // into target-b, identical sums.
    for name in &names {
        assert!(union(&repo, name, "target-a", "m").unwrap().complete());
    }
    for i in [3usize, 1, 0, 2] {
        assert!(union(&repo, &names[i], "target-b", "m").unwrap().complete());
    }
    let a = repo.current_state("target-a").unwrap().sum;
    let b = repo.current_state("target-b").unwrap().sum;
    assert_eq!(a.hexdigest(), b.hexdigest(), "patches commute; union order is irrelevant");

    // Idempotence: a second union carries nothing.
    for name in &names {
        let again = union(&repo, name, "main", "maintainer").unwrap();
        assert!(again.already_identical || again.landed.is_empty());
        assert!(again.landed.is_empty());
    }

    // Every fork is fully subsumed: removal without force succeeds.
    for name in &names {
        repo.remove_fork(name, false).unwrap();
    }
}

////////////////////////////////////// correctness: strata ///////////////////////////////////////

/// Deletes, chmods, and disjoint-span edits travel through union; the exact
/// stratum that lands each line is asserted.
#[test]
fn deltas_of_every_kind_travel_through_union() {
    let repo = temp_repo("kinds");
    repo.apply("main", create("/a.txt", b"alpha\nbeta\ngamma\n"), note("t")).unwrap();
    repo.apply("main", create("/doomed.txt", b"bye\n"), note("t")).unwrap();
    repo.apply("main", create("/script.sh", b"#!/bin/sh\n"), note("t")).unwrap();
    repo.create_fork("s", "main").unwrap();

    // The fork: an edit, a delete, a chmod, a create, and a symlink.
    repo.apply("s", edit("/a.txt", "beta", "BETA"), note("s")).unwrap();
    repo.apply(
        "s",
        Intent { ops: vec![Op::Delete { path: "/doomed.txt".to_string(), blob: sha3_hex(b"bye\n") }] },
        note("s"),
    )
    .unwrap();
    repo.apply(
        "s",
        Intent {
            ops: vec![Op::Chmod {
                path: "/script.sh".to_string(),
                old_mode: "100644".to_string(),
                new_mode: "100755".to_string(),
            }],
        },
        note("s"),
    )
    .unwrap();
    repo.apply("s", create("/new.txt", b"fresh\n"), note("s")).unwrap();
    repo.apply(
        "s",
        Intent {
            ops: vec![Op::Create {
                path: "/link".to_string(),
                mode: "120000".to_string(),
                blob: None,
                content_b64: Some(b64::encode(b"a.txt")),
            }],
        },
        note("s"),
    )
    .unwrap();

    // Main drifts in a disjoint span of the edited file: the edit must fall
    // through stratum 2 to stratum 3; everything untouched by the drift
    // still lands at stratum 2.
    repo.apply("main", edit("/a.txt", "gamma", "GAMMA"), note("t")).unwrap();

    let outcome = union(&repo, "s", "main", "maintainer").unwrap();
    assert!(outcome.complete(), "all five kinds land mechanically");
    assert_eq!(outcome.landed.len(), 5);
    assert_eq!(outcome.landed[0].stratum, Stratum::IntentReplay, "edit re-validated under drift");
    for landed in &outcome.landed[1..] {
        assert_eq!(landed.stratum, Stratum::RealizedReplay, "undrifted records replay directly");
    }

    let state = repo.current_state("main").unwrap();
    let blob = repo.blobs().get(&state.manifest.get("/a.txt").unwrap().blob).unwrap();
    assert_eq!(blob, b"alpha\nBETA\nGAMMA\n", "both sides' edits are present");
    assert!(state.manifest.get("/doomed.txt").is_none(), "the delete travelled");
    assert_eq!(state.manifest.get("/script.sh").unwrap().mode, "100755", "the chmod travelled");
    assert_eq!(state.manifest.get("/link").unwrap().mode, "120000", "the symlink travelled");
    let (expected, actual) = repo.check("main").unwrap();
    assert_eq!(expected.hexdigest(), actual.hexdigest(), "working tree matches, symlink included");
}

/// Stratum 4 is a gate, not a stratum: conflicting assumptions stop union
/// cold, lines before the conflict land, and the target is never partially
/// wrong.
#[test]
fn conflicts_gate_reenactment_and_leave_the_target_sound() {
    let repo = temp_repo("gate");
    repo.apply("main", create("/x.txt", b"needle\n"), note("t")).unwrap();
    repo.create_fork("s", "main").unwrap();

    // Line 1 commutes; line 2's span dies on main; line 3 never gets a turn.
    let ok = repo.apply("s", create("/ok.txt", b"fine\n"), note("s")).unwrap();
    let dead = repo.apply("s", edit("/x.txt", "needle", "thread"), note("s")).unwrap();
    repo.apply("s", create("/after.txt", b"later\n"), note("s")).unwrap();
    repo.apply("main", edit("/x.txt", "needle", "nail"), note("t")).unwrap();

    let outcome = union(&repo, "s", "main", "maintainer").unwrap();
    assert!(!outcome.complete());
    assert_eq!(outcome.landed.len(), 1, "the commuting line landed before the gate");
    assert_eq!(outcome.landed[0].origin_id, ok.id);
    let (gated_id, evidence) = outcome.needs_reenactment.as_ref().unwrap();
    assert_eq!(gated_id, &dead.id, "the gate names the line whose assumptions died");
    assert!(evidence.contains("0 times"), "evidence is the failed precondition: {evidence}");

    let state = repo.current_state("main").unwrap();
    assert!(state.manifest.get("/after.txt").is_none(), "union stopped at the gate");
    let blob = repo.blobs().get(&state.manifest.get("/x.txt").unwrap().blob).unwrap();
    assert_eq!(blob, b"nail\n", "the target's own history is untouched");

    // Duplicate creates gate too: the same path born on both sides is a
    // conflict, not a merge.
    repo.create_fork("dup", "main").unwrap();
    repo.apply("dup", create("/same.txt", b"mine\n"), note("d")).unwrap();
    repo.apply("main", create("/same.txt", b"yours\n"), note("t")).unwrap();
    let outcome = union(&repo, "dup", "main", "maintainer").unwrap();
    assert!(!outcome.complete(), "duplicate create is stratum-4 material");
    assert!(outcome.landed.is_empty());

    // Delete-vs-edit gates: the fork consumed the whole element, main
    // rewrote it.
    repo.create_fork("del", "main").unwrap();
    repo.apply(
        "del",
        Intent { ops: vec![Op::Delete { path: "/ok.txt".to_string(), blob: sha3_hex(b"fine\n") }] },
        note("d"),
    )
    .unwrap();
    repo.apply("main", edit("/ok.txt", "fine", "changed"), note("t")).unwrap();
    let outcome = union(&repo, "del", "main", "maintainer").unwrap();
    assert!(!outcome.complete(), "delete of a rewritten element must gate");
}

/// Fork-of-fork chains and snapshot-repointed anchors: union still carries,
/// and an empty union is arithmetic (stratum 1).
#[test]
fn union_across_fork_chains_and_snapshots() {
    let repo = temp_repo("chains");
    let mut model = Model::default();
    let mut rng = Rng::new(0xC0FFEE);

    for _ in 0..4 {
        let intent = model.gen_delta(&mut rng, "/base");
        repo.apply("main", intent, note("t")).unwrap();
    }
    repo.snapshot("main").unwrap();

    // A chain: main -> a -> b; b works, unions into a, a unions into main.
    repo.create_fork("a", "main").unwrap();
    repo.create_fork("b", "a").unwrap();
    for _ in 0..6 {
        let intent = model.gen_delta(&mut rng, "/deep");
        repo.apply("b", intent, note("b")).unwrap();
    }
    assert!(union(&repo, "b", "a", "m").unwrap().complete());
    assert!(union(&repo, "a", "main", "m").unwrap().complete());
    let state = repo.current_state("main").unwrap();
    assert_eq!(state.sum.hexdigest(), model.expected_sum().hexdigest());

    // A snapshot on the target between fork and union: anchors repoint,
    // logs remain, union does not care.
    repo.create_fork("late", "main").unwrap();
    let intent = model.gen_delta(&mut rng, "/late");
    repo.apply("late", intent, note("l")).unwrap();
    let drift = model.gen_delta(&mut rng, "/base");
    repo.apply("main", drift, note("t")).unwrap();
    repo.snapshot("main").unwrap();
    assert!(union(&repo, "late", "main", "m").unwrap().complete());
    let state = repo.current_state("main").unwrap();
    assert_eq!(state.sum.hexdigest(), model.expected_sum().hexdigest());

    // An empty fork unions by arithmetic alone.
    repo.create_fork("empty", "main").unwrap();
    let outcome = union(&repo, "empty", "main", "m").unwrap();
    assert!(outcome.already_identical);
    assert!(outcome.landed.is_empty());
}

///////////////////////////////////////////// benchmark ///////////////////////////////////////////

/// The benchmark: prove the fork/union paths are fast, on this machine, in
/// this run, with the sums checked at the end so speed never comes at the
/// price of the arithmetic.
///
/// Three measurements:
///   - interactive applies (`Repository::apply`): fsync-per-commit, the
///     durability floor of the disk;
///   - batched fast-forward (`Repository::append_realized_batch`): one
///     fsync per batch, the designed high-throughput path — this is where
///     the 1k commits/s target lives;
///   - union carry: landing a fork's log on an undrifted target.
#[test]
fn bench_fork_union_throughput() {
    const APPLY_N: usize = 200;
    const BATCH_N: usize = 5_000;
    const BATCH_SIZE: usize = 500;
    const UNION_N: usize = 200;
    const BLOB_POOL: usize = 32;
    const PATHS: usize = 512;

    let repo = temp_repo("bench");

    // Phase A: interactive applies.  Every commit is a full durability
    // cycle (blob fsync, log fsync, directory fsync), so this measures the
    // disk, not the arithmetic.
    let mut model = Model::default();
    let mut rng = Rng::new(0xBE9C);
    repo.create_fork("writer", "main").unwrap();
    let start = Instant::now();
    for _ in 0..APPLY_N {
        let intent = model.gen_delta(&mut rng, "/w");
        repo.apply("writer", intent, note("w")).unwrap();
    }
    let apply_el = start.elapsed();
    let apply_rate = APPLY_N as f64 / apply_el.as_secs_f64();
    let state = repo.current_state("writer").unwrap();
    assert_eq!(
        state.sum.hexdigest(),
        model.expected_sum().hexdigest(),
        "phase A: {APPLY_N} interactive commits agree with the model"
    );

    // Phase B: batched fast-forward.  Blobs are durable before any line
    // references them (I8), so blob ingestion is a separate stage; here it
    // is a pool put once, untimed, and the timed region is pure commit
    // path: seal, chain, sum, one fsync per batch.
    let blobs = repo.blobs();
    let pool: Vec<String> =
        (0..BLOB_POOL).map(|i| blobs.put(format!("blob {i}\n").as_bytes()).unwrap()).collect();
    repo.create_fork("firehose", "main").unwrap();
    let mut current: Vec<Option<usize>> = vec![None; PATHS]; // path -> pool index
    let mut brng = Rng::new(0xF14E);
    let mut committed = 0usize;
    let start = Instant::now();
    while committed < BATCH_N {
        let n = BATCH_SIZE.min(BATCH_N - committed);
        let mut batch = Vec::with_capacity(n);
        for _ in 0..n {
            let p = brng.below(PATHS);
            let path = format!("/hose/f{p:04}.txt");
            let next = brng.below(BLOB_POOL);
            let add = ElementRecord::new("100644", &path, &pool[next]).unwrap();
            let remove = current[p]
                .map(|old| ElementRecord::new("100644", &path, &pool[old]).unwrap());
            if remove.as_ref().is_some_and(|r| r.blob == add.blob) {
                // A no-op delta is not a commit; nudge the content.
                continue;
            }
            current[p] = Some(next);
            batch.push((
                vec![RealizedEntry {
                    remove: remove.map(|r| r.to_line()),
                    add: Some(add.to_line()),
                }],
                note("firehose"),
                0u64,
            ));
        }
        committed += repo.append_realized_batch("firehose", batch).unwrap();
    }
    let batch_el = start.elapsed();
    let batch_rate = committed as f64 / batch_el.as_secs_f64();

    // Prove the firehose wrote history, not noise: replay the whole chain
    // and compare against an independent model manifest.
    let state = repo.current_state("firehose").unwrap();
    assert_eq!(state.lines.len(), committed);
    let records = current.iter().enumerate().filter_map(|(p, blob)| {
        blob.map(|b| {
            ElementRecord::new("100644", &format!("/hose/f{p:04}.txt"), &pool[b]).unwrap()
        })
    });
    let expected = Manifest::from_records(records).unwrap().sum();
    assert_eq!(
        state.sum.hexdigest(),
        expected.hexdigest(),
        "phase B: {committed} batched commits replay to the model's sum"
    );
    // History is arithmetic: an interior state is reachable by replay.
    let mid = &state.lines[committed / 2];
    let mid_manifest = repo.manifest_at("firehose", &mid.sum_after).unwrap();
    assert_eq!(mid_manifest.sum().hexdigest(), mid.sum_after);

    // Phase C: union carry.  An undrifted target lands every line at
    // stratum 2; each landing is a full durability cycle.
    let mut umodel = Model::default();
    let mut urng = Rng::new(0x0912);
    repo.create_fork("carrier", "main").unwrap();
    for _ in 0..UNION_N {
        let intent = umodel.gen_delta(&mut urng, "/c");
        repo.apply("carrier", intent, note("c")).unwrap();
    }
    repo.create_fork("landing", "main").unwrap();
    let start = Instant::now();
    let outcome = union(&repo, "carrier", "landing", "maintainer").unwrap();
    let union_el = start.elapsed();
    assert!(outcome.complete());
    assert_eq!(outcome.landed.len(), UNION_N);
    let union_rate = UNION_N as f64 / union_el.as_secs_f64();
    assert_eq!(
        repo.current_state("landing").unwrap().sum.hexdigest(),
        umodel.expected_sum().hexdigest(),
        "phase C: the carried lines replay to the model's sum"
    );

    eprintln!("bench_fork_union_throughput:");
    eprintln!("  interactive apply : {APPLY_N:>6} commits in {apply_el:>10.3?} = {apply_rate:>9.1} commits/s");
    eprintln!("  batched firehose  : {committed:>6} commits in {batch_el:>10.3?} = {batch_rate:>9.1} commits/s");
    eprintln!("  union carry       : {UNION_N:>6} lines   in {union_el:>10.3?} = {union_rate:>9.1} lines/s");

    // Floors.  The fsync-bound paths must beat 10/s anywhere; the batched
    // path must beat 1,000 commits/s — the target this test exists to prove.
    assert!(apply_rate >= 10.0, "interactive applies too slow: {apply_rate:.1}/s < 10/s");
    assert!(union_rate >= 10.0, "union carry too slow: {union_rate:.1}/s < 10/s");
    assert!(batch_rate >= 1_000.0, "batched commits too slow: {batch_rate:.1}/s < 1000/s");
}

/////////////////////////////////////// error-path coverage ///////////////////////////////////////

/// The generator's ops fail cleanly when their preconditions are violated;
/// nothing is written on failure.
#[test]
fn violated_preconditions_write_nothing() {
    let repo = temp_repo("preconditions");
    repo.apply("main", create("/f.txt", b"content\n"), note("t")).unwrap();
    let before = repo.current_state("main").unwrap();

    // Edit of an absent path.
    let err = repo.apply("main", edit("/absent.txt", "a", "b"), note("t")).unwrap_err();
    assert!(matches!(err, Error::Precondition(_)));
    // Create of a present path.
    let err = repo.apply("main", create("/f.txt", b"again\n"), note("t")).unwrap_err();
    assert!(matches!(err, Error::Precondition(_)));
    // Delete with the wrong blob hash.
    let err = repo
        .apply(
            "main",
            Intent {
                ops: vec![Op::Delete { path: "/f.txt".to_string(), blob: sha3_hex(b"wrong\n") }],
            },
            note("t"),
        )
        .unwrap_err();
    assert!(matches!(err, Error::Precondition(_)));
    // Edit whose span is ambiguous (occurs zero times).
    let err = repo.apply("main", edit("/f.txt", "no such span", "x"), note("t")).unwrap_err();
    assert!(matches!(err, Error::Precondition(_)));

    let after = repo.current_state("main").unwrap();
    assert_eq!(before.sum.hexdigest(), after.sum.hexdigest(), "failures wrote nothing");
    assert_eq!(before.lines.len(), after.lines.len());
}
