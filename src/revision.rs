//! Revision language: the names every other command takes as arguments.
//!
//! gitrevisions(7) is a man page, not a marquee command, but nothing else
//! can be spelled until it exists: `HEAD`, `HEAD~3`, a fork name, a line id,
//! a bare sum.  A revision resolves to a *state* — a point on a fork's
//! lineage, identified by its sum.  Because history here is a chain whose
//! arithmetic never needed order, "the state N steps back" is a well-defined
//! index into the lineage, not a graph walk with merge ambiguity.
//!
//! The namespace is strictly richer than git's: a resolution carries not
//! just the sum but the fork it was read on and, when a line named the
//! state, that line's id — so `blame`, `diff`, and `restore` can speak of
//! patches and spans, not only commits.

use crate::ident::{Sum, is_hex64};
use crate::log::LogLine;
use crate::patch::{RealizedEntry, apply_realized_to_sum};
use crate::repo::Repository;
use crate::{Error, Result};

/// A resolved revision: a state on a fork's lineage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The fork whose lineage the revision was read on.
    pub fork: String,
    /// The state sum the revision names, 64 hex.
    pub sum: String,
    /// The id of the log line whose `sum_after` is this state, if the state
    /// is one a line produced (the base anchor names no line).
    pub line: Option<String>,
}

/// One point on a fork's lineage: a state, and the line (if any) that
/// produced it.  Index 0 is the base anchor, which no line produced; each
/// subsequent point is the state after one line of the continuity log.
struct StatePoint {
    sum: String,
    line: Option<String>,
}

/// Build the lineage as an ordered list of states, oldest first.  Point 0 is
/// the base anchor — the state before the earliest line of the whole lineage
/// — recovered by inverting the first line's realized delta; every later
/// point is a line's `sum_after`.  HEAD is the last point.
fn lineage_states(repo: &Repository, fork: &str) -> Result<Vec<StatePoint>> {
    let history = repo.continuity_log(fork)?;
    let lines: Vec<&LogLine> = history.iter().map(|(_, l)| l).collect();
    let base = match lines.first() {
        None => repo.current_state(fork)?.sum.hexdigest(),
        Some(first) => {
            // The state before the first line is the inverse of its realized
            // delta applied to the state after it — undo is the inverse.
            let after = Sum::from_hexdigest(&first.sum_after)?;
            let inverse: Vec<RealizedEntry> = first
                .realized
                .iter()
                .map(|e| RealizedEntry {
                    remove: e.add.clone(),
                    add: e.remove.clone(),
                })
                .collect();
            apply_realized_to_sum(&after, &inverse)?.hexdigest()
        }
    };
    let mut points = vec![StatePoint {
        sum: base,
        line: None,
    }];
    for line in lines {
        points.push(StatePoint {
            sum: line.sum_after.clone(),
            line: Some(line.id.clone()),
        });
    }
    Ok(points)
}

/// Split a spec into its base and its suffix operators.  A suffix is `^`,
/// `~N`, or `@{N}`; everything before the first suffix operator is the base.
/// A bare leading `@` (an alias for HEAD) is a base; a leading `@{N}` is the
/// `@{N}` suffix on an implicit HEAD, so `@{0}` means `HEAD@{0}` as in git.
fn split_suffixes(spec: &str) -> (&str, Vec<&str>) {
    let bytes = spec.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'~' | b'^' => break,
            b'@' if i > 0 => break,
            // Leading `@{` opens the `@{N}` suffix on an empty (HEAD) base;
            // a leading bare `@` is HEAD itself, so consume it as the base.
            b'@' if bytes.get(i + 1) == Some(&b'{') => break,
            b'@' => i += 1,
            _ => i += 1,
        }
    }
    let base = &spec[..i];
    let mut suffixes = Vec::new();
    let rest = &spec[i..];
    let rb = rest.as_bytes();
    let mut j = 0;
    while j < rb.len() {
        match rb[j] {
            b'^' => {
                suffixes.push(&rest[j..j + 1]);
                j += 1;
            }
            b'~' => {
                let mut k = j + 1;
                while k < rb.len() && rb[k].is_ascii_digit() {
                    k += 1;
                }
                suffixes.push(&rest[j..k]);
                j = k;
            }
            b'@' if rb.get(j + 1) == Some(&b'{') => match rest[j..].find('}') {
                Some(off) => {
                    suffixes.push(&rest[j..j + off + 1]);
                    j += off + 1;
                }
                None => {
                    suffixes.push(&rest[j..]);
                    j = rb.len();
                }
            },
            _ => {
                // Not a recognized suffix operator; fold it back into nothing
                // (the base already ended here), treat as a stray character.
                // Slice the whole character: a one-byte slice of a multibyte
                // char would panic at the char boundary.
                let ch_len = rest[j..].chars().next().map(char::len_utf8).unwrap_or(1);
                suffixes.push(&rest[j..j + ch_len]);
                j += ch_len;
            }
        }
    }
    (base, suffixes)
}

