# abelian

*A version control substrate in which states are sums, patches commute, and
history is arithmetic.*

abelian is version control for an instrumented author population. Its authors
are agents: processes whose every read and write passes through a harness, so
that the preconditions of a change are *observed* rather than inferred. Humans
participate the way they did in 2005 — by mailing patches to a maintainer —
except the maintainer is an agent, and the emergency exit is load-bearing and
documented below.

The system is named for the group. State identity in abelian forms an abelian
group: patches are elements, composition is addition, undo is the inverse, and
order never matters. The command shares the name. Its merge takes after the
split tally stick — the twelfth century's distributed, two-party, checksummed
ledger. Notch the
stick, split it lengthwise, keep the *stock*, hand over the *foil*; settlement
is whether the halves still align. `abelian union` is pressing the halves
together.

abelian descends from the patch-theory lineage of darcs and Pijul —
patches-as-primary, commutation as the foundation of merging. What it adds is
the thing that lineage could not have: authors whose read sets are complete,
mechanical, and free to record. Textual merge conflicts were always a heuristic
approximation of read sets, forced on us by authors who read with their eyes.
abelian's authors do not.

## How to read this document

This ANDON.md teaches abelian by hand: a text editor, coreutils, and a few lines
of Python. At every step, the `abelian` command that automates the step is named
in passing. Do not skip the by-hand path, because it is not pedagogy alone.
abelian guarantees that with zero models available — and, if necessary, zero
`abelian` binaries available — the substrate degrades to a complete, operable
version control system. The procedure for operating it in that state is this
document. If you finish the walkthrough, you have finished the spec, and you
are holding the emergency cord. We will name it when we get there.

You will need `python3` and an editor. Nothing here talks to a network or a
model.

## 1. Elements

A state — what git would call a tree — is a finite set of **elements**. An
element is a file, fully qualified:

```
mode <TAB> path <TAB> sha3-256(blob) <LF>
```

`mode` is octal (`100644`, `100755`, `120000` for symlinks). `path` is
absolute from the repository root, beginning with `/`. The blob hash is the
SHA3-256 of the file's content, in lowercase hex. Those canonical bytes — tab
separators, trailing newline, nothing else — are the element's **record**.
There are no directories. A directory is a prefix query over paths, which is
an index concern, not a substrate concern.

Compute a record by hand:

```sh
python3 -c "import hashlib,sys; print(hashlib.sha3_256(open(sys.argv[1],'rb').read()).hexdigest())" src/main.rs
# then write:  100644 <TAB> /src/main.rs <TAB> <that hex> <LF>
```

`abelian sum` walks the working tree and produces every record automatically.

## 2. The sum

