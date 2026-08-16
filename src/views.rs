//! §2.6 Fuses and views: interpretations, rendered.
//!
//! A `fuse` names one interval of the log under a different interpretation
//! — what git called a commit, squash, and fixup, unified.  A fuse is a log
//! line with provenance `fuse`, a named `fuse` span in its annotation, and
//! an empty realized delta: an arithmetic identity, so it travels through
//! union like any other line.  It is lossless by construction because the
//! fused lines remain in the log underneath, forever.
//!
//! A view is not a line: it is a render-time filter naming the fuses to
//! collapse.  Fuses overlap freely — no fuse supersedes another — and the
//! rendering at any log prefix is a pure function of that prefix.

use std::collections::BTreeSet;

use crate::log::LogLine;

/// One beat of a fused rendering: either a fused span or a single line.
#[derive(Debug)]
pub enum Beat<'a> {
    /// Lines composed into one narrative beat by a fuse.
    Fused {
        /// The fuse line that named them; its annotation carries the name,
        /// the span, the prose, and the author.
        fuse: &'a LogLine,
        /// The underlying lines — the fine structure remains underneath,
        /// forever.
        lines: &'a [LogLine],
    },
    /// A line rendered as itself: either no selected fuse covers it, or it
    /// is a fuse line whose beat was filtered out or did not resolve.
    Single(&'a LogLine),
}

