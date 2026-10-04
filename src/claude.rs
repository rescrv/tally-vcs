//! Claude Code: ingesting a whole coding session into the substrate, after
//! the fact.
//!
//! pi can be handed a replacement editor, so every mutation enters tally as
//! it happens (see `.pi/extensions/tally-editor.ts` and `tally-pi-editor`).
//! Claude Code cannot: its tool set is fixed, and a hook only observes a call
//! once it has already touched the file.  So this module takes the other
//! route available to a fixed toolchain — it reads the session back.  Claude
//! Code writes a JSONL transcript of every entry in the session, and an
//! `Edit`/`MultiEdit`/`Write` call records exactly the fields tally wants:
//! `old_string` is the span precondition, `content` is the whole-file
//! overwrite, `file_path` is the element.  The transcript is the exhaust; this
//! module is the funnel.
//!
//! Three things make the funnel honest.
//!
//! **A session is a fork** (§2.3), which tally does not distinguish from a
//! branch.  So the session lands on `claude-<session id>`, and it lands as
//! *many* lines — one per observed tool call, in the order the harness made
//! them — not one squashed commit.  The fine structure is the point; `tally
//! fuse` collapses it into a narrative beat afterwards, losslessly (§2.6).
//!
//! **The fork goes back to where the session uniquely originates.**  A
//! resumed or continued session begins with entries copied from the session
//! it continues; those entries are another fork's work and ingesting them
//! again would duplicate it.  The origin is therefore the first entry in the
//! transcript that no sibling transcript carries — the first entry that is
//! uniquely this session's — and the fork is anchored on the fork of the
//! session those inherited entries belong to when it exists, so the lineage
//! of sessions is the lineage of forks and `tally log` reads back through
//! both.
//!
//! **What the session cannot explain is reconciled, not invented.**  The
//! transcript knows about `Edit`, `MultiEdit` and `Write`; it does not know
//! what a `cargo fmt` run inside `Bash` did, nor what a `NotebookEdit` wrote,
//! nor what was already pending in the working tree before the session
//! started.  So after replaying every call this module diffs the
//! fork against the working tree and appends one final line for the residue:
//! the same patch `tally commit` would make, carrying no read set, because
//! none was observed.  The fork then *equals* the working tree, which is the
//! reconciliation the operation promises.
//!
//! **What a shell run rewrote of the session's own spans folds back into
//! the span's line.**  A formatter is the common case: the session writes
//! an edit, a `cargo fmt` inside `Bash` (or a hook, which the transcript
//! cannot tell from one) reformats exactly what the edit wrote, and the
//! reconciling line ends up re-touching lines the session just landed —
//! churn with an owner, recorded ownerless.  So before landing, the replay
//! diffs its reconstruction against the working tree and *aligns* each
//! difference that falls entirely within one op's `new_str` back into that
//! op: the edit lands the way the tree holds it, and the reconciling patch
//! keeps only what no edit explains — the formatter's footprint on
//! pre-existing code, if any.  Alignment never invents: it rewrites only
//! bytes no other call names, the replay re-validates the aligned intent,
//! and a landed line that was adjusted says so ([`Landed::aligned`]).
//!
//! **A call that only rewrites what an earlier call wrote folds into it.**
//! The model writes a span and then fixes it; recorded call by call, the
//! log shows the same lines rewritten twice.  When every span of a later
//! call falls within bytes one earlier call of the same ingest wrote, the
//! earlier line lands the final form and the later call lands no line of
//! its own — it is listed in the earlier line's [`HarnessCall::absorbed`],
//! so a later ingest counts it recorded.  The minimal diff wins over a
//! faithful call-by-call record; a span reaching past the earlier call's
//! bytes still lands as written, so no line gains a precondition its call
//! never named.
//!
//! Every line records the call behind it ([`HarnessCall`]), so a second run
//! over the same session appends only what is new.
//!
//! One shape is translated rather than accommodated: an `Edit` with
//! `replace_all` has no counterpart in the format — tally's `Op::Edit`
//! consumes a span that occurs exactly once, on purpose — so such a call is
//! materialized as the series of uniquely-anchored edits that produces the
//! same bytes (see [`crate::patch::anchored_replace_all`]).  The resulting
//! line still re-validates span by span, which is what lets it travel.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::blobs::BlobStore;
use crate::log::{Annotation, HarnessCall, LogLine, Provenance};
use crate::manifest::Manifest;
use crate::patch::{Intent, Op, anchored_replace_all, apply_intent, find_unique, replace_unique};
use crate::repo::Repository;
use crate::{Error, Result, ioerr};

/// The harness this module speaks for, as recorded in [`HarnessCall`].
pub const HARNESS: &str = "claude";

/// The author stamped on ingested lines when none is given.
pub const DEFAULT_AUTHOR: &str = "claude-code";

//////////////////////////////////////////// discovery ////////////////////////////////////////////

/// Claude Code's configuration directory: `$CLAUDE_CONFIG_DIR`, else
/// `~/.claude`.
pub fn config_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .ok_or_else(|| {
            Error::Invalid(
                "cannot find Claude Code's transcripts: neither CLAUDE_CONFIG_DIR nor HOME is set"
                    .to_string(),
            )
        })?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// The session Claude Code is running right now, from the environment it
/// exports to its own tools.  `None` outside a session.
pub fn current_session() -> Option<String> {
    for key in ["CLAUDE_CODE_SESSION_ID", "CLAUDE_SESSION_ID"] {
        if let Ok(id) = std::env::var(key)
            && !id.is_empty()
        {
            return Some(id);
        }
    }
    None
}

/// Find a session's transcript under `<config>/projects/*/<session>.jsonl`.
///
/// Claude Code files transcripts by a slug of the directory the session was
/// started in, and a session may have been started anywhere — in a
/// subdirectory of the repository, or in a parent of it.  The file name is the
/// session id, which is unique, so searching the project directories finds the
/// transcript without having to reproduce the slug rule.
pub fn find_transcript(session: &str) -> Result<PathBuf> {
    let projects = config_dir()?.join("projects");
    let name = format!("{session}.jsonl");
    let entries =
        std::fs::read_dir(&projects).map_err(ioerr(format!("listing {}", projects.display())))?;
    for entry in entries {
        let entry = entry.map_err(ioerr("listing Claude Code projects"))?;
        let candidate = entry.path().join(&name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(Error::Invalid(format!(
        "no transcript for session {session} under {}; name one with --transcript",
        projects.display()
    )))
}

//////////////////////////////////////////// transcript ///////////////////////////////////////////

/// A session's transcript, parsed, with the point it stops being someone
/// else's work marked.
pub struct Transcript {
    /// The session id.
    pub session: String,
    /// The file it was read from.
    pub path: PathBuf,
    /// Every entry, in file order — which is observation order: the
    /// transcript is append-only.
    pub entries: Vec<Value>,
    /// The index of the first entry uniquely this session's; `entries.len()`
    /// when the session carries nothing of its own.
    pub origin: usize,
    /// The session the entries before `origin` were inherited from, if any.
    pub inherited: Option<String>,
}

/// Read a transcript and locate the session's unique origin.
///
/// A transcript line that fails to parse is skipped rather than fatal: the
/// file is a live append-only log and its final line may be half-written, the
/// same way [`Repository::read_log`] tolerates a torn tail.
pub fn read_transcript(path: &Path, session: &str) -> Result<Transcript> {
    let text = std::fs::read_to_string(path)
        .map_err(ioerr(format!("reading transcript {}", path.display())))?;
    let entries: Vec<Value> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    // Every uuid any sibling transcript carries.  Claude Code copies the
    // entries of a session it continues into the new session's transcript, so
    // an entry another transcript also holds is inherited work, not ours.
    let mut inherited_uuids = BTreeSet::new();
    if let Some(dir) = path.parent() {
        let siblings =
            std::fs::read_dir(dir).map_err(ioerr(format!("listing {}", dir.display())))?;
        for sibling in siblings {
            let sibling = sibling.map_err(ioerr("listing transcripts"))?.path();
            if sibling == path || sibling.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&sibling) else {
                continue;
            };
            for line in text.lines() {
                if let Ok(entry) = serde_json::from_str::<Value>(line)
                    && let Some(uuid) = str_field(&entry, "uuid")
                {
                    inherited_uuids.insert(uuid.to_string());
                }
            }
        }
    }
    // The origin: the first entry that is uniquely ours — one no sibling
    // transcript carries and that the session did not stamp with another
    // session's id.
    let unique = |entry: &Value| -> bool {
        let Some(uuid) = str_field(entry, "uuid") else {
            return false;
        };
        if inherited_uuids.contains(uuid) {
            return false;
        }
        match str_field(entry, "sessionId") {
            Some(id) => id == session,
            None => true,
        }
    };
    let origin = entries.iter().position(unique).unwrap_or(entries.len());
    // Who the inherited prefix belongs to: the session of the newest entry
    // before the origin that names one.
    let inherited = entries[..origin]
        .iter()
        .rev()
        .find_map(|entry| str_field(entry, "sessionId"))
        .filter(|id| *id != session)
        .map(str::to_string);
    Ok(Transcript {
        session: session.to_string(),
        path: path.to_path_buf(),
        entries,
        origin,
        inherited,
    })
}

/// A string field of a JSON object, if it is there and is a string.
fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/////////////////////////////////////////////// calls /////////////////////////////////////////////

/// One span replacement an editor performed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The text replaced.
    pub old: String,
    /// What replaced it.
    pub new: String,
    /// The call asked for every occurrence, not one.  tally has no such op
    /// (see [`crate::patch::anchored_replace_all`]), so the span is
    /// materialized as a series of uniquely-anchored edits instead.
    pub all: bool,
}

/// What an observed tool call did to one element.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Wrote {
    /// Span replacements, in order: `Edit` and `MultiEdit` alike.
    Spans(Vec<Span>),
    /// A whole-file write: create, or overwrite.
    Whole(String),
}

/// One file-mutating tool call, as the transcript recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    /// The transcript entry it appeared in.
    pub entry: String,
    /// The call's id.
    pub id: String,
    /// The tool: `Edit`, `MultiEdit`, or `Write`.
    pub tool: String,
    /// The element path it wrote.
    pub path: String,
    /// What it wrote.
    pub wrote: Wrote,
    /// The assistant's narrative for the turn, claimed by the turn's first
    /// mutation.
    pub prose: Option<String>,
    /// Element paths the session read before this call and that no earlier
    /// call has carried yet.
    pub reads: Vec<String>,
}

