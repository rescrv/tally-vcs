# Tally

Tally is an agents-only version control system that represents patches as morphisms in a groupoid.
It is designed to plug into a GitHub-centric workflow, allowing you to adopt Tally one developer at
a time.

## Quick Start

```console
cargo install --path .
cd /path/to/existing/git/repo
tally init
tally git pull main
```

You've successfully, deterministically imported your git repo up to the first commit that has less
than or greater than one parent.  Subsequent pulls will pull more.

## Why your checksum should work like a bank balance

A bank balance is a checksum with properties SHA-256 doesn't have. When a
deposit clears, the bank doesn't re-add every transaction since your account
opened — it adds one number. When two banks reconcile, they don't replay each
other's ledgers entry by entry — they compare balances. And it doesn't matter
in which order the deposits cleared: three checks arriving
Tuesday–Wednesday–Thursday or Thursday–Tuesday–Wednesday leave the same
balance. Deposits commute. Withdrawals undo them exactly. Nobody finds any of
this surprising, and whole civilizations of accounting rest on it.

A cryptographic hash is the opposite kind of object. It's a fingerprint of a
frozen artifact: change one byte and you must re-read everything. Chain the
hashes — git, blockchains — and you gain history, but you bake the *order of
events* into the identity of the state. This is why two git servers that
arrive at byte-identical trees by different routes hold different commit SHAs
for the same code. Git can answer "did we follow the same path?" It cannot
cheaply answer "did we arrive at the same place?"

That second question is the one replication systems ask thousands of times a
second. Run a fleet of replicas and patches arrive out of order — a gossip
packet delayed, a batch split differently, a compaction racing a push. Same
patches, same final files, and no cheap way to prove it. Comparing heads
fails: identical trees, different SHAs. Re-hashing both repositories works but
costs a full scan of a monorepo, per check, per replica — which is why systems
like GitHub's Spokes end up maintaining constantly-updated checksum tables for
every copy of every repository, and treating each copy as a pet.

Setsum is the missing third kind of checksum: a fingerprint that behaves like
a balance. Hash each element of the state — each `(mode, path, blob-hash)`
record — into a huge number, and define the checksum of the state as the sum.
Adding a file adds its number. Deleting subtracts it. A patch is
`− old records + new records`, so maintaining the checksum costs time
proportional to the patch, never the repository. The same set of patches
yields the same 256-bit sum in any arrival order. Reconciling two replicas is
comparing 32 bytes — and if they disagree, subtracting one sum from the other
yields the checksum of *exactly the elements that differ*. Collisions remain
as hard to engineer as for an ordinary cryptographic hash. Only the
brittleness is gone.

The subtraction deserves a moment, because a "negative file" sounds like
nonsense until you remember that negative numbers were invented for exactly
this. There is no pile of −3 apples anywhere in the world; −3 means "I owe
you three apples." Negative numbers aren't statements about piles — they're
statements about transactions, and merchants accepted them centuries before
mathematicians did, because ledgers demanded it. Likewise a sum with negative
parts is not a state — no directory contains an anti-file — it's an
obligation: *take this exact record away from me*. That extra room in the
arithmetic is where the verbs live. A patch is literally `− old + new`. An
undo is a sign flip — negate a patch's digest and you hold the reverting
patch, as arithmetic. The difference of two divergent replicas is a repair
ticket. And negate a state's own sum and you hold its anti-repo: add the two
and they annihilate to sixty-four zeros, the checksum of the empty state.
States are the nouns; the group also holds the verbs.

One consequence deserves its own paragraph, because git cannot do it even in
principle: the checksum of a merge is computable *before the merge happens*.
Two agents fork the same anchor and produce non-conflicting patches. Anchor
sum, plus patch A's digest, plus patch B's digest — three additions — is the
sum the merged state must have. Perform the merge, fold the result: if it
doesn't match the prediction, the merge is corrupt, and anyone holding three
32-byte values can check it. Every merge becomes independently verifiable,
and its result has a name both sides agreed on before either did the work.

