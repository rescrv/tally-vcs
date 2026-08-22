# Tally

Tally is an agents-only version control system that represents patches as
morphisms in a groupoid.  That's the entire design.

Unpacking it:
- What git calls a tree is an object in the groupoid.
- What git represents as a difference between trees is a patch.
- An algebra connects the two such that some patches transform one object into another.

It's a groupoid.

## The name

Tally is named for the split tally stick — the twelfth century's distributed,
two-party, checksummed ledger.  Notch the stick, split it lengthwise, keep the
*stock*, hand over the *foil*; settlement is whether the halves still align.
State identity here forms an abelian group — patches are elements, composition
is addition, undo is the inverse, and order never matters — and `tally union`
is pressing the two halves back together to see that they agree.

## Quick Start

```console
cargo install --path .
cd /path/to/existing/git/repo
tally init
tally git pull main
```

You've successfully, deterministically imported your git repo up to the first commit that has less
than or greater than one parent.  Subsequent pulls will pull more.

## How it Works

See ANDON.md for the spec that doubles as the human escape hatch.  A text editor, core utils, and 11
lines of Python gives you all you need to repair Tally.

Tally works by representing the tree from git as a set with unordered membership.  It then uses
the setsum crate to give each tree a digest.  This is a homomorphism from tally's commit to a
setsum.  It uses the same representation, combined with the inverse of the abelian group of digests,
to represent a patch.  Therefore the combination of a commit and compatible patch will be a commit
whose digest is the setsum operator on the commit and patch digests.

This makes the outcome of a merge independently verifiable and addressable.  If the merge _doesn't_
yield the exact setsum predicted, you know that the merge is corrupt.

For more about what's included, see `git log --oneline`.  It follows its own pattern of AI does the
work until a human's in the loop.

## What's Implemented

- A working Pi integration
- A working tally protocol that can work against GitHub by importing from Git an exporting a PR.
- Naive push to object storage.

## Next Steps

I want to build a GitHub clone.  Every major lab has an incentive to make one that works in their
harness, and that means lock-in.

Tally is open under the Apache 2.0 license.  Someone needs to build the GitHub replacement.