impl Call {
    /// A one-line label for reporting: `Edit /src/main.rs`.
    pub fn label(&self) -> String {
        format!("{} {}", self.tool, self.path)
    }
}

/// The file-mutating calls a transcript observed from its origin onward, in
/// order, each carrying the reads and prose the harness saw around it.
///
/// Calls whose tool reported an error are left out — the file was not
/// written, so there is nothing to record.  Calls against paths outside the
/// repository are left out for the same reason: no element, no record.
/// Everything else the session did (`Bash`, `NotebookEdit`, a write outside
/// the tree) is invisible here by construction and lands in the
/// reconciliation instead.
pub fn calls(repo: &Repository, transcript: &Transcript) -> Vec<Call> {
    let errored = errored_calls(&transcript.entries);
    let mut out: Vec<Call> = Vec::new();
    let mut pending_reads: Vec<String> = Vec::new();
    // The narrative, waiting for the change it describes.  Claude Code writes
    // one transcript entry per content block, so the sentence the model wrote
    // before acting is in an entry of its own — usually the same turn as the
    // call, sometimes the turn before it, and the entry carrying the call has
    // no text at all.  So prose is carried forward until a mutation claims it:
    // the first mutation after the narration takes it, its siblings carry
    // none.  Unclaimed narration accumulates (a turn can narrate without
    // touching a file); a claimed one leaves nothing behind.
    let mut pending_prose: Option<String> = None;
    for entry in &transcript.entries[transcript.origin..] {
        if str_field(entry, "type") != Some("assistant") {
            continue;
        }
        let Some(blocks) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        let uuid = str_field(entry, "uuid").unwrap_or("").to_string();
        let cwd = str_field(entry, "cwd").unwrap_or(".");
        if let Some(text) = block_text(blocks) {
            pending_prose = Some(match pending_prose.take() {
                Some(prior) => format!("{prior}\n\n{text}"),
                None => text,
            });
        }
        for block in blocks {
            if str_field(block, "type") != Some("tool_use") {
                continue;
            }
            let (Some(id), Some(tool)) = (str_field(block, "id"), str_field(block, "name")) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&Value::Null);
            // A read is exhaust too: the harness saw this file enter the
            // model's context, so the next recorded line carries it.
            if tool == "Read"
                && let Some(raw) = str_field(input, "file_path")
                && let Ok(path) = element_path(repo, cwd, raw)
            {
                pending_reads.push(path);
                continue;
            }
            if errored.contains(id) {
                continue;
            }
            let Some((path, wrote)) = observed_write(repo, cwd, tool, input) else {
                continue;
            };
            out.push(Call {
                entry: uuid.clone(),
                id: id.to_string(),
                tool: tool.to_string(),
                path,
                wrote,
                prose: pending_prose.take(),
                reads: std::mem::take(&mut pending_reads),
            });
        }
    }
    out
}