A checksum cannot resolve a merge conflict, and setsum doesn't pretend to.
The arithmetic guarantees sums; *preconditions* guarantee states. Every patch
records what it read — an edit requires its target span to still exist,
exactly once, in the file it's about to change. When two patches touch
disjoint spans, either order finds its precondition intact, the patches
commute, and the predicted sum is the law. When they collide, the second
patch's precondition fails — cheaply, mechanically, before anything is
mangled — and the conflict escalates to an actual resolution, which becomes a
new patch with a new digest. So conflict isn't "same file," and it isn't
silently summed over; it's "your writes hit what I read," detected by check
rather than assumed away. Everything outside that set — in practice, almost
everything — needs no merge machinery at all. Just addition.

Tally is named for the split tally stick — the twelfth century's two-party,
checksummed ledger. Notch the stick, split it lengthwise; settlement is
whether the halves still align. `tally union` is pressing the halves back
together.

*For the algebraically inclined: states and patches form an abelian group
under this arithmetic — "order never matters" is commutativity, "undo is
subtraction" is inverses, and everything above is a homomorphism from states
to sums. The rest of the design falls out of those three words, but you
never need them to use the tool.*

# A worked example: two agents, one anchor, sixty-four characters

Every number on this page is real. The sums are computed by the setsum
construction exactly as specified — eight u32 columns, eight primes just below
2³², SHA3-256 per record — and the git hashes come from actually running git.
A ~25-line Python script at the end reproduces all of it.

## The anchor

A repository is a *set of element records*, one per file:

```
mode <TAB> path <TAB> sha3-256(blob) <LF>
```

Our repo has three files. Its manifest:

```
100644  /Cargo.toml    42ddabec…4b44e1
100644  /README.md     845d216c…aaafcb
100644  /src/main.rs   011f543c…72adde
```

Read that table carefully, because there are **two layers of hashing** and
keeping them apart is everything. `011f543c…` is the SHA3-256 of the *bytes of
main.rs* — it names the content, and it lives inside the record as text. But
what feeds the setsum is the SHA3-256 of the *record line itself* — those
canonical bytes `100644<TAB>/src/main.rs<TAB>011f543c…<LF>`, which hash to a
completely different value (`576ca889…`). So a "file" enters the arithmetic as
one indivisible item: its mode, its path, and its content-hash, welded into
one line and hashed together. Rename a file, chmod it, or change one byte of
it, and its old record leaves the set and a new record enters — there is no
partial membership. Blob hashes name content; record hashes are the atoms of
the sum.

Fold the three records into a setsum — hash each record, split the digest into
eight columns, add columnwise mod each column's prime — and the state has a
name:

```
anchor = 8a5139f698914ccf4adcd5328821ce102be3c288c7ed5683f136329b28a29282
```

Note what the name does *not* contain: no parent pointer, no timestamp, no
author, no order. It is a pure function of the set of files. Any two parties
holding these three files compute this exact string, forever.

## Two agents fork the anchor

**Agent A** edits the greeting in `src/main.rs`. In the log this is one line
with an *intent* (the precondition-carrying edit) and a *realized* delta (the
record-level effect it had):

```json
{"intent":   {"ops": [{"edit": {"path": "/src/main.rs",
                                "old_str": "hello",
                                "new_str": "hello, tally"}}]},
 "realized": [{"remove": "100644\t/src/main.rs\t011f543c…72adde",
               "add":    "100644\t/src/main.rs\td31875dd…d2368a"}]}
```

The patch's digest is pure arithmetic on the realized delta —
`(− removed record) + (added record)`:

```
digest(A) = d6a1c9d5e67b7a0adf8c13f1d29d68a6518f7f5339fea76267d1445e3a053e5b
```

**Agent B**, concurrently and unaware of A, creates `src/lib.rs` and mentions
it in the README:

```json
{"intent":   {"ops": [{"create": {"path": "/src/lib.rs", "mode": "100644",
                                  "blob": "6c2931dd…a22f3b"}},
                      {"edit":   {"path": "/README.md",
                                  "old_str": "A tiny demo service.\n",
                                  "new_str": "A tiny demo service.\n\nSee src/lib.rs for the API.\n"}}]},
 "realized": [{"remove": null,
               "add":    "100644\t/src/lib.rs\t6c2931dd…a22f3b"},
              {"remove": "100644\t/README.md\t845d216c…aaafcb",
               "add":    "100644\t/README.md\t20ae5a3b…9c15a1"}]}
```

