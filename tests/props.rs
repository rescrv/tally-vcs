//! Property tests over the substrate's risky paths.
//!
//! "Risky" here means: anywhere identity is computed (the Wall, SPEC §0),
//! anywhere hostile bytes are parsed (records, manifests, base64, segments),
//! and anywhere a precondition gate protects state (apply_intent's
//! validate-then-mutate, manifest adjudication per I9).  Each module below
//! names the invariant it exercises.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;

use tally::b64;
use tally::blobs::BlobStore;
use tally::diff::pending_sum;
use tally::ident::{
    ElementRecord, Sum, canonical_json, record_id, sha3_hex, sum_of_records, validate_path,
    verify_record_id,
};
use tally::manifest::Manifest;
use tally::patch::{
    Intent, Op, apply_intent, apply_realized_to_manifest, apply_realized_to_sum,
    count_occurrences, replace_unique,
};
use tally::segment::{ImageItem, Segment, SegmentInput, build_segment, image_setsum};

/////////////////////////////////////////// strategies ////////////////////////////////////////////

/// A path component that cannot be `.` or `..` (no dots in the alphabet).
fn arb_component() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,8}"
}

/// A valid element path per §1.1: absolute, no NUL/LF/TAB, no dot components.
fn arb_path() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_component(), 1..4).prop_map(|parts| format!("/{}", parts.join("/")))
}

fn arb_mode() -> impl Strategy<Value = &'static str> {
    prop_oneof![Just("100644"), Just("100755"), Just("120000")]
}

fn arb_content() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..128)
}

fn arb_record() -> impl Strategy<Value = ElementRecord> {
    (arb_mode(), arb_path(), arb_content())
        .prop_map(|(m, p, c)| ElementRecord::new(m, &p, &sha3_hex(&c)).unwrap())
}

/// Records with pairwise-distinct paths, so they form a valid manifest.
fn arb_state() -> impl Strategy<Value = Vec<ElementRecord>> {
    prop::collection::vec(arb_record(), 0..12).prop_map(|recs| {
        let mut by_path = BTreeMap::new();
        for r in recs {
            by_path.insert(r.path.clone(), r);
        }
        by_path.into_values().collect()
    })
}

/// JSON with integer numbers only (canonical JSON forbids floats, §1.3).
fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::from),
        any::<i64>().prop_map(serde_json::Value::from),
        "[a-zA-Z0-9 /\\\\\"\u{10}\u{7f}\u{1F600}-]{0,12}".prop_map(serde_json::Value::from),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(serde_json::Value::from),
            prop::collection::btree_map("[a-zA-Z0-9_]{0,6}", inner, 0..4)
                .prop_map(|m| serde_json::Value::from(serde_json::Map::from_iter(m))),
        ]
    })
}

/// An identified record: a JSON object (record ids only exist for objects).
fn arb_json_object() -> impl Strategy<Value = serde_json::Value> {
    prop::collection::btree_map("[a-zA-Z0-9_]{1,6}", arb_json(), 0..4)
        .prop_map(|m| serde_json::Value::from(serde_json::Map::from_iter(m)))
}

//////////////////////////////////////// ident: the sum ///////////////////////////////////////////
// I1's substrate: state identity is arithmetic in an abelian group.  If any
// of these laws fails, "patches commute" is false and everything downstream
// (union, pending_sum, segment images) silently corrupts.