/// The concatenated text blocks of one entry, or `None` when it carries none.
/// Thinking is not prose: it is not addressed to anyone, and the annotation's
/// `prose` is the narrative for humans — so a session that narrates nothing
/// records nothing rather than something faked up from reasoning.
fn block_text(blocks: &[Value]) -> Option<String> {
    let mut text = String::new();
    for block in blocks {
        if str_field(block, "type") == Some("text")
            && let Some(part) = str_field(block, "text")
        {
            text.push_str(part);
        }
    }
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The tool-call ids whose result came back an error: the call did not write.
fn errored_calls(entries: &[Value]) -> BTreeSet<String> {
    let mut errored = BTreeSet::new();
    for entry in entries {
        let Some(blocks) = entry.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for block in blocks {
            if str_field(block, "type") == Some("tool_result")
                && block.get("is_error").and_then(Value::as_bool) == Some(true)
                && let Some(id) = str_field(block, "tool_use_id")
            {
                errored.insert(id.to_string());
            }
        }
    }
    errored
}

/// Translate one tool call into the element path and the write it performed,
/// or `None` when the call is not a file mutation this module can carry.
fn observed_write(
    repo: &Repository,
    cwd: &str,
    tool: &str,
    input: &Value,
) -> Option<(String, Wrote)> {
    let path = element_path(repo, cwd, str_field(input, "file_path")?).ok()?;
    let span = |value: &Value| -> Option<Span> {
        Some(Span {
            old: str_field(value, "old_string")?.to_string(),
            new: str_field(value, "new_string")?.to_string(),
            all: value
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    };
    match tool {
        "Edit" => Some((path, Wrote::Spans(vec![span(input)?]))),
        "MultiEdit" => {
            let mut spans = Vec::new();
            for edit in input.get("edits")?.as_array()? {
                spans.push(span(edit)?);
            }
            (!spans.is_empty()).then_some((path, Wrote::Spans(spans)))
        }
        "Write" => Some((path, Wrote::Whole(str_field(input, "content")?.to_string()))),
        _ => None,
    }
}

/// Convert a path a tool reported — absolute, or relative to the session's
/// working directory — into an element path (§1.1).  Mirrors
/// `tally-pi-editor`'s own `element_path`.
fn element_path(repo: &Repository, cwd: &str, raw: &str) -> Result<String> {
    let abs = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        PathBuf::from(cwd).join(raw)
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

/////////////////////////////////////////////// intent ////////////////////////////////////////////

/// The intent one observed call realizes against the state it meets.
///
/// The ops are the tool call's own fields, not a diff of the bytes it left
/// behind: `old_string` *is* the span precondition, so replaying it
/// re-validates the same thing the editor checked, and the resulting line
/// travels by union stratum 3 instead of landing only arithmetically.
fn intent_for(blobs: &BlobStore, manifest: &Manifest, call: &Call) -> Result<Intent> {
    let existing = manifest.get(&call.path).cloned();
    let ops = match &call.wrote {
        Wrote::Spans(spans) => {
            let record = existing.ok_or_else(|| {
                Error::Precondition(format!("edit of absent path: {}", call.path))
            })?;
            // The content is carried forward across the call's spans, because
            // each span meets what the previous one left — and because an
            // every-occurrence span can only be anchored against the bytes it
            // will actually meet.
            let mut content = blobs.get(&record.blob)?;
            let mut ops = Vec::with_capacity(spans.len());
            for span in spans {
                let series = if span.all {
                    anchored_replace_all(&content, &span.old, &span.new).ok_or_else(|| {
                        Error::Precondition(format!(
                            "replace-all span does not occur in {}",
                            call.path
                        ))
                    })?
                } else {
                    vec![(span.old.clone(), span.new.clone())]
                };
                for (old_str, new_str) in series {
                    content = replace_unique(&content, old_str.as_bytes(), new_str.as_bytes())
                        .map_err(|n| {
                            Error::Precondition(format!(
                                "old_str occurs {n} times in {}; exactly one required",
                                call.path
                            ))
                        })?;
                    ops.push(Op::Edit {
                        path: call.path.clone(),
                        old_str,
                        new_str,
                    });
                }
            }
            ops
        }
        // A whole-file write is a create when the path is absent, and
        // delete-then-create when it is not: the delete carries the prior blob
        // hash, which is exactly the whole-file read the editor performed.
        Wrote::Whole(content) => {
            let content_b64 = crate::b64::encode(content.as_bytes());
            match existing {
                None => vec![Op::Create {
                    path: call.path.clone(),
                    mode: "100644".to_string(),
                    blob: None,
                    content_b64: Some(content_b64),
                }],
                Some(record) => vec![
                    Op::Delete {
                        path: call.path.clone(),
                        blob: record.blob.clone(),
                    },
                    Op::Create {
                        path: call.path.clone(),
                        mode: record.mode.clone(),
                        blob: None,
                        content_b64: Some(content_b64),
                    },
                ],
            }
        }
    };
    Ok(Intent { ops })
}

/// The read set of `intent` landing on `manifest`, plus the element paths
/// the session read before it (`read_paths`).  Every span an edit consumes
/// is a span it read, of the blob it met; a delete read the whole file it
/// deleted.  A read of a path tally does not carry is still a read — the
/// model learned it was absent — so it is recorded with a null blob rather
/// than dropped.
///
/// Derived from the intent and the state it lands on, never from the
/// transcript, so a line whose spans were aligned or absorbed names the
/// blobs its own replay meets.
fn reads_for<'a>(
    manifest: &Manifest,
    intent: &Intent,
    read_paths: impl IntoIterator<Item = &'a String>,
) -> Value {
    let blob_of = |path: &str| {
        manifest
            .get(path)
            .map(|r| json!(r.blob))
            .unwrap_or(Value::Null)
    };
    let mut reads: Vec<Value> = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for op in &intent.ops {
        let path = op.path();
        if seen.contains(&path) {
            continue;
        }
        seen.push(path);
        let path_ops = intent.ops.iter().filter(|o| o.path() == path);
        if let Some(blob) = path_ops.clone().find_map(|o| match o {
            Op::Delete { blob, .. } => Some(blob.clone()),
            _ => None,
        }) {
            reads.push(json!({ "path": path, "blob": blob, "spans": ["<whole file>"] }));
            continue;
        }
        let consumed: Vec<Value> = path_ops
            .filter_map(|o| match o {
                Op::Edit { old_str, .. } => Some(Value::String(old_str.clone())),
                _ => None,
            })
            .collect();
        if !consumed.is_empty() {
            reads.push(json!({ "path": path, "blob": blob_of(path), "spans": consumed }));
        }
    }
    for path in read_paths {
        if reads.iter().any(|r| r.get("path") == Some(&json!(path))) {
            continue;
        }
        reads.push(json!({ "path": path, "blob": blob_of(path), "spans": ["<whole file>"] }));
    }
    Value::Array(reads)
}

/// The annotation one ingested line carries: its call's, plus the ids and
/// prose of the later calls it absorbed, so neither the narrative nor the
/// record of what landed goes missing with their lines.
fn annotation(
    session: &str,
    author: &str,
    call: &Call,
    absorbed: &[&Call],
    reads: Value,
) -> Annotation {
    let prose: Vec<&str> = std::iter::once(call)
        .chain(absorbed.iter().copied())
        .filter_map(|c| c.prose.as_deref())
        .collect();
    Annotation {
        author: author.to_string(),
        provenance: Provenance::Agent,
        session: Some(session.to_string()),
        prose: (!prose.is_empty()).then(|| prose.join("\n\n")),
        reads: Some(reads),
        harness: Some(HarnessCall {
            harness: HARNESS.to_string(),
            tool: call.tool.clone(),
            entry: call.entry.clone(),
            call: call.id.clone(),
            absorbed: absorbed.iter().map(|c| c.id.clone()).collect(),
        }),
        ..Annotation::default()
    }
}

/// The calls a fork's whole lineage already records (§2.3), so a second
/// ingest of the same session appends only what is new.  Union carries the
/// derivation facts, so a call that landed elsewhere and was unioned in counts
/// as recorded too.
fn recorded_calls(repo: &Repository, fork: &str) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for (_, line) in repo.continuity_log(fork)? {
        if let Some(harness) = &line.annotation.harness
            && harness.harness == HARNESS
        {
            ids.insert(harness.call.clone());
            ids.extend(harness.absorbed.iter().cloned());
        }
    }
    Ok(ids)
}

/////////////////////////////////////////////// plan ////////////////////////////////////////////

/// A run of bytes in a replayed path that one planned op wrote:
/// `content[start..end]` is `new_str[at..at + (end - start)]` of the
/// `op`-th op of the `call`-th planned call.  Bytes no landed-this-run op
/// wrote — the base's bytes, and whole-file `Write` content — carry no run.
#[derive(Clone, Copy, Debug)]
struct Run {
    start: usize,
    end: usize,
    call: usize,
    op: usize,
    at: usize,
}

/// What the replay knows about who wrote a path's bytes: the runs, and the
/// scars — offsets where an op deleted bytes and left nothing to own.  A
/// stretch of unowned bytes with no scar inside is exactly the stretch the
/// base (or a whole-file write) carried, contiguous as it was then.
#[derive(Clone, Debug, Default)]
struct Track {
    runs: Vec<Run>,
    scars: Vec<usize>,
}

impl Track {
    /// Replace `[at, at + old_len)` with `new_len` bytes owned by `owner`,
    /// keeping every run's seat: a run before the edit stands, a run after
    /// it shifts, and a run the edit bites loses the bitten part — those
    /// bytes now belong to the edit's owner.  A pure deletion leaves a scar.
    fn edit(&mut self, at: usize, old_len: usize, new_len: usize, owner: OpSeat) {
        let end = at + old_len;
        let delta = new_len as isize - old_len as isize;
        let shift = |pos: usize| (pos as isize + delta) as usize;
        let mut out: Vec<Run> = Vec::with_capacity(self.runs.len() + 1);
        for run in self.runs.drain(..) {
            if run.end <= at {
                out.push(run);
            } else if run.start >= end {
                out.push(Run {
                    start: shift(run.start),
                    end: shift(run.end),
                    ..run
                });
            } else {
                if run.start < at {
                    out.push(Run {
                        start: run.start,
                        end: at,
                        ..run
                    });
                }
                if run.end > end {
                    out.push(Run {
                        start: at + new_len,
                        end: shift(run.end),
                        at: run.at + (end - run.start),
                        ..run
                    });
                }
            }
        }
        if new_len > 0 {
            out.push(Run {
                start: at,
                end: at + new_len,
                call: owner.0,
                op: owner.1,
                at: 0,
            });
        }
        out.sort_by_key(|run| run.start);
        self.runs = out;
        self.shift_scars(at, end, delta);
        if new_len == 0 {
            self.scars.push(at);
            self.scars.sort_unstable();
            self.scars.dedup();
        }
    }

    /// Replace `[at, at + old_len)`, which lies inside `self.runs[idx]`,
    /// with `new_len` bytes that join that run: the run's op rewrote part of
    /// its own `new_str`, so the run grows or shrinks in place and every
    /// later piece of the same op slides along its `new_str`.
    fn rewrite_within(&mut self, idx: usize, at: usize, old_len: usize, new_len: usize) {
        let end = at + old_len;
        let delta = new_len as isize - old_len as isize;
        let shift = |pos: usize| (pos as isize + delta) as usize;
        let owner = self.runs[idx];
        for (i, run) in self.runs.iter_mut().enumerate() {
            if i == idx {
                run.end = shift(run.end);
                continue;
            }
            if run.start >= end {
                run.start = shift(run.start);
                run.end = shift(run.end);
            }
            if (run.call, run.op) == (owner.call, owner.op) && run.at > owner.at {
                run.at = (run.at as isize + delta) as usize;
            }
        }
        self.shift_scars(at, end, delta);
    }

    /// Scars before the edit stand, scars after it shift, and scars inside
    /// the replaced bytes go with them.
    fn shift_scars(&mut self, at: usize, end: usize, delta: isize) {
        self.scars.retain(|&s| s <= at || s >= end);
        for scar in &mut self.scars {
            if *scar >= end && *scar > at {
                *scar = (*scar as isize + delta) as usize;
            }
        }
    }

    /// Whether `[start, end)` holds no run's bytes.
    fn unowned(&self, start: usize, end: usize) -> bool {
        !self.runs.iter().any(|r| r.start < end && start < r.end)
    }

    /// Whether a scar falls within `range`, inclusive of both ends.
    fn scarred(&self, start: usize, end: usize) -> bool {
        self.scars.iter().any(|&s| start <= s && s <= end)
    }
}

/// A `(call, op)` seat in the plan.
type OpSeat = (usize, usize);

/// What the replay made of one call.
enum PlannedOutcome {
    /// Land `intent` with `reads`.  `faithful` is the intent as the call's
    /// own fields wrote it, kept so a plan whose rewrites fail to replay
    /// can fall back to the calls as written.  `aligned` says the working
    /// tree adjusted the spans; `absorbed` names the later calls (by plan
    /// seat) folded into this line.
    Land {
        intent: Intent,
        faithful: Intent,
        reads: Value,
        aligned: bool,
        absorbed: Vec<usize>,
    },
    /// The call only rewrote bytes one earlier call wrote, so it folded
    /// into that call's line (which lists it in `absorbed`) and lands none
    /// of its own.
    Absorbed { faithful: Intent },
    /// The call's preconditions did not hold in the replay; the
    /// reconciliation carries what the call did, as before.
    Skip(String),
}

/// One call's plan for landing.
struct Planned<'a> {
    /// The call it plans for.
    call: &'a Call,
    /// What the replay made of it.
    outcome: PlannedOutcome,
}

/// Replay the unrecorded calls against `start` in memory and plan the line
/// each lands: a later call that only rewrites bytes an earlier call of
/// this run wrote folds into the earlier call's line, and spans are aligned
/// with the working tree.  Returns the plan and the manifest it produces —
/// the dry-run preview's state, and the real run's expectation.
///
/// The replay validates exactly what landing validates.  It does write
/// blobs — a call after the first reads the content the previous one
/// produced, so that content has to be somewhere, and the pool is where
/// content lives.  This is what `tally sum` already does with the bytes it
/// hashes: pooling is not committing, the pool is content-addressed and
/// idempotent, and `gc-blobs` collects whatever no fork ends up reaching.
fn plan<'a>(
    repo: &Repository,
    start: &Manifest,
    calls: &'a [Call],
    recorded: &BTreeSet<String>,
) -> Result<(Vec<Planned<'a>>, Manifest)> {
    let blobs = repo.blobs();
    // Pass 1: replay each call as the harness wrote it, attributing every
    // byte a span writes to the op that wrote it.  A call whose every span
    // rewrites only bytes one earlier call wrote is absorbed into that call;
    // the bytes it leaves are the same either way, so the replay goes on
    // unchanged.  A call whose preconditions do not hold skips, exactly as
    // it would at landing.
    let mut manifest = start.clone();
    let mut planned: Vec<Planned> = Vec::new();
    let mut tracks: HashMap<String, Track> = HashMap::new();
    let mut contents: HashMap<String, Vec<u8>> = HashMap::new();
    for call in calls {
        if recorded.contains(&call.id) {
            continue;
        }
        let seat = planned.len();
        let outcome = match intent_for(&blobs, &manifest, call) {
            Ok(intent) => {
                // Stage the pre-call content of every path the intent
                // touches, so attribution can walk the ops against the state
                // each op met — the state `intent_for` just validated.
                stage(&mut contents, &blobs, &manifest, &intent);
                match apply_intent(&intent, &mut manifest, &blobs) {
                    Ok(realization) => {
                        for (_, content) in &realization.new_blobs {
                            blobs.put_unsynced(content)?;
                        }
                        if absorb(&mut planned, &mut tracks, &mut contents, seat, &intent).is_some()
                        {
                            PlannedOutcome::Absorbed { faithful: intent }
                        } else {
                            attribute(&mut tracks, &mut contents, seat, &intent);
                            PlannedOutcome::Land {
                                intent: intent.clone(),
                                faithful: intent,
                                reads: Value::Null,
                                aligned: false,
                                absorbed: Vec::new(),
                            }
                        }
                    }
                    Err(Error::Precondition(why)) => PlannedOutcome::Skip(why.to_string()),
                    Err(err) => return Err(err),
                }
            }
            Err(Error::Precondition(why)) => PlannedOutcome::Skip(why.to_string()),
            Err(err) => return Err(err),
        };
        planned.push(Planned { call, outcome });
    }

    // Alignment: the working tree is the oracle for what the session's own
    // spans became.  A formatter run inside Bash rewrites bytes an edit
    // just wrote, and a difference that falls within one op's `new_str` —
    // or within it and the untouched bytes beside it — has an owner, that
    // op, so it folds back into the op rather than land ownerless in the
    // reconciliation.
    align(&mut planned, &tracks, &contents, repo)?;

    // Pass 2: replay the plan from the start to produce the state it
    // promises, computing each line's read set against the state it meets.
    // Absorption and alignment rewrite only bytes no other op names, but an
    // earlier landing can still change what a later precondition finds
    // unique.  Replay is cheap and honest: a plan that fails it falls back
    // to the calls as written, which pass 1 already validated.
    let final_manifest = match replay(&mut planned, start, &blobs, true)? {
        Some(manifest) => manifest,
        None => {
            unwind(&mut planned);
            replay(&mut planned, start, &blobs, false)?.expect("a lenient replay never declines")
        }
    };
    Ok((planned, final_manifest))
}

