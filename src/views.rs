//! §2.6 Views: fuse records, rendered.
//!
//! `fuse` composes a span of patches into one narrative beat — what git
//! called a commit, squash, and fixup, unified.  A view is a log line with
//! provenance `view`, a `view` span in its annotation, and an empty
//! realized delta: an arithmetic identity, so it travels through union like
//! any other line.  It is lossless by construction because the fused lines
//! remain in the log underneath, forever.  Views are ordered by chain
//! position, so a later view supersedes an earlier one it overlaps — the
//! rendering at any log prefix is a pure function of that prefix.

use crate::log::LogLine;

/// One beat of a fused rendering: either a fused span or a single line.
#[derive(Debug)]
pub enum Beat<'a> {
    /// Lines composed into one narrative beat by a fuse view.
    Fused {
        /// The view line that fused them; its annotation carries the span,
        /// the prose, and the author.
        view: &'a LogLine,
        /// The underlying lines — the fine structure remains underneath,
        /// forever.
        lines: &'a [LogLine],
    },
    /// A line no fuse covers.
    Single(&'a LogLine),
}

/// Render a log at the fused zoom level: active fuse views collapse their
/// spans; view lines themselves do not render; everything else renders
/// singly.  Views are ordered by chain position, so a later view supersedes
/// any earlier view whose span it overlaps — both lines are retained, and
/// rendering a shorter prefix shows the earlier view again.  Dangling views
/// (ids not in the prefix) are ignored rather than fatal — views carry no
/// authority.
pub fn fused_beats(lines: &[LogLine]) -> Vec<Beat<'_>> {
    let index_of = |id: &str| lines.iter().position(|l| l.id == id);
    // Views in chain order: later supersedes earlier on overlap.
    let mut spans: Vec<(usize, usize, &LogLine)> = Vec::new();
    for line in lines {
        let Some(view) = &line.annotation.view else {
            continue;
        };
        let (Some(a), Some(b)) = (index_of(&view.from), index_of(&view.to)) else {
            continue;
        };
        if a > b {
            continue;
        }
        spans.retain(|&(x, y, _)| y < a || b < x);
        spans.push((a, b, line));
    }
    spans.sort_by_key(|&(a, _, _)| a);
    let mut beats = Vec::new();
    let mut spans = spans.into_iter().peekable();
    let mut i = 0;
    while i < lines.len() {
        match spans.peek() {
            Some(&(a, b, view)) if a == i => {
                beats.push(Beat::Fused { view, lines: &lines[a..=b] });
                i = b + 1;
                spans.next();
            }
            _ => {
                if lines[i].annotation.view.is_none() {
                    beats.push(Beat::Single(&lines[i]));
                }
                i += 1;
            }
        }
    }
    beats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{Annotation, Provenance, ViewSpan};
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

    fn fuse(id: &str, prev: &str, from: &str, to: &str, prose: &str) -> LogLine {
        let mut l = line(id, prev);
        l.annotation.provenance = Provenance::View;
        l.annotation.view =
            Some(ViewSpan { from: from.to_string(), to: to.to_string() });
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
            fuse("v", "d", "b", "c", "retry loop: bounded backoff"),
        ];
        let beats = fused_beats(&lines);
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
    fn dangling_views_are_ignored() {
        let lines = vec![line("a", ""), fuse("v", "a", "nope", "a", "x")];
        let beats = fused_beats(&lines);
        assert_eq!(beats.len(), 1);
        assert!(matches!(beats[0], Beat::Single(l) if l.id == "a"));
    }

    #[test]
    fn later_views_supersede_and_prefixes_render_the_past() {
        // The motivating case: fuse a span as an active incident, then
        // append a second view marking it resolved.  The status is a pure
        // function of the log prefix, with both lines retained.
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            fuse("v1", "b", "a", "b", "incident: active"),
            fuse("v2", "v1", "a", "b", "incident: resolved"),
        ];
        // Any read between the two views shows it active…
        let beats = fused_beats(&lines[..3]);
        assert_eq!(beats.len(), 1);
        let Beat::Fused { view, lines: fused } = &beats[0] else {
            panic!("expected a fused beat");
        };
        assert_eq!(view.id, "v1");
        assert_eq!(view.annotation.prose.as_deref(), Some("incident: active"));
        assert_eq!(fused.len(), 2);
        // …and any read after shows it resolved, v1 superseded but retained.
        let beats = fused_beats(&lines);
        assert_eq!(beats.len(), 1);
        let Beat::Fused { view, lines: fused } = &beats[0] else {
            panic!("expected a fused beat");
        };
        assert_eq!(view.id, "v2");
        assert_eq!(view.annotation.prose.as_deref(), Some("incident: resolved"));
        assert_eq!(fused.len(), 2);
        assert!(lines.iter().any(|l| l.id == "v1"), "superseded views are retained");
    }

    #[test]
    fn non_overlapping_views_coexist() {
        let lines = vec![
            line("a", ""),
            line("b", "a"),
            line("c", "b"),
            line("d", "c"),
            fuse("v1", "d", "a", "b", "first"),
            fuse("v2", "v1", "c", "d", "second"),
        ];
        let beats = fused_beats(&lines);
        assert_eq!(beats.len(), 2);
        assert!(matches!(&beats[0], Beat::Fused { view, .. } if view.id == "v1"));
        assert!(matches!(&beats[1], Beat::Fused { view, .. } if view.id == "v2"));
    }
}
