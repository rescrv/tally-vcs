//! `.tallyignore`: gitignore-style exclusion for the working-tree walk.
//!
//! One file at the repository root, read by `records_of_working_tree`.
//! Semantics follow gitignore(5): blank lines and `#` comments are skipped,
//! `!` negates, a trailing `/` restricts a rule to directories, a `/`
//! anywhere else anchors the rule to the root, and the glob language is
//! `*` (never crossing `/`), `?`, `[...]` classes, and `**` for any number
//! of path segments.  The last matching rule wins.  Because the walk prunes
//! ignored directories, a negation cannot re-include a file whose parent
//! directory is excluded — the same limitation gitignore documents.

/// The parsed rules of an ignore file.  An empty set ignores nothing.
pub struct Ignore {
    rules: Vec<Rule>,
}

struct Rule {
    negated: bool,
    dir_only: bool,
    anchored: bool,
    segments: Vec<String>,
}

impl Ignore {
    /// An ignore file with no rules.
    pub fn empty() -> Self {
        Ignore { rules: Vec::new() }
    }

    /// Parse the text of an ignore file.  Unparseable lines are skipped;
    /// an ignore file can never make the walk fail.
    pub fn parse(text: &str) -> Self {
        let rules = text.lines().filter_map(parse_line).collect();
        Ignore { rules }
    }

    /// True if `rel_path` (root-relative, `/`-separated, no leading slash)
    /// is excluded.  `is_dir` gates directory-only rules.
    pub fn is_ignored(&self, rel_path: &str, is_dir: bool) -> bool {
        let segs: Vec<&str> = rel_path.split('/').collect();
        let mut ignored = false;
        for rule in &self.rules {
            if rule.dir_only && !is_dir {
                continue;
            }
            if rule.matches(&segs) {
                ignored = !rule.negated;
            }
        }
        ignored
    }
}

impl Rule {
    fn matches(&self, segs: &[&str]) -> bool {
        let pats: Vec<&str> = self.segments.iter().map(String::as_str).collect();
        if self.anchored {
            match_segments(&pats, segs)
        } else {
            // Unanchored: the pattern may begin at any depth.
            (0..segs.len()).any(|i| match_segments(&pats, &segs[i..]))
        }
    }
}

fn parse_line(line: &str) -> Option<Rule> {
    // Trailing spaces are ignored unless backslash-escaped.
    let mut line = line;
    while line.ends_with(' ') && !line.ends_with("\\ ") {
        line = &line[..line.len() - 1];
    }
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let mut pattern = line;
    let mut negated = false;
    if let Some(rest) = pattern.strip_prefix('!') {
        negated = true;
        pattern = rest;
    }
    // A leading `\!` or `\#` stays in the pattern; the segment matcher
    // treats `\` as an escape, so it matches the literal character.
    let mut dir_only = false;
    if let Some(rest) = pattern.strip_suffix('/') {
        dir_only = true;
        pattern = rest;
    }
    let anchored = if let Some(rest) = pattern.strip_prefix('/') {
        pattern = rest;
        true
    } else {
        pattern.contains('/')
    };
    if pattern.is_empty() {
        return None;
    }
    let segments: Vec<String> = pattern.split('/').map(str::to_string).collect();
    if segments.iter().any(String::is_empty) {
        return None; // `a//b` and friends: not a meaningful rule.
    }
    Some(Rule {
        negated,
        dir_only,
        anchored,
        segments,
    })
}

/// Match a list of pattern segments against path segments.  A trailing
/// unmatched directory prefix counts: pattern `target` matches the path
/// `target/debug` because ignoring a directory ignores its contents.
fn match_segments(pats: &[&str], segs: &[&str]) -> bool {
    match pats.split_first() {
        None => true, // all pattern segments consumed: segs is under a match
        Some((&"**", rest)) => {
            if rest.is_empty() {
                // A trailing `/**` matches everything inside, not the
                // directory itself: at least one segment must remain.
                !segs.is_empty()
            } else {
                (0..=segs.len()).any(|i| match_segments(rest, &segs[i..]))
            }
        }
        Some((pat, rest)) => match segs.split_first() {
            None => false,
            Some((seg, segs_rest)) => match_segment(pat, seg) && match_segments(rest, segs_rest),
        },
    }
}

/// Glob a single path segment: `*`, `?`, `[...]`, and `\` escapes.
/// Neither `*` nor `?` matches `/` — segments never contain one.
fn match_segment(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let txt: Vec<char> = text.chars().collect();
    match_chars(&pat, &txt)
}

