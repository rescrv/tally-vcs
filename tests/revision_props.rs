//! Property tests over the revision language (src/revision.rs).
//!
//! The risky paths: `split_suffixes` parses hostile specs; `apply_suffixes`
//! does lineage index arithmetic that must never underflow into a panic or
//! silently wrap; and `resolve`'s base heuristic dispatches across three
//! hash domains (fork names, state sums, line ids) with global fallbacks.
//!
//! One fixture repo is built once and shared: `resolve` is read-only, and
//! `apply` is fsync-bound, so the repo lives outside the per-case loop.  The
//! lineage model is maintained independently from the recorded `LogLine`s,
//! and every generated spec is checked against a reference interpreter.

use std::sync::OnceLock;

use proptest::prelude::*;

use tally::b64;
use tally::ident::is_hex64;
use tally::log::Annotation;
use tally::patch::{Intent, Op};
use tally::repo::Repository;
use tally::revision::resolve;

/////////////////////////////////////////// the fixture ///////////////////////////////////////////

/// One point on a lineage, as the model sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Point {
    sum: String,
    line: Option<String>,
}

/// A base spec the generator may choose, with the model's answer for it:
/// which lineage it starts on, at which index, and the fork label the
/// resolution must report.
#[derive(Clone, Debug)]
struct Base {
    spec: String,
    lineage: Lineage,
    start: usize,
    fork: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lineage {
    Main,
    Session,
}

struct Fixture {
    repo: Repository,
    /// main's lineage: base anchor (zero) then six lines.
    main: Vec<Point>,
    /// session's lineage: base anchor, main's first three lines, two more.
    session: Vec<Point>,
    bases: Vec<Base>,
}

impl Fixture {
    fn points(&self, lineage: Lineage) -> &[Point] {
        match lineage {
            Lineage::Main => &self.main,
            Lineage::Session => &self.session,
        }
    }
}

fn create(path: &str, content: &[u8]) -> Intent {
    Intent {
        ops: vec![Op::Create {
            path: path.to_string(),
            mode: "100644".to_string(),
            blob: None,
            content_b64: Some(b64::encode(content)),
        }],
    }
}

fn note() -> Annotation {
    Annotation { author: "rev-props".to_string(), ..Annotation::default() }
}

fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let dir = std::env::temp_dir()
            .join(format!("tally-revision-props-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::init(&dir).unwrap();

        let zero = "0".repeat(64);
        let mut main = vec![Point { sum: zero.clone(), line: None }];
        for i in 1..=3 {
            let line = repo
                .apply("main", create(&format!("/m{i}"), format!("m{i}\n").as_bytes()), note())
                .unwrap();
            main.push(Point { sum: line.sum_after, line: Some(line.id) });
        }
        // Fork at three commits; session inherits main's first three lines.
        repo.create_fork("session", "main").unwrap();
        let mut session = main.clone();
        for i in 4..=6 {
            let line = repo
                .apply("main", create(&format!("/m{i}"), format!("m{i}\n").as_bytes()), note())
                .unwrap();
            main.push(Point { sum: line.sum_after, line: Some(line.id) });
        }
        for i in 1..=2 {
            let line = repo
                .apply("session", create(&format!("/s{i}"), format!("s{i}\n").as_bytes()), note())
                .unwrap();
            session.push(Point { sum: line.sum_after, line: Some(line.id) });
        }

        // Every base spelling the resolver documents, with the model answer.
        // The default fork is always "main" in these tests.
        let mut bases = vec![
            Base { spec: "HEAD".into(), lineage: Lineage::Main, start: main.len() - 1, fork: "main" },
            Base { spec: "@".into(), lineage: Lineage::Main, start: main.len() - 1, fork: "main" },
            Base { spec: "main".into(), lineage: Lineage::Main, start: main.len() - 1, fork: "main" },
            Base {
                spec: "session".into(),
                lineage: Lineage::Session,
                start: session.len() - 1,
                fork: "session",
            },
            // The base anchor's sum (all zeros) is a state on the lineage.
            Base { spec: zero.clone(), lineage: Lineage::Main, start: 0, fork: "main" },
            Base { spec: format!("sum:{zero}"), lineage: Lineage::Main, start: 0, fork: "main" },
        ];
        for (i, p) in main.iter().enumerate().skip(1) {
            let id = p.line.as_ref().unwrap();
            // Bare sum, sum:, bare full id, line: — all on the default fork.
            bases.push(Base { spec: p.sum.clone(), lineage: Lineage::Main, start: i, fork: "main" });
            bases.push(Base {
                spec: format!("sum:{}", p.sum),
                lineage: Lineage::Main,
                start: i,
                fork: "main",
            });
            bases.push(Base { spec: id.clone(), lineage: Lineage::Main, start: i, fork: "main" });
            bases.push(Base {
                spec: format!("line:{id}"),
                lineage: Lineage::Main,
                start: i,
                fork: "main",
            });
        }
        // Session-only points (indexes past the shared prefix): line ids are
        // globally unique so they resolve from main via the repo-wide
        // search; sums require the explicit sum: prefix to search globally
        // (a bare off-fork sum does not resolve — asserted separately).
        for (i, p) in session.iter().enumerate().skip(4) {
            let id = p.line.as_ref().unwrap();
            bases.push(Base {
                spec: id.clone(),
                lineage: Lineage::Session,
                start: i,
                fork: "session",
            });
            bases.push(Base {
                spec: format!("line:{id}"),
                lineage: Lineage::Session,
                start: i,
                fork: "session",
            });
            bases.push(Base {
                spec: format!("sum:{}", p.sum),
                lineage: Lineage::Session,
                start: i,
                fork: "session",
            });
        }
        Fixture { repo, main, session, bases }
    })
}

