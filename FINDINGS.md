- There is no canonical json---it trusts serde json.  Write adversarial tests and fix.
- We should switch to the handled::SError error type for any error not part of the API boundary.
- edit_applies_and_realizes in src/patch.rs shall test the complete string, not a window.
- parse_log_lenient in src/log.rs shall verify there are no valid lines after the sheered line.
  A corruption in the middle doesn't truncate, it shall always be an error.
- The log should have a notion of commit time embedded within each line.
- construct_blob in src/segment.rs duplicates the edit functionality implemented elsewhere;
  $dry-principal.
- construct_blob:  What if a Op::Create op precedes a Op::Edit in line.intent.ops
- hostile_bytes_never_reach_the_decompressor shall assert the actual error, not just is_err.
- ditto for amplification_is_rejected_by_arithmetic
- `pack` in src/serve.rs needs to use an atomic put-if-absent (use `link` to hard link followed by
  `unlink` to cleanup the temp).
- `unpack_segments` shall not use the `_` pattern match for blobs.  Explicitly spell out the
  pattern.
- `rposition` used to find the anchor looks like a DRY violation.
- I've added the object store crate.  It has put_if_absent.  Use it over the object_store in
  src/wire.rs.
