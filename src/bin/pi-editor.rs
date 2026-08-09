//! `abelian-pi-editor`: the hook pi's text editor calls, and nothing else.
//!
//! This binary is the *purpose-built* integration point between the pi coding
//! agent and the abelian substrate.  pi's built-in `edit` and `write` tools
//! are overridden (see `pi/abelian-editor.ts`) so that every file mutation the
//! agent performs is routed here instead of touching the filesystem directly.
//!
//! It is deliberately thin.  It does not reimplement abelian: it translates a
//! harness tool call into an abelian [`Intent`] and hands it to
//! [`Repository::apply`], which performs the exact-match precondition check,
//! the membership adjudication, the sum arithmetic, the durable log append,
//! and the best-effort working-tree refresh.  The one thing this adapter adds
//! is the thing ANDON.md §8 says the harness is uniquely able to add: the
//! observed **read set**.  The `old_str` a str-replace edit names *is* a read;
//! a whole-file overwrite reads the whole prior blob; a create reads nothing.
//! We record that here as exhaust, because the harness saw it.
//!
//! Protocol (kept minimal on purpose):
//!
//! ```text
//! abelian-pi-editor edit  < {"path": "...", "edits": [{"oldText": "...", "newText": "..."}]}
//! abelian-pi-editor write < {"path": "...", "content": "..."}
//! ```
//!
//! On success it prints one JSON object to stdout describing the applied
//! patch; on any precondition or I/O failure it prints the substrate's own
//! error to stderr and exits non-zero, so pi surfaces it to the model verbatim.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::exit;

use serde::Deserialize;
use serde_json::json;

use abelian::log::{Annotation, Provenance};
use abelian::patch::{Intent, Op};
use abelian::repo::Repository;
use abelian::{Error, Result};

/// pi's `edit` tool input shape (see pi's `core/tools/edit.js`).
#[derive(Deserialize)]
struct EditInput {
    path: String,
    edits: Vec<EditSpan>,
}

#[derive(Deserialize)]
struct EditSpan {
    #[serde(rename = "oldText")]
    old_text: String,
    #[serde(rename = "newText")]
    new_text: String,
}

/// pi's `write` tool input shape (see pi's `core/tools/write.js`).
#[derive(Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("edit") => run_edit(),
        Some("write") => run_write(),
        _ => {
            eprintln!(
                "USAGE: abelian-pi-editor <edit|write>\n\
                 reads pi's tool-call JSON on stdin; applies it as an abelian patch"
            );
            exit(64);
        }
    };
    if let Err(err) = result {
        // Speak the substrate's vocabulary straight through to the model.
        eprintln!("{err}");
        exit(1);
    }
}

/// Read all of stdin.
fn stdin_bytes() -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buf)
        .map_err(abelian::ioerr("reading tool-call json on stdin"))?;
    Ok(buf)
}

/// Discover the abelian repository from the current working directory.
fn repo() -> Result<Repository> {
    let cwd = std::env::current_dir().map_err(abelian::ioerr("getting cwd"))?;
    Repository::discover(cwd)
}

/// Convert a filesystem path (relative to cwd or absolute) into an abelian
/// element path: absolute from the repository root, beginning with `/`.
fn element_path(repo: &Repository, raw: &str) -> Result<String> {
    let cwd = std::env::current_dir().map_err(abelian::ioerr("getting cwd"))?;
    let abs: PathBuf = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    let rel = abs.strip_prefix(repo.root()).map_err(|_| {
        Error::Invalid(format!(
            "path {raw:?} is outside the repository root {}",
            repo.root().display()
        ))
    })?;
    let mut out = String::from("/");
    out.push_str(&rel.to_string_lossy().replace('\\', "/"));
    Ok(out)
}

/// The fork (session) this edit belongs to.  pi exposes its session id in the
/// environment; abelian treats sessions and forks as the same object, so we
/// use it when present and fall back to `main`.  The name becomes a directory
/// under `forks/`, so we validate it against abelian's own fork-name charset
/// and reject a path-hostile session id loudly rather than silently mangling
/// it.
fn fork() -> Result<String> {
    let name = std::env::var("ABELIAN_FORK")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string());
    abelian::fork::validate_fork_name(&name)?;
    Ok(name)
}