////////////////////////////////////// reference interpreter //////////////////////////////////////

/// One suffix operator, as the generator produces it.
#[derive(Clone, Debug)]
enum Suffix {
    Caret,
    Tilde(usize),
    AtN(usize),
}

impl Suffix {
    fn render(&self) -> String {
        match self {
            Suffix::Caret => "^".to_string(),
            Suffix::Tilde(n) => format!("~{n}"),
            Suffix::AtN(n) => format!("@{{{n}}}"),
        }
    }
}

fn arb_suffix() -> impl Strategy<Value = Suffix> {
    prop_oneof![
        Just(Suffix::Caret),
        (0usize..9).prop_map(Suffix::Tilde),
        (0usize..9).prop_map(Suffix::AtN),
    ]
}

/// The documented semantics: `^`/`~N` step back toward the base and error
/// past it; `@{N}` selects the state N back from head and errors past the
/// base.  Returns the final index, or None for "must error".
fn interpret(start: usize, len: usize, suffixes: &[Suffix]) -> Option<usize> {
    let mut idx = start;
    for s in suffixes {
        match s {
            Suffix::Caret => idx = idx.checked_sub(1)?,
            Suffix::Tilde(n) => idx = idx.checked_sub(*n)?,
            Suffix::AtN(n) => {
                if *n >= len {
                    return None;
                }
                idx = len - 1 - n;
            }
        }
    }
    Some(idx)
}

//////////////////////////////////////////// properties ///////////////////////////////////////////

