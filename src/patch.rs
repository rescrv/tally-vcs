//! §4 Patches: intent form and applied form, and the distinction matters.
//!
//! Intent is span operations with content-addressed, position-independent
//! preconditions; it commutes and travels.  Realization is the concrete
//! element delta an application produced against the state it met; it is a
//! fact about one application.  The log records both, so the sum can be
//! replayed and inverted by pure arithmetic without re-running span logic.

use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
use crate::ident::{ElementRecord, Sum, sha3_hex, validate_mode, validate_path};
use crate::manifest::Manifest;
use crate::{Error, Result, b64};

/////////////////////////////////////////////// ops ///////////////////////////////////////////////

/// One span operation.  Serialized externally tagged, matching the ANDON:
/// `{"edit": {...}}`, `{"create": {...}}`, `{"delete": {...}}`,
/// `{"chmod": {...}}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    /// Replace `old_str` with `new_str` in the blob at `path`.  The
    /// precondition is that `old_str` occurs in the current blob exactly
    /// once; widening `old_str` with context until unique makes the span
    /// content-addressed within the file and position-independent.
    Edit {
        /// The element to edit.
        path: String,
        /// The span consumed; must occur exactly once.
        old_str: String,
        /// The replacement.
        new_str: String,
    },
    /// Create a new element.  The precondition is the path's absence.
    Create {
        /// The path to create.
        path: String,
        /// The new element's mode.
        mode: String,
        /// Content by blob-store reference (SPEC §2.5).  A portable patch
        /// bundle is the intent JSON plus its referenced blobs.
        #[serde(skip_serializing_if = "Option::is_none")]
        blob: Option<String>,
        /// Content inline, base64 (ANDON §4); the Andon path needs no blob
        /// store to be populated in advance.
        #[serde(skip_serializing_if = "Option::is_none")]
        content_b64: Option<String>,
    },
    /// Delete an element.  The precondition is the full blob hash — you
    /// consume the whole element.
    Delete {
        /// The path to delete.
        path: String,
        /// The blob hash the element must carry.
        blob: String,
    },
    /// Change an element's mode.
    Chmod {
        /// The element to chmod.
        path: String,
        /// The mode the element must carry.
        old_mode: String,
        /// The mode to set.
        new_mode: String,
    },
}

impl Op {
    /// The path this op writes.
    pub fn path(&self) -> &str {
        match self {
            Op::Edit { path, .. }
            | Op::Create { path, .. }
            | Op::Delete { path, .. }
            | Op::Chmod { path, .. } => path,
        }
    }
}

/// Intent form: what an author writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Intent {
    /// The span operations, applied in sequence.
    pub ops: Vec<Op>,
}

///////////////////////////////////////////// realized ////////////////////////////////////////////

/// One realized delta entry: the concrete element records removed and added.
/// Either side may be null (create, delete).  Records are stored sans
/// trailing LF, as JSON strings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RealizedEntry {
    /// The element record consumed, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remove: Option<String>,
    /// The element record produced, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add: Option<String>,
}

impl RealizedEntry {
    /// Parse the removed record, if present.
    pub fn removed(&self) -> Result<Option<ElementRecord>> {
        self.remove.as_deref().map(ElementRecord::parse).transpose()
    }

    /// Parse the added record, if present.
    pub fn added(&self) -> Result<Option<ElementRecord>> {
        self.add.as_deref().map(ElementRecord::parse).transpose()
    }
}

/// The product of applying an intent: realized deltas plus the new blobs the
/// application produced.  Blobs must be durable before any log line
/// referencing them is appended (I8 write-ahead ordering), so they travel
/// with the realization until the caller commits them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Realization {
    /// The concrete element deltas, in op order.
    pub realized: Vec<RealizedEntry>,
    /// New blob contents produced by the application, keyed by hash.
    pub new_blobs: Vec<(String, Vec<u8>)>,
}