/// Resolve a bare line id (or unambiguous id prefix) anywhere in the repo.
///
/// A line id is `record_id` over the line's content *and* its `prev` — a
/// hash chain that recursively commits to the entire lineage prefix — so it
/// names exactly one line across the whole repository.  When the id is not on
/// `default_fork`'s lineage, we search every fork.  `continuity_log` walks
/// fork→parent to the root and labels each line with the fork that authored
/// it (inherited segments are truncated at the branch boundary, so a shared
/// line reports the same origin fork on every descendant); we dedupe by id so
/// one line seen from many descendants counts once.  The authoring fork
/// becomes `Resolved.fork` — any fork carrying the id would replay the same
/// sum, since the shared prefix is byte-identical, so the choice only labels
/// the result.
fn resolve_line_id_global(
    repo: &Repository,
    base: &str,
    default_fork: &str,
) -> Result<(String, usize, Vec<StatePoint>)> {
    use std::collections::BTreeMap;
    let mut authoring: BTreeMap<String, String> = BTreeMap::new();
    for fork in repo.fork_names()? {
        for (name, line) in repo.continuity_log(&fork)? {
            if line.id.starts_with(base) {
                authoring.entry(line.id).or_insert(name);
            }
        }
    }
    match authoring.len() {
        0 => Err(Error::Invalid(format!(
            "revision {base:?} is neither HEAD, a fork, a sum on fork \
             {default_fork}'s lineage, nor a line id in the repository"
        ))),
        1 => {
            let (id, fork) = authoring.into_iter().next().expect("len == 1");
            let points = lineage_states(repo, &fork)?;
            let idx = points
                .iter()
                .rposition(|p| p.line.as_deref() == Some(id.as_str()))
                .ok_or_else(|| {
                    Error::Corrupt(format!(
                        "line id {id} was authored on fork {fork} but is absent from \
                         its replayed lineage"
                    ))
                })?;
            Ok((fork, idx, points))
        }
        n => Err(Error::Invalid(format!(
            "line id prefix {base:?} is ambiguous across the repository: {n} lines match"
        ))),
    }
}

