//! §2.6 Views: fuse records.
//!
//! `fuse` composes a span of patches into one narrative beat — what git
//! called a commit, squash, and fixup, unified.  It is lossless by
//! construction because it writes here and never to the log.  Views are
//! unordered, unchained, and carry no authority: they are renderings, and
//! the human view of history is a default zoom level, not a different
//! interface.

use serde::{Deserialize, Serialize};

use crate::log::LogLine;
use crate::{Error, Result};

/// The annotation on a view.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewAnnotation {
    /// The narrative beat.
    pub prose: String,
    /// Who fused.
    pub author: String,
}

/// One record in `views.jsonl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum View {
    /// A fuse of the span `from..=to`, by line id.
    Fuse {
        /// First line id of the span.
        from: String,
        /// Last line id of the span, inclusive.
        to: String,
        /// The narrative.
        annotation: ViewAnnotation,
    },
}

/// Parse `views.jsonl`; unordered, so any well-formed subset is valid.
pub fn parse_views(bytes: &[u8]) -> Result<Vec<View>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Corrupt("views.jsonl is not UTF-8".to_string()))?;
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).map_err(Error::from))
        .collect()
}

/// Serialize one view as a JSONL line, trailing LF included.
pub fn view_line(view: &View) -> Result<Vec<u8>> {
    let mut bytes = crate::ident::canonical_json(&serde_json::to_value(view)?)?.into_bytes();
    bytes.push(b'\n');
    Ok(bytes)
}

/// One beat of a fused rendering: either a fused span or a single line.
#[derive(Debug)]
pub enum Beat<'a> {
    /// Lines composed into one narrative beat by a fuse view.
    Fused {
        /// The view that fused them.
        view: &'a View,
        /// The underlying lines — the fine structure remains underneath,
        /// forever.
        lines: &'a [LogLine],
    },
    /// A line no fuse covers.
    Single(&'a LogLine),
}

/// Render a log at the fused zoom level: fuse views collapse their spans;
/// everything else renders singly.  Overlapping or dangling views are
/// ignored rather than fatal — views carry no authority.
pub fn fused_beats<'a>(lines: &'a [LogLine], views: &'a [View]) -> Vec<Beat<'a>> {
    let index_of = |id: &str| lines.iter().position(|l| l.id == id);
    let mut spans: Vec<(usize, usize, &View)> = Vec::new();
    for view in views {
        let View::Fuse { from, to, .. } = view;
        if let (Some(a), Some(b)) = (index_of(from), index_of(to))
            && a <= b
        {
            spans.push((a, b, view));
        }
    }
    spans.sort_by_key(|&(a, _, _)| a);
    let mut beats = Vec::new();
    let mut i = 0;
    let mut spans = spans.into_iter().peekable();
    while i < lines.len() {
        match spans.peek() {
            Some(&(a, b, view)) if a == i => {
                beats.push(Beat::Fused { view, lines: &lines[a..=b] });
                i = b + 1;
                spans.next();
            }
            Some(&(a, _, _)) if a < i => {
                // Overlaps a prior beat: no authority, skip it.
                spans.next();
            }
            _ => {
                beats.push(Beat::Single(&lines[i]));
                i += 1;
            }
        }
    }
    beats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Annotation;
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

    fn fuse(from: &str, to: &str) -> View {
        View::Fuse {
            from: from.to_string(),
            to: to.to_string(),
            annotation: ViewAnnotation {
                prose: "retry loop: bounded backoff".to_string(),
                author: "sid@fable-5".to_string(),
            },
        }
    }

    #[test]
    fn views_round_trip() {
        let v = fuse("id-17", "id-42");
        let bytes = view_line(&v).unwrap();
        let parsed = parse_views(&bytes).unwrap();
        assert_eq!(parsed, vec![v]);
    }

    #[test]
    fn fusing_is_a_rendering() {
        let lines =
            vec![line("a", ""), line("b", "a"), line("c", "b"), line("d", "c")];
        let views = vec![fuse("b", "c")];
        let beats = fused_beats(&lines, &views);
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
        let lines = vec![line("a", "")];
        let views = vec![fuse("nope", "a")];
        let beats = fused_beats(&lines, &views);
        assert_eq!(beats.len(), 1);
        assert!(matches!(beats[0], Beat::Single(_)));
    }
}
