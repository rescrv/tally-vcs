//! Comparing two states, path by path.
//!
//! A flat path→blob map makes the difference of two states an O(paths)
//! merge, no tree walk: the shared primitive under `status` (working tree
//! vs. a ref) and `diff` (any two refs).  Because the states are setsums,
//! the group difference of their sums is itself a verifiable checksum of the
//! symmetric difference — a fingerprint of exactly what changed, computed
//! without inspecting a single byte of content.

use std::collections::BTreeSet;

use crate::ident::{ElementRecord, Sum};
use crate::manifest::Manifest;

/// One path's change between two states.  `before` absent is an addition;
/// `after` absent is a removal; both present is a modification (blob or mode).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    /// The element path.
    pub path: String,
    /// The record in the earlier state, if the path existed there.
    pub before: Option<ElementRecord>,
    /// The record in the later state, if the path exists there.
    pub after: Option<ElementRecord>,
}

impl PathChange {
    /// A single-character status code, git-style: `A` added, `D` deleted,
    /// `M` modified.
    pub fn code(&self) -> char {
        match (&self.before, &self.after) {
            (None, Some(_)) => 'A',
            (Some(_), None) => 'D',
            _ => 'M',
        }
    }
}

/// The changes taking state `before` to state `after`, one per differing
/// path, in bytewise path order.  A path present in both with an identical
/// record is not a change and does not appear.
pub fn diff_manifests(before: &Manifest, after: &Manifest) -> Vec<PathChange> {
    let mut paths: Vec<&str> = Vec::new();
    for r in before.records() {
        paths.push(&r.path);
    }
    for r in after.records() {
        paths.push(&r.path);
    }
    paths.sort_unstable();
    paths.dedup();
    let mut changes = Vec::new();
    for path in paths {
        let b = before.get(path);
        let a = after.get(path);
        if a == b {
            continue;
        }
        changes.push(PathChange {
            path: path.to_string(),
            before: b.cloned(),
            after: a.cloned(),
        });
    }
    changes
}

/// A rename, recorded as fact: the same blob left one path and arrived at
/// another.  Trees do not record renames, so git's `-M` is a similarity
/// heuristic; here a patch records remove-at-a/add-at-b with blob identity
/// intact, so the rename is stored, not inferred.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rename {
    /// The path the blob left.
    pub from: ElementRecord,
    /// The path the blob arrived at.
    pub to: ElementRecord,
}

/// Separate exact renames from the remaining path changes.  A delete and an
/// add that carry the identical blob hash are one blob moving: paired as a
/// rename and removed from the plain change list.  Pairing is one-to-one in
/// bytewise order, so the result is deterministic; unmatched adds and
/// deletes (and all modifications) stay in the change list untouched.
pub fn detect_renames(changes: &[PathChange]) -> (Vec<Rename>, Vec<PathChange>) {
    let mut adds: Vec<&PathChange> =
        changes.iter().filter(|c| c.before.is_none() && c.after.is_some()).collect();
    let mut renames = Vec::new();
    let mut consumed_adds: BTreeSet<String> = BTreeSet::new();
    let mut consumed_dels: BTreeSet<String> = BTreeSet::new();
    for change in changes {
        // Only pure deletes are rename sources.
        let (Some(from), None) = (&change.before, &change.after) else {
            continue;
        };
        if let Some(pos) = adds.iter().position(|a| {
            a.after.as_ref().map(|r| &r.blob) == Some(&from.blob)
                && !consumed_adds.contains(&a.path)
        }) {
            let add = adds.remove(pos);
            let to = add.after.clone().expect("add carries an after record");
            consumed_adds.insert(add.path.clone());
            consumed_dels.insert(change.path.clone());
            renames.push(Rename { from: from.clone(), to });
        }
    }
    let remaining: Vec<PathChange> = changes
        .iter()
        .filter(|c| !consumed_adds.contains(&c.path) && !consumed_dels.contains(&c.path))
        .cloned()
        .collect();
    (renames, remaining)
}

/// A unified diff of two byte blobs, git-style, with `context` lines of
/// context.  Non-UTF-8 content on either side yields a one-line
/// `Binary files differ` note, matching git's behaviour.
pub fn unified(old: &[u8], new: &[u8], label_a: &str, label_b: &str, context: usize) -> String {
    let (Ok(old_s), Ok(new_s)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return format!("--- {label_a}\n+++ {label_b}\nBinary files differ\n");
    };
    let old_lines: Vec<&str> = split_keep_lines(old_s);
    let new_lines: Vec<&str> = split_keep_lines(new_s);
    let edits = lcs_edits(&old_lines, &new_lines);
    let hunks = group_hunks(&edits, context);
    if hunks.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {label_a}\n+++ {label_b}\n");
    for hunk in hunks {
        out.push_str(&render_hunk(&hunk, &old_lines, &new_lines));
    }
    out
}