/// Replay the plan's landing lines against `start`, filling in each line's
/// read set.  `strict` declines (`None`) on the first precondition failure;
/// otherwise a failing line skips to the reconciliation.
fn replay(
    planned: &mut [Planned],
    start: &Manifest,
    blobs: &BlobStore,
    strict: bool,
) -> Result<Option<Manifest>> {
    let mut manifest = start.clone();
    for i in 0..planned.len() {
        let PlannedOutcome::Land {
            intent, absorbed, ..
        } = &planned[i].outcome
        else {
            continue;
        };
        // An absorbed call's reads travel with the line that carries it.
        let read_paths = std::iter::once(planned[i].call)
            .chain(absorbed.iter().map(|&s| planned[s].call))
            .flat_map(|c| c.reads.iter());
        let fresh = reads_for(&manifest, intent, read_paths);
        let applied = apply_intent(intent, &mut manifest, blobs);
        match applied {
            Ok(realization) => {
                for (_, content) in &realization.new_blobs {
                    blobs.put_unsynced(content)?;
                }
                if let PlannedOutcome::Land { reads, .. } = &mut planned[i].outcome {
                    *reads = fresh;
                }
            }
            Err(Error::Precondition(_)) if strict => return Ok(None),
            Err(Error::Precondition(why)) => planned[i].outcome = PlannedOutcome::Skip(why),
            Err(err) => return Err(err),
        }
    }
    Ok(Some(manifest))
}

/// Return every call to the intent its own fields wrote: no absorption, no
/// alignment.
fn unwind(planned: &mut [Planned]) {
    for p in planned {
        match &mut p.outcome {
            PlannedOutcome::Land {
                intent,
                faithful,
                aligned,
                absorbed,
                ..
            } => {
                *intent = faithful.clone();
                *aligned = false;
                absorbed.clear();
            }
            PlannedOutcome::Absorbed { faithful, .. } => {
                p.outcome = PlannedOutcome::Land {
                    intent: faithful.clone(),
                    faithful: faithful.clone(),
                    reads: Value::Null,
                    aligned: false,
                    absorbed: Vec::new(),
                };
            }
            PlannedOutcome::Skip(_) => {}
        }
    }
}

/// The content of every element an intent touches, staged from the manifest
/// the intent was validated against, so attribution can walk the ops against
/// the state each op meets.
fn stage(
    contents: &mut HashMap<String, Vec<u8>>,
    blobs: &BlobStore,
    manifest: &Manifest,
    intent: &Intent,
) {
    for op in &intent.ops {
        if contents.contains_key(op.path()) {
            continue;
        }
        let Some(record) = manifest.get(op.path()) else {
            continue;
        };
        if let Ok(content) = blobs.get(&record.blob) {
            contents.insert(op.path().to_string(), content);
        }
    }
}

/// The offset at which `old` replaces uniquely in `content`, and the
/// content it leaves — one scan finds and proves the occurrence.  `None`
/// when it does not occur exactly once.
fn locate(content: &[u8], old: &[u8], new: &[u8]) -> Option<(usize, Vec<u8>)> {
    let at = find_unique(content, old).ok()?;
    let mut out = Vec::with_capacity(content.len() - old.len() + new.len());
    out.extend_from_slice(&content[..at]);
    out.extend_from_slice(new);
    out.extend_from_slice(&content[at + old.len()..]);
    Some((at, out))
}

/// Fold the call at `seat` into an earlier call when every span it writes
/// rewrites only bytes that one earlier call of this run wrote: the earlier
/// call's `new_str`s take the rewrite, and the later call lands nothing of
/// its own.  `None` when the fold declines, leaving the plan untouched.
///
/// Containment is the whole rule.  A span that reaches past the earlier
/// call's bytes would pull bytes the earlier call never named into its
/// precondition, so such a call lands as written.
fn absorb(
    planned: &mut [Planned],
    tracks: &mut HashMap<String, Track>,
    contents: &mut HashMap<String, Vec<u8>>,
    seat: usize,
    intent: &Intent,
) -> Option<()> {
    let path = intent.ops.first()?.path().to_string();
    let mut track = tracks.get(&path)?.clone();
    let mut content = contents.get(&path)?.clone();
    let mut owner: Option<usize> = None;
    // The owner's rewritten `new_str`s, by op index.
    let mut rewritten: BTreeMap<usize, String> = BTreeMap::new();
    for op in &intent.ops {
        let Op::Edit {
            path: op_path,
            old_str,
            new_str,
        } = op
        else {
            return None;
        };
        if *op_path != path || old_str.is_empty() {
            return None;
        }
        let (at, replaced) = locate(&content, old_str.as_bytes(), new_str.as_bytes())?;
        let end = at + old_str.len();
        let idx = track
            .runs
            .iter()
            .position(|r| r.start <= at && end <= r.end)?;
        let run = track.runs[idx];
        if *owner.get_or_insert(run.call) != run.call {
            return None;
        }
        let target = match rewritten.get(&run.op) {
            Some(s) => s.clone(),
            None => match &planned.get(run.call)?.outcome {
                PlannedOutcome::Land { intent, .. } => match intent.ops.get(run.op)? {
                    Op::Edit { new_str, .. } => new_str.clone(),
                    _ => return None,
                },
                _ => return None,
            },
        };
        let seat_start = run.at + (at - run.start);
        let seat_end = seat_start + old_str.len();
        if target.get(seat_start..seat_end)? != old_str {
            return None;
        }
        let mut spliced = String::with_capacity(target.len() + new_str.len());
        spliced.push_str(&target[..seat_start]);
        spliced.push_str(new_str);
        spliced.push_str(&target[seat_end..]);
        rewritten.insert(run.op, spliced);
        track.rewrite_within(idx, at, old_str.len(), new_str.len());
        content = replaced;
    }
    let into = owner?;
    let PlannedOutcome::Land {
        intent: owner_intent,
        absorbed,
        ..
    } = &mut planned[into].outcome
    else {
        return None;
    };
    for (op_idx, spliced) in rewritten {
        if let Some(Op::Edit { new_str, .. }) = owner_intent.ops.get_mut(op_idx) {
            *new_str = spliced;
        }
    }
    absorbed.push(seat);
    tracks.insert(path.clone(), track);
    contents.insert(path, content);
    Some(())
}

/// Attribute every byte an applied intent's spans write to the op that wrote
/// it, carrying each touched path's staged content forward to what the
/// intent left.  Attribution is advisory — a create's wholesale bytes simply
/// start unattributed — so a surprise degrades to no runs, never to a failed
/// ingest; after a successful `apply_intent` the spans sit where validation
/// put them and the fallback below is unreachable.
fn attribute(
    tracks: &mut HashMap<String, Track>,
    contents: &mut HashMap<String, Vec<u8>>,
    seat: usize,
    intent: &Intent,
) {
    for (op_idx, op) in intent.ops.iter().enumerate() {
        match op {
            Op::Edit {
                path,
                old_str,
                new_str,
            } => {
                let Some(content) = contents.get_mut(path) else {
                    continue;
                };
                // Unreachable after a successful `apply_intent`; degrade to
                // no runs and no stale content rather than misattribute.
                let Some((at, new_content)) =
                    locate(content, old_str.as_bytes(), new_str.as_bytes())
                else {
                    tracks.remove(path);
                    contents.remove(path);
                    continue;
                };
                tracks.entry(path.clone()).or_default().edit(
                    at,
                    old_str.len(),
                    new_str.len(),
                    (seat, op_idx),
                );
                *content = new_content;
            }
            // A create's bytes are the harness's whole-file write, not a
            // span, so they carry no runs; a delete leaves nothing to
            // attribute.  Either way the staged content is stale afterwards.
            Op::Create { path, .. } | Op::Delete { path, .. } => {
                tracks.remove(path);
                contents.remove(path);
            }
            Op::Chmod { .. } => {}
        }
    }
}

/// How one op absorbs the working tree: the untouched bytes it widens over
/// on either side, and each `(seat, range)` replacement of `seat` — in the
/// widened `prefix + new_str + suffix` — by `tree[range]`.
#[derive(Default)]
struct Alignment {
    prefix: Option<Range<usize>>,
    suffix: Option<Range<usize>>,
    replacements: Vec<(Range<usize>, Range<usize>)>,
}

/// Narrow a changed-range pair to the bytes that actually differ: the
/// line-aligned hunk minus its common prefix and suffix.
fn trim(
    old: &[u8],
    new: &[u8],
    (o, n): (Range<usize>, Range<usize>),
) -> (Range<usize>, Range<usize>) {
    let (a, b) = (&old[o.clone()], &new[n.clone()]);
    let head = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let room = a.len().min(b.len()) - head;
    let tail = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(room)
        .take_while(|(x, y)| x == y)
        .count();
    (o.start + head..o.end - tail, n.start + head..n.end - tail)
}

/// The run that owns a changed range outright.
fn containing(runs: &[Run], o: &Range<usize>) -> Option<Run> {
    runs.iter()
        .find(|run| {
            if o.is_empty() {
                // An insertion belongs inside the run, not at its edge:
                // at the edge it may as well be the neighbor's bytes.
                run.start < o.start && o.start < run.end
            } else {
                run.start <= o.start && o.end <= run.end
            }
        })
        .copied()
}