/// Resolve `spec` on `default_fork`'s lineage to a state.
///
/// Bases: `HEAD` or `@` (the fork's head), a fork name (that fork's head,
/// and the resolution is read on that fork), a 64-hex sum (a state on the
/// lineage), or a log-line id or unambiguous id prefix (the state that line
/// produced).  Two prefixes disambiguate the hash domains explicitly:
/// `sum:S` forces a state-sum resolution and `line:ID` forces a line-id
/// resolution — a bare 64-hex string is ambiguous because line ids and
/// setsums are both 256-bit, so the prefixes name the domain at the UI.
/// Suffixes walk the lineage: `^` and `~N` step back N states toward the
/// base; `@{N}` selects the state N steps back from head.
pub fn resolve(repo: &Repository, spec: &str, default_fork: &str) -> Result<Resolved> {
    if spec.is_empty() {
        return Err(Error::Invalid("empty revision".to_string()));
    }
    let (base, suffixes) = split_suffixes(spec);
    // Domain prefixes short-circuit the heuristic: they name which of the
    // two 256-bit domains — state sum or line id — the base lives in.
    if let Some(sum) = base.strip_prefix("sum:") {
        let (fork, index, points) = resolve_sum(repo, sum, default_fork)?;
        return apply_suffixes(spec, &fork, index, &points, suffixes);
    }
    if let Some(id) = base.strip_prefix("line:") {
        let (fork, index, points) = resolve_line(repo, id, default_fork)?;
        return apply_suffixes(spec, &fork, index, &points, suffixes);
    }
    // Decide which fork's lineage we read on, and the starting index.
    let known_forks = repo.fork_names()?;
    let (fork, index, points) = if base == "HEAD" || base == "@" || base.is_empty() {
        let points = lineage_states(repo, default_fork)?;
        let idx = points.len() - 1;
        (default_fork.to_string(), idx, points)
    } else if known_forks.iter().any(|f| f == base) {
        let points = lineage_states(repo, base)?;
        let idx = points.len() - 1;
        (base.to_string(), idx, points)
    } else {
        // A 64-hex string could be a state sum or a line id; a shorter
        // string is a line-id prefix.  Prefer an exact sum match — a sum
        // names a state directly — then fall back to a line id.
        let points = lineage_states(repo, default_fork)?;
        if is_hex64(base)
            && let Some(idx) = points.iter().rposition(|p| p.sum == base)
        {
            (default_fork.to_string(), idx, points)
        } else {
            let matches: Vec<usize> = points
                .iter()
                .enumerate()
                .filter(|(_, p)| p.line.as_deref().is_some_and(|id| id.starts_with(base)))
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                // Not on the default fork's lineage.  A sum is lineage-relative
                // (it can recur — land then revert lands you on a sum you've
                // seen), so it stays fork-scoped; but a line id is a hash chain
                // over its whole lineage prefix, hence globally unique.  Fall
                // through to a repo-wide search: the id names one line in the
                // whole repository, on whatever fork authored it.
                [] => resolve_line_id_global(repo, base, default_fork)?,
                [one] => (default_fork.to_string(), *one, points),
                many => {
                    return Err(Error::Invalid(format!(
                        "line id prefix {base:?} is ambiguous on fork {default_fork}: \
                         {} lines match",
                        many.len()
                    )));
                }
            }
        }
    };
    apply_suffixes(spec, &fork, index, &points, suffixes)
}

/// A slice of one fork's continuity log, resolved from a `log` argument.
///
/// The bounds index into `Repository::continuity_log(fork)`: `start` is the
/// first line to show, `end` one past the last.  A range's left endpoint is
/// exclusive — its own line is the first one dropped — so `A..B` is exactly
/// the lines that took the state from A to B.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogBounds {
    /// The fork whose continuity log the bounds index into.
    pub fork: String,
    /// First line to show, 0-based.
    pub start: usize,
    /// One past the last line to show.
    pub end: usize,
}

/// Resolve a `log` argument to bounds on a fork's continuity log.
///
/// `None` selects the whole lineage of `default_fork`.  A bare revision
/// selects the lineage up to and including the state it names — the log read
/// backwards from that point.  `A..B` selects the lines after A through B: A
/// exclusive, B inclusive; an empty side defaults to `HEAD`, as in git.  The
/// right endpoint names the lineage to read, and the left must name a state
/// on it, else the range is an error.  A three-dot range asks for the
/// symmetric difference of two lineages, which a linear chain cannot have.
pub fn log_range(repo: &Repository, spec: Option<&str>, default_fork: &str) -> Result<LogBounds> {
    let or_head: fn(&str) -> &str = |s| if s.is_empty() { "HEAD" } else { s };
    let (left, right) = match spec {
        None => (None, "HEAD"),
        Some(s) if s.contains("...") => {
            return Err(Error::Invalid(format!(
                "revision range {s:?}: the three-dot form needs a graph; a lineage is linear"
            )));
        }
        Some(s) => match s.split_once("..") {
            None => (None, s),
            Some((_, b)) if b.contains("..") => {
                return Err(Error::Invalid(format!(
                    "revision range {s:?}: only one \"..\" is allowed"
                )));
            }
            Some((a, b)) => (Some(a), b),
        },
    };
    // The right endpoint names the lineage to read.  Point 0 of the lineage
    // is the base anchor; point i is the state after log line i-1, so a
    // point's index is the count of lines at or before it — the log bound.
    let right_r = resolve(repo, or_head(right), default_fork)?;
    let points = lineage_states(repo, &right_r.fork)?;
    let position = |r: &Resolved, side: &str| -> Result<usize> {
        let idx = match &r.line {
            Some(id) => points.iter().rposition(|p| p.line.as_deref() == Some(id)),
            // A revision that names no line names the base of whatever
            // lineage it resolved on; it bounds this lineage only if it is
            // this lineage's base.
            None if r.sum == points[0].sum => Some(0),
            None => None,
        };
        idx.ok_or_else(|| {
            Error::Invalid(format!(
                "{side} does not name a state on fork {}'s lineage",
                right_r.fork
            ))
        })
    };
    let end = position(&right_r, right)?;
    let start = match left {
        None => 0,
        Some(a) => position(&resolve(repo, or_head(a), &right_r.fork)?, a)?,
    };
    Ok(LogBounds {
        fork: right_r.fork,
        start,
        // A range whose left endpoint is the newer state is empty, as in git.
        end: end.max(start),
    })
}