/// The insertions and deletions between two blobs, as line counts, for
/// `--stat`.  Modifications count as the deleted and inserted lines they
/// entail.
pub fn line_stat(old: &[u8], new: &[u8]) -> (usize, usize) {
    let (Ok(old_s), Ok(new_s)) = (std::str::from_utf8(old), std::str::from_utf8(new)) else {
        return (0, 0);
    };
    let old_lines = split_keep_lines(old_s);
    let new_lines = split_keep_lines(new_s);
    let edits = lcs_edits(&old_lines, &new_lines);
    let mut ins = 0;
    let mut del = 0;
    for edit in &edits {
        match edit {
            Edit::Insert(_) => ins += 1,
            Edit::Delete(_) => del += 1,
            Edit::Equal(_, _) => {}
        }
    }
    (ins, del)
}

/// Split into lines, keeping enough to distinguish a missing final newline.
fn split_keep_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = s.split_inclusive('\n').collect();
    // split_inclusive drops nothing; a trailing "\n" yields no empty tail,
    // which is what we want: N lines, each carrying its own terminator.
    lines.retain(|l| !l.is_empty());
    lines
}

/// One edit-script element over line indices.
enum Edit {
    /// A line present in both, at (old_index, new_index).
    Equal(usize, usize),
    /// A line only in old, at old_index.
    Delete(usize),
    /// A line only in new, at new_index.
    Insert(usize),
}

/// The classic LCS edit script: dynamic programming over line equality,
/// backtracked into equals, deletes, and inserts.
fn lcs_edits(a: &[&str], b: &[&str]) -> Vec<Edit> {
    let n = a.len();
    let m = b.len();
    // lcs[i][j] = length of the LCS of a[i..] and b[j..].
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut edits = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            edits.push(Edit::Equal(i, j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            edits.push(Edit::Delete(i));
            i += 1;
        } else {
            edits.push(Edit::Insert(j));
            j += 1;
        }
    }
    while i < n {
        edits.push(Edit::Delete(i));
        i += 1;
    }
    while j < m {
        edits.push(Edit::Insert(j));
        j += 1;
    }
    edits
}

/// A hunk: a run of edits with surrounding context, plus its old/new
/// starting line numbers (1-based) and lengths.
struct Hunk {
    old_start: usize,
    old_len: usize,
    new_start: usize,
    new_len: usize,
    edits: Vec<HunkEdit>,
}

enum HunkEdit {
    Equal(usize),
    Delete(usize),
    Insert(usize),
}

/// Group an edit script into hunks, each carrying `context` unchanged lines
/// on either side of every change, merging hunks that would overlap.
fn group_hunks(edits: &[Edit], context: usize) -> Vec<Hunk> {
    // Indices of the changed (non-Equal) edits.
    let changed: Vec<usize> = edits
        .iter()
        .enumerate()
        .filter(|(_, e)| !matches!(e, Edit::Equal(_, _)))
        .map(|(i, _)| i)
        .collect();
    if changed.is_empty() {
        return Vec::new();
    }
    // Merge changed edits whose context windows touch into ranges of the
    // edit script.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for &c in &changed {
        let lo = c.saturating_sub(context);
        let hi = (c + context).min(edits.len() - 1);
        match ranges.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => ranges.push((lo, hi)),
        }
    }
    let mut hunks = Vec::new();
    for (lo, hi) in ranges {
        let mut hunk_edits = Vec::new();
        let (mut old_start, mut new_start) = (None, None);
        let (mut old_len, mut new_len) = (0, 0);
        for edit in &edits[lo..=hi] {
            match *edit {
                Edit::Equal(oi, ni) => {
                    old_start.get_or_insert(oi);
                    new_start.get_or_insert(ni);
                    old_len += 1;
                    new_len += 1;
                    hunk_edits.push(HunkEdit::Equal(oi));
                }
                Edit::Delete(oi) => {
                    old_start.get_or_insert(oi);
                    old_len += 1;
                    hunk_edits.push(HunkEdit::Delete(oi));
                }
                Edit::Insert(ni) => {
                    new_start.get_or_insert(ni);
                    new_len += 1;
                    hunk_edits.push(HunkEdit::Insert(ni));
                }
            }
        }
        hunks.push(Hunk {
            old_start: old_start.map(|s| s + 1).unwrap_or(0),
            old_len,
            new_start: new_start.map(|s| s + 1).unwrap_or(0),
            new_len,
            edits: hunk_edits,
        });
    }
    hunks
}

/// Render one hunk in unified-diff form, including a `\ No newline at end of
/// file` marker when a line lacks its terminator.
fn render_hunk(hunk: &Hunk, old_lines: &[&str], new_lines: &[&str]) -> String {
    let mut out = format!(
        "@@ -{},{} +{},{} @@\n",
        hunk.old_start, hunk.old_len, hunk.new_start, hunk.new_len
    );
    let emit = |out: &mut String, sign: char, line: &str| {
        out.push(sign);
        if let Some(stripped) = line.strip_suffix('\n') {
            out.push_str(stripped);
            out.push('\n');
        } else {
            out.push_str(line);
            out.push('\n');
            out.push_str("\\ No newline at end of file\n");
        }
    };
    for edit in &hunk.edits {
        match *edit {
            HunkEdit::Equal(oi) => emit(&mut out, ' ', old_lines[oi]),
            HunkEdit::Delete(oi) => emit(&mut out, '-', old_lines[oi]),
            HunkEdit::Insert(ni) => emit(&mut out, '+', new_lines[ni]),
        }
    }
    out
}