/// Fold every working-tree difference with an owner back into that op, so
/// the line lands the way the tree holds it.  A difference has an owner
/// when its changed bytes fall within one op's `new_str`, or within it and
/// the untouched bytes immediately beside the op's span — a formatter that
/// reindents a line the edit began mid-way through.  The op then widens
/// over those bytes, which it can name because nothing has touched them
/// since it ran.  Differences with no owner — bytes the base carried, or
/// churn across two ops — stay for the reconciliation, which is what keeps
/// the formatter's patch honest: it shows only what the session's edits do
/// not explain.
fn align(
    planned: &mut [Planned],
    tracks: &HashMap<String, Track>,
    contents: &HashMap<String, Vec<u8>>,
    repo: &Repository,
) -> Result<()> {
    let blobs = repo.blobs();
    // Nothing attributed, nothing to align: spare the working-tree scan.
    if tracks.values().all(|track| track.runs.is_empty()) {
        return Ok(());
    }
    let working = repo.working_tree_manifest()?;
    for (path, track) in tracks {
        if track.runs.is_empty() {
            continue;
        }
        let (Some(replayed), Some(record)) = (contents.get(path), working.get(path)) else {
            continue;
        };
        let tree = blobs.get(&record.blob)?;
        if *replayed == tree {
            continue;
        }
        let Some(hunks) = crate::diff::changed_ranges(replayed, &tree) else {
            continue;
        };
        let mut per_op: BTreeMap<OpSeat, Alignment> = BTreeMap::new();
        // Owned hunks, in the owner's run coordinates, until every op's
        // widening is known.
        let mut owned: Vec<(Run, Range<usize>, Range<usize>)> = Vec::new();
        for hunk in hunks {
            if let Some(run) = containing(&track.runs, &hunk.0) {
                owned.push((run, hunk.0, hunk.1));
                continue;
            }
            let (o, n) = trim(replayed, &tree, hunk);
            if let Some(run) = containing(&track.runs, &o) {
                owned.push((run, o, n));
                continue;
            }
            if o.is_empty() {
                continue;
            }
            // Widening: exactly one run overlaps the changed bytes, and
            // whatever of them lies outside it is unowned and unscarred, at
            // the very start or end of the op's `new_str`.
            let mut overlapping = track
                .runs
                .iter()
                .filter(|r| r.start < o.end && o.start < r.end);
            let (Some(&run), None) = (overlapping.next(), overlapping.next()) else {
                continue;
            };
            let Some(Op::Edit { new_str, .. }) = (match &planned[run.call].outcome {
                PlannedOutcome::Land { intent, .. } => intent.ops.get(run.op),
                _ => None,
            }) else {
                continue;
            };
            let alignment = per_op.entry((run.call, run.op)).or_default();
            if o.start < run.start {
                if run.at != 0
                    || alignment.prefix.is_some()
                    || !track.unowned(o.start, run.start)
                    || track.scarred(o.start + 1, run.start)
                {
                    continue;
                }
                alignment.prefix = Some(o.start..run.start);
            }
            if o.end > run.end {
                if run.at + (run.end - run.start) != new_str.len()
                    || alignment.suffix.is_some()
                    || !track.unowned(run.end, o.end)
                    || track.scarred(run.end, o.end - 1)
                {
                    continue;
                }
                alignment.suffix = Some(run.end..o.end);
            }
            owned.push((run, o, n));
        }
        for (run, o, n) in owned {
            let alignment = per_op.entry((run.call, run.op)).or_default();
            let lead = alignment.prefix.as_ref().map_or(0, |p| p.len()) + run.at;
            let seat = lead + o.start - run.start..lead + o.end - run.start;
            alignment.replacements.push((seat, n));
        }
        for ((seat, op_idx), alignment) in per_op {
            let PlannedOutcome::Land {
                intent, aligned, ..
            } = &mut planned[seat].outcome
            else {
                continue;
            };
            if alignment.replacements.is_empty() {
                continue;
            }
            if let Some(adjusted) = align_op(intent, op_idx, &alignment, replayed, &tree) {
                *intent = adjusted;
                *aligned = true;
            }
        }
    }
    Ok(())
}

/// The intent with op `op_idx` widened over the alignment's prefix and
/// suffix and its `new_str` spliced with the tree's bytes.  `None` when a
/// seat or a widening does not sit on char boundaries — the op then stands
/// as written and the churn stays with the reconciliation.
fn align_op(
    intent: &Intent,
    op_idx: usize,
    alignment: &Alignment,
    replayed: &[u8],
    tree: &[u8],
) -> Option<Intent> {
    let mut intent = intent.clone();
    let Op::Edit {
        old_str, new_str, ..
    } = intent.ops.get_mut(op_idx)?
    else {
        return None;
    };
    let text = |range: &Option<Range<usize>>| -> Option<&str> {
        match range {
            Some(r) => std::str::from_utf8(replayed.get(r.clone())?).ok(),
            None => Some(""),
        }
    };
    let (prefix, suffix) = (text(&alignment.prefix)?, text(&alignment.suffix)?);
    let widened = format!("{prefix}{new_str}{suffix}");
    let mut replacements = alignment.replacements.clone();
    replacements.sort_by_key(|(seat, _)| seat.start);
    let mut out = String::with_capacity(widened.len());
    let mut at = 0;
    for (seat, range) in replacements {
        if seat.start < at
            || seat.end > widened.len()
            || !widened.is_char_boundary(seat.start)
            || !widened.is_char_boundary(seat.end)
        {
            return None;
        }
        out.push_str(&widened[at..seat.start]);
        out.push_str(std::str::from_utf8(tree.get(range)?).ok()?);
        at = seat.end;
    }
    out.push_str(&widened[at..]);
    *old_str = format!("{prefix}{old_str}{suffix}");
    *new_str = out;
    Some(intent)
}

/////////////////////////////////////////////// commit ////////////////////////////////////////////

/// What to ingest, and where.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// The session to ingest; defaults to the one in the environment.
    pub session: Option<String>,
    /// The transcript to read; defaults to the session's own under
    /// `<config>/projects/`.
    pub transcript: Option<PathBuf>,
    /// The fork to land on; defaults to `claude-<session>`.
    pub fork: Option<String>,
    /// The fork to anchor a new session fork on; defaults to the fork of the
    /// session this one continues, else `main`.
    pub from: Option<String>,
    /// The author to stamp; defaults to [`DEFAULT_AUTHOR`].
    pub author: Option<String>,
    /// Report what would be ingested and write nothing.
    pub dry_run: bool,
    /// Skip the reconciling line, leaving the residue pending.
    pub no_reconcile: bool,
}

/// One line the ingest appended.
#[derive(Clone, Debug)]
pub struct Landed {
    /// The line id.
    pub id: String,
    /// The call it came from, as a label.
    pub label: String,
    /// The line's spans were aligned with the working tree: a shell-side
    /// rewrite (a formatter, a hook) had adjusted bytes that fall entirely
    /// within the call's own `new_str`, so the line lands the way the tree
    /// holds it rather than the way the transcript recorded it.
    pub aligned: bool,
    /// Later calls folded into this line, as labels: each only rewrote
    /// bytes this line's call wrote, so the line lands their final form and
    /// they land no line of their own.
    pub absorbed: Vec<String>,
}

/// One call the ingest could not replay.
#[derive(Clone, Debug)]
pub struct Skipped {
    /// The call, as a label.
    pub label: String,
    /// Why the replay did not hold.
    pub why: String,
}

/// The outcome of `tally claude commit`.
#[derive(Clone, Debug)]
pub struct Summary {
    /// The session ingested.
    pub session: String,
    /// The transcript it was read from.
    pub transcript: PathBuf,
    /// The fork it landed on.
    pub fork: String,
    /// The fork that fork is anchored on.
    pub base: String,
    /// Whether this run brought the fork into being.
    pub created: bool,
    /// The session this one continues, when it inherited a prefix.
    pub inherited: Option<String>,
    /// The index of the session's unique origin, and the transcript's length.
    pub origin: usize,
    /// How many entries the transcript carries.
    pub entries: usize,
    /// How many file-mutating calls the session made from its origin onward.
    pub observed: usize,
    /// How many of them an earlier run had already recorded.
    pub already: usize,
    /// The lines appended, in order.
    pub landed: Vec<Landed>,
    /// The calls whose preconditions no longer hold; the reconciliation
    /// carries what they did.
    pub skipped: Vec<Skipped>,
    /// The working-tree paths the session's calls do not explain, as
    /// `<code> <path>`.
    pub residue: Vec<String>,
    /// The reconciling line, when one was appended.
    pub reconciled: Option<String>,
    /// The fork's sum when the ingest finished.
    pub head: String,
    /// Whether anything was written.
    pub dry_run: bool,
}

/// Ingest a Claude Code session into its own fork: one line per observed tool
/// call from the session's unique origin onward, then one reconciling line for
/// whatever the working tree holds that the session's calls do not explain.
///
/// The session is planned in memory first ([`plan`]): replayed, aligned with
/// the working tree where a shell-side rewrite fell entirely within a call's
/// own span, and re-validated.  The landing then appends the plan's lines.
///
/// The landing uses [`Repository::record_observed`], not
/// [`Repository::apply`]: the edits have already landed on disk, and a
/// working-tree refresh here would rewrite a file from tally's reconstruction
/// of it — clobbering a later edit the ingest has not reached yet.  Recording
/// history must not rewrite the present.
pub fn commit(repo: &Repository, options: &Options) -> Result<Summary> {
    let session = match options.session.clone().or_else(current_session) {
        Some(session) => session,
        None => {
            return Err(Error::Invalid(
                "no session: CLAUDE_CODE_SESSION_ID is unset, so name one with --session"
                    .to_string(),
            ));
        }
    };
    let path = match &options.transcript {
        Some(path) => path.clone(),
        None => find_transcript(&session)?,
    };
    let transcript = read_transcript(&path, &session)?;
    let fork = match &options.fork {
        Some(fork) => fork.clone(),
        None => format!("claude-{session}"),
    };
    crate::fork::validate_fork_name(&fork)?;
    // Where the session originates, in fork terms: the fork of the session it
    // continues, when that session was itself ingested, so the lineage of
    // sessions is the lineage of forks.  Otherwise `main`.
    let base = match &options.from {
        Some(from) => from.clone(),
        None => {
            let inherited = transcript
                .inherited
                .as_ref()
                .map(|session| format!("claude-{session}"));
            match inherited {
                Some(candidate) if repo.fork_exists(&candidate)? => candidate,
                _ => "main".to_string(),
            }
        }
    };
    let author = options.author.clone().unwrap_or_else(|| {
        std::env::var("TALLY_AUTHOR").unwrap_or_else(|_| DEFAULT_AUTHOR.to_string())
    });
    let calls = calls(repo, &transcript);
    let exists = repo.fork_exists(&fork)?;
    let recorded = recorded_calls(repo, if exists { &fork } else { &base })?;

    let mut summary = Summary {
        session: session.clone(),
        transcript: transcript.path.clone(),
        fork: fork.clone(),
        base: base.clone(),
        created: false,
        inherited: transcript.inherited.clone(),
        origin: transcript.origin,
        entries: transcript.entries.len(),
        observed: calls.len(),
        already: 0,
        landed: Vec::new(),
        skipped: Vec::new(),
        residue: Vec::new(),
        reconciled: None,
        head: String::new(),
        dry_run: options.dry_run,
    };

    // Plan the session against the state it meets: the same replay for the
    // preview and for the landing, so the dry run prints the plan, not a
    // guess.  Already-recorded calls are the state the plan starts from.
    let start = repo
        .current_state(if exists { &fork } else { &base })?
        .manifest;
    let (planned, planned_manifest) = plan(repo, &start, &calls, &recorded)?;
    summary.already = calls.len() - planned.len();

    if options.dry_run {
        // No fork, no log line, no linearization point: the plan is the
        // preview.
        for p in &planned {
            match &p.outcome {
                PlannedOutcome::Land {
                    aligned, absorbed, ..
                } => summary.landed.push(Landed {
                    id: "(dry run)".to_string(),
                    label: p.call.label(),
                    aligned: *aligned,
                    absorbed: absorbed.iter().map(|&s| planned[s].call.label()).collect(),
                }),
                PlannedOutcome::Absorbed { .. } => {}
                PlannedOutcome::Skip(why) => summary.skipped.push(Skipped {
                    label: p.call.label(),
                    why: why.clone(),
                }),
            }
        }
        summary.residue = residue(repo, &planned_manifest)?;
        summary.head = planned_manifest.sum().hexdigest();
        return Ok(summary);
    }

    summary.created = repo.ensure_fork(&fork, &base)?;
    for p in &planned {
        match &p.outcome {
            PlannedOutcome::Skip(why) => summary.skipped.push(Skipped {
                label: p.call.label(),
                why: why.clone(),
            }),
            // Its line carries it.
            PlannedOutcome::Absorbed { .. } => {}
            PlannedOutcome::Land {
                intent,
                reads,
                aligned,
                absorbed,
                ..
            } => {
                let absorbed: Vec<&Call> = absorbed.iter().map(|&s| planned[s].call).collect();
                // The intent is planned, but the append re-validates it
                // against the state under the fork lock: each line is its own
                // linearization point, and a fork that moved since planning
                // fails the precondition.  That is the expected failure, not
                // an error: the file drifted (a `Bash` run, a `replace_all`
                // that hit twice, an edit the transcript did not see).  The
                // call's effect is still on disk, and the reconciliation
                // carries it.
                let landed = repo.record_observed(
                    &fork,
                    intent.clone(),
                    annotation(&session, &author, p.call, &absorbed, reads.clone()),
                );
                match landed {
                    Ok(line) => summary.landed.push(Landed {
                        id: line.id,
                        label: p.call.label(),
                        aligned: *aligned,
                        absorbed: absorbed.iter().map(|c| c.label()).collect(),
                    }),
                    Err(Error::Precondition(why)) => summary.skipped.push(Skipped {
                        label: p.call.label(),
                        why,
                    }),
                    Err(err) => return Err(err),
                }
            }
        }
    }

    let state = repo.current_state(&fork)?;
    summary.residue = residue(repo, &state.manifest)?;
    if !summary.residue.is_empty() && !options.no_reconcile {
        let line = reconcile(repo, &fork, &session, &author, summary.residue.len())?;
        summary.reconciled = Some(line.id);
    }
    summary.head = repo.current_state(&fork)?.sum.hexdigest();
    Ok(summary)
}

