//! Blame: provenance by lookup, not reconstruction.
//!
//! Git's blame walks history backward with rename heuristics — O(history)
//! and approximate — because a tree records only snapshots, so who touched a
//! line must be inferred.  Here the span is *declared* at write time: an edit
//! op carries `old_str`/`new_str`, the exact bytes consumed and produced.
//! Attribution is therefore a forward replay that stamps each produced byte
//! with the line that produced it — provenance stored, not inferred.  It is
//! this stored provenance that makes `Co-authored-by: Billy, age 8` a fact
//! rather than a courtesy.

use crate::blobs::BlobStore;
use crate::log::LogLine;
use crate::patch::Op;
use crate::{Error, Result, b64};

/// One line of a file with the log line that last produced any of its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlamedLine {
    /// The id of the log line that owns this text; empty if unattributed
    /// (which a well-formed history never leaves).
    pub owner: String,
    /// The line's text, its terminating newline stripped.
    pub text: String,
}

/// Blame `path` across `history` (oldest first).  Each produced byte is
/// stamped with the index of the log line that produced it; a rendered line
/// is attributed to the newest log line among its bytes — "who last touched
/// this line."  Blob content for `create` ops is read from the pool by the
/// hash the realization recorded, so attribution is grounded in what the
/// application actually stored, not in the intent alone.
pub fn blame_path(path: &str, history: &[&LogLine], blobs: &BlobStore) -> Result<Vec<BlamedLine>> {
    // Byte-parallel ownership: owners[i] is the index into `history` of the
    // line that produced content[i].
    let mut content: Vec<u8> = Vec::new();
    let mut owners: Vec<usize> = Vec::new();
    let mut exists = false;
    for (idx, line) in history.iter().enumerate() {
        for op in &line.intent.ops {
            if op.path() != path {
                continue;
            }
            match op {
                Op::Create {
                    blob, content_b64, ..
                } => {
                    let bytes = match (blob, content_b64) {
                        (Some(hash), _) => blobs.get(hash)?,
                        (None, Some(b64_content)) => b64::decode(b64_content)?,
                        (None, None) => {
                            return Err(Error::Corrupt(format!(
                                "create of {path} names neither blob nor content"
                            )));
                        }
                    };
                    owners = vec![idx; bytes.len()];
                    content = bytes;
                    exists = true;
                }
                Op::Edit {
                    old_str, new_str, ..
                } => {
                    let old = old_str.as_bytes();
                    let new = new_str.as_bytes();
                    let at = find_unique(&content, old).ok_or_else(|| {
                        Error::Corrupt(format!(
                            "blame replay of {path}: line {} edits a span that is not \
                             uniquely present",
                            line.id
                        ))
                    })?;
                    let mut next = Vec::with_capacity(content.len() - old.len() + new.len());
                    let mut next_owners = Vec::with_capacity(owners.len());
                    next.extend_from_slice(&content[..at]);
                    next_owners.extend_from_slice(&owners[..at]);
                    next.extend_from_slice(new);
                    next_owners.extend(std::iter::repeat_n(idx, new.len()));
                    next.extend_from_slice(&content[at + old.len()..]);
                    next_owners.extend_from_slice(&owners[at + old.len()..]);
                    content = next;
                    owners = next_owners;
                }
                Op::Delete { .. } => {
                    content.clear();
                    owners.clear();
                    exists = false;
                }
                Op::Chmod { .. } => {}
            }
        }
    }
    if !exists {
        return Err(Error::Invalid(format!(
            "path {path} does not exist at this state"
        )));
    }
    Ok(roll_up_lines(&content, &owners, history))
}

