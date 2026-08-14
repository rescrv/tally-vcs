# Abelian

Abelian is an agents-only version control system that represents patches as
morphisms in a groupoid.  That's the entire design.

Unpacking it:
- What git calls a tree is an object in the groupoid.
- What git represents as a difference between trees is a patch.
- An algebra connects the two such that some patches transform one object into another.

It's a groupoid.

## Quick Start

```console
cargo install --path .
cd /path/to/existing/git/repo
abelian init
abelian git pull main
```

You've successfully, deterministically imported your git repo up to the first commit that has less
than or greater than one parent.  Subsequent pulls will pull more.

## How it Works

Abelian works by representing the tree from git as a set with unordered membership.  It then uses
the setsum crate to give each tree a digest.  This is a homomorphism from abelian's commit to a
setsum.  It uses the same representation, combined with the inverse of the abelian group of digests,
to represent a patch.  Therefore the combination of a commit and compatible patch will be a commit
whose digest is the setsum operator on the commit and patch digests.

This makes the outcome of a merge independently verifiable and addressable.  If the merge _doesn't_
yield the exact setsum predicted, you know that the merge is corrupt.

## What's Not Implemented

- I haven't implemented integration with any editor beyond pi.
- Git support is experimental.  The goal is to make it so anyone who squash-merges-to-main on GitHub
  has a path to move to an abelian host.
- Thorough testing of repositories at scale.  I know the pack and push algorithm needs work.

## Next Steps

arxiv rejected this paper saying it was not a sufficient contribution to warrant submission.  Given
the shit papers I've seen from arxiv over the years, and the fact that arxiv is supposed to be a
venue where people who can timestamp work publicly, this stings.

If you disagree with arxiv, and are on the PC for a committee that would accept this work, please
reach out.

Otherwise, I'm going to build this in the open and am looking for contributors.