/// The fork a freshly auto-initialized session forks from.  Defaults to
/// `main`; override with `ABELIAN_FORK_FROM` to seat a session on another
/// fork's current state.
fn fork_from() -> String {
    std::env::var("ABELIAN_FORK_FROM")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Auto-initialize the session's fork so the first write does not fail on an
/// absent log.  A pi session is a fork (§2.3); if it has never been created,
/// bring it into being anchored at `ABELIAN_FORK_FROM` (default `main`).  A
/// no-op once the fork exists.
fn ensure_fork(repo: &Repository, fork: &str) -> Result<()> {
    repo.ensure_fork(fork, &fork_from())?;
    Ok(())
}

/// Build the annotation, including the observed read set (the whole point).
fn annotation(reads: serde_json::Value) -> Annotation {
    Annotation {
        author: std::env::var("ABELIAN_AUTHOR").unwrap_or_else(|_| "pi-agent".to_string()),
        provenance: Provenance::Agent,
        session: std::env::var("PI_SESSION_ID").ok(),
        prose: std::env::var("ABELIAN_PROSE").ok(),
        reads: Some(reads),
        ..Default::default()
    }
}

/// Print the success result pi will render as the tool output.
fn report(op: &str, path: &str, id: &str, sum: &str) {
    println!(
        "{}",
        json!({ "ok": true, "op": op, "path": path, "id": id, "sum": sum })
    );
}

/// `edit`: one abelian `edit` op per str-replace span; every `old_str` is a
/// recorded read at the file's current blob.
fn run_edit() -> Result<()> {
    let input: EditInput = serde_json::from_slice(&stdin_bytes()?)?;
    let repo = repo()?;
    let path = element_path(&repo, &input.path)?;
    let fork = fork()?;
    ensure_fork(&repo, &fork)?;

    // The blob we are reading against, for the read-set record.
    let state = repo.current_state(&fork)?;
    let blob = state
        .manifest
        .get(&path)
        .map(|r| r.blob.clone())
        .ok_or_else(|| Error::Precondition(format!("edit of absent path: {path}")))?;

    let mut spans = Vec::new();
    let mut ops = Vec::new();
    for span in input.edits {
        spans.push(json!(span.old_text));
        ops.push(Op::Edit {
            path: path.clone(),
            old_str: span.old_text,
            new_str: span.new_text,
        });
    }

    let reads = json!([{ "path": path, "blob": blob, "spans": spans }]);
    let line = repo.apply(&fork, Intent { ops }, annotation(reads))?;
    report("edit", &path, &line.id, &line.sum_after);
    Ok(())
}

/// `write`: create when the path is absent, otherwise a whole-file overwrite
/// expressed as delete-then-create (the delete carries the prior blob hash as
/// its precondition, which is exactly the whole-file read the harness saw).
fn run_write() -> Result<()> {
    let input: WriteInput = serde_json::from_slice(&stdin_bytes()?)?;
    let repo = repo()?;
    let path = element_path(&repo, &input.path)?;
    let fork = fork()?;
    ensure_fork(&repo, &fork)?;
    let content_b64 = abelian::b64::encode(input.content.as_bytes());

    let state = repo.current_state(&fork)?;
    let existing = state.manifest.get(&path).cloned();

    let (ops, reads) = match existing {
        None => {
            let ops = vec![Op::Create {
                path: path.clone(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(content_b64),
            }];
            // A create reads nothing: honest empty read set.
            (ops, json!([]))
        }
        Some(record) => {
            let ops = vec![
                Op::Delete { path: path.clone(), blob: record.blob.clone() },
                Op::Create {
                    path: path.clone(),
                    mode: record.mode.clone(),
                    blob: None,
                    content_b64: Some(content_b64),
                },
            ];
            // Overwriting reads the whole prior blob.
            let reads = json!([{ "path": path, "blob": record.blob, "spans": ["<whole file>"] }]);
            (ops, reads)
        }
    };

    let line = repo.apply(&fork, Intent { ops }, annotation(reads))?;
    report("write", &path, &line.id, &line.sum_after);
    Ok(())
}
