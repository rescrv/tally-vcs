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
use crate::patch::{Intent, Op};

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

/// The 0-based indices of `old`'s lines that a change to `new` touched: every
/// line the edit script *deletes*, which includes lines that were modified
/// (a modification is a delete of the old line plus an insert of the new).
/// Pure insertions of brand-new lines touch no existing old line and are
/// excluded — a read of an untouched line is not disturbed by an insertion
/// elsewhere in the file.  Returns `None` when either side is not UTF-8, so
/// the caller can fall back to a conservative whole-file conflict rather than
/// silently under-reporting on binary blobs.
pub fn changed_old_lines(old: &[u8], new: &[u8]) -> Option<BTreeSet<usize>> {
    let (old_s, new_s) = (std::str::from_utf8(old).ok()?, std::str::from_utf8(new).ok()?);
    let old_lines = split_keep_lines(old_s);
    let new_lines = split_keep_lines(new_s);
    let mut changed = BTreeSet::new();
    for edit in lcs_edits(&old_lines, &new_lines) {
        if let Edit::Delete(i) = edit {
            changed.insert(i);
        }
    }
    Some(changed)
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

//////////////////////////////////////// intent synthesis /////////////////////////////////////////

/// Synthesize the intent form of a path-level change set (`abelian commit`).
///
/// The realized delta of a commit is mechanical; the intent is what lets the
/// line travel — union stratum 3 replays intent, and re-enactment reads it.
/// So a commit synthesizes real span preconditions rather than recording an
/// empty intent that could only ever land arithmetically:
///
/// - addition → `create` by blob reference (the scan already pooled it)
/// - removal → `delete` consuming the whole element
/// - mode-only change → `chmod`
/// - content change → `edit` whose `old_str` is the changed span widened
///   with context until it occurs exactly once (the same content-addressing
///   discipline agents use), falling back to `delete` + `create` when the
///   content is not UTF-8 or the element is a symlink
///
/// `read` resolves a blob hash to its bytes.  Op order follows change order;
/// within a path, `delete` precedes `create`, so the intent replays cleanly
/// against the pre-state.
pub fn synthesize_intent(
    changes: &[PathChange],
    mut read: impl FnMut(&str) -> crate::Result<Vec<u8>>,
) -> crate::Result<Intent> {
    let create = |r: &ElementRecord| Op::Create {
        path: r.path.clone(),
        mode: r.mode.clone(),
        blob: Some(r.blob.clone()),
        content_b64: None,
    };
    let delete = |r: &ElementRecord| Op::Delete { path: r.path.clone(), blob: r.blob.clone() };
    let mut ops = Vec::new();
    for change in changes {
        match (&change.before, &change.after) {
            (None, Some(after)) => ops.push(create(after)),
            (Some(before), None) => ops.push(delete(before)),
            (Some(before), Some(after)) => {
                if before.blob == after.blob {
                    // Mode-only: the content did not move.
                    ops.push(Op::Chmod {
                        path: change.path.clone(),
                        old_mode: before.mode.clone(),
                        new_mode: after.mode.clone(),
                    });
                    continue;
                }
                // Content changed.  A span edit only when the mode held and
                // the content is text; otherwise consume and recreate.
                let edit = if before.mode == after.mode && before.mode != "120000" {
                    let old = read(&before.blob)?;
                    let new = read(&after.blob)?;
                    synthesize_edit(&old, &new)
                } else {
                    None
                };
                match edit {
                    Some((old_str, new_str)) => ops.push(Op::Edit {
                        path: change.path.clone(),
                        old_str,
                        new_str,
                    }),
                    None => {
                        ops.push(delete(before));
                        ops.push(create(after));
                    }
                }
            }
            (None, None) => {
                return Err(crate::Error::Invalid(format!(
                    "change at {} has neither side",
                    change.path
                )));
            }
        }
    }
    Ok(Intent { ops })
}

/// The span edit taking `old` to `new`: the changed region, widened with
/// context until `old_str` occurs in `old` exactly once — content-addressed
/// within the file, position-independent (ANDON §4).  `None` when either
/// side is not UTF-8 or `old` is empty (an empty `old_str` never matches;
/// the caller falls back to `delete` + `create`).
pub fn synthesize_edit(old: &[u8], new: &[u8]) -> Option<(String, String)> {
    let old_s = std::str::from_utf8(old).ok()?;
    let new_s = std::str::from_utf8(new).ok()?;
    if old_s.is_empty() {
        return None;
    }
    // The changed region: strip the common prefix and suffix, on char
    // boundaries, never letting the two overlap.
    let mut p = old
        .iter()
        .zip(new.iter())
        .take_while(|(a, b)| a == b)
        .count();
    while !old_s.is_char_boundary(p) {
        p -= 1;
    }
    let max_s = old.len().min(new.len()) - p;
    let mut s = old[p..]
        .iter()
        .rev()
        .zip(new[p..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
        .min(max_s);
    // The suffix bytes are identical in both strings, so one boundary check
    // covers both sides.
    while s > 0 && !old_s.is_char_boundary(old.len() - s) {
        s -= 1;
    }
    // Widen until unique.  The whole file occurs exactly once, so this
    // terminates.
    loop {
        let old_span = &old_s[p..old.len() - s];
        if !old_span.is_empty()
            && crate::patch::count_occurrences(old, old_span.as_bytes()) == 1
        {
            return Some((old_span.to_string(), new_s[p..new.len() - s].to_string()));
        }
        debug_assert!(p > 0 || s > 0, "the whole file occurs exactly once");
        if p > 0 {
            p -= 1;
            while !old_s.is_char_boundary(p) {
                p -= 1;
            }
        }
        if s > 0 {
            s -= 1;
            while s > 0 && !old_s.is_char_boundary(old.len() - s) {
                s -= 1;
            }
        }
    }
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
    fn changed_old_lines_marks_modifications_not_pure_insertions() {
        // Modify line 0, insert a new line after: only the modified old line
        // is reported.  The unchanged old line 1 is not, so a read of it
        // survives the insertion.
        let old = b"MAX=10\nMIN=1\n";
        let new = b"MAX=1000\nEXTRA=9\nMIN=1\n";
        let changed = changed_old_lines(old, new).unwrap();
        assert!(changed.contains(&0), "the MAX line was modified");
        assert!(!changed.contains(&1), "the MIN line is untouched by the edit and the insert");
    }

    #[test]
    fn changed_old_lines_is_none_on_non_utf8() {
        assert!(changed_old_lines(&[0xff, 0xfe], b"ok\n").is_none());
    }

    #[test]
    fn pending_sum_is_zero_iff_identical() {
        let a = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        let b = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        assert_eq!(pending_sum(&a.sum(), &b.sum()), Sum::zero());
        let c = Manifest::from_records([rec("100644", "/a", b"different")]).unwrap();
        assert_ne!(pending_sum(&a.sum(), &c.sum()), Sum::zero());
    }

    #[test]
    fn synthesize_edit_takes_the_minimal_unique_span() {
        let old = b"fn main() {\n    println!(\"hello\");\n}\n";
        let new = b"fn main() {\n    println!(\"hello, abelian\");\n}\n";
        let (old_str, new_str) = synthesize_edit(old, new).unwrap();
        assert_eq!(
            crate::patch::count_occurrences(old, old_str.as_bytes()),
            1,
            "the precondition must hold"
        );
        assert_eq!(
            crate::patch::replace_unique(old, old_str.as_bytes(), new_str.as_bytes()).unwrap(),
            new.to_vec(),
            "applying the edit must reproduce the new content"
        );
        assert!(old_str.len() < old.len(), "a small change must not widen to the whole file");
    }

    #[test]
    fn synthesize_edit_widens_an_ambiguous_span_until_unique() {
        // Insert a line at the second of two identical sites: the naive
        // changed region occurs twice; widening with context disambiguates.
        let old = b"a\nx\nb\na\nx\nb\n";
        let new = b"a\nx\nb\na\nx\ny\nb\n";
        let (old_str, new_str) = synthesize_edit(old, new).unwrap();
        assert_eq!(crate::patch::count_occurrences(old, old_str.as_bytes()), 1);
        assert_eq!(
            crate::patch::replace_unique(old, old_str.as_bytes(), new_str.as_bytes()).unwrap(),
            new.to_vec()
        );
    }

    #[test]
    fn synthesize_edit_declines_binary_and_empty() {
        assert_eq!(synthesize_edit(&[0xff, 0xfe], b"text"), None, "not UTF-8");
        assert_eq!(synthesize_edit(b"", b"text"), None, "empty old never matches");
    }

    #[test]
    fn synthesize_edit_respects_multibyte_boundaries() {
        let old = "héllo héllo\n".as_bytes();
        let new = "héllo hállo\n".as_bytes();
        let (old_str, new_str) = synthesize_edit(old, new).unwrap();
        assert_eq!(
            crate::patch::replace_unique(old, old_str.as_bytes(), new_str.as_bytes()).unwrap(),
            new.to_vec()
        );
    }

    #[test]
    fn synthesized_intent_replays_against_the_pre_state() {
        // The point of synthesis: apply_intent against the before-state must
        // land exactly on the after-state (union stratum 3 viability).
        use std::collections::HashMap;
        let contents: Vec<(&str, &[u8])> = vec![
            ("/edit", b"line one\nline two\n"),
            ("/edit2", b"line one\nline three\n"),
            ("/gone", b"doomed\n"),
            ("/mode", b"tool\n"),
        ];
        let mut pool: HashMap<String, Vec<u8>> = HashMap::new();
        for (_, c) in &contents {
            pool.insert(sha3_hex(c), c.to_vec());
        }
        let new_edit: &[u8] = b"line one\nline 2\n";
        let new_file: &[u8] = b"fresh\n";
        pool.insert(sha3_hex(new_edit), new_edit.to_vec());
        pool.insert(sha3_hex(new_file), new_file.to_vec());
        let before = Manifest::from_records([
            rec("100644", "/edit", contents[0].1),
            rec("100644", "/edit2", contents[1].1),
            rec("100644", "/gone", contents[2].1),
            rec("100644", "/mode", contents[3].1),
        ])
        .unwrap();
        let after = Manifest::from_records([
            rec("100644", "/edit", new_edit),
            rec("100644", "/edit2", contents[1].1),
            rec("100755", "/mode", contents[3].1),
            rec("100644", "/new", new_file),
        ])
        .unwrap();
        let changes = diff_manifests(&before, &after);
        let intent =
            synthesize_intent(&changes, |hash| Ok(pool.get(hash).cloned().unwrap())).unwrap();
        // One op per change: edit, delete, chmod, create.
        let kinds: Vec<&str> = intent
            .ops
            .iter()
            .map(|op| match op {
                Op::Edit { .. } => "edit",
                Op::Create { .. } => "create",
                Op::Delete { .. } => "delete",
                Op::Chmod { .. } => "chmod",
            })
            .collect();
        assert_eq!(kinds, vec!["edit", "delete", "chmod", "create"]);
        // Replay: the intent against the before-manifest reproduces after.
        let dir = std::env::temp_dir()
            .join(format!("abelian-synth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = crate::blobs::BlobStore::init(&dir).unwrap();
        for content in pool.values() {
            blobs.put(content).unwrap();
        }
        let mut manifest = before.clone();
        crate::patch::apply_intent(&intent, &mut manifest, &blobs).unwrap();
        assert_eq!(manifest.sum(), after.sum());
    }
}