State identity is a **setsum** over the element records ([setsum
crate](https://crates.io/crates/setsum); the construction below is that crate's,
verbatim). A setsum is 256 bits organized as eight little-endian u32 columns.
Each column lives in its own field, defined by eight distinct primes just below
2³²:

```python
PRIMES = [4294967291, 4294967279, 4294967231, 4294967197,
          4294967189, 4294967161, 4294967143, 4294967111]
```

To insert an item, hash its record with SHA3-256, read the 32-byte digest as
eight little-endian u32s, reduce each modulo its column's prime, and add
column-wise, again modulo the prime. To remove an item, add its columnwise
inverse, `P[i] - x[i]`. The empty state is all zeros.

```python
import hashlib

PRIMES = [4294967291, 4294967279, 4294967231, 4294967197,
          4294967189, 4294967161, 4294967143, 4294967111]

def state_of(record: bytes):
    h = hashlib.sha3_256(record).digest()
    cols = [int.from_bytes(h[4*i:4*i+4], 'little') for i in range(8)]
    return [c % p for c, p in zip(cols, PRIMES)]

def add(a, b):  return [(x + y) % p for x, y, p in zip(a, b, PRIMES)]
def neg(a):     return [(p - x) % p for x, p in zip(a, PRIMES)]

def sum_hex(s):
    return b''.join(x.to_bytes(4, 'little') for x in s).hex()
```

Fold every element record in your tree through `add`, starting from zeros.
The order in which you fold does not matter. That sentence is the entire
design; everything below is a consequence of it.

Three laws, which you can check by hand and which `abelian` property-tests:

1. **Commutativity and associativity.** Any fold order yields the same sum.
2. **Identity and inverses.** Zeros is the empty state; `add(s, neg(s))` is
   zeros; inserting then removing an element is a no-op.
3. **Equality means equality.** Two sums are equal iff the underlying
   multisets are equal, with high probability, under non-adversarial authors.

And one warning, which is the substrate's central soundness obligation: the
sum lives in the *free* abelian group. Removing an element that was never
present does not fail — it accrues a **placeholder debt** that a future insert
of that element will silently consume, exactly as the setsum crate documents.
Valid repository states are a small subset of the group. Therefore: **the sum
attests; the manifest adjudicates.** No remove is ever applied without a
membership check against the manifest (§3). The checksum proves you did what
you said; only the manifest can prove what you said was meaningful.

`abelian sum` computes the working tree's sum. `abelian check` recomputes it and
compares against the log's expectation.

## 3. Snapshots

A **manifest** is a materialized state: a header, then every element record,
sorted bytewise. Sorting is for humans and diff tools; the sum does not care.

```
abelian-manifest v0
sum 3b9f…e2
100644	/README.md	7c41…
100644	/src/main.rs	ab12…
100755	/tools/apply	90ee…
```

Write one by hand with the loop from §2 and `sort`. Verify it by folding its
records and comparing against its own `sum` line. A manifest whose sum line
disagrees with its records is corrupt, full stop.

Snapshots are *derived*, never primary. abelian stores a log of patches
(§5) with periodic manifests as anchors; a manifest is a compaction of the
log, the way an SST is a compaction of a WAL. You never need a manifest to
know a state's identity — the log's arithmetic gives you that — only to
adjudicate membership and to materialize working trees.

`abelian snapshot` writes a manifest at the current state. `abelian materialize
<sum>` produces a working tree from the nearest anchor plus replayed patches.

## 4. Patches

A patch has two forms, and the distinction matters.

**Intent form** is what an author writes. It is a JSON object of span
operations:

```json
{"ops": [
  {"edit":   {"path": "/src/main.rs",
              "old_str": "println!(\"hello\");",
              "new_str": "println!(\"hello, abelian\");"}},
  {"create": {"path": "/src/lib.rs", "mode": "100644",
              "content_b64": "…"}},
  {"delete": {"path": "/old.rs", "blob": "ab12…"}},
  {"chmod":  {"path": "/tools/apply", "old_mode": "100644",
              "new_mode": "100755"}}
]}
```

The precondition of an `edit` is that `old_str` occurs in the current blob at
`path` **exactly once**. Zero matches or several matches: the patch does not
apply. This is the same contract as the `str_replace` tool every agent harness
already ships, and the uniqueness requirement is doing quiet, deep work: by
widening `old_str` with surrounding context until it is unique, the span
becomes content-addressed within the file, position-independent. Unified
diff's hunks are positional and need fuzz; span patches need none. That is
what lets two edits to disjoint spans of the *same file* commute. The
precondition of a `delete` is the full blob hash — you consume the whole
element. The precondition of a `create` is the path's absence.

**Applied form** is what the log records: the intent, plus the **realized
delta** — the concrete element records removed and added, which depend on the
state the patch met:

```json
{"realized": [
  {"remove": "100644\t/src/main.rs\tab12…",
   "add":    "100644\t/src/main.rs\tcd34…"}
]}
```

Intent commutes and travels; realization is a fact about one application. The
log stores both so the sum can be replayed and inverted by pure arithmetic
(§2) without re-running span logic.

Apply a patch by hand:

1. For each `edit`, count occurrences of `old_str` in the current blob.
   Anything but one: stop.
2. Make the edits in your editor.
3. Rehash each touched blob; write the realized delta.
4. New sum = old sum, `add` the `neg` of each removed record, `add` each
   added record.
5. Check each removed record against the manifest first — the placeholder-debt
   rule from §2 is enforced *here*, at application, always.

`abelian apply <patch.json>` does all five steps, atomically, and appends to the
log.

## 5. The log

The log is JSONL, one applied patch per line:

```json
{"id": "…", "prev": "…",
 "intent": {"ops": […]},
 "realized": […],
 "sum_after": "3b9f…",
 "annotation": {
   "author": "sid@fable-5", "provenance": "agent",
   "session": "…", "prose": "narrow the retry loop",
   "reads": [{"path": "/src/retry.rs", "blob": "77aa…",
              "spans": ["fn backoff("]},
             {"grep": "unwrap\\(\\)", "matches": [], "over_sum": "3b9f…"}]}}
```

`id` is the SHA3-256 of the line's canonical bytes with the `id` field empty;
`prev` chains the line to its predecessor. The chain orders the *narrative*;
the arithmetic never needed it.

One annotation field carries the whole reason abelian exists:

**`reads`** is the patch's observed read set — not what a tool scanned, but
what entered the author's context. A viewed span is a read. A grep that
returned three lines read three spans *plus* one universally quantified
negative — "pattern absent elsewhere at this sum" — and the negative is
recorded too, because the author may have acted on the absence. Humans cannot
honestly produce this field, which is why humans do not write to the substrate
directly (§8).

Record everything. The economics are not close: generating a gigabyte of
model exhaust costs north of $10,000 in tokens; storing it costs about $0.02
per month. Information the harness observed and discarded is the only true
waste in this system. `fuse` (§7) is lossless for the same reason.

`abelian log` renders the chain; `abelian show <id>` renders one line at any
granularity.

## 6. Fork and union

A **fork** is an anchor and an empty log:

```
abelian-fork v0
anchor 3b9f…e2
```

That is the whole file. Forks are what git called branches and also what a
harness calls sessions; abelian does not distinguish. Abandoned forks are not
deleted — they are the searchable, auditable exhaust of work that didn't ship.
`abelian fork <name>` writes the file and materializes a working tree if asked.

**Union** brings a fork's log into a target state, and it is stratified so
that each stratum is strictly cheaper than the next and almost all work stops
early:

1. **Arithmetic.** If the fork's final sum equals the target's, the states are
   already identical. Done, O(1).
2. **Realized replay.** For each incoming applied patch, check its removed
   records against the target manifest. All present: apply the realized delta
   directly — membership plus addition, no content inspection.
3. **Intent replay.** A consumed record is missing because the target drifted.
   Re-validate the *span* precondition against the target's current blob:
   `old_str` still unique? Apply, realize fresh deltas. This is where
   disjoint-span edits to the same file sail through.
4. **Re-enactment.** All mechanical strata failed: the patch's assumptions are
   genuinely dead. Hand the intent, its prose, and the conflict evidence to a
   model to re-derive against the current state. This is the only stratum
   that costs tokens, and the design's job is to make it rare.

Two patches **commute** iff neither's write spans intersect the other's read
set and their write spans are pairwise disjoint. Union order among commuting
patches is irrelevant — the group guarantees the sum, and the precondition
discipline guarantees the states. Conflict is not textual overlap; conflict is
`W₁∩R₂ ≠ ∅`, checked against *observed* reads. Rebase does not exist:
reordering commuting patches changes nothing the substrate can see, so
history-rewriting is a rendering option (§7), not an operation.

By hand, union is a loop over incoming log lines applying strata 1–3 with the
tools you already built in §§2–5. `abelian union <fork>` runs the loop and
stops before stratum 4 unless invited.

## 7. Fuse and reading

`fuse` composes a span of patches into one narrative beat — what git called a
commit, squash, and fixup, unified. It is **lossless**: a fuse is a view — a
log line with an empty realized delta, never a mutation of the lines it
covers:

```json
{"intent": {"ops": []}, "realized": [], "sum_after": "…",
 "annotation": {"provenance": "view", "view": {"from": "id-17", "to": "id-42"},
                "prose": "retry loop: bounded backoff", "author": "…"}}
```

The fine structure — every tool call — remains underneath, forever, at
$0.02/GB-month. Because a view is a line, it travels: union lands it like
any other patch (its arithmetic is the identity) and re-keys the span onto
the target's ids, so the narrative survives fuse → union → remove-fork. And
because views are ordered, a span can be annotated after the fact and a
later view supersedes an earlier one it overlaps — fuse a span as an active
incident, later mark it resolved; both lines stay, and any log prefix
renders the status it had then. `abelian read` renders at a chosen zoom:
`--fused` for the human narrative, `--raw` for the tool-call stream. The
human view of history is a default zoom level, not a different interface.

## 8. Authors

Agents are the only direct writers. An agent's harness emits the applied
patch and the observed reads as a side effect of working — the annotation in
§5 is not extra work, it is exhaust that the harness stops throwing away.

Humans contribute the way maintainers always received work: by submitting a
PR to an agent. Two forms, one treatment:

**A code patch is not applied — it is re-enacted.** The human's diff is
testimony. The agent performs the change through its own instrumented
toolchain against the *current* state, generating live preconditions and real
reads. Staleness against HEAD is irrelevant; the diff was evidence of intent,
never bytes to copy. Merge conflicts with human patches cease to be a
category.

**A natural-language patch is the same thing minus the worked example.** The
PR conversation has a defined termination condition: negotiation until a
predicate is agreed. The agent proposes the characterization test; the human
confirms *that test is what I meant*; the test enters the tree and the
implementation becomes the agent's problem. The human signs the predicate,
not the patch.

Provenance is a chain, retained verbatim: human PR → agent session → span
patches. `abelian submit` opens the negotiation; `abelian enact` runs the
re-enactment under a maintainer policy.

Policy — which checks must pass for a union into a given fork, who may pull
the cord below, what re-enactment may touch — is rules over facts, and lives
in a Datalog layer over the log, not in this document.

## 9. The Andon cord

Now the reveal, though you may have seen it coming: **you have just operated
abelian end to end with an editor and eleven lines of Python.** No model. If
you used the Python instead of the binary, no `abelian` either.

That was the point. This walkthrough is the **Andon cord**: the guaranteed
path by which a human applies a non-interpreted patch with no LLM in the
loop. The 3 a.m. security fix, the provider outage, the maintainer-agent that
*is* the bug — you pull the cord:

1. Write the intent JSON by hand (§4).
2. Apply it: `abelian apply --provenance=andon --reason="CVE-2026-…" --sign`,
   or, if even `abelian` is unavailable, the five steps of §4 and a hand-written
   log line with `"provenance": "andon"`.
3. The patch is a first-class citizen of the mechanical layer — exact-match
   validation, membership check, sum update, chained log line. What it
   honestly lacks is instrumented reads, and it does not fake them: the
   `reads` field is absent.
4. The pull is loud. An andon-provenance line is an event agents are required
   to reconcile against post hoc: review moves after apply; it does not
   disappear.

The cord needs no special concurrency handling. An Andon patch lands, and
every in-flight patch whose reads it invalidated simply fails stratum 2 or 3
at union and re-derives — which is what happens when *any* concurrent write
lands. The substrate cannot tell an emergency from a Tuesday. That is the
group structure earning its keep.

Two invariants follow, and they are the two we hold hardest:

- **The native format is human-writable.** If the emergency path cannot be
  operated with an editor and one static binary, the format is wrong.
- **With zero models, abelian degrades to a complete VCS.** The model is a
  layer on the substrate, never a load-bearing wall in it.

## The spec, compressed

An element is `mode\tpath\tsha3(blob)\n`. A state is a set of elements; its
identity is the setsum of their records — eight u32 columns, distinct primes
below 2³², SHA3-256 per item, columnwise modular addition, inverse `P−x`. The
sum lives in the free abelian group; validity does not: every remove is
membership-checked against a manifest, because placeholder debt is silent.
A patch is span operations with content-addressed, position-independent
preconditions (`old_str` unique in blob); its application realizes a concrete
element delta; the log records both, chained, with observed reads as
annotation. Union is stratified — arithmetic, realized replay, intent
replay, re-enactment — and only the last stratum costs tokens. Two patches commute
iff writes miss each other's reads and each other. Snapshots are compactions;
forks are anchors plus logs; sessions and branches are the same object;
fuse is a lossless view; rebase is a rendering option. Agents write; humans
submit; the Andon cord is this document. Nothing observed is ever discarded,
because generation costs five orders of magnitude more than storage.

---

*abelian* is the group; its merge is the stick: the stock stays with the
substrate, the foil goes home with you, and history is whatever still aligns
when you press the halves together.