proptest! {
    /// Any documented base followed by any chain of suffix operators agrees
    /// with the reference interpreter: same state, same line, same fork
    /// label — and the walks that leave the lineage error instead of
    /// panicking or wrapping.
    #[test]
    fn suffix_walks_match_model(
        base_ix in any::<prop::sample::Index>(),
        suffixes in prop::collection::vec(arb_suffix(), 0..5),
    ) {
        let fx = fixture();
        let base = &fx.bases[base_ix.index(fx.bases.len())];
        let points = fx.points(base.lineage);
        let spec: String = std::iter::once(base.spec.clone())
            .chain(suffixes.iter().map(Suffix::render))
            .collect();
        match interpret(base.start, points.len(), &suffixes) {
            Some(idx) => {
                let r = resolve(&fx.repo, &spec, "main").unwrap();
                prop_assert_eq!(&r.sum, &points[idx].sum, "spec {}", spec);
                prop_assert_eq!(&r.line, &points[idx].line, "spec {}", spec);
                prop_assert_eq!(&r.fork, base.fork, "spec {}", spec);
            }
            None => {
                let denied = resolve(&fx.repo, &spec, "main");
                prop_assert!(denied.is_err(), "spec {} should walk off the lineage", spec);
            }
        }
    }

    /// `~N` is N carets, for every N — including past the base, where both
    /// spellings must fail alike.
    #[test]
    fn tilde_is_iterated_caret(n in 0usize..10) {
        let fx = fixture();
        let carets: String = std::iter::once("HEAD".to_string())
            .chain((0..n).map(|_| "^".to_string()))
            .collect();
        let a = resolve(&fx.repo, &format!("HEAD~{n}"), "main");
        let b = resolve(&fx.repo, &carets, "main");
        match (a, b) {
            (Ok(a), Ok(b)) => prop_assert_eq!(a, b),
            (Err(_), Err(_)) => {}
            (a, b) => prop_assert!(false, "~{} diverged from carets: {:?} vs {:?}", n, a, b),
        }
    }

    /// Suffix arithmetic is additive: `HEAD~a~b` is `HEAD~(a+b)`.
    #[test]
    fn tilde_is_additive(a in 0usize..8, b in 0usize..8) {
        let fx = fixture();
        let chained = resolve(&fx.repo, &format!("HEAD~{a}~{b}"), "main");
        let summed = resolve(&fx.repo, &format!("HEAD~{}", a + b), "main");
        match (chained, summed) {
            (Ok(x), Ok(y)) => prop_assert_eq!(x, y),
            (Err(_), Err(_)) => {}
            (x, y) => prop_assert!(false, "~{}~{} diverged from ~{}: {:?} vs {:?}", a, b, a + b, x, y),
        }
    }

    /// From head, `@{N}` and `~N` agree — including their error domain.
    #[test]
    fn at_n_is_tilde_n_from_head(n in 0usize..10) {
        let fx = fixture();
        let at = resolve(&fx.repo, &format!("HEAD@{{{n}}}"), "main");
        let tilde = resolve(&fx.repo, &format!("HEAD~{n}"), "main");
        match (at, tilde) {
            (Ok(x), Ok(y)) => prop_assert_eq!(x, y),
            (Err(_), Err(_)) => {}
            (x, y) => prop_assert!(false, "@{{{}}} diverged from ~{}: {:?} vs {:?}", n, n, x, y),
        }
    }

    /// Hostile specs never panic, and anything that resolves names a real
    /// state: a 64-hex sum on a fork the repo knows.
    #[test]
    fn hostile_specs_never_panic(spec in "\\PC{0,24}") {
        let fx = fixture();
        if let Ok(r) = resolve(&fx.repo, &spec, "main") {
            prop_assert!(is_hex64(&r.sum));
            prop_assert!(fx.repo.fork_names().unwrap().contains(&r.fork));
        }
    }

    /// The same, over the suffix-operator alphabet, to hammer the parser's
    /// edge cases: unclosed braces, bare tildes, stacked @s, stray colons.
    #[test]
    fn hostile_suffix_soup_never_panics(spec in "[~^@{}0-9a-f:HEADmainses]{0,20}") {
        let fx = fixture();
        if let Ok(r) = resolve(&fx.repo, &spec, "main") {
            prop_assert!(is_hex64(&r.sum));
        }
    }
}

///////////////////////////////////// deterministic edge cases ////////////////////////////////////