proptest! {
    /// Order never matters: the fold of any permutation is the same sum.
    #[test]
    fn sum_is_commutative(recs in arb_state().prop_shuffle()) {
        let mut sorted = recs.clone();
        sorted.sort();
        prop_assert_eq!(
            sum_of_records(recs.iter()).hexdigest(),
            sum_of_records(sorted.iter()).hexdigest()
        );
    }

    /// Every insert has an inverse: insert-all then remove-all is zero.
    #[test]
    fn sum_remove_inverts_insert(recs in arb_state()) {
        let mut sum = Sum::zero();
        for r in &recs {
            sum.insert(&r.to_bytes());
        }
        for r in &recs {
            sum.remove(&r.to_bytes());
        }
        prop_assert_eq!(sum.hexdigest(), Sum::zero().hexdigest());
    }

    /// Add/Sub agree with insert/remove: (a + b) - b == a.
    #[test]
    fn sum_add_sub_roundtrip(a in arb_state(), b in arb_state()) {
        let sa = sum_of_records(a.iter());
        let sb = sum_of_records(b.iter());
        let back = (sa.clone() + sb.clone()) - sb;
        prop_assert_eq!(back.hexdigest(), sa.hexdigest());
    }

    /// The 64-hex rendering round-trips.
    #[test]
    fn sum_hexdigest_roundtrip(recs in arb_state()) {
        let sum = sum_of_records(recs.iter());
        let back = Sum::from_hexdigest(&sum.hexdigest()).unwrap();
        prop_assert_eq!(back.hexdigest(), sum.hexdigest());
    }

    /// from_hexdigest never panics on hostile input, and only accepts
    /// 64 lowercase hex characters.
    #[test]
    fn sum_from_hexdigest_hostile(s in "\\PC{0,80}") {
        if Sum::from_hexdigest(&s).is_ok() {
            prop_assert_eq!(s.len(), 64);
            prop_assert!(s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
    }

    /// pending_sum is the group difference: before + pending == after, and
    /// it is zero exactly when the states have equal sums.
    #[test]
    fn pending_sum_is_group_difference(a in arb_state(), b in arb_state()) {
        let sa = sum_of_records(a.iter());
        let sb = sum_of_records(b.iter());
        let pending = pending_sum(&sa, &sb);
        prop_assert_eq!((sa.clone() + pending.clone()).hexdigest(), sb.hexdigest());
        prop_assert_eq!(
            pending.hexdigest() == Sum::zero().hexdigest(),
            sa.hexdigest() == sb.hexdigest()
        );
    }
}

///////////////////////////////////// ident: element records //////////////////////////////////////
// Record bytes are hash preimages; parse accepts hostile bytes.  Parsing
// must be the exact inverse of rendering and must reject anything that
// violates §1.1 (the risky rejects: traversal components, control bytes).

proptest! {
    /// A valid record survives to_line/parse and to_bytes/parse unchanged.
    #[test]
    fn record_parse_roundtrip(rec in arb_record()) {
        let via_line = ElementRecord::parse(&rec.to_line()).unwrap();
        prop_assert_eq!(&via_line, &rec);
        let bytes = rec.to_bytes();
        let via_bytes = ElementRecord::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        prop_assert_eq!(&via_bytes, &rec);
    }

    /// Hostile parse never panics; when it accepts, the reserialization is
    /// byte-identical to the (newline-stripped) input — no lossy parse.
    #[test]
    fn record_parse_hostile(s in "\\PC{0,64}") {
        if let Ok(rec) = ElementRecord::parse(&s) {
            prop_assert_eq!(rec.to_line(), s.strip_suffix('\n').unwrap_or(&s));
        }
    }

    /// Traversal and control-byte paths are rejected (§1.1).
    #[test]
    fn path_rejects_traversal_and_controls(
        prefix in prop::collection::vec(arb_component(), 0..2),
        bad in prop_oneof![
            Just(".".to_string()),
            Just("..".to_string()),
            Just("a\tb".to_string()),
            Just("a\nb".to_string()),
            Just("a\0b".to_string()),
        ],
        suffix in prop::collection::vec(arb_component(), 0..2),
    ) {
        let mut parts = prefix;
        parts.push(bad);
        parts.extend(suffix);
        let path = format!("/{}", parts.join("/"));
        prop_assert!(validate_path(&path).is_err(), "accepted {path:?}");
    }

    /// Relative paths are rejected.
    #[test]
    fn path_rejects_relative(parts in prop::collection::vec(arb_component(), 1..3)) {
        prop_assert!(validate_path(&parts.join("/")).is_err());
    }

    /// Bad modes and bad blob hashes are rejected at construction.
    #[test]
    fn record_new_rejects_bad_fields(
        path in arb_path(),
        mode in "\\PC{0,8}",
        blob in "\\PC{0,70}",
    ) {
        let good_blob = sha3_hex(b"x");
        if !["100644", "100755", "120000"].contains(&mode.as_str()) {
            prop_assert!(ElementRecord::new(&mode, &path, &good_blob).is_err());
        }
        let blob_ok = blob.len() == 64
            && blob.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !blob_ok {
            prop_assert!(ElementRecord::new("100644", &path, &blob).is_err());
        }
    }
}

///////////////////////////////// ident: canonical JSON, record ids ///////////////////////////////
// Canonical JSON bytes are hash preimages (§1.3): the risky property is
// that the serializer is deterministic and matches its normative reference,
// and that id verification actually catches tampering.

proptest! {
    /// canonical_json matches the normative reference
    /// (`json.dumps(sort_keys=True, separators=(',',':'))` semantics), as
    /// reproduced by serde_json on sorted maps with compact separators —
    /// and it round-trips through a JSON parser to the same value.
    #[test]
    fn canonical_json_roundtrips(v in arb_json()) {
        let s = canonical_json(&v).unwrap();
        let back: serde_json::Value = serde_json::from_str(&s).unwrap();
        prop_assert_eq!(&back, &v);
        // Compact: no insignificant whitespace outside strings.
        prop_assert!(!s.starts_with(' ') && !s.ends_with(' '));
        // Deterministic: re-canonicalizing the parse yields identical bytes.
        prop_assert_eq!(canonical_json(&back).unwrap(), s);
    }

    /// A record stamped with its own id verifies; any change to any field
    /// (or to the id itself) fails verification.
    #[test]
    fn record_id_detects_tampering(obj in arb_json_object()) {
        let id = record_id(&obj).unwrap();
        let mut stamped = obj.clone();
        stamped["id"] = serde_json::Value::from(id.clone());
        prop_assert!(verify_record_id(&stamped).is_ok());

        // Tamper with the id.
        let mut bad_id = stamped.clone();
        let flipped: String = id
            .chars()
            .enumerate()
            .map(|(i, c)| if i == 0 { if c == '0' { '1' } else { '0' } } else { c })
            .collect();
        bad_id["id"] = serde_json::Value::from(flipped);
        prop_assert!(verify_record_id(&bad_id).is_err());

        // Tamper with the content.
        let mut bad_content = stamped.clone();
        bad_content["__tamper"] = serde_json::Value::from(1);
        prop_assert!(verify_record_id(&bad_content).is_err());
    }

    /// Floats are on the wrong side of the Wall: canonical_json rejects them.
    #[test]
    fn canonical_json_rejects_floats(f in any::<f64>().prop_filter("finite non-int", |f| {
        f.is_finite() && f.fract() != 0.0
    })) {
        let v = serde_json::json!({ "x": f });
        prop_assert!(canonical_json(&v).is_err());
    }
}

/////////////////////////////////////////////// b64 ///////////////////////////////////////////////
// Inline patch content travels as base64; the decoder sees hostile input.

proptest! {
    /// encode/decode is the identity on bytes.
    #[test]
    fn b64_roundtrip(bytes in prop::collection::vec(any::<u8>(), 0..256)) {
        let encoded = b64::encode(&bytes);
        prop_assert_eq!(b64::decode(&encoded).unwrap(), bytes);
    }

    /// Hostile decode never panics; anything it accepts is well-formed
    /// (multiple-of-4 length, alphabet + trailing padding only).
    #[test]
    fn b64_decode_hostile(s in "\\PC{0,48}") {
        if b64::decode(&s).is_ok() {
            prop_assert_eq!(s.len() % 4, 0);
            let trimmed = s.trim_end_matches('=');
            prop_assert!(s.len() - trimmed.len() <= 2);
            prop_assert!(trimmed.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/'));
        }
    }
}

////////////////////////////////// patch: spans and replace_unique ////////////////////////////////
// replace_unique is the content-addressed span edit: exactly-once or fail.

/// Reference count via naive scan.
fn naive_count(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    haystack.windows(needle.len()).filter(|w| *w == needle).count()
}

proptest! {
    /// count_occurrences agrees with a naive reference.
    #[test]
    fn count_occurrences_matches_reference(
        haystack in prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 0..64),
        needle in prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 0..6),
    ) {
        prop_assert_eq!(
            count_occurrences(&haystack, &needle),
            naive_count(&haystack, &needle)
        );
    }

    /// replace_unique errs with the exact count when the needle is not
    /// unique, and splices correctly when it is.
    #[test]
    fn replace_unique_is_exactly_once(
        haystack in prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 0..64),
        needle in prop::collection::vec(prop::sample::select(vec![b'a', b'b']), 1..6),
        replacement in prop::collection::vec(any::<u8>(), 0..8),
    ) {
        let n = naive_count(&haystack, &needle);
        match replace_unique(&haystack, &needle, &replacement) {
            Err(got) => prop_assert_eq!(got, n),
            Ok(out) => {
                prop_assert_eq!(n, 1);
                let at = haystack.windows(needle.len()).position(|w| w == &needle[..]).unwrap();
                let mut expect = haystack[..at].to_vec();
                expect.extend_from_slice(&replacement);
                expect.extend_from_slice(&haystack[at + needle.len()..]);
                prop_assert_eq!(out, expect);
            }
        }
    }
}

