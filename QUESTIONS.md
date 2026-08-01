- How will a manifest stay under 64k with many forks?
- total_image in src/serve.rs assumes the log winds up from zero.  Is that accurate?  What does it
  mean?  Is it just the patch?
- Does `repo.materialize` in `restore` intend to restore the working tree of every fork?
- Why does `push` in src/wire.rs only fail an advanced fork if head ID is non-empty?