/// Every prefix of every line id resolves per the documented algorithm:
/// unique on the default fork → that line; ambiguous on the default fork →
/// error; absent from the default fork → global search (unique → the
/// authoring fork, ambiguous → error).  Exhaustive over all prefix lengths.
#[test]
fn id_prefixes_resolve_per_the_algorithm() {
    let fx = fixture();
    let all: Vec<(&str, Lineage, usize)> = fx
        .main
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.line.as_deref().map(|id| (id, Lineage::Main, i)))
        .chain(
            fx.session
                .iter()
                .enumerate()
                .skip(4)
                .filter_map(|(i, p)| p.line.as_deref().map(|id| (id, Lineage::Session, i))),
        )
        .collect();
    for (id, _, _) in &all {
        for len in 1..=id.len() {
            let prefix = &id[..len];
            let on_main: Vec<&(&str, Lineage, usize)> = all
                .iter()
                .filter(|(i, l, _)| *l == Lineage::Main && i.starts_with(prefix))
                .collect();
            let anywhere: Vec<&(&str, Lineage, usize)> =
                all.iter().filter(|(i, _, _)| i.starts_with(prefix)).collect();
            // A 64-hex prefix is the whole id; shorter prefixes could in
            // principle collide with a sum, but the resolver only prefers
            // sums for exact 64-hex matches on the lineage, which our
            // distinct-content fixture cannot produce for a proper prefix.
            let result = resolve(&fx.repo, prefix, "main");
            match (on_main.len(), anywhere.len()) {
                (1, _) => {
                    let (full, lineage, idx) = on_main[0];
                    let r = result.unwrap_or_else(|e| {
                        panic!("unique-on-main prefix {prefix:?} failed: {e}")
                    });
                    assert_eq!(r.line.as_deref(), Some(*full));
                    assert_eq!(r.sum, fx.points(*lineage)[*idx].sum);
                }
                (0, 1) => {
                    let (full, lineage, idx) = anywhere[0];
                    let r = result.unwrap_or_else(|e| {
                        panic!("globally unique prefix {prefix:?} failed: {e}")
                    });
                    assert_eq!(r.line.as_deref(), Some(*full));
                    assert_eq!(r.fork, "session");
                    assert_eq!(r.sum, fx.points(*lineage)[*idx].sum);
                }
                _ => assert!(
                    result.is_err(),
                    "ambiguous or absent prefix {prefix:?} resolved to {result:?}"
                ),
            }
        }
    }
}

/// Bare off-fork sums do not resolve (a sum is lineage-relative); the
/// explicit `sum:` prefix searches globally.  Domain prefixes never
/// reinterpret across domains.
#[test]
fn domain_prefixes_are_strict() {
    let fx = fixture();
    let s1 = &fx.session[4];
    let s1_id = s1.line.as_deref().unwrap();
    // Bare session sum from main: no resolution.
    assert!(resolve(&fx.repo, &s1.sum, "main").is_err());
    // sum:-prefixed, it resolves globally and reports the fork read on.
    let r = resolve(&fx.repo, &format!("sum:{}", s1.sum), "main").unwrap();
    assert_eq!(r.fork, "session");
    assert_eq!(r.sum, s1.sum);
    // sum: never reinterprets a line id, even a real one.
    assert!(resolve(&fx.repo, &format!("sum:{s1_id}"), "main").is_err());
    // line: never reinterprets a sum, even a real one.
    assert!(resolve(&fx.repo, &format!("line:{}", s1.sum), "main").is_err());
    // sum: demands 64-hex.
    assert!(resolve(&fx.repo, "sum:", "main").is_err());
    assert!(resolve(&fx.repo, "sum:abc", "main").is_err());
    // line: demands an id.
    assert!(resolve(&fx.repo, "line:", "main").is_err());
}

/// Malformed suffixes are errors, not panics and not silent HEADs.
#[test]
fn malformed_suffixes_are_rejected() {
    let fx = fixture();
    for spec in [
        "",          // empty revision
        "HEAD~",     // ~ without a count
        "HEAD~x",    // ~ with a non-count
        "HEAD@{}",   // @{} without a count
        "HEAD@{x}",  // only numeric @{N} is supported
        "HEAD@{1",   // unclosed brace
        "HEAD$",     // stray character
        "HEAD^~",    // valid then invalid
    ] {
        assert!(resolve(&fx.repo, spec, "main").is_err(), "accepted {spec:?}");
    }
    // Suffixes on domain-prefixed bases walk the same lineage.
    let head = resolve(&fx.repo, "HEAD", "main").unwrap();
    let back = resolve(&fx.repo, "HEAD^", "main").unwrap();
    let by_sum = resolve(&fx.repo, &format!("sum:{}^", head.sum), "main").unwrap();
    assert_eq!(by_sum.sum, back.sum);
    let by_line = resolve(&fx.repo, &format!("line:{}^", head.line.as_deref().unwrap()), "main")
        .unwrap();
    assert_eq!(by_line.sum, back.sum);
}