/// Render a log at the fused zoom level.  `view` is the filter: `None`
/// collapses every fuse; `Some(names)` collapses only fuses whose name is
/// in `names` — the caller declares the names, just in time, each time.
///
/// Every selected fuse whose span resolves renders a beat.  Fuses may
/// overlap in all cases: nothing supersedes, so a line under overlapping
/// fuses appears in each covering beat — filter the view to disambiguate.
/// A fuse line renders as its beat, or as an ordinary line when its span
/// dangles (ids not in the prefix), is reversed, or is filtered out —
/// dangling is never fatal, because fuses carry no authority.
pub fn fused_beats<'a>(lines: &'a [LogLine], view: Option<&[&str]>) -> Vec<Beat<'a>> {
    let selected = |name: &str| view.is_none_or(|names| names.contains(&name));
    let index_of = |id: &str| lines.iter().position(|l| l.id == id);
    // Beats in chain order; overlap supersedes nothing.
    let mut spans: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let Some(fuse) = &line.annotation.fuse else {
            continue;
        };
        if !selected(&fuse.name) {
            continue;
        }
        let (Some(a), Some(b)) = (index_of(&fuse.from), index_of(&fuse.to)) else {
            continue;
        };
        if a > b {
            continue;
        }
        spans.push((a, b, idx));
    }
    spans.sort_by_key(|&(a, _, idx)| (a, idx));
    // Fuse lines that produce a beat render as that beat, never as singles.
    let beat_lines: BTreeSet<usize> = spans.iter().map(|&(_, _, idx)| idx).collect();
    let mut beats = Vec::new();
    let mut i = 0;
    let mut single = |beats: &mut Vec<Beat<'a>>, j: usize| {
        if !beat_lines.contains(&j) {
            beats.push(Beat::Single(&lines[j]));
        }
    };
    for (a, b, idx) in spans {
        // Lines before the beat, not already rendered, render singly.
        for j in i..a {
            single(&mut beats, j);
        }
        beats.push(Beat::Fused {
            fuse: &lines[idx],
            lines: &lines[a..=b],
        });
        i = i.max(b + 1);
    }
    for j in i..lines.len() {
        single(&mut beats, j);
    }
    beats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Annotation, Fuse, Provenance};
    use crate::patch::Intent;

    fn line(id: &str, prev: &str) -> LogLine {
        LogLine {
            id: id.to_string(),
            prev: prev.to_string(),
            intent: Intent::default(),
            realized: vec![],
            sum_after: "0".repeat(64),
            committed_ms: 0,
            annotation: Annotation::default(),
        }
    }

    fn fuse(id: &str, prev: &str, name: &str, from: &str, to: &str, prose: &str) -> LogLine {
        let mut l = line(id, prev);
        l.annotation.provenance = Provenance::Fuse;
        l.annotation.fuse = Some(Fuse {
            name: name.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        });
        l.annotation.prose = Some(prose.to_string());
        l
    }

    #[test]
    fn fusing_is_a_rendering() {
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            line("c", "b"),
            line("d", "c"),
            fuse("f", "d", "retry", "b", "c", "retry loop: bounded backoff"),
        ];
        let beats = fused_beats(&lines, None);
        assert_eq!(beats.len(), 3);
        assert!(matches!(beats[0], Beat::Single(l) if l.id == "a"));
        assert!(matches!(beats[1], Beat::Fused { lines, .. } if lines.len() == 2));
        assert!(matches!(beats[2], Beat::Single(l) if l.id == "d"));
        // Lossless: the fine structure remains underneath.
        if let Beat::Fused { lines, .. } = &beats[1] {
            assert_eq!(lines[0].id, "b");
            assert_eq!(lines[1].id, "c");
        }
    }

    #[test]
    fn dangling_fuses_render_as_ordinary_lines() {
        // A fuse whose span does not resolve is never fatal: the fuse line
        // renders as itself, prose and all.
        let lines = vec![line("a", ""), fuse("f", "a", "x", "nope", "a", "x")];
        let beats = fused_beats(&lines, None);
        assert_eq!(beats.len(), 2);
        assert!(matches!(beats[0], Beat::Single(l) if l.id == "a"));
        assert!(matches!(beats[1], Beat::Single(l) if l.id == "f"));
    }

    #[test]
    fn overlapping_fuses_both_render_and_prefixes_render_the_past() {
        // The motivating case: fuse a span as an active incident, then
        // append a second fuse marking it resolved.  Overlap supersedes
        // nothing: both beats render, and the covered lines appear under
        // each.  The status is a pure function of the log prefix.
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            fuse("f1", "b", "incident", "a", "b", "incident: active"),
            fuse("f2", "f1", "incident", "a", "b", "incident: resolved"),
        ];
        // Any read between the two fuses shows it active…
        let beats = fused_beats(&lines[..3], None);
        assert_eq!(beats.len(), 1);
        let Beat::Fused { fuse, lines: fused } = &beats[0] else {
            panic!("expected a fused beat");
        };
        assert_eq!(fuse.id, "f1");
        assert_eq!(fuse.annotation.prose.as_deref(), Some("incident: active"));
        assert_eq!(fused.len(), 2);
        // …and any read after shows both interpretations, both retained.
        let beats = fused_beats(&lines, None);
        assert_eq!(beats.len(), 2);
        assert!(matches!(&beats[0], Beat::Fused { fuse, .. } if fuse.id == "f1"));
        assert!(matches!(&beats[1], Beat::Fused { fuse, .. } if fuse.id == "f2"));
        // A view filters to one interpretation of the span.
        let beats = fused_beats(&lines, Some(&["incident"]));
        assert_eq!(beats.len(), 2, "both fuses share the name; both collapse");
    }

    #[test]
    fn partially_overlapping_fuses_both_render() {
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            line("c", "b"),
            line("d", "c"),
            line("e", "d"),
            fuse("f1", "e", "first", "a", "c", "one"),
            fuse("f2", "f1", "second", "c", "d", "two"),
        ];
        let beats = fused_beats(&lines, None);
        assert_eq!(beats.len(), 3);
        assert!(matches!(&beats[0], Beat::Fused { lines, .. } if lines.len() == 3));
        // The overlap renders under both beats: c appears again.
        assert!(matches!(&beats[1], Beat::Fused { lines, .. } if lines.len() == 2));
        assert!(matches!(beats[2], Beat::Single(l) if l.id == "e"));
    }

    #[test]
    fn non_overlapping_fuses_coexist() {
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            line("c", "b"),
            line("d", "c"),
            fuse("f1", "d", "first", "a", "b", "first"),
            fuse("f2", "f1", "second", "c", "d", "second"),
        ];
        let beats = fused_beats(&lines, None);
        assert_eq!(beats.len(), 2);
        assert!(matches!(&beats[0], Beat::Fused { fuse, .. } if fuse.id == "f1"));
        assert!(matches!(&beats[1], Beat::Fused { fuse, .. } if fuse.id == "f2"));
    }

    #[test]
    fn a_view_filters_to_named_fuses() {
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            line("c", "b"),
            line("d", "c"),
            fuse("f1", "d", "first", "a", "b", "first"),
            fuse("f2", "f1", "second", "c", "d", "second"),
        ];
        // Only `second` collapses; everything else — including the filtered-
        // out fuse line f1 — renders as itself, in chain order.
        let beats = fused_beats(&lines, Some(&["second"]));
        assert_eq!(beats.len(), 4);
        assert!(matches!(beats[0], Beat::Single(l) if l.id == "a"));
        assert!(matches!(beats[1], Beat::Single(l) if l.id == "b"));
        assert!(matches!(&beats[2], Beat::Fused { fuse, .. } if fuse.id == "f2"));
        assert!(matches!(beats[3], Beat::Single(l) if l.id == "f1"));
        // A name no fuse carries collapses nothing — never fatal.
        let beats = fused_beats(&lines, Some(&["nope"]));
        assert_eq!(beats.len(), 6);
        assert!(beats.iter().all(|b| matches!(b, Beat::Single(_))));
    }
}