/// Find the unique occurrence of `needle` in `haystack`, returning its start
/// offset only when it occurs exactly once (mirroring the edit precondition).
fn find_unique(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        // An empty or oversize needle cannot be a unique span.
        return None;
    }
    let mut found = None;
    for i in 0..=haystack.len() - needle.len() {
        if &haystack[i..i + needle.len()] == needle {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

/// Roll byte ownership up to lines: split on `\n`, and attribute each line to
/// the newest (greatest-index) owner among its bytes, including its
/// terminator.  A final line without a trailing newline is still a line.
fn roll_up_lines(content: &[u8], owners: &[usize], history: &[&LogLine]) -> Vec<BlamedLine> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let emit = |out: &mut Vec<BlamedLine>, range: std::ops::Range<usize>| {
        if range.is_empty() {
            return;
        }
        let newest = owners[range.clone()].iter().copied().max();
        let owner = newest.map(|n| history[n].id.clone()).unwrap_or_default();
        let text_end = if content[range.end - 1] == b'\n' {
            range.end - 1
        } else {
            range.end
        };
        let text = String::from_utf8_lossy(&content[range.start..text_end]).into_owned();
        out.push(BlamedLine { owner, text });
    };
    while i < content.len() {
        if content[i] == b'\n' {
            emit(&mut out, start..i + 1);
            start = i + 1;
        }
        i += 1;
    }
    if start < content.len() {
        emit(&mut out, start..content.len());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Annotation;
    use crate::patch::{Intent, Op};

    fn blobs(name: &str) -> BlobStore {
        let dir = std::env::temp_dir().join(format!("tally-blame-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        BlobStore::init(dir).unwrap()
    }

    fn line(id: &str, author: &str, ops: Vec<Op>) -> LogLine {
        LogLine {
            id: id.to_string(),
            prev: String::new(),
            intent: Intent { ops },
            realized: vec![],
            sum_after: "0".repeat(64),
            committed_ms: 0,
            annotation: Annotation {
                author: author.to_string(),
                ..Annotation::default()
            },
        }
    }

    #[test]
    fn create_then_edit_attributes_by_span() {
        let store = blobs("span");
        let create = line(
            "aaa",
            "alice",
            vec![Op::Create {
                path: "/f".into(),
                mode: "100644".into(),
                blob: None,
                content_b64: Some(b64::encode(b"one\ntwo\nthree\n")),
            }],
        );
        let edit = line(
            "bbb",
            "billy",
            vec![Op::Edit {
                path: "/f".into(),
                old_str: "two".into(),
                new_str: "TWO".into(),
            }],
        );
        let history = [&create, &edit];
        let blamed = blame_path("/f", &history, &store).unwrap();
        assert_eq!(blamed.len(), 3);
        // The edited line belongs to billy; the untouched lines to alice.
        assert_eq!(
            blamed[0],
            BlamedLine {
                owner: "aaa".into(),
                text: "one".into()
            }
        );
        assert_eq!(
            blamed[1],
            BlamedLine {
                owner: "bbb".into(),
                text: "TWO".into()
            }
        );
        assert_eq!(
            blamed[2],
            BlamedLine {
                owner: "aaa".into(),
                text: "three".into()
            }
        );
    }

    #[test]
    fn later_edit_wins_on_a_shared_line() {
        let store = blobs("shared");
        let create = line(
            "c1",
            "a",
            vec![Op::Create {
                path: "/f".into(),
                mode: "100644".into(),
                blob: None,
                content_b64: Some(b64::encode(b"alpha beta\n")),
            }],
        );
        let e1 = line(
            "c2",
            "b",
            vec![Op::Edit {
                path: "/f".into(),
                old_str: "alpha".into(),
                new_str: "ALPHA".into(),
            }],
        );
        let e2 = line(
            "c3",
            "c",
            vec![Op::Edit {
                path: "/f".into(),
                old_str: "beta".into(),
                new_str: "BETA".into(),
            }],
        );
        let history = [&create, &e1, &e2];
        let blamed = blame_path("/f", &history, &store).unwrap();
        // Both edits touched the one line; the newest owner (c3) wins.
        assert_eq!(blamed.len(), 1);
        assert_eq!(blamed[0].owner, "c3");
        assert_eq!(blamed[0].text, "ALPHA BETA");
    }

    #[test]
    fn deleted_path_has_no_blame() {
        let store = blobs("deleted");
        let create = line(
            "c1",
            "a",
            vec![Op::Create {
                path: "/f".into(),
                mode: "100644".into(),
                blob: None,
                content_b64: Some(b64::encode(b"x\n")),
            }],
        );
        let del = line(
            "c2",
            "a",
            vec![Op::Delete {
                path: "/f".into(),
                blob: "0".repeat(64),
            }],
        );
        let history = [&create, &del];
        assert!(blame_path("/f", &history, &store).is_err());
    }

    #[test]
    fn unknown_path_is_an_error() {
        let store = blobs("unknown");
        let create = line(
            "c1",
            "a",
            vec![Op::Create {
                path: "/f".into(),
                mode: "100644".into(),
                blob: None,
                content_b64: Some(b64::encode(b"x\n")),
            }],
        );
        assert!(blame_path("/other", &[&create], &store).is_err());
    }
}