/// `sum:S` — resolve `s` strictly as a state sum, `default_fork` first then
/// the whole repository.  Unlike the bare-hex heuristic, this never falls
/// back to a line id: a `sum:` prefix asserts the setsum domain, so a non-hex
/// or absent sum is an error, not a reinterpretation.  Like `line:`, it does
/// not require the sum to live on the named fork — a sum names a state, and a
/// state's content is fixed by its sum, so any fork carrying it names the
/// same state.
fn resolve_sum(
    repo: &Repository,
    s: &str,
    default_fork: &str,
) -> Result<(String, usize, Vec<StatePoint>)> {
    if !is_hex64(s) {
        return Err(Error::Invalid(format!(
            "sum:{s:?} is not a 64-hex state sum"
        )));
    }
    let points = lineage_states(repo, default_fork)?;
    match points.iter().rposition(|p| p.sum == s) {
        Some(idx) => Ok((default_fork.to_string(), idx, points)),
        None => resolve_sum_global(repo, s),
    }
}

/// Resolve a state sum anywhere in the repo, when it is absent from the
/// default fork's lineage.
///
/// A sum is lineage-relative — it can recur (land then revert lands you on a
/// sum you've already seen) — so unlike a line id it is not globally unique.
/// But its *content* is: a state's sum is the setsum of that state, so every
/// occurrence of a sum, on any fork, names byte-identical state.  We scan
/// forks in name order (from `fork_names`, which sorts) and take the last
/// occurrence on the first fork that carries the sum; the fork label only
/// records where we read it, since the state is the same wherever it lives.
fn resolve_sum_global(repo: &Repository, s: &str) -> Result<(String, usize, Vec<StatePoint>)> {
    for fork in repo.fork_names()? {
        let points = lineage_states(repo, &fork)?;
        if let Some(idx) = points.iter().rposition(|p| p.sum == s) {
            return Ok((fork, idx, points));
        }
    }
    Err(Error::Invalid(format!(
        "sum {s} is not a state on any fork's lineage"
    )))
}

/// `line:ID` — resolve `id` strictly as a log-line id (or unambiguous
/// prefix).  This asserts the line-id domain: it takes the default fork's
/// lineage first, then the whole repository, and never reinterprets the id
/// as a sum.
fn resolve_line(
    repo: &Repository,
    id: &str,
    default_fork: &str,
) -> Result<(String, usize, Vec<StatePoint>)> {
    if id.is_empty() {
        return Err(Error::Invalid("line: requires a line id".to_string()));
    }
    let points = lineage_states(repo, default_fork)?;
    let matches: Vec<usize> = points
        .iter()
        .enumerate()
        .filter(|(_, p)| p.line.as_deref().is_some_and(|l| l.starts_with(id)))
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [] => resolve_line_id_global(repo, id, default_fork),
        [one] => Ok((default_fork.to_string(), *one, points)),
        many => Err(Error::Invalid(format!(
            "line id prefix {id:?} is ambiguous on fork {default_fork}: {} lines match",
            many.len()
        ))),
    }
}