/// Fold a realized delta into a sum: new = old + Σ neg(removes) + Σ adds.
/// This is pure arithmetic; adjudication (I9) happens in
/// [`apply_realized_to_manifest`], never here.
pub fn apply_realized_to_sum(sum: &Sum, realized: &[RealizedEntry]) -> Result<Sum> {
    let mut sum = sum.clone();
    for entry in realized {
        if let Some(removed) = entry.removed()? {
            sum.remove(&removed.to_bytes());
        }
        if let Some(added) = entry.added()? {
            sum.insert(&added.to_bytes());
        }
    }
    Ok(sum)
}

/// Apply a realized delta to a manifest.  Every remove is membership-checked
/// here — the placeholder-debt rule is enforced at application, always (I9).
pub fn apply_realized_to_manifest(manifest: &mut Manifest, realized: &[RealizedEntry]) -> Result<()> {
    for entry in realized {
        if let Some(removed) = entry.removed()? {
            manifest.remove(&removed)?;
        }
        if let Some(added) = entry.added()? {
            manifest.insert(added)?;
        }
    }
    Ok(())
}

////////////////////////////////////////////// apply //////////////////////////////////////////////

/// Count the starting positions at which `needle` occurs in `haystack`,
/// overlapping occurrences included.  Uniqueness means exactly one.
pub fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    (0..=haystack.len() - needle.len())
        .filter(|&i| &haystack[i..i + needle.len()] == needle)
        .count()
}

/// Replace the unique occurrence of `old` in `content` with `new` — the one
/// span operation, shared by intent application and construct replay.  When
/// the occurrence count is not exactly one, `Err` carries the count so the
/// caller can speak its own error vocabulary: a precondition failure at
/// apply time, corruption at replay time.
pub fn replace_unique(
    content: &[u8],
    old: &[u8],
    new: &[u8],
) -> std::result::Result<Vec<u8>, usize> {
    let n = count_occurrences(content, old);
    if n != 1 {
        return Err(n);
    }
    let at = content
        .windows(old.len())
        .position(|w| w == old)
        .expect("counted exactly one occurrence");
    let mut out = Vec::with_capacity(content.len() - old.len() + new.len());
    out.extend_from_slice(&content[..at]);
    out.extend_from_slice(new);
    out.extend_from_slice(&content[at + old.len()..]);
    Ok(out)
}

