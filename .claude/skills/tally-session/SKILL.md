---
name: tally-session
description: Record this Claude Code session in tally. Use at the end of a session in a tally repository — when the user says they are done, wrapping up, or asks to commit, record, save, or ingest the session — and whenever the user asks what tally would record. Also read it before editing files in a tally repository, because one editor habit (replace_all) costs the substrate its read set.
---

# Recording a session in tally

This repository is version-controlled by tally, where **a session is a fork**
and **every tool call is a patch**. Claude Code's tool set is fixed, so tally
cannot be handed an instrumented editor the way pi can; instead it reads the
session's transcript back afterwards. One command does that:

```console
tally claude commit
```

It reads this session's transcript and the working tree, lands one log line per
observed `Edit`/`MultiEdit`/`Write` call on the fork `claude-<session id>`, and
then appends one reconciling line for whatever the working tree holds that those
calls do not explain. The fork goes back to where the session uniquely
originates: a continued session skips the entries it inherited and anchors on
the fork of the session it continues.

## When to run it

- At the **end of a session** in this repository — the user wrapping up ("that's
  it", "we're done", "ship it") is the cue. Say what landed in one line.
- Whenever the user asks to **record, ingest, save, or commit the session**.
- Before handing work to another fork (`tally union`) or to git
  (`tally git pr`), since both read from a fork, not from the working tree.

Previewing it first is free and writes nothing:

```console
tally claude commit --dry-run
```

If the output says this session **continues** an earlier one whose fork does not
exist yet, ingest that one first — `tally claude commit --session <that id>` —
and run it again; the two forks then chain, and `tally log` reads back through
both.

Running it twice is safe: each line records the call behind it, so a second run
appends only what is new. If the command reports `skipped` calls, that is not a
failure — the file moved under tally since the call, and the reconciling line
carries what the call did.

## While editing: keep the spans unique

tally's edit op consumes a span that occurs **exactly once** in the file. That
is what makes a patch commute with someone else's and what makes its read set a
span rather than a whole file.

- **Do not use `replace_all`.** tally has no every-occurrence op and will not
  grow one. An observed `replace_all` is still recorded — it is materialized as
  a series of spans each widened with context until unique — but those spans are
  tally's reconstruction, not the ones you chose. Widen `old_string` with
  surrounding context until it is unique instead, and issue one edit per site.
- **Prefer `Edit`/`MultiEdit`/`Write` over shell redirection.** A `python`,
  `sed`, or heredoc rewrite inside `Bash` is invisible to the transcript, so it
  lands in the reconciling line with no read set and no span. Edits made through
  the editing tools land as patches with their preconditions intact.
- **Run formatters without ceremony.** A `cargo fmt` (or prettier, or a hook)
  rewrites inside `Bash`, which the transcript cannot see — but when its churn
  falls entirely within a span one of your edits wrote, `tally claude commit`
  folds it back into that edit's line and marks the line `(aligned with the
  working tree)`. The reconciling line then carries only the formatter's
  footprint on code the session did not write.
- **Write a sentence before a change.** The prose in a turn becomes the
  narrative of that turn's first patch; `tally log` is read by humans.

## Reading it back

```console
tally log --fork claude-<session id>     # the session as a chain of patches
tally status --fork claude-<session id>  # the fork against the working tree
tally fuse --fork <fork> session <first> <last>   # collapse it to one beat
```

`tally claude commit` prints the `fuse` invocation for the span it landed.
Fusing is lossless: the per-call lines stay underneath forever.