```
digest(B) = f206e9d0b2789d08b346abf0cb7a390638372715a09a92c0ab461b3626206b55
```

A patch digest is a first-class value. It doesn't mention the anchor. It is
"what this patch does," as a number, portable to any state that satisfies the
patch's preconditions.

## The arithmetic never sees your edit

Notice what did *not* happen above: `old_str` and `new_str` were never hashed
into anything. An intent has no setsum. The pipeline that turns an edit into
arithmetic runs entirely through content:

```
intent            apply the edit to the file's bytes
  │
  ▼
new blob          fn main() {\n    println!("hello, tally");\n}\n
  │  sha3-256 of the bytes
  ▼
new blob hash     d31875dd…d2368a
  │  weld into a record line
  ▼
new record        100644 \t /src/main.rs \t d31875dd…d2368a \n
  │
  ▼
realized delta    remove old record, add new record
  │  digest = (−cols(old record)) + (+cols(new record))
  ▼
digest(A)         d6a1c9d5…3a053e5b
```

The intent and the realized delta answer different questions and are kept for
different reasons. The intent — "replace this span with that span" — carries
the patch's *preconditions* and travels to states the author never saw; it's
how the patch re-validates and re-applies elsewhere. The realized delta is
the record-level *effect* the patch had on the state it actually met, and it
is the only thing the arithmetic ever touches. Two different intents that
produce the same bytes produce the same realized delta and the same digest;
an intent that fails its precondition produces no delta at all. Sums are
functions of states and their changes — never of the editing instructions
that caused them.



## The prediction

Neither agent has seen the other's work. No merge has been attempted. Yet
anyone holding the three values above can compute what the merged state must
sum to — three columnwise additions:

```
anchor + digest(A) + digest(B)
      = 5cfaeb9c308664e25eb09414253a70bdb4a969f1278791a69c4f922f41c83b33
```

Write that down. It's a commitment, made before the work.

## Two replicas, two orders

Replica 1 receives patch A first, then B. Replica 2 receives them the other
way around — a delayed packet, nothing exotic. Here is every running sum each
replica ever holds:

```
                    replica 1                 replica 2
start    S₀       = 8a5139f6…28a29282       = 8a5139f6…28a29282     (anchor)
1st patch          + digest(A)               + digest(B)
mid      S₀+A     = 65f302cc…62a7d0dd   S₀+B = 815822c7…4ec2fdd7    ← DIFFERENT
2nd patch          + digest(B)               + digest(A)
end      S₀+A+B   = 5cfaeb9c…41c83b33  S₀+B+A = 5cfaeb9c…41c83b33   ← EQUAL
```

Stare at the middle row: the mid-flight sums *differ*, and they should — at
that instant the replicas genuinely hold different states. Replica 1 has the
new greeting but no `lib.rs`; replica 2 has `lib.rs` but the old greeting.
Each mid-flight sum is exactly the fold of the three-or-four files that
replica holds at that moment (you can check: fold `{Cargo.toml, README v1,
main.rs v2}` from scratch and you get `65f302cc…`). The sum is never a hash
of the journey — it is always, at every step, the identity of the current
set of files. That's why the journeys can differ and the destinations can't:
both replicas end holding the same four files, so both sums *must* be the
fold of that set, and there is only one.

Reconciliation between the replicas is therefore a comparison of 64 hex
characters, valid at any moment — matching sums mean matching states, right
now, regardless of what order anything arrived in. No log replay, no tree
walk, no canonical ordering imposed on anyone.

For contrast, here is git given the exact same scenario — same files, same
edits, merge order swapped, and (to be maximally generous) even the commit
timestamps pinned equal on both replicas:

```
replica 1  tree 3abca69b86090b7b   commit 9f9cb6cf024aa24c
replica 2  tree 3abca69b86090b7b   commit 164ea185a7905575
```