/// Apply the suffix operators left to right, walking the lineage.
fn apply_suffixes(
    spec: &str,
    fork: &str,
    mut index: usize,
    points: &[StatePoint],
    suffixes: Vec<&str>,
) -> Result<Resolved> {
    for suffix in suffixes {
        let step_back = |index: &mut usize, n: usize, sfx: &str| -> Result<()> {
            if *index < n {
                return Err(Error::Invalid(format!(
                    "revision {spec:?}: {sfx} walks before the base of fork {fork}'s lineage"
                )));
            }
            *index -= n;
            Ok(())
        };
        match suffix {
            "^" => step_back(&mut index, 1, suffix)?,
            s if s.starts_with('~') => {
                let n: usize = s[1..].parse().map_err(|_| {
                    Error::Invalid(format!("revision {spec:?}: {s} is not ~<number>"))
                })?;
                step_back(&mut index, n, suffix)?;
            }
            s if s.starts_with("@{") && s.ends_with('}') => {
                let inner = &s[2..s.len() - 1];
                let n: usize = inner.parse().map_err(|_| {
                    Error::Invalid(format!(
                        "revision {spec:?}: only numeric @{{N}} is supported, not {inner:?}"
                    ))
                })?;
                if n >= points.len() {
                    return Err(Error::Invalid(format!(
                        "revision {spec:?}: @{{{n}}} walks before the base of fork {fork}"
                    )));
                }
                index = points.len() - 1 - n;
            }
            other => {
                return Err(Error::Invalid(format!(
                    "revision {spec:?}: unrecognized suffix {other:?}"
                )));
            }
        }
    }
    let point = &points[index];
    Ok(Resolved {
        fork: fork.to_string(),
        sum: point.sum.clone(),
        line: point.line.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Annotation;
    use crate::patch::{Intent, Op};

    fn temp_repo(name: &str) -> Repository {
        let dir = std::env::temp_dir().join(format!("tally-rev-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Repository::init(&dir).unwrap()
    }

    fn create(path: &str, content: &[u8]) -> Intent {
        Intent {
            ops: vec![Op::Create {
                path: path.to_string(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(crate::b64::encode(content)),
            }],
        }
    }

    fn delete(path: &str, content: &[u8]) -> Intent {
        Intent {
            ops: vec![Op::Delete {
                path: path.to_string(),
                blob: crate::ident::sha3_hex(content),
            }],
        }
    }

    fn note() -> Annotation {
        Annotation {
            author: "t".to_string(),
            ..Annotation::default()
        }
    }

    #[test]
    fn head_and_ancestors() {
        let repo = temp_repo("head");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        let b = repo.apply("main", create("/b", b"b\n"), note()).unwrap();
        // HEAD is the latest state.
        let head = resolve(&repo, "HEAD", "main").unwrap();
        assert_eq!(head.sum, b.sum_after);
        assert_eq!(head.line.as_deref(), Some(b.id.as_str()));
        // @ is an alias for HEAD.
        assert_eq!(resolve(&repo, "@", "main").unwrap(), head);
        // A leading @{N} is HEAD@{N}, as in git: @{0} is HEAD, @{1} is HEAD~1.
        assert_eq!(resolve(&repo, "@{0}", "main").unwrap(), head);
        assert_eq!(
            resolve(&repo, "@{1}", "main").unwrap(),
            resolve(&repo, "HEAD@{1}", "main").unwrap()
        );
        // HEAD~1 and HEAD^ are the state after a.
        let back = resolve(&repo, "HEAD~1", "main").unwrap();
        assert_eq!(back.sum, a.sum_after);
        assert_eq!(back.line.as_deref(), Some(a.id.as_str()));
        assert_eq!(resolve(&repo, "HEAD^", "main").unwrap(), back);
        // HEAD~2 is the base anchor (empty repo): the zero state, no line.
        let base = resolve(&repo, "HEAD~2", "main").unwrap();
        assert_eq!(base.sum, "0".repeat(64));
        assert_eq!(base.line, None);
        // HEAD~3 walks off the end.
        assert!(resolve(&repo, "HEAD~3", "main").is_err());
    }

    #[test]
    fn sums_and_line_ids_and_prefixes() {
        let repo = temp_repo("names");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        // A bare sum resolves to itself.
        let by_sum = resolve(&repo, &a.sum_after, "main").unwrap();
        assert_eq!(by_sum.sum, a.sum_after);
        assert_eq!(by_sum.line.as_deref(), Some(a.id.as_str()));
        // A full line id resolves to its state.
        assert_eq!(resolve(&repo, &a.id, "main").unwrap().sum, a.sum_after);
        // An unambiguous prefix resolves too.
        let short = &a.id[..8];
        assert_eq!(resolve(&repo, short, "main").unwrap().sum, a.sum_after);
        // Garbage does not resolve.
        assert!(resolve(&repo, "not-a-thing", "main").is_err());
    }

    #[test]
    fn fork_names_resolve_on_their_own_lineage() {
        let repo = temp_repo("forks");
        repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.create_fork("session", "main").unwrap();
        let c = repo.apply("session", create("/c", b"c\n"), note()).unwrap();
        // The fork name resolves to that fork's head, read on that fork.
        let r = resolve(&repo, "session", "main").unwrap();
        assert_eq!(r.fork, "session");
        assert_eq!(r.sum, c.sum_after);
        // @{1} on the session is the state before c — main's head, across
        // the lineage boundary.
        let back = resolve(&repo, "session@{1}", "main").unwrap();
        assert_eq!(
            back.sum,
            repo.current_state("main").unwrap().sum.hexdigest()
        );
    }

    #[test]
    fn line_id_resolves_across_forks() {
        // A line id is globally unique, so it should resolve without naming
        // the fork that authored it — even from a different default fork.
        let repo = temp_repo("global-id");
        repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.create_fork("session", "main").unwrap();
        let c = repo.apply("session", create("/c", b"c\n"), note()).unwrap();
        // Resolving `c`'s id with `main` as the default fork still finds it,
        // and reports the authoring fork.
        let r = resolve(&repo, &c.id, "main").unwrap();
        assert_eq!(r.sum, c.sum_after);
        assert_eq!(r.fork, "session");
        assert_eq!(r.line.as_deref(), Some(c.id.as_str()));
        // An unambiguous prefix resolves globally too.
        let short = &c.id[..8];
        assert_eq!(resolve(&repo, short, "main").unwrap().sum, c.sum_after);
        // A genuinely unknown id still errors.
        assert!(resolve(&repo, "ffffffffffff", "main").is_err());
    }

    #[test]
    fn sum_resolves_across_forks() {
        // Like a line id, `sum:` should not require the state to live on the
        // default fork: a sum names a state, and a state's content is fixed
        // by its sum, so any fork carrying it names the same state.
        let repo = temp_repo("global-sum");
        repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.create_fork("session", "main").unwrap();
        let c = repo.apply("session", create("/c", b"c\n"), note()).unwrap();
        // `session`'s head sum is not on `main`'s lineage, yet `sum:` finds
        // it with `main` as the default fork, and reports the fork it read on.
        let spec = format!("sum:{}", c.sum_after);
        let r = resolve(&repo, &spec, "main").unwrap();
        assert_eq!(r.sum, c.sum_after);
        assert_eq!(r.fork, "session");
        assert_eq!(r.line.as_deref(), Some(c.id.as_str()));
        // A sum on the shared prefix still prefers the default fork.
        let a_sum = repo.current_state("main").unwrap().sum.hexdigest();
        let shared = resolve(&repo, &format!("sum:{a_sum}"), "main").unwrap();
        assert_eq!(shared.fork, "main");
        assert_eq!(shared.sum, a_sum);
        // A genuinely unknown sum still errors.
        assert!(resolve(&repo, &format!("sum:{}", "f".repeat(64)), "main").is_err());
    }

    #[test]
    fn land_then_revert_ambiguous_sum_but_id_resolves() {
        // Land a change and revert it: the state sum recurs, but each line's
        // id is distinct because it chains its own predecessor.
        let repo = temp_repo("revert");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.apply("main", create("/b", b"b\n"), note()).unwrap();
        let d = repo.apply("main", delete("/b", b"b\n"), note()).unwrap();
        // Reverting /b returns to the sum after /a was created — the same
        // state — yet the two lines have distinct ids.
        assert_eq!(d.sum_after, a.sum_after);
        assert_ne!(a.id, d.id);
        // A bare sum is ambiguous and resolves to the *last* occurrence — the
        // revert, not the original land.
        let by_sum = resolve(&repo, &a.sum_after, "main").unwrap();
        assert_eq!(by_sum.line.as_deref(), Some(d.id.as_str()));
        // But the two lines have distinct ids, so each id resolves cleanly to
        // its own point, even though they name the same state.
        assert_eq!(
            resolve(&repo, &a.id, "main").unwrap().line.as_deref(),
            Some(a.id.as_str())
        );
        assert_eq!(
            resolve(&repo, &d.id, "main").unwrap().line.as_deref(),
            Some(d.id.as_str())
        );
    }

    fn bounds(fork: &str, start: usize, end: usize) -> LogBounds {
        LogBounds {
            fork: fork.to_string(),
            start,
            end,
        }
    }

    #[test]
    fn log_range_bare_ref_logs_backwards() {
        let repo = temp_repo("range-ref");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.apply("main", create("/b", b"b\n"), note()).unwrap();
        // No spec: the whole lineage of the default fork.
        assert_eq!(
            log_range(&repo, None, "main").unwrap(),
            bounds("main", 0, 2)
        );
        // A bare ref: the lines at or before the state it names.
        assert_eq!(
            log_range(&repo, Some("HEAD~1"), "main").unwrap(),
            bounds("main", 0, 1)
        );
        assert_eq!(
            log_range(&repo, Some(&a.id), "main").unwrap(),
            bounds("main", 0, 1)
        );
        // The base anchor precedes every line: nothing is at or before it.
        assert_eq!(
            log_range(&repo, Some("HEAD~2"), "main").unwrap(),
            bounds("main", 0, 0)
        );
    }

    #[test]
    fn log_range_range_is_left_exclusive() {
        let repo = temp_repo("range-excl");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        let b = repo.apply("main", create("/b", b"b\n"), note()).unwrap();
        let c = repo.apply("main", create("/c", b"c\n"), note()).unwrap();
        // a..HEAD: the lines that took the state from a to HEAD — b and c,
        // a itself excluded.
        let spec = format!("{}..HEAD", a.id);
        let r = log_range(&repo, Some(&spec), "main").unwrap();
        assert_eq!(r, bounds("main", 1, 3));
        let history = repo.continuity_log(&r.fork).unwrap();
        let ids: Vec<&str> = history[r.start..r.end]
            .iter()
            .map(|(_, l)| l.id.as_str())
            .collect();
        assert_eq!(ids, [b.id.as_str(), c.id.as_str()]);
        // An empty side defaults to HEAD: a.. is a..HEAD.
        assert_eq!(
            log_range(&repo, Some(&format!("{}..", a.id)), "main").unwrap(),
            r
        );
        // Both endpoints resolve as revisions, suffixes included.
        assert_eq!(
            log_range(&repo, Some("HEAD~2..HEAD~1"), "main").unwrap(),
            bounds("main", 1, 2)
        );
        // A range whose left is the right's own state (or newer) is empty.
        assert_eq!(
            log_range(&repo, Some("HEAD~1..HEAD~1"), "main").unwrap(),
            bounds("main", 2, 2)
        );
        assert_eq!(
            log_range(&repo, Some("HEAD..HEAD~1"), "main").unwrap(),
            bounds("main", 3, 3)
        );
        // The left endpoint must name a state on the right's lineage.
        assert!(log_range(&repo, Some("zz..HEAD"), "main").is_err());
        // The three-dot form needs a graph; a lineage is linear.
        assert!(log_range(&repo, Some("a...b"), "main").is_err());
        assert!(log_range(&repo, Some("a..b..c"), "main").is_err());
    }

    #[test]
    fn log_range_right_endpoint_names_the_lineage() {
        let repo = temp_repo("range-forks");
        let a = repo.apply("main", create("/a", b"a\n"), note()).unwrap();
        repo.create_fork("session", "main").unwrap();
        let c = repo.apply("session", create("/c", b"c\n"), note()).unwrap();
        // A bare fork name: that fork's whole lineage, inherited lines too.
        assert_eq!(
            log_range(&repo, Some("session"), "main").unwrap(),
            bounds("session", 0, 2)
        );
        // A range may cross the fork boundary: the shared prefix is one
        // lineage, so main's head is a valid left endpoint on it.
        assert_eq!(
            log_range(&repo, Some(&format!("{}..session", a.id)), "main").unwrap(),
            bounds("session", 1, 2)
        );
        // But the reverse direction is an error: c never touched main.
        assert!(log_range(&repo, Some(&format!("{}..HEAD", c.id)), "main").is_err());
    }

    #[test]
    fn log_range_empty_repo_is_empty() {
        let repo = temp_repo("range-empty");
        assert_eq!(
            log_range(&repo, None, "main").unwrap(),
            bounds("main", 0, 0)
        );
        assert_eq!(
            log_range(&repo, Some("HEAD"), "main").unwrap(),
            bounds("main", 0, 0)
        );
    }
}
