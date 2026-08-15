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

## What's Not Implemented

- I haven't implemented integration with any editor beyond pi.
- Thorough testing of repositories at scale.  I know the pack and push algorithm needs work to be
  efficient.

## Next Steps

arxiv rejected this as a paper saying it was not a sufficient contribution to warrant submission.
Given the shit papers I've seen from arxiv over the years, and the fact that arxiv is supposed to be
a venue where people who can timestamp work publicly, this stings.

I believe in open research and part of that is being able to timestamp your work.  I wanted to have
the arxiv paper as an anchor so I could comfortably reach out to strangers and ask them to consider
this work.

If you disagree with arxiv, and are on the PC for a committee that would accept this work, please
reach out.

Otherwise, I'm going to build this in the open and am looking for contributors.