Byte-identical trees; different names. The merge commit hashes its parents,
and the parents arrived in different orders. To discover that these replicas
agree, git must walk and compare trees; commit-level comparison answers
"different" for states that are the same. Order was baked into identity, and
here is the invoice.

## Settlement

Now perform the actual merge on either replica, list the resulting four
files, and fold their records from scratch — no history involved, just the
set:

```
100644  /Cargo.toml    42ddabec…4b44e1
100644  /README.md     20ae5a3b…9c15a1
100644  /src/lib.rs    6c2931dd…a22f3b
100644  /src/main.rs   d31875dd…d2368a

fold = 5cfaeb9c308664e25eb09414253a70bdb4a969f1278791a69c4f922f41c83b33
```

The fold of the state equals the prediction made before the merge existed.
Had the merge mangled anything — dropped B's README edit, resurrected the old
`main.rs` — the fold would disagree and the merge would be *provably* corrupt,
checkable by anyone holding the anchor and the two patch digests. This is the
split tally stick: each side kept its half; settlement is pressing them
together and seeing that they still align.

## When they don't align

Suppose replica 2's disk quietly flips `version = "0.1.0"` to `"0.1.1"` in
`Cargo.toml`. The sums now differ — that much any checksum gives you. But
subtract them:

```
healthy − corrupt = c35036ff489978b8cc2f84d425273f4d5fe8fa8e7ca4e294b12e7d6d757372a1
```

and compare with the digest of the patch "replace bad Cargo.toml with good
Cargo.toml":

```
digest(good − bad) = c35036ff489978b8cc2f84d425273f4d5fe8fa8e7ca4e294b12e7d6d757372a1
```

The *difference between two states is itself a patch digest* — a checksum of
exactly what diverged. Repair is targeted, not forensic.

## The negative half

Every sum on this page so far described a state you could `ls`. But the
arithmetic is roomier than that, and the extra room is not slack — it's where
the verbs live. A "negative file" sounds like nonsense until you remember
that negative numbers were invented for exactly this: there is no pile of −3
apples, yet −3 is perfectly meaningful — it means *I owe you three apples*.
Negative numbers describe transactions, not piles. A setsum with negative
parts describes an obligation, not a directory: *take this exact record away
from me*.

Three demonstrations, all real.

**Undo is a sign flip.** Negate digest(A) and you hold the patch that reverts
A — not a data structure describing reversal, the reverting patch itself, as
arithmetic:

```
 digest(A) = d6a1c9d5e67b7a0adf8c13f1d29d68a6518f7f5339fea76267d1445e3a053e5b
−digest(A) = 255e362a098485f5e072ec0ecb619759447080ac4001589d002ebba10dfac1a4

(S₀ + A) + (−digest(A))
           = 8a5139f698914ccf4adcd5328821ce102be3c288c7ed5683f136329b28a29282  = S₀  ✓
```

The `−old main.rs` inside digest(A) met the `+old main.rs` inside the anchor
and annihilated it; negating the patch runs the annihilation in reverse. No
reflog, no revert-commit machinery — the inverse of "do it" is a minus sign.
(The §6 divergence was this same move wearing work clothes: `healthy −
corrupt` is a sum with negative parts, meaningless as a directory listing and
exact as a repair ticket.)

**Every state carries its own anti-state.** Negate the anchor sum itself:

```
 anchor      = 8a5139f698914ccf4adcd5328821ce102be3c288c7ed5683f136329b28a29282
 anti-anchor = 71aec609576eb33075232acd15de31ef6a1c3d77b211a97c76c8cd641f5d6d7d

 anchor + anti-anchor
             = 0000000000000000000000000000000000000000000000000000000000000000
```

Sixty-four zeros is not "no checksum" — it is the checksum of the empty set,
and every state annihilates to it against its own negation. The anti-anchor
is the "delete everything" patch, and it was always there, implicit in the
anchor's own sum.

**Debt can dangle — and that's both the power and the hazard.** The
arithmetic is *free*: subtract a record that isn't present and nothing
errors. Subtract the `lib.rs` record from the anchor — which doesn't contain
it — and you get a perfectly well-formed sum:

```
 anchor − lib.rs record = b38aa69423d319790a4f2475b20916a013ed1172d3d9cabdd4be2afca9352a21
```

No state of any repository folds to this value; it describes an impossible
thing, a repo containing −1 copies of `lib.rs`. The anti-record just lingers
as unsettled debt until a matching insert cancels it — add the record back
and the sum returns to the anchor exactly. This freedom is what makes patch
digests portable and order-blind, and it's also why the arithmetic alone
can't be trusted to keep states real: a sum can silently describe a state
that cannot exist. Hence the spec's soundness rule, which is the right
sentence to end on — no remove is applied without a membership check, because
*the sum attests; the manifest adjudicates*. Arithmetic keeps the books; the
manifest checks that the debtor exists.

## What made the patches commute

Nothing about setsum forced A and B to merge cleanly; arithmetic guarantees
the *sum*, preconditions guarantee the *state*. A and B commute because
neither's writes touched the other's reads: A rewrote a span in `main.rs`; B
created `lib.rs` and rewrote a span in `README.md`. Each edit carries its
precondition (`old_str` must occur exactly once), so either order finds its
precondition intact. When preconditions do collide — both agents rewrite the
same span — the arithmetic still predicts a sum, but the second patch fails
its precondition and escalates to an actual resolution. Conflict is not
"same file"; conflict is "your writes hit my reads." That's a far smaller set
than what a line-based merge flags, and everything outside it needs no merge
tool at all — just addition.

## Reproduce it

```python
import hashlib

PRIMES = [4294967291, 4294967279, 4294967231, 4294967197,
          4294967189, 4294967161, 4294967143, 4294967111]

def cols(rec):
    d = hashlib.sha3_256(rec).digest()
    return [int.from_bytes(d[i*4:(i+1)*4], 'little') % PRIMES[i] for i in range(8)]

def add(a, b): return [(a[i] + b[i]) % PRIMES[i] for i in range(8)]
def neg(a):    return [(PRIMES[i] - a[i]) % PRIMES[i] for i in range(8)]
def render(s): return b''.join(c.to_bytes(4, 'little') for c in s).hex()

def rec(mode, path, blob):
    h = hashlib.sha3_256(blob).hexdigest()
    return f"{mode}\t{path}\t{h}\n".encode()

readme1 = b"# hello\n\nA tiny demo service.\n"
readme2 = b"# hello\n\nA tiny demo service.\n\nSee src/lib.rs for the API.\n"
main1   = b'fn main() {\n    println!("hello");\n}\n'
main2   = b'fn main() {\n    println!("hello, tally");\n}\n'
cargo   = b'[package]\nname = "hello"\nversion = "0.1.0"\n'
lib     = b"pub fn greeting() -> &'static str {\n    \"hello, tally\"\n}\n"

anchor = [0]*8
for r in (rec("100644","/Cargo.toml",cargo), rec("100644","/README.md",readme1),
          rec("100644","/src/main.rs",main1)):
    anchor = add(anchor, cols(r))

dA = add(neg(cols(rec("100644","/src/main.rs",main1))),
         cols(rec("100644","/src/main.rs",main2)))
dB = add(add(cols(rec("100644","/src/lib.rs",lib)),
             neg(cols(rec("100644","/README.md",readme1)))),
         cols(rec("100644","/README.md",readme2)))

assert render(add(add(anchor,dA),dB)) == render(add(add(anchor,dB),dA))
print("anchor   ", render(anchor))
print("A then B ", render(add(add(anchor,dA),dB)))
print("B then A ", render(add(add(anchor,dB),dA)))
```

## What's Implemented

- A working Pi integration
- A working tally protocol that can work against GitHub by importing from Git and exporting a PR.
- Naive push to object storage.

## What's Not Implemented

- Efficient delta push

## Related Work

Isn't this Darcs/Pijul?

They've done commutative patches for 20+ years.

Tally differs in that it observes tool calls rather than inferring from the text, and the setsum
gives a predictable, fixed-size verifiable identity for the resulting state.

## Next Steps

I'm looking for someone who wants to build a GitHub replacement in the open.  This repo name at
@cl4p-tp.ai will get you to my inbox.

The labs will do it if we don't.