/// Validate and apply an intent against a manifest and blob store.  On any
/// failure, nothing is written anywhere (§2.7 step 2: any failure → write
/// nothing).  On success the manifest reflects the new state and the
/// returned realization carries the deltas and new blob contents.
pub fn apply_intent(
    intent: &Intent,
    manifest: &mut Manifest,
    blobs: &BlobStore,
) -> Result<Realization> {
    // Validate-then-mutate: work on a scratch manifest so a mid-intent
    // failure leaves the caller's state untouched.
    let mut scratch = manifest.clone();
    let mut realization = Realization::default();
    let read_blob = |hash: &str, realization: &Realization| -> Result<Vec<u8>> {
        for (new_hash, content) in &realization.new_blobs {
            if new_hash == hash {
                return Ok(content.clone());
            }
        }
        blobs.get(hash)
    };
    for op in &intent.ops {
        validate_path(op.path())?;
        match op {
            Op::Edit { path, old_str, new_str } => {
                let element = scratch.get(path).cloned().ok_or_else(|| {
                    Error::Precondition(format!("edit of absent path: {path}"))
                })?;
                let content = read_blob(&element.blob, &realization)?;
                let new_content =
                    replace_unique(&content, old_str.as_bytes(), new_str.as_bytes())
                        .map_err(|n| {
                            Error::Precondition(format!(
                                "old_str occurs {n} times in {path}; exactly one required"
                            ))
                        })?;
                let new_hash = sha3_hex(&new_content);
                let new_record = ElementRecord::new(&element.mode, path, &new_hash)?;
                scratch.remove(&element)?;
                scratch.insert(new_record.clone())?;
                realization.new_blobs.push((new_hash, new_content));
                realization.realized.push(RealizedEntry {
                    remove: Some(element.to_line()),
                    add: Some(new_record.to_line()),
                });
            }
            Op::Create { path, mode, blob, content_b64 } => {
                validate_mode(mode)?;
                if scratch.get(path).is_some() {
                    return Err(Error::Precondition(format!("create of present path: {path}")));
                }
                let hash = match (blob, content_b64) {
                    (Some(hash), None) => {
                        let known_new =
                            realization.new_blobs.iter().any(|(h, _)| h == hash);
                        if !known_new && !blobs.has(hash)? {
                            return Err(Error::Precondition(format!(
                                "create references absent blob {hash}"
                            )));
                        }
                        hash.clone()
                    }
                    (None, Some(b64_content)) => {
                        let content = b64::decode(b64_content)?;
                        let hash = sha3_hex(&content);
                        realization.new_blobs.push((hash.clone(), content));
                        hash
                    }
                    _ => {
                        return Err(Error::Invalid(
                            "create carries exactly one of blob, content_b64".to_string(),
                        ));
                    }
                };
                let record = ElementRecord::new(mode, path, &hash)?;
                scratch.insert(record.clone())?;
                realization
                    .realized
                    .push(RealizedEntry { remove: None, add: Some(record.to_line()) });
            }
            Op::Delete { path, blob } => {
                let element = scratch.get(path).cloned().ok_or_else(|| {
                    Error::Precondition(format!("delete of absent path: {path}"))
                })?;
                if &element.blob != blob {
                    return Err(Error::Precondition(format!(
                        "delete of {path}: blob {blob} does not match element {}",
                        element.blob
                    )));
                }
                scratch.remove(&element)?;
                realization
                    .realized
                    .push(RealizedEntry { remove: Some(element.to_line()), add: None });
            }
            Op::Chmod { path, old_mode, new_mode } => {
                validate_mode(new_mode)?;
                let element = scratch.get(path).cloned().ok_or_else(|| {
                    Error::Precondition(format!("chmod of absent path: {path}"))
                })?;
                if &element.mode != old_mode {
                    return Err(Error::Precondition(format!(
                        "chmod of {path}: mode {old_mode} does not match element {}",
                        element.mode
                    )));
                }
                let new_record = ElementRecord::new(new_mode, path, &element.blob)?;
                scratch.remove(&element)?;
                scratch.insert(new_record.clone())?;
                realization.realized.push(RealizedEntry {
                    remove: Some(element.to_line()),
                    add: Some(new_record.to_line()),
                });
            }
        }
    }
    *manifest = scratch;
    Ok(realization)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> BlobStore {
        let dir =
            std::env::temp_dir().join(format!("tally-patch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        BlobStore::init(dir).unwrap()
    }

    fn seeded(name: &str, content: &[u8]) -> (Manifest, BlobStore, ElementRecord) {
        let blobs = store(name);
        let hash = blobs.put(content).unwrap();
        let record = ElementRecord::new("100644", "/src/main.rs", &hash).unwrap();
        let manifest = Manifest::from_records([record.clone()]).unwrap();
        (manifest, blobs, record)
    }

    #[test]
    fn edit_applies_and_realizes() {
        let (mut manifest, blobs, old) =
            seeded("edit", b"fn main() { println!(\"hello\"); }\n");
        let intent = Intent {
            ops: vec![Op::Edit {
                path: "/src/main.rs".to_string(),
                old_str: "hello".to_string(),
                new_str: "hello, tally".to_string(),
            }],
        };
        let mut sum = manifest.sum();
        let realization = apply_intent(&intent, &mut manifest, &blobs).unwrap();
        assert_eq!(realization.realized.len(), 1);
        assert_eq!(realization.realized[0].remove, Some(old.to_line()));
        sum = apply_realized_to_sum(&sum, &realization.realized).unwrap();
        assert_eq!(sum, manifest.sum(), "arithmetic must agree with the manifest");
        let (hash, content) = &realization.new_blobs[0];
        assert_eq!(*hash, sha3_hex(content));
        assert_eq!(content, b"fn main() { println!(\"hello, tally\"); }\n");
    }

    #[test]
    fn edit_requires_unique_span() {
        let (mut manifest, blobs, _) = seeded("unique", b"aaa bbb aaa\n");
        let mut hit = |old: &str| {
            apply_intent(
                &Intent {
                    ops: vec![Op::Edit {
                        path: "/src/main.rs".to_string(),
                        old_str: old.to_string(),
                        new_str: "x".to_string(),
                    }],
                },
                &mut manifest,
                &blobs,
            )
        };
        assert!(matches!(hit("aaa"), Err(Error::Precondition(_))), "two matches");
        assert!(matches!(hit("zzz"), Err(Error::Precondition(_))), "zero matches");
        assert!(hit("bbb").is_ok(), "one match");
    }

    #[test]
    fn failure_writes_nothing() {
        let (mut manifest, blobs, _) = seeded("atomic", b"one two\n");
        let before = manifest.clone();
        let intent = Intent {
            ops: vec![
                Op::Edit {
                    path: "/src/main.rs".to_string(),
                    old_str: "one".to_string(),
                    new_str: "1".to_string(),
                },
                Op::Edit {
                    path: "/src/main.rs".to_string(),
                    old_str: "absent".to_string(),
                    new_str: "x".to_string(),
                },
            ],
        };
        assert!(apply_intent(&intent, &mut manifest, &blobs).is_err());
        assert_eq!(manifest, before, "mid-intent failure must leave state untouched");
    }

    #[test]
    fn create_delete_chmod() {
        let blobs = store("cdc");
        let mut manifest = Manifest::new();
        let content = b"#!/bin/sh\necho hi\n";
        let intent = Intent {
            ops: vec![Op::Create {
                path: "/tools/apply".to_string(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(b64::encode(content)),
            }],
        };
        let r = apply_intent(&intent, &mut manifest, &blobs).unwrap();
        let hash = sha3_hex(content);
        assert_eq!(r.new_blobs[0].0, hash);
        assert!(manifest.get("/tools/apply").is_some());

        // Creating again violates the absence precondition.
        assert!(apply_intent(&intent, &mut manifest.clone(), &blobs).is_err());

        let chmod = Intent {
            ops: vec![Op::Chmod {
                path: "/tools/apply".to_string(),
                old_mode: "100644".to_string(),
                new_mode: "100755".to_string(),
            }],
        };
        apply_intent(&chmod, &mut manifest, &blobs).unwrap();
        assert_eq!(manifest.get("/tools/apply").unwrap().mode, "100755");

        // Delete consumes the whole element: wrong blob hash refuses.
        let bad = Intent {
            ops: vec![Op::Delete {
                path: "/tools/apply".to_string(),
                blob: "0".repeat(64),
            }],
        };
        assert!(apply_intent(&bad, &mut manifest, &blobs).is_err());
        let good = Intent {
            ops: vec![Op::Delete { path: "/tools/apply".to_string(), blob: hash }],
        };
        apply_intent(&good, &mut manifest, &blobs).unwrap();
        assert!(manifest.is_empty());
    }

    #[test]
    fn intent_json_matches_readme_shape() {
        let json = r#"{"ops": [
            {"edit": {"path": "/src/main.rs", "old_str": "a", "new_str": "b"}},
            {"delete": {"path": "/old.rs", "blob": "ab12"}},
            {"chmod": {"path": "/tools/apply", "old_mode": "100644", "new_mode": "100755"}}
        ]}"#;
        let intent: Intent = serde_json::from_str(json).unwrap();
        assert_eq!(intent.ops.len(), 3);
        let round = serde_json::to_string(&intent).unwrap();
        let again: Intent = serde_json::from_str(&round).unwrap();
        assert_eq!(intent, again);
    }

    #[test]
    fn realized_arithmetic_inverts() {
        let (mut manifest, blobs, _) = seeded("invert", b"before\n");
        let sum_before = manifest.sum();
        let intent = Intent {
            ops: vec![Op::Edit {
                path: "/src/main.rs".to_string(),
                old_str: "before".to_string(),
                new_str: "after".to_string(),
            }],
        };
        let r = apply_intent(&intent, &mut manifest, &blobs).unwrap();
        let sum_after = apply_realized_to_sum(&sum_before, &r.realized).unwrap();
        // Undo is the inverse: swap removes and adds.
        let inverse: Vec<RealizedEntry> = r
            .realized
            .iter()
            .map(|e| RealizedEntry { remove: e.add.clone(), add: e.remove.clone() })
            .collect();
        assert_eq!(apply_realized_to_sum(&sum_after, &inverse).unwrap(), sum_before);
    }
}