/// The group difference of two states' sums: `after - before`.  This is a
/// setsum fingerprint of the symmetric difference — zero exactly when the
/// states are identical, and independent of the order changes were made.
pub fn pending_sum(before: &Sum, after: &Sum) -> Sum {
    after.clone() - before.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::sha3_hex;

    fn rec(mode: &str, path: &str, content: &[u8]) -> ElementRecord {
        ElementRecord::new(mode, path, &sha3_hex(content)).unwrap()
    }

    #[test]
    fn classifies_add_delete_modify() {
        let before = Manifest::from_records([
            rec("100644", "/keep", b"same"),
            rec("100644", "/gone", b"x"),
            rec("100644", "/edit", b"v1"),
        ])
        .unwrap();
        let after = Manifest::from_records([
            rec("100644", "/keep", b"same"),
            rec("100644", "/edit", b"v2"),
            rec("100644", "/new", b"y"),
        ])
        .unwrap();
        let changes = diff_manifests(&before, &after);
        let codes: Vec<(char, &str)> =
            changes.iter().map(|c| (c.code(), c.path.as_str())).collect();
        // Bytewise path order: /edit, /gone, /new.  /keep is unchanged.
        assert_eq!(codes, vec![('M', "/edit"), ('D', "/gone"), ('A', "/new")]);
    }

    #[test]
    fn mode_only_change_is_a_modification() {
        let before = Manifest::from_records([rec("100644", "/t", b"x")]).unwrap();
        let after = Manifest::from_records([rec("100755", "/t", b"x")]).unwrap();
        let changes = diff_manifests(&before, &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].code(), 'M');
    }

    #[test]
    fn renames_are_fact_not_heuristic() {
        // The same blob leaves /old and arrives at /new: a rename, paired
        // by identical blob hash, not by similarity.
        let blob = sha3_hex(b"the content that moved\n");
        let old_rec = ElementRecord::new("100644", "/old", &blob).unwrap();
        let new_rec = ElementRecord::new("100644", "/new", &blob).unwrap();
        let changes = vec![
            PathChange { path: "/new".into(), before: None, after: Some(new_rec.clone()) },
            PathChange { path: "/old".into(), before: Some(old_rec.clone()), after: None },
        ];
        let (renames, remaining) = detect_renames(&changes);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].from.path, "/old");
        assert_eq!(renames[0].to.path, "/new");
        assert!(remaining.is_empty(), "the pair is consumed as a rename");
    }

    #[test]
    fn unrelated_add_and_delete_are_not_a_rename() {
        let a = rec("100644", "/a", b"alpha");
        let b = rec("100644", "/b", b"beta");
        let changes = vec![
            PathChange { path: "/a".into(), before: Some(a), after: None },
            PathChange { path: "/b".into(), before: None, after: Some(b) },
        ];
        let (renames, remaining) = detect_renames(&changes);
        assert!(renames.is_empty());
        assert_eq!(remaining.len(), 2);
    }

    #[test]
    fn unified_diff_shows_hunks() {
        let old = b"one\ntwo\nthree\n";
        let new = b"one\nTWO\nthree\n";
        let text = unified(old, new, "a/f", "b/f", 3);
        assert!(text.contains("--- a/f"));
        assert!(text.contains("+++ b/f"));
        assert!(text.contains("-two"));
        assert!(text.contains("+TWO"));
        assert!(text.contains(" one"), "context is shown");
    }

    #[test]
    fn unified_diff_marks_missing_final_newline() {
        let old = b"a\n";
        let new = b"a";
        let text = unified(old, new, "a/f", "b/f", 3);
        assert!(text.contains("\\ No newline at end of file"));
    }

    #[test]
    fn binary_content_is_reported_not_diffed() {
        let text = unified(&[0, 159, 146], b"still binary\xff", "a/f", "b/f", 3);
        assert!(text.contains("Binary files differ"));
    }

    #[test]
    fn line_stat_counts_insertions_and_deletions() {
        let old = b"a\nb\nc\n";
        let new = b"a\nc\nd\n";
        let (ins, del) = line_stat(old, new);
        assert_eq!((ins, del), (1, 1), "b deleted, d inserted; c unchanged");
    }

    #[test]
    fn pending_sum_is_zero_iff_identical() {
        let a = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        let b = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        assert_eq!(pending_sum(&a.sum(), &b.sum()), Sum::zero());
        let c = Manifest::from_records([rec("100644", "/a", b"different")]).unwrap();
        assert_ne!(pending_sum(&a.sum(), &c.sum()), Sum::zero());
    }
}