fn match_chars(pat: &[char], txt: &[char]) -> bool {
    match pat.split_first() {
        None => txt.is_empty(),
        Some(('*', rest)) => (0..=txt.len()).any(|i| match_chars(rest, &txt[i..])),
        Some(('?', rest)) => !txt.is_empty() && match_chars(rest, &txt[1..]),
        Some(('\\', rest)) => match (rest.split_first(), txt.split_first()) {
            (Some((esc, rest)), Some((t, txt_rest))) => esc == t && match_chars(rest, txt_rest),
            _ => false,
        },
        Some(('[', rest)) => match txt.split_first() {
            None => false,
            Some((t, txt_rest)) => match match_class(rest, *t) {
                Some((hit, after)) => hit && match_chars(after, txt_rest),
                None => false, // unterminated class: match nothing
            },
        },
        Some((p, rest)) => match txt.split_first() {
            Some((t, txt_rest)) => p == t && match_chars(rest, txt_rest),
            None => false,
        },
    }
}

/// Match `c` against a character class body (after the `[`).  Returns
/// whether it matched and the pattern remainder after the closing `]`,
/// or None if the class never closes.
fn match_class(pat: &[char], c: char) -> Option<(bool, &[char])> {
    let mut i = 0;
    let mut negated = false;
    if pat.get(i) == Some(&'!') || pat.get(i) == Some(&'^') {
        negated = true;
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    loop {
        let p = *pat.get(i)?;
        if p == ']' && !first {
            return Some((hit != negated, &pat[i + 1..]));
        }
        first = false;
        if pat.get(i + 1) == Some(&'-') && pat.get(i + 2).is_some_and(|c| *c != ']') {
            let hi = *pat.get(i + 2)?;
            if p <= c && c <= hi {
                hit = true;
            }
            i += 3;
        } else {
            if p == c {
                hit = true;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(text: &str) -> Ignore {
        Ignore::parse(text)
    }

    #[test]
    fn blank_and_comments_are_skipped() {
        let i = ig("\n# comment\n   \n");
        assert!(!i.is_ignored("anything", false));
    }

    #[test]
    fn unanchored_matches_any_depth() {
        let i = ig("*.o\n");
        assert!(i.is_ignored("a.o", false));
        assert!(i.is_ignored("src/deep/a.o", false));
        assert!(!i.is_ignored("a.oo", false));
    }

    #[test]
    fn star_does_not_cross_slash() {
        let i = ig("src/*.rs\n");
        assert!(i.is_ignored("src/main.rs", false));
        assert!(!i.is_ignored("src/sub/main.rs", false));
    }

    #[test]
    fn leading_slash_anchors() {
        let i = ig("/target\n");
        assert!(i.is_ignored("target", true));
        assert!(i.is_ignored("target/debug/foo", false));
        assert!(!i.is_ignored("sub/target", true));
    }

    #[test]
    fn inner_slash_anchors_too() {
        let i = ig("doc/frotz\n");
        assert!(i.is_ignored("doc/frotz", true));
        assert!(!i.is_ignored("a/doc/frotz", true));
    }

    #[test]
    fn trailing_slash_is_directories_only() {
        let i = ig("build/\n");
        assert!(i.is_ignored("build", true));
        assert!(!i.is_ignored("build", false));
        assert!(i.is_ignored("src/build", true));
    }

    #[test]
    fn negation_last_match_wins() {
        let i = ig("*.log\n!keep.log\n");
        assert!(i.is_ignored("debug.log", false));
        assert!(!i.is_ignored("keep.log", false));
        assert!(!i.is_ignored("logs/keep.log", false));
        let i = ig("!keep.log\n*.log\n");
        assert!(i.is_ignored("keep.log", false));
    }

    #[test]
    fn double_star_spans_segments() {
        let i = ig("a/**/b\n");
        assert!(i.is_ignored("a/b", false));
        assert!(i.is_ignored("a/x/b", false));
        assert!(i.is_ignored("a/x/y/b", false));
        let i = ig("**/foo\n");
        assert!(i.is_ignored("foo", false));
        assert!(i.is_ignored("x/y/foo", false));
        let i = ig("abc/**\n");
        assert!(i.is_ignored("abc/x", false));
        assert!(!i.is_ignored("abc", true));
    }

    #[test]
    fn question_and_classes() {
        let i = ig("a?c\n");
        assert!(i.is_ignored("abc", false));
        assert!(!i.is_ignored("ac", false));
        let i = ig("*.[oa]\n");
        assert!(i.is_ignored("x.o", false));
        assert!(i.is_ignored("x.a", false));
        assert!(!i.is_ignored("x.c", false));
        let i = ig("[!a-c]*\n");
        assert!(i.is_ignored("dog", false));
        assert!(!i.is_ignored("cat", false));
    }

    #[test]
    fn escaped_literals() {
        let i = ig("\\#notacomment\n");
        assert!(i.is_ignored("#notacomment", false));
        let i = ig("\\!important\n");
        assert!(i.is_ignored("!important", false));
        let i = ig("a\\*b\n");
        assert!(i.is_ignored("a*b", false));
        assert!(!i.is_ignored("axb", false));
    }

    #[test]
    fn ignored_directory_prefix_covers_contents() {
        let i = ig("target\n");
        assert!(i.is_ignored("target/debug/deps/foo.rlib", false));
        assert!(i.is_ignored("sub/target/debug", true));
    }
}