/// The working-tree paths a state does not explain, as `<code> <path>`.
fn residue(repo: &Repository, manifest: &Manifest) -> Result<Vec<String>> {
    let working = repo.working_tree_manifest()?;
    Ok(crate::diff::diff_manifests(manifest, &working)
        .iter()
        .map(|change| format!("{} {}", change.code(), change.path))
        .collect())
}

/// Append the reconciling line: the pending working-tree patch, carrying no
/// read set because none was observed.  Degraded provenance is marked, never
/// faked — and here what is missing is the reads, not the authorship.
fn reconcile(
    repo: &Repository,
    fork: &str,
    session: &str,
    author: &str,
    paths: usize,
) -> Result<LogLine> {
    let annotation = Annotation {
        author: author.to_string(),
        provenance: Provenance::Agent,
        session: Some(session.to_string()),
        prose: Some(format!(
            "reconcile the working tree with session {session}: {paths} path(s) its tool \
             calls do not explain"
        )),
        ..Annotation::default()
    };
    repo.commit(fork, None, annotation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::Op;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tally-claude-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_repo(name: &str) -> Repository {
        Repository::init(temp_dir(&format!("repo-{name}"))).unwrap()
    }

    fn note() -> Annotation {
        Annotation {
            author: "seed".to_string(),
            ..Annotation::default()
        }
    }

    /// Seed a file on `main`, working tree included (apply refreshes it).
    fn seed(repo: &Repository, path: &str, content: &str) {
        let intent = Intent {
            ops: vec![Op::Create {
                path: path.to_string(),
                mode: "100644".to_string(),
                blob: None,
                content_b64: Some(crate::b64::encode(content.as_bytes())),
            }],
        };
        repo.apply("main", intent, note()).unwrap();
    }

    /// An assistant entry carrying one tool call, and optionally prose.
    fn assistant(repo: &Repository, uuid: &str, session: &str, prose: &str, call: Value) -> Value {
        let mut content = Vec::new();
        if !prose.is_empty() {
            content.push(json!({"type": "text", "text": prose}));
        }
        content.push(call);
        json!({
            "type": "assistant", "uuid": uuid, "sessionId": session,
            "cwd": repo.root().to_string_lossy(),
            "message": {"role": "assistant", "content": content},
        })
    }

    fn tool_use(id: &str, name: &str, input: Value) -> Value {
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    }

    fn write_transcript(dir: &Path, session: &str, entries: &[Value]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{session}.jsonl"));
        let mut body = String::new();
        for entry in entries {
            body.push_str(&serde_json::to_string(entry).unwrap());
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    fn options(transcript: &Path, session: &str) -> Options {
        Options {
            session: Some(session.to_string()),
            transcript: Some(transcript.to_path_buf()),
            author: Some("claude-code".to_string()),
            ..Options::default()
        }
    }

    #[test]
    fn a_session_lands_as_one_line_per_call_and_a_reconciliation() {
        let repo = temp_repo("ingest");
        seed(&repo, "/src/main.rs", "fn main() {\n    hello();\n}\n");
        let session = "s-ingest";
        let root = repo.root().to_path_buf();
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "",
                tool_use(
                    "toolu_read",
                    "Read",
                    json!({"file_path": root.join("src/main.rs").to_string_lossy()}),
                ),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "teach main to greet",
                tool_use(
                    "toolu_edit",
                    "Edit",
                    json!({"file_path": root.join("src/main.rs").to_string_lossy(),
                           "old_string": "hello()", "new_string": "greet()"}),
                ),
            ),
            assistant(
                &repo,
                "u3",
                session,
                "and a note",
                tool_use(
                    "toolu_write",
                    "Write",
                    json!({"file_path": root.join("NOTES.md").to_string_lossy(),
                           "content": "# notes\n"}),
                ),
            ),
        ];
        // What the harness did to the working tree, which tally is only now
        // being told about — plus one change no tool call explains (a `Bash`
        // run, as far as the transcript is concerned).
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    greet();\n}\n").unwrap();
        std::fs::write(root.join("NOTES.md"), "# notes\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-ingest"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        let fork = format!("claude-{session}");
        assert_eq!(summary.fork, fork);
        assert_eq!(summary.base, "main");
        assert!(summary.created);
        assert_eq!(summary.origin, 0, "nothing was inherited");
        assert_eq!(summary.observed, 2, "a Read is not a mutation");
        assert_eq!(summary.landed.len(), 2, "{:?}", summary.skipped);
        assert!(summary.skipped.is_empty(), "{:?}", summary.skipped);
        assert_eq!(summary.residue, vec!["A /Cargo.toml".to_string()]);
        assert!(summary.reconciled.is_some());

        // The promise: after the ingest the fork *is* the working tree.
        let (expected, actual) = repo.check(&fork).unwrap();
        assert_eq!(expected, actual);

        let lines = repo.current_state(&fork).unwrap().lines;
        assert_eq!(lines.len(), 3, "two calls plus the reconciliation");
        let first = &lines[0];
        assert_eq!(
            first.annotation.prose.as_deref(),
            Some("teach main to greet")
        );
        assert_eq!(first.annotation.session.as_deref(), Some(session));
        let harness = first.annotation.harness.as_ref().unwrap();
        assert_eq!(harness.harness, HARNESS);
        assert_eq!(harness.tool, "Edit");
        assert_eq!(harness.call, "toolu_edit");
        assert_eq!(harness.entry, "u2");
        // The read set carries both the span the edit consumed and the whole
        // file the session had read before it.
        let reads = first.annotation.reads.as_ref().unwrap().to_string();
        assert!(reads.contains("hello()"), "{reads}");
        assert!(reads.contains("/src/main.rs"), "{reads}");
        // The reconciliation is honest: no reads, because none were observed.
        let last = lines.last().unwrap();
        assert!(last.annotation.reads.is_none());
        assert!(last.annotation.harness.is_none());

        // Idempotent: a second run over the same session appends nothing.
        let again = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(again.already, 2);
        assert!(again.landed.is_empty());
        assert!(again.residue.is_empty(), "{:?}", again.residue);
        assert_eq!(repo.current_state(&fork).unwrap().lines.len(), 3);
    }

    #[test]
    fn a_dry_run_writes_nothing_and_previews_the_same_plan() {
        let repo = temp_repo("dry");
        seed(&repo, "/a.txt", "one\n");
        let session = "s-dry";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "two",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("a.txt").to_string_lossy(),
                       "old_string": "one", "new_string": "two"}),
            ),
        )];
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-dry"), session, &entries);
        let mut opts = options(&transcript, session);
        opts.dry_run = true;
        let summary = commit(&repo, &opts).unwrap();
        assert_eq!(summary.landed.len(), 1);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        assert!(!repo.fork_exists(&format!("claude-{session}")).unwrap());
    }

    #[test]
    fn a_replace_all_edit_lands_as_a_series_of_anchored_spans() {
        let repo = temp_repo("all");
        seed(&repo, "/t.txt", "alpha x\nbeta x\ngamma x\n");
        let session = "s-all";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "rename x to y everywhere",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("t.txt").to_string_lossy(),
                       "old_string": "x", "new_string": "y", "replace_all": true}),
            ),
        )];
        std::fs::write(root.join("t.txt"), "alpha y\nbeta y\ngamma y\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-all"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let lines = repo
            .current_state(&format!("claude-{session}"))
            .unwrap()
            .lines;
        assert_eq!(lines.len(), 1);
        // One call, three uniquely-anchored edits: the format has no
        // every-occurrence op and does not need one.
        assert_eq!(lines[0].intent.ops.len(), 3);
        for op in &lines[0].intent.ops {
            match op {
                Op::Edit { old_str, .. } => assert!(
                    old_str.contains('\n') || old_str.len() > 1,
                    "a span widened until unique: {old_str:?}"
                ),
                other => panic!("expected an edit, got {other:?}"),
            }
        }
    }

    #[test]
    fn an_errored_call_is_not_recorded() {
        let repo = temp_repo("errored");
        seed(&repo, "/a.txt", "one\n");
        let session = "s-err";
        let root = repo.root().to_path_buf();
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "try it",
                tool_use(
                    "toolu_1",
                    "Edit",
                    json!({"file_path": root.join("a.txt").to_string_lossy(),
                           "old_string": "nope", "new_string": "two"}),
                ),
            ),
            json!({"type": "user", "uuid": "u2", "sessionId": session,
                   "message": {"role": "user", "content": [
                       {"type": "tool_result", "tool_use_id": "toolu_1",
                        "is_error": true, "content": "String to replace not found"}]}}),
        ];
        let transcript = write_transcript(&temp_dir("tx-err"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.observed, 0);
        assert!(summary.landed.is_empty());
        assert!(summary.skipped.is_empty());
    }

    #[test]
    fn prose_written_in_its_own_entry_reaches_the_change_it_describes() {
        let repo = temp_repo("prose");
        seed(&repo, "/a.txt", "one\n");
        seed(&repo, "/b.txt", "one\n");
        let session = "s-prose";
        let root = repo.root().to_path_buf();
        let edit = |id: &str, file: &str, old: &str, new: &str| {
            tool_use(
                id,
                "Edit",
                json!({"file_path": root.join(file).to_string_lossy(),
                       "old_string": old, "new_string": new}),
            )
        };
        // Claude Code splits a turn into one entry per content block: the
        // sentence lands in its own entry, the call in the next.
        let entries = vec![
            json!({"type": "assistant", "uuid": "u1", "sessionId": session,
                   "cwd": root.to_string_lossy(),
                   "message": {"role": "assistant", "content": [
                       {"type": "thinking", "thinking": "not addressed to anyone"},
                       {"type": "text", "text": "promote both files to two"}]}}),
            assistant(
                &repo,
                "u2",
                session,
                "",
                edit("toolu_a", "a.txt", "one", "two"),
            ),
            assistant(
                &repo,
                "u3",
                session,
                "",
                edit("toolu_b", "b.txt", "one", "two"),
            ),
        ];
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-prose"), session, &entries);
        commit(&repo, &options(&transcript, session)).unwrap();
        let lines = repo
            .current_state(&format!("claude-{session}"))
            .unwrap()
            .lines;
        assert_eq!(lines.len(), 2);
        // The first mutation after the narration claims it; its sibling carries
        // none rather than the same sentence twice.  Thinking is never prose.
        assert_eq!(
            lines[0].annotation.prose.as_deref(),
            Some("promote both files to two")
        );
        assert_eq!(lines[1].annotation.prose, None);
    }

    #[test]
    fn the_origin_is_the_first_entry_no_sibling_transcript_carries() {
        let dir = temp_dir("origin");
        let repo = temp_repo("origin");
        // The parent session.
        let parent = "s-parent";
        let shared = vec![
            assistant(
                &repo,
                "p1",
                parent,
                "parent work",
                tool_use("toolu_p", "Read", json!({})),
            ),
            assistant(
                &repo,
                "p2",
                parent,
                "more",
                tool_use("toolu_q", "Read", json!({})),
            ),
        ];
        write_transcript(&dir, parent, &shared);
        // The child continues it: the parent's entries are copied in, then its
        // own begin.
        let child = "s-child";
        let mut entries = shared.clone();
        entries.push(assistant(
            &repo,
            "c1",
            child,
            "child work",
            tool_use("toolu_c", "Read", json!({})),
        ));
        let path = write_transcript(&dir, child, &entries);
        let transcript = read_transcript(&path, child).unwrap();
        assert_eq!(transcript.entries.len(), 3);
        assert_eq!(transcript.origin, 2, "the first entry uniquely the child's");
        assert_eq!(transcript.inherited.as_deref(), Some(parent));
    }

    #[test]
    fn a_continued_session_anchors_on_the_fork_of_the_session_it_continues() {
        let repo = temp_repo("lineage");
        seed(&repo, "/a.txt", "one\n");
        let dir = temp_dir("tx-lineage");
        let root = repo.root().to_path_buf();
        let parent = "s-p";
        let parent_entries = vec![assistant(
            &repo,
            "p1",
            parent,
            "one to two",
            tool_use(
                "toolu_p",
                "Edit",
                json!({"file_path": root.join("a.txt").to_string_lossy(),
                       "old_string": "one", "new_string": "two"}),
            ),
        )];
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let parent_path = write_transcript(&dir, parent, &parent_entries);
        commit(&repo, &options(&parent_path, parent)).unwrap();

        let child = "s-c";
        let mut entries = parent_entries.clone();
        entries.push(assistant(
            &repo,
            "c1",
            child,
            "two to three",
            tool_use(
                "toolu_c",
                "Edit",
                json!({"file_path": root.join("a.txt").to_string_lossy(),
                       "old_string": "two", "new_string": "three"}),
            ),
        ));
        std::fs::write(root.join("a.txt"), "three\n").unwrap();
        let child_path = write_transcript(&dir, child, &entries);
        let summary = commit(&repo, &options(&child_path, child)).unwrap();
        assert_eq!(summary.base, format!("claude-{parent}"));
        assert_eq!(
            summary.landed.len(),
            1,
            "the parent's call is not re-landed"
        );
        assert_eq!(
            summary.already, 0,
            "it was never on this fork to begin with"
        );
        // The lineage reads back through both sessions: the child's own line,
        // then the parent's, then main's.
        let history = repo.continuity_log(&summary.fork).unwrap();
        let forks: Vec<&str> = history.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(
            forks,
            vec![
                "main",
                &format!("claude-{parent}"),
                &format!("claude-{child}")
            ]
        );
    }

    #[test]
    fn a_shell_rewrite_within_the_span_folds_back_into_the_edit() {
        // The session writes an edit; a `cargo fmt` inside Bash then
        // reformats exactly what the edit wrote.  The transcript cannot see
        // the formatter, but the churn has an owner — the edit — so it
        // folds back into the edit's line and no reconciliation is needed.
        let repo = temp_repo("align");
        seed(&repo, "/src/main.rs", "fn main() {}\n");
        let session = "s-align";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "give main a body",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("src/main.rs").to_string_lossy(),
                       "old_string": "fn main() {}",
                       "new_string": "fn main() {\n        hello();\n}"}),
            ),
        )];
        // What the formatter left: the same body, reindented.
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-align"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(
            summary.landed[0].aligned,
            "the formatter's churn folds back"
        );
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        assert!(summary.reconciled.is_none(), "nothing left to reconcile");
        // The fork is the working tree, and the line lands the span the way
        // the tree holds it, not the way the transcript recorded it.
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        assert_eq!(lines.len(), 1);
        match &lines[0].intent.ops[0] {
            Op::Edit {
                old_str, new_str, ..
            } => {
                assert_eq!(old_str, "fn main() {}");
                assert_eq!(new_str, "fn main() {\n    hello();\n}");
            }
            other => panic!("expected an edit, got {other:?}"),
        }
        // Idempotent: the aligned line records its call, so a second ingest
        // plans nothing new.
        let again = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(again.already, 1);
        assert!(again.landed.is_empty());
        assert!(again.residue.is_empty(), "{:?}", again.residue);
    }

    /// An `Edit` of `/src/main.rs` in the repo's tree.
    fn main_edit(repo: &Repository, id: &str, old: &str, new: &str) -> Value {
        tool_use(
            id,
            "Edit",
            json!({"file_path": repo.root().join("src/main.rs").to_string_lossy(),
                   "old_string": old, "new_string": new}),
        )
    }

    /// The `(old_str, new_str)` of every op of every line on the fork.
    fn edits(repo: &Repository, fork: &str) -> Vec<Vec<(String, String)>> {
        repo.current_state(fork)
            .unwrap()
            .lines
            .iter()
            .map(|line| {
                line.intent
                    .ops
                    .iter()
                    .map(|op| match op {
                        Op::Edit {
                            old_str, new_str, ..
                        } => (old_str.clone(), new_str.clone()),
                        other => panic!("expected an edit, got {other:?}"),
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_later_rewrite_of_the_same_bytes_folds_into_one_span() {
        // Edit 1 writes a body; edit 2 rewrites a call inside it; then the
        // formatter reindents edit 1's region.  Edit 2 touched only bytes
        // edit 1 wrote, so it folds into edit 1's line: one span, the
        // minimal diff, carrying both calls' ids and prose.
        let repo = temp_repo("absorb");
        seed(&repo, "/src/main.rs", "fn main() {}\n");
        let session = "s-absorb";
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "a body",
                main_edit(
                    &repo,
                    "toolu_1",
                    "fn main() {}",
                    "fn main() {\n        alpha();\n    beta();\n}",
                ),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "rename beta",
                main_edit(&repo, "toolu_2", "beta()", "gamma()"),
            ),
        ];
        std::fs::write(
            repo.root().join("src/main.rs"),
            "fn main() {\n    alpha();\n    gamma();\n}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-absorb"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned, "the formatter's churn folds too");
        assert_eq!(summary.landed[0].absorbed, vec!["Edit /src/main.rs"]);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
        assert_eq!(
            edits(&repo, &summary.fork),
            vec![vec![(
                "fn main() {}".to_string(),
                "fn main() {\n    alpha();\n    gamma();\n}".to_string()
            )]],
            "one span, no reconciliation"
        );
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        let annotation = &lines[0].annotation;
        let harness = annotation.harness.as_ref().unwrap();
        assert_eq!(harness.call, "toolu_1");
        assert_eq!(harness.absorbed, vec!["toolu_2".to_string()]);
        assert_eq!(annotation.prose.as_deref(), Some("a body\n\nrename beta"));
        // Idempotent: the absorbed call counts as recorded.
        let again = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(again.already, 2);
        assert!(again.landed.is_empty());
        assert!(again.skipped.is_empty(), "{:?}", again.skipped);
        assert!(again.residue.is_empty(), "{:?}", again.residue);
    }

    #[test]
    fn a_rewrite_reaching_past_the_earlier_span_lands_on_its_own() {
        // Edit 2's old_string reaches into the base's bytes beyond what
        // edit 1 wrote.  Folding it would pull bytes edit 1 never named into
        // edit 1's precondition, so both calls land as written.
        let repo = temp_repo("overlap");
        seed(&repo, "/src/main.rs", "fn main() {}\nfn tail() {}\n");
        let session = "s-overlap";
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "a body",
                main_edit(&repo, "toolu_1", "fn main() {}", "fn main() { a(); }"),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "across the seam",
                main_edit(&repo, "toolu_2", "a(); }\nfn tail", "b(); }\nfn tail2"),
            ),
        ];
        std::fs::write(
            repo.root().join("src/main.rs"),
            "fn main() { b(); }\nfn tail2() {}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-overlap"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 2, "{:?}", summary.skipped);
        assert!(summary.landed.iter().all(|l| l.absorbed.is_empty()));
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn a_line_after_an_aligned_one_reads_the_blob_the_fork_holds() {
        // Edit 1 is aligned with the formatter; edit 2, on base bytes of the
        // same file, met the aligned content when it landed.  Its read set
        // names that content's blob, not the transcript's unaligned one.
        let repo = temp_repo("reads");
        seed(&repo, "/src/main.rs", "fn main() {}\nfn tail() {}\n");
        let session = "s-reads";
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "a body",
                main_edit(
                    &repo,
                    "toolu_1",
                    "fn main() {}",
                    "fn main() {\n        a();\n}",
                ),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "rename tail",
                main_edit(&repo, "toolu_2", "fn tail()", "fn end()"),
            ),
        ];
        std::fs::write(
            repo.root().join("src/main.rs"),
            "fn main() {\n    a();\n}\nfn end() {}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-reads"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 2, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let met = repo
            .blobs()
            .put(b"fn main() {\n    a();\n}\nfn tail() {}\n")
            .unwrap();
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        let reads = lines[1].annotation.reads.as_ref().unwrap();
        assert_eq!(reads[0]["path"], json!("/src/main.rs"));
        assert_eq!(reads[0]["blob"], json!(met));
        assert_eq!(reads[0]["spans"], json!(["fn tail()"]));
    }

    #[test]
    fn churn_on_the_line_a_mid_line_span_began_widens_the_span() {
        // The edit began mid-line, and the formatter changed bytes both
        // inside its span and just before it on the same line.  The bytes
        // before it are the base's, untouched since, so the span widens over
        // them and the churn folds back.
        let repo = temp_repo("widen");
        seed(&repo, "/src/main.rs", "let x  =  old();\nkeep();\n");
        let session = "s-widen";
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "call new",
            main_edit(&repo, "toolu_1", "old();", "new( );"),
        )];
        std::fs::write(repo.root().join("src/main.rs"), "let x = new();\nkeep();\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-widen"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
        assert_eq!(
            edits(&repo, &summary.fork),
            vec![vec![(" =  old();".to_string(), "= new();".to_string())]]
        );
    }

    #[test]
    fn a_fold_that_breaks_a_later_precondition_falls_back_to_the_calls() {
        // Edit 3 rewrites bytes edit 1 wrote, so it would fold into edit 1 —
        // but the folded edit 1 writes `y();` before edit 2 runs, and edit
        // 2's `y();` is then no longer unique.  The plan's replay catches it
        // and every call lands as written.
        let repo = temp_repo("fallback");
        seed(&repo, "/src/main.rs", "fn a() {}\ny();\n");
        let session = "s-fallback";
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "one",
                main_edit(&repo, "toolu_1", "fn a() {}", "fn a() { z(); }"),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "two",
                main_edit(&repo, "toolu_2", "y();", "w();"),
            ),
            assistant(
                &repo,
                "u3",
                session,
                "three",
                main_edit(&repo, "toolu_3", "z()", "y()"),
            ),
        ];
        std::fs::write(repo.root().join("src/main.rs"), "fn a() { y(); }\nw();\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-fallback"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 3, "{:?}", summary.skipped);
        assert!(summary.landed.iter().all(|l| l.absorbed.is_empty()));
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn churn_spanning_two_calls_stays_with_the_reconciliation() {
        // Two calls wrote neighbouring spans; the formatter changed bytes
        // across both.  Neither owns the change, so it is reconciled.
        let repo = temp_repo("two-owners");
        seed(&repo, "/src/main.rs", "a(); b();\n");
        let session = "s-two-owners";
        let entries = vec![
            assistant(
                &repo,
                "u1",
                session,
                "first",
                main_edit(&repo, "toolu_1", "a();", "aa( );"),
            ),
            assistant(
                &repo,
                "u2",
                session,
                "second",
                main_edit(&repo, "toolu_2", " b();", " bb( );"),
            ),
        ];
        std::fs::write(repo.root().join("src/main.rs"), "aa(); bb();\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-two-owners"), session, &entries);
        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 2, "{:?}", summary.skipped);
        assert!(summary.landed.iter().all(|l| !l.aligned));
        assert_eq!(summary.residue, vec!["M /src/main.rs".to_string()]);
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
    }

    #[test]
    fn formatter_churn_outside_any_span_stays_with_the_reconciliation() {
        // The formatter fixed the indentation of both the function the
        // session wrote and a pre-existing one.  The session's own span
        // folds back into its line; the pre-existing churn is nobody's
        // span, so it stays with the reconciliation.
        let repo = temp_repo("split");
        seed(
            &repo,
            "/src/main.rs",
            "fn helper() {\n        help();\n}\n\nfn main() {}\n",
        );
        let session = "s-split";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "give main a body",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("src/main.rs").to_string_lossy(),
                       "old_string": "fn main() {}",
                       "new_string": "fn main() {\n        work();\n}"}),
            ),
        )];
        std::fs::write(
            root.join("src/main.rs"),
            "fn helper() {\n    help();\n}\n\nfn main() {\n    work();\n}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-split"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned);
        assert_eq!(summary.residue, vec!["M /src/main.rs".to_string()]);
        assert!(summary.reconciled.is_some());
        let (expected, actual) = repo.check(&summary.fork).unwrap();
        assert_eq!(expected, actual);
        // The reconciling line is the formatter's own footprint: one small
        // span that does not re-touch the line the session wrote.
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        assert_eq!(lines.len(), 2);
        let mut carried = String::new();
        for op in &lines[1].intent.ops {
            match op {
                Op::Edit {
                    old_str, new_str, ..
                } => {
                    carried.push_str(old_str);
                    carried.push_str(new_str);
                }
                other => panic!("expected an edit, got {other:?}"),
            }
        }
        assert!(
            !carried.contains("work();"),
            "the reconciliation leaves the session's line alone: {carried:?}"
        );
    }

    #[test]
    fn a_multi_edit_aligns_each_span_the_formatter_touched() {
        let repo = temp_repo("multi");
        seed(&repo, "/src/lib.rs", "fn a() {}\n\nfn b() {}\n");
        let session = "s-multi";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "bodies for both",
            tool_use(
                "toolu_1",
                "MultiEdit",
                json!({"file_path": root.join("src/lib.rs").to_string_lossy(),
                "edits": [
                    {"old_string": "fn a() {}",
                     "new_string": "fn a() {\n        alpha();\n}"},
                    {"old_string": "fn b() {}",
                     "new_string": "fn b() {\n        beta();\n}"},
                ]}),
            ),
        )];
        std::fs::write(
            root.join("src/lib.rs"),
            "fn a() {\n    alpha();\n}\n\nfn b() {\n    beta();\n}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-multi"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        assert_eq!(lines.len(), 1, "no reconciliation");
        let news: Vec<&str> = lines[0]
            .intent
            .ops
            .iter()
            .map(|op| match op {
                Op::Edit { new_str, .. } => new_str.as_str(),
                other => panic!("expected an edit, got {other:?}"),
            })
            .collect();
        assert_eq!(
            news,
            vec!["fn a() {\n    alpha();\n}", "fn b() {\n    beta();\n}"]
        );
    }

    #[test]
    fn a_formatter_insertion_inside_the_span_folds_back() {
        // A pure insertion inside the written span — the formatter spaced
        // the two calls apart — belongs to the span's owner.
        let repo = temp_repo("align-ins");
        seed(&repo, "/src/main.rs", "fn main() {}\n");
        let session = "s-align-ins";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "two calls",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("src/main.rs").to_string_lossy(),
                       "old_string": "fn main() {}",
                       "new_string": "fn main() {\n    one();\n    two();\n}"}),
            ),
        )];
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    one();\n\n    two();\n}\n",
        )
        .unwrap();
        let transcript = write_transcript(&temp_dir("tx-align-ins"), session, &entries);

        let summary = commit(&repo, &options(&transcript, session)).unwrap();
        assert_eq!(summary.landed.len(), 1, "{:?}", summary.skipped);
        assert!(summary.landed[0].aligned);
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        let lines = repo.current_state(&summary.fork).unwrap().lines;
        match &lines[0].intent.ops[0] {
            Op::Edit { new_str, .. } => {
                assert_eq!(new_str, "fn main() {\n    one();\n\n    two();\n}")
            }
            other => panic!("expected an edit, got {other:?}"),
        }
    }

    #[test]
    fn a_dry_run_previews_the_same_alignment() {
        let repo = temp_repo("align-dry");
        seed(&repo, "/src/main.rs", "fn main() {}\n");
        let session = "s-align-dry";
        let root = repo.root().to_path_buf();
        let entries = vec![assistant(
            &repo,
            "u1",
            session,
            "a body",
            tool_use(
                "toolu_1",
                "Edit",
                json!({"file_path": root.join("src/main.rs").to_string_lossy(),
                       "old_string": "fn main() {}",
                       "new_string": "fn main() {\n        hello();\n}"}),
            ),
        )];
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    hello();\n}\n").unwrap();
        let transcript = write_transcript(&temp_dir("tx-align-dry"), session, &entries);
        let mut opts = options(&transcript, session);
        opts.dry_run = true;
        let summary = commit(&repo, &opts).unwrap();
        assert_eq!(summary.landed.len(), 1);
        assert!(summary.landed[0].aligned, "the preview is the plan");
        assert!(summary.residue.is_empty(), "{:?}", summary.residue);
        assert!(!repo.fork_exists(&format!("claude-{session}")).unwrap());
    }
}