//////////////////////////////// patch: apply_intent, model-checked ///////////////////////////////
// The riskiest write path: validate-then-mutate against a manifest and
// blob store, with the realized delta feeding both sum arithmetic and
// manifest adjudication (I9).  Model: a BTreeMap<path, (mode, content)>.

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_blobs() -> (std::path::PathBuf, BlobStore) {
    let dir = std::env::temp_dir().join(format!(
        "tally-props-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let store = BlobStore::init(&dir).unwrap();
    (dir, store)
}

/// A generation seed for one op; interpreted against the evolving model so
/// every generated op is valid by construction.
#[derive(Clone, Debug)]
enum OpSeed {
    Create { path_ix: u8, mode_ix: u8, content: Vec<u8>, inline: bool },
    Edit { target_ix: u8, replacement: Vec<u8> },
    Delete { target_ix: u8 },
    Chmod { target_ix: u8, mode_ix: u8 },
}

fn arb_op_seed() -> impl Strategy<Value = OpSeed> {
    prop_oneof![
        (any::<u8>(), 0u8..3, prop::collection::vec(any::<u8>(), 0..48), any::<bool>())
            .prop_map(|(path_ix, mode_ix, content, inline)| OpSeed::Create {
                path_ix,
                mode_ix,
                content,
                inline
            }),
        (any::<u8>(), prop::collection::vec(any::<u8>(), 0..16))
            .prop_map(|(target_ix, replacement)| OpSeed::Edit { target_ix, replacement }),
        any::<u8>().prop_map(|target_ix| OpSeed::Delete { target_ix }),
        (any::<u8>(), 0u8..3).prop_map(|(target_ix, mode_ix)| OpSeed::Chmod {
            target_ix,
            mode_ix
        }),
    ]
}

const MODES: [&str; 3] = ["100644", "100755", "120000"];

/// Interpret seeds into concrete, valid ops against the model, mutating the
/// model as the intent would.  Edits use the whole current content as
/// old_str so uniqueness holds regardless of the bytes involved.
fn realize_seeds(
    seeds: &[OpSeed],
    model: &mut BTreeMap<String, (String, Vec<u8>)>,
    blobs: &BlobStore,
) -> Vec<Op> {
    let mut ops = Vec::new();
    for seed in seeds {
        let paths: Vec<String> = model.keys().cloned().collect();
        match seed {
            OpSeed::Create { path_ix, mode_ix, content, inline } => {
                let path = format!("/f{path_ix}");
                if model.contains_key(&path) {
                    continue;
                }
                let mode = MODES[*mode_ix as usize];
                let op = if *inline {
                    Op::Create {
                        path: path.clone(),
                        mode: mode.to_string(),
                        blob: None,
                        content_b64: Some(b64::encode(content)),
                    }
                } else {
                    let hash = blobs.put(content).unwrap();
                    Op::Create {
                        path: path.clone(),
                        mode: mode.to_string(),
                        blob: Some(hash),
                        content_b64: None,
                    }
                };
                model.insert(path, (mode.to_string(), content.clone()));
                ops.push(op);
            }
            OpSeed::Edit { target_ix, replacement } => {
                if paths.is_empty() {
                    continue;
                }
                let path = &paths[*target_ix as usize % paths.len()];
                let (_, content) = model.get(path).unwrap().clone();
                // old_str must be non-empty, occur exactly once, and be
                // UTF-8 (Op carries String).  The whole content is unique
                // by definition; require it be valid UTF-8 and non-empty.
                let Ok(old_str) = std::str::from_utf8(&content) else { continue };
                if old_str.is_empty() {
                    continue;
                }
                let Ok(new_str) = std::str::from_utf8(replacement) else { continue };
                ops.push(Op::Edit {
                    path: path.clone(),
                    old_str: old_str.to_string(),
                    new_str: new_str.to_string(),
                });
                model.get_mut(path).unwrap().1 = replacement.clone();
            }
            OpSeed::Delete { target_ix } => {
                if paths.is_empty() {
                    continue;
                }
                let path = paths[*target_ix as usize % paths.len()].clone();
                let (_, content) = model.get(&path).unwrap();
                ops.push(Op::Delete { path: path.clone(), blob: sha3_hex(content) });
                model.remove(&path);
            }
            OpSeed::Chmod { target_ix, mode_ix } => {
                if paths.is_empty() {
                    continue;
                }
                let path = paths[*target_ix as usize % paths.len()].clone();
                let new_mode = MODES[*mode_ix as usize];
                let entry = model.get_mut(&path).unwrap();
                if entry.0 == new_mode {
                    continue;
                }
                ops.push(Op::Chmod {
                    path,
                    old_mode: entry.0.clone(),
                    new_mode: new_mode.to_string(),
                });
                entry.0 = new_mode.to_string();
            }
        }
    }
    ops
}

fn model_sum(model: &BTreeMap<String, (String, Vec<u8>)>) -> Sum {
    let records: Vec<ElementRecord> = model
        .iter()
        .map(|(path, (mode, content))| ElementRecord::new(mode, path, &sha3_hex(content)).unwrap())
        .collect();
    sum_of_records(records.iter())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Valid intents drive the manifest to the model's state; the realized
    /// delta replayed by pure arithmetic (apply_realized_to_sum) and by
    /// adjudicated replay (apply_realized_to_manifest) both agree.
    #[test]
    fn apply_intent_matches_model(seeds in prop::collection::vec(arb_op_seed(), 1..16)) {
        let (dir, blobs) = temp_blobs();
        let mut model = BTreeMap::new();
        let mut manifest = Manifest::new();
        let ops = realize_seeds(&seeds, &mut model, &blobs);
        prop_assume!(!ops.is_empty());

        let before_sum = manifest.sum();
        let realization = apply_intent(&Intent { ops }, &mut manifest, &blobs).unwrap();

        // The manifest reached the model's state.
        prop_assert_eq!(manifest.sum().hexdigest(), model_sum(&model).hexdigest());
        prop_assert_eq!(manifest.len(), model.len());
        for (path, (mode, content)) in &model {
            let rec = manifest.get(path).unwrap();
            prop_assert_eq!(&rec.mode, mode);
            prop_assert_eq!(&rec.blob, &sha3_hex(content));
        }

        // Pure arithmetic replay of the realized delta agrees.
        let replayed = apply_realized_to_sum(&before_sum, &realization.realized).unwrap();
        prop_assert_eq!(replayed.hexdigest(), manifest.sum().hexdigest());

        // Adjudicated replay against a fresh manifest agrees.
        let mut fresh = Manifest::new();
        apply_realized_to_manifest(&mut fresh, &realization.realized).unwrap();
        prop_assert_eq!(fresh.sum().hexdigest(), manifest.sum().hexdigest());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Validate-then-mutate: an intent that fails midway (its last op edits
    /// an absent path) writes nothing — the manifest is untouched.
    #[test]
    fn failed_intent_leaves_manifest_untouched(
        seeds in prop::collection::vec(arb_op_seed(), 1..12),
    ) {
        let (dir, blobs) = temp_blobs();
        let mut model = BTreeMap::new();
        let mut manifest = Manifest::new();

        // Establish some state first.
        let setup = realize_seeds(&seeds, &mut model, &blobs);
        if !setup.is_empty() {
            apply_intent(&Intent { ops: setup }, &mut manifest, &blobs).unwrap();
        }
        let before_bytes = manifest.to_bytes();

        // A poisoned intent: valid-looking ops ending in a guaranteed failure.
        let mut model2 = model.clone();
        let mut ops = realize_seeds(&seeds, &mut model2, &blobs);
        ops.push(Op::Edit {
            path: "/definitely/absent".to_string(),
            old_str: "x".to_string(),
            new_str: "y".to_string(),
        });
        let result = apply_intent(&Intent { ops }, &mut manifest, &blobs);
        prop_assert!(result.is_err());
        prop_assert_eq!(manifest.to_bytes(), before_bytes);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Precondition gates fire: stale delete hashes, stale chmod modes,
    /// double creates, and non-unique old_strs are all rejected.
    #[test]
    fn preconditions_reject_stale_ops(content in prop::collection::vec(any::<u8>(), 1..32)) {
        let (dir, blobs) = temp_blobs();
        let mut manifest = Manifest::new();
        let create = Op::Create {
            path: "/f".to_string(),
            mode: "100644".to_string(),
            blob: None,
            content_b64: Some(b64::encode(&content)),
        };
        apply_intent(&Intent { ops: vec![create.clone()] }, &mut manifest, &blobs).unwrap();
        let before = manifest.to_bytes();

        // Delete with the wrong blob hash.
        let stale_delete = Op::Delete { path: "/f".to_string(), blob: sha3_hex(b"not it") };
        let denied = apply_intent(&Intent { ops: vec![stale_delete] }, &mut manifest, &blobs);
        prop_assert!(denied.is_err());
        // Chmod with the wrong old mode.
        let stale_chmod = Op::Chmod {
            path: "/f".to_string(),
            old_mode: "100755".to_string(),
            new_mode: "100644".to_string(),
        };
        let denied = apply_intent(&Intent { ops: vec![stale_chmod] }, &mut manifest, &blobs);
        prop_assert!(denied.is_err());
        // Create over a present path.
        let denied = apply_intent(&Intent { ops: vec![create] }, &mut manifest, &blobs);
        prop_assert!(denied.is_err());
        // Edit whose old_str occurs zero times.
        let no_such_span = Op::Edit {
            path: "/f".to_string(),
            old_str: "\u{10FFFF}never-present\u{10FFFF}".to_string(),
            new_str: String::new(),
        };
        let denied = apply_intent(&Intent { ops: vec![no_such_span] }, &mut manifest, &blobs);
        prop_assert!(denied.is_err());
        // Nothing above touched the manifest.
        prop_assert_eq!(manifest.to_bytes(), before);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

///////////////////////////////////// manifest: parse and I9 //////////////////////////////////////
// Manifests adjudicate removes (I9) and are parsed from bytes that may lie
// about their own sum.

proptest! {
    /// to_bytes/parse round-trips, and the parse verifies the sum.
    #[test]
    fn manifest_roundtrip(recs in arb_state()) {
        let manifest = Manifest::from_records(recs.clone()).unwrap();
        let bytes = manifest.to_bytes();
        let back = Manifest::parse(&bytes).unwrap();
        prop_assert_eq!(back.sum().hexdigest(), manifest.sum().hexdigest());
        prop_assert_eq!(back.len(), recs.len());
    }

    /// A manifest whose sum line lies is corrupt, full stop.
    #[test]
    fn manifest_rejects_lying_sum(recs in arb_state().prop_filter("non-empty", |r| !r.is_empty())) {
        let manifest = Manifest::from_records(recs).unwrap();
        let text = String::from_utf8(manifest.to_bytes()).unwrap();
        // Flip one hex digit of the sum line.
        let tampered = if text.contains("sum 0") {
            text.replacen("sum 0", "sum 1", 1)
        } else {
            let ix = text.find("sum ").unwrap() + 4;
            let mut t = text.into_bytes();
            t[ix] = if t[ix] == b'0' { b'1' } else { b'0' };
            String::from_utf8(t).unwrap()
        };
        prop_assert!(Manifest::parse(tampered.as_bytes()).is_err());
    }

    /// I9: inserting a duplicate path and removing an absent or mismatched
    /// record are both adjudicated failures, and the sum is not perturbed.
    #[test]
    fn manifest_adjudicates(rec in arb_record()) {
        let mut manifest = Manifest::new();
        manifest.insert(rec.clone()).unwrap();
        let sum = manifest.sum();
        // Duplicate path.
        prop_assert!(manifest.insert(rec.clone()).is_err());
        // Remove a record that isn't the member (different blob).
        let other = ElementRecord::new(&rec.mode, &rec.path, &sha3_hex(b"other")).unwrap();
        if other != rec {
            prop_assert!(manifest.remove(&other).is_err());
        }
        // Remove an absent path.
        let absent = ElementRecord::new("100644", "/absent-adjudication", &rec.blob).unwrap();
        if manifest.get(&absent.path).is_none() {
            prop_assert!(manifest.remove(&absent).is_err());
        }
        prop_assert_eq!(manifest.sum().hexdigest(), sum.hexdigest());
    }
}

//////////////////////////////// segment: hostile bytes, the Wall /////////////////////////////////
// Segments are opened from hostile bytes: hash-before-decompress (I11),
// and the packed form must round-trip to the exact logical bytes at any
// compression level (the Wall: level is an encoding parameter, so identity
// is invariant across levels).

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// build → open → materialize returns the exact input bytes, and the
    /// image setsum is invariant across compression levels.
    #[test]
    fn segment_roundtrip(
        blobs in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..96), 1..6),
        level in 1i32..8,
    ) {
        let inputs: Vec<SegmentInput> =
            blobs.iter().cloned().map(SegmentInput::Blob).collect();
        let built = build_segment(&inputs, level).unwrap();
        let built_min = build_segment(&inputs, 1).unwrap();
        // The Wall: compression level does not perturb the logical image.
        prop_assert_eq!(
            built.image_setsum.hexdigest(),
            built_min.image_setsum.hexdigest()
        );
        let expected: Vec<ImageItem> =
            blobs.iter().map(|b| ImageItem::Blob(sha3_hex(b))).collect();
        prop_assert_eq!(
            built.image_setsum.hexdigest(),
            image_setsum(&expected).hexdigest()
        );

        let segment = Segment::open(&built.pk, &built.idx, &built.segid, &built.idx_sha3).unwrap();
        let no_blob = |h: &str| -> tally::Result<Vec<u8>> {
            Err(tally::Error::Corrupt(format!("unexpected blob fetch {h}")))
        };
        let no_line = |id: &str| -> tally::Result<tally::log::LogLine> {
            Err(tally::Error::Corrupt(format!("unexpected line fetch {id}")))
        };
        for (entry, original) in segment.entries.iter().zip(&blobs) {
            let bytes = segment.materialize(entry, &no_blob, &no_line).unwrap();
            prop_assert_eq!(&bytes, original);
        }
    }

    /// I11: a single flipped byte in .pk or .idx is rejected before any
    /// parse or decompression, as are wrong expected hashes.
    #[test]
    fn segment_open_rejects_tampering(
        blobs in prop::collection::vec(prop::collection::vec(any::<u8>(), 1..64), 1..4),
        flip in any::<prop::sample::Index>(),
    ) {
        let inputs: Vec<SegmentInput> =
            blobs.iter().cloned().map(SegmentInput::Blob).collect();
        let built = build_segment(&inputs, 3).unwrap();

        // Flip one byte of the .pk payload.
        let mut pk = built.pk.clone();
        let at = flip.index(pk.len());
        pk[at] ^= 0x01;
        prop_assert!(Segment::open(&pk, &built.idx, &built.segid, &built.idx_sha3).is_err());

        // Flip one byte of the .idx table.
        let mut idx = built.idx.clone();
        let at = flip.index(idx.len());
        idx[at] ^= 0x01;
        prop_assert!(Segment::open(&built.pk, &idx, &built.segid, &built.idx_sha3).is_err());

        // Lie about the expected hashes.
        let wrong = sha3_hex(b"wrong");
        prop_assert!(Segment::open(&built.pk, &built.idx, &wrong, &built.idx_sha3).is_err());
        prop_assert!(Segment::open(&built.pk, &built.idx, &built.segid, &wrong).is_err());
    }
}
