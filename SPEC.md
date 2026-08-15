# tally: specification, v0

This document is normative. The [ANDON](ANDON.md) is the walkthrough and the
rationale; where the two disagree, this document is wrong and should be fixed,
because the ANDON was approved first and the design flows from it. Conformance
language: MUST and MUST NOT mark requirements whose violation makes an
implementation not-tally; everything else is stated as fact about the format
and is equally binding, just less shouted.

A repository has one logical content and up to two physical forms. The **loose
format** (§2) is the logical content, laid out on a filesystem; it is the
interchange form, the emergency form, and the definition of truth. The **packed
format** (§3) is an encoding of the loose format; it is the at-rest form and,
byte for byte, the wire form (§4). The server is dumb: it stores segments and
swaps manifests, and it performs no computation an object store cannot.

## §0. The Wall

One invariant governs everything in this document, so it comes first.

> **I1 (the identity/encoding wall).** No parameter of any encoding appears in
> the preimage of any hash. Identity — blob hashes, record ids, sums — is
> computed over uncompressed canonical bytes, always, everywhere.

Compression level, keyframe interval, dictionary choice, segment boundaries,
and every other packing decision live strictly on the encoding side of the
wall. Consequently any of them may be changed, per path, per segment, or
retroactively, by any process holding no lock on anything logical, without
perturbing a single hash. Anything that violates I1 is a bug in this
specification, not a tuning decision.

## §1. Identity layer

### §1.1 Element records

An element is a file, fully qualified. Its **record** is the canonical bytes:

```
mode <TAB> path <TAB> lowercase-hex(sha3-256(blob)) <LF>
```

`mode` is octal ASCII: `100644`, `100755`, or `120000` (symlink; the blob is
the link target). `path` is absolute from the repository root, begins with
`/`, is UTF-8, NFC-normalized, and contains no `\0`, no `\n`, no `\t`, and no
`.` or `..` components. There are no directory objects; hierarchy is a prefix
query, which is an index concern.

### §1.2 The sum

State identity is a setsum over element records, exactly as implemented by the
[setsum crate](https://crates.io/crates/setsum):

- 256 bits of state, organized as eight u32 columns, little-endian.
- Column primes: `4294967291, 4294967279, 4294967231, 4294967197, 4294967189,
  4294967161, 4294967143, 4294967111`.
- Insert: SHA3-256 the record; read the digest as eight LE u32s; reduce each
  modulo its column prime; add columnwise modulo the prime.
- Remove: add the columnwise inverse `P[i] − x[i]`.
- The empty state is all zeros. Rendering: concatenate the eight columns LE
  and print 64 hex characters.

The sum lives in the free abelian group; valid states do not. Removing an
absent record accrues placeholder debt silently. Therefore:

> **I9 (adjudication).** No remove is applied to any sum without a membership
> check of the removed record against a manifest of the state being modified.
> The sum attests; the manifest adjudicates.

### §1.3 Canonical JSON and record ids

Wherever this specification says *canonical JSON*: UTF-8; object keys sorted
bytewise; separators `,` and `:` with no whitespace; strings NFC; no floats —
every numeric field in every schema in this document is an integer or a hex
string, so no number-canonicalization question arises. This is reproducible
as `json.dumps(x, sort_keys=True, separators=(',',':'), ensure_ascii=False)`.

The id of an identified record (log line, serve-manifest) is the lowercase
hex SHA3-256 of the canonical JSON of the record *with the `id` key absent*.
The record as stored includes its id. Verification strips `id`,
re-canonicalizes, re-hashes.

Records whose ids hash their bytes are **byte-preserved artifacts**: they MUST
be stored and transported with their exact bytes (compressed, but never
transcoded). Blobs are **content artifacts**: they verify by content hash
after decoding, so their encoding is unconstrained. This is the practical
edge of the Wall:

> **I4.** Log lines and serve-manifests are byte-preserved. Blobs are
> re-encodable.

## §2. Loose format

### §2.1 Layout

```
.tally/
  version                    # "tally v0\n"
  blobs/ab/cdef01…           # raw content; filename is the sha3-256 hex
  forks/<name>/
    fork                     # anchor reference
    log.jsonl                # the primary artifact; fuse lines live in it
    lock
  anchors/<sum>.manifest
  index/                     # derived; deleting it is always safe
```

> **I3 (append-only).** `blobs/`, `forks/*/log.jsonl`, and `anchors/` are
> append-only or immutable. Nothing in them is ever rewritten. `index/` is a
> cache reconstructible from the rest.

### §2.2 Blobs

Raw bytes. No framing, no compression, no type tag. The path is
`blobs/<first 2 hex>/<remaining 62 hex>`. Write protocol: stream to
`blobs/tmp/<random>` hashing as you go; `rename(2)` into place. A single
write fsyncs the temp file and the fan-out directory; a bulk write (git
import, working-tree ingest, union replay, unpack) skips both fsyncs and
syncs the device once (`syncfs`/`sync`) before the commit that references
the blobs. A collision on rename is a deduplication hit; discard the temp
file. Everything content-shaped shares this pool: file contents, PR prose,
spilled read sets, zstd dictionaries.

### §2.3 Fork file

```
tally-fork v0
anchor <64 hex>
manifest <64 hex>
```

`manifest` names `anchors/<sum>.manifest`, which MUST exist and whose own
`sum` header MUST equal `anchor`. The empty repository is anchor all-zeros
with an empty manifest. A fork is an anchor plus a log; a fork is also a
session; tally does not distinguish. The current state of a fork is the
anchor manifest plus the replay of its log.

### §2.4 Manifests

```
tally-manifest v0
sum <64 hex>
<element record>
<element record>
…
```

Records sorted bytewise. Sorting serves humans and diff tools; the sum is
order-blind. A manifest whose `sum` disagrees with the fold of its records is
corrupt. Manifests are compactions — derived, never primary. `tally snapshot`
writes one and MAY repoint the fork file at it; earlier log lines remain.

### §2.5 The log

`log.jsonl`: one applied patch per line, canonical JSON plus a trailing `\n`.

```json
{"id": "…",
 "prev": "…",
 "intent": {"ops": [
   {"edit":   {"path": "/src/main.rs", "old_str": "…", "new_str": "…"}},
   {"create": {"path": "/src/lib.rs", "mode": "100644", "blob": "<hex>"}},
   {"delete": {"path": "/old.rs", "blob": "<hex>"}},
   {"chmod":  {"path": "/tools/apply", "old_mode": "100644",
               "new_mode": "100755"}}]},
 "realized": [
   {"remove": "100644\t/src/main.rs\tab12…",
    "add":    "100644\t/src/main.rs\tcd34…"}],
 "sum_after": "<64 hex>",
 "annotation": {
   "author": "…", "provenance": "agent",
   "reason": null, "sig": null,
   "session": "…", "prose": "…",
   "reads": [ … ],
   "origin": null}}
```

Semantics, pinned:

- `prev` is the id of the preceding line; `""` on the first line. The chain
  orders the narrative; the arithmetic never needed it.
- `intent` ops carry span preconditions. An `edit` requires `old_str` to
  occur in the current blob at `path` exactly once — zero or several matches
  and the patch does not apply. A `delete` consumes the whole element (blob
  hash must match). A `create` requires the path's absence. Intent commutes
  and travels; `create.blob` references the blob store, and a portable patch
  bundle is the intent JSON plus its referenced blobs.
- `realized` is the concrete element delta the application produced against
  the state it met. Either side of an entry may be null (create, delete).
  Replay and inversion use `realized` and pure arithmetic; `intent` is for
  re-validation and re-enactment.
- `sum_after` is deliberately redundant: it makes every log prefix
  independently verifiable and corruption bisectable. A line whose arithmetic
  disagrees with its own `sum_after` marks where history stops being
  trustworthy.
- `provenance` ∈ `"agent" | "andon" | "union" | "fuse"`. When `"andon"`: `reason` MUST
  be a non-empty string, `sig` MUST be present (scheme: v0 accepts a detached
  signature blob hash; do not over-specify yet), and `reads` MAY be absent —
  degraded provenance is marked, never faked. When `"union"`: `origin` MUST
  be `{"fork": "…", "id": "…"}` naming the source line; landed lines are new
  lines with new ids, because strata 2–3 may realize fresh deltas and the
  target chain needs its own linkage. When `"fuse"`: the annotation MUST
  carry `fuse` (§2.6), `intent.ops` MUST be empty, and `realized` MUST be
  empty — a fuse is an arithmetic identity, so `sum_after` equals the
  preceding line's.
- `fuse`, when present on a line of any provenance, is
  `{"name": "<name>", "from": "<line id>", "to": "<line id>"}` naming an
  interval under one interpretation (§2.6), and `realized` MUST be empty: a
  fuse is an interpretation, never a mutation.  When union lands a fuse
  line, it re-keys `from` and `to` through the `origin → landed id`
  correspondence it already computes, so the span names lines on the target
  chain; the name travels untouched.
- **Spill rule.** A log line MUST be under 65536 bytes. If `reads` pushes
  past the limit, the array moves whole into a blob and the field becomes
  `{"reads_blob": "<hex>"}`. The log stays line-scannable; the exhaust stays
  kept.

### §2.6 Fuses and views

A fuse is a log line that names one interval of the log under a different
interpretation:

```json
{"id": "…", "prev": "…",
 "intent": {"ops": []}, "realized": [], "sum_after": "<64 hex>",
 "annotation": {"author": "…", "provenance": "fuse",
                "prose": "one narrative beat",
                "fuse": {"name": "<name>", "from": "<line id>",
                         "to": "<line id>"}, …}}
```

The name is the fuse's handle: non-empty, and not unique — two fuses may
share a name, and a later fuse may answer an earlier one under the same
name (mark a span an active incident, later append a fuse marking it
resolved; any read between the two shows it active, any read after shows
both chapters).

Fuses carry no authority over content: `realized` is empty, so a fuse is an
arithmetic identity and every sum, replay, and inversion is blind to it. But
a fuse is a line, so it inherits everything lines have: durability (I3),
byte preservation (I4), transport (§3, §4), union travel (strata 1–2 land an
identity trivially, re-keying `from`/`to` per §2.5 while the name travels
untouched), and the unmerged-work protection of fork removal. Fusing is
lossless by construction because the fused lines remain in the log
underneath, forever.

A **view** is not a line. It is a render-time filter naming the fuses to
collapse, declared just in time on each render: `tally log` collapses
every fuse; `tally log --view incident,release` collapses only fuses
named `incident` or `release`. Nothing about a view is stored, so nothing
about it needs consensus, transport, or re-keying.

Rendering at any log prefix is a pure function of that prefix. Fuses may
overlap in all cases — no fuse supersedes another; every selected fuse
whose span resolves renders a beat, and a line under overlapping fuses
appears under each covering beat (filter the view to disambiguate). A fuse
line renders as its beat, or as an ordinary line when its span dangles, is
reversed, or is filtered out — dangling is never fatal.

### §2.7 Apply

```
0. flock forks/<f>/lock
1. LOAD     current manifest (index cache if fresh, else anchor + replay)
2. VALIDATE every op against manifest + blobs; any failure → write nothing
3. WRITE    new blobs (tmp+rename, unsynced); idempotent, uncommitted
4. REALIZE  deltas; fold: sum ← sum + Σ neg(removes) + Σ adds
5. APPEND   log line; device sync (blobs), fsync file, fsync directory
            ← LINEARIZATION POINT
6. REFRESH  index and working tree; best-effort, derived
7. unlock
```

> **I8 (commit points).** Loose: the fsync'd log append is the sole
> linearization point. Blobs MUST be durable before any line referencing them
> is appended (write-ahead ordering), so a committed line never dangles.
> Bulk blob writes fsync nothing per blob; one device sync (`syncfs`/`sync`)
> before the append carries the ordering for the whole pool.

Crash recovery: a torn final line (invalid JSON, or id fails
re-verification) is truncated as never-committed. Orphan blobs from aborted
applies are retained — exhaust is exhaust. One writer per fork log, enforced
by `lock`; a fork is a session, so this is the model, not a concession.

## §3. Packed format

### §3.1 Segments

A **segment** is an immutable pair:

```
seg/<segid>.pk               # payload: standard zstd frames
seg/<segid>.idx              # entry table, plain text
```

`segid` is the lowercase hex SHA3-256 of the `.pk` bytes — segments are
content-addressed, so immutability is enforced by naming, uploads are
idempotent, and caches never invalidate. `.pk` payloads use standard zstd
frames so that a raw segment yields to `zstd -d` without `tally` present.

`.idx` is one line per entry, space-separated, LF-terminated:

```
<entry-sha3> <frame#> <offset> <len> <enc> [<aux1>] [<aux2>]
```

where offsets and lengths refer to the decompressed frame, and:

| enc | meaning | aux1 | aux2 |
|---|---|---|---|
| `raw` | bytes as-is | — | — |
| `zstd` | bytes are in the zstd frame | — | — |
| `zstdd` | zstd with dictionary | dictionary blob hash | — |
| `construct` | no bytes stored | base blob hash | log line id |
| `lines` | byte-preserved log span | fork name | first..last line id |

`construct` says: this blob is the result of applying the named line's edit
ops for this path to the base blob. It stores nothing but the reference —
the op bytes already live in the log, so versioned files are deduplicated
against the history that produced them. A `construct` entry MUST reproduce
the blob's exact bytes; since the blob's name is its content hash, every
materialization is self-verifying. Chains bottom out at **keyframes** —
versions stored `raw`/`zstd`/`zstdd`.

> **Keyframe interval K is a cache policy, not a format parameter** (corollary
> of I1). It may differ per path, per class, per segment, and may be changed
> retroactively by re-encoding: deepening chains demotes known-good keyframes
> to references; shallowing chains materializes (replay + hash check) and
> promotes. Neither direction touches any hash. Default K = 32 for text,
> K = 1 for blobs the packer cannot construct (binaries, `create` payloads,
> union re-enactments).

Log spans (`lines`) are byte-preserved per I4: exact JSONL bytes inside the
zstd frame, never transcoded — ids must re-verify after decode.
Dictionaries are blobs, content-addressed, packed like any other
entry; `zstdd` references them by hash, so encoding stays deterministic and
dictionaries travel with the repository.

### §3.2 The serve-manifest

The packed repository is described by a chained sequence of manifests:

```
manifest/<seq>.json          # canonical JSON, id per §1.3, put-if-absent
```

```json
{"id": "…", "v": 0, "seq": 41, "prev": "<id of seq 40>",
 "forks": {
   "main": {"anchor": "<64 hex>", "head_id": "<line id>",
            "head_sum": "<64 hex>",
            "log_segments": ["<segid>", …]}},
 "segments": {
   "<segid>": {"pk_sha3": "<hex, equals segid>", "idx_sha3": "<hex>",
               "bytes": 123456, "entries": 400,
               "image_setsum": "<64 hex>",
               "log_span": ["<first id>", "<last id>"]}},
 "anchors": ["<sum>", …],
 "retire": ["<segid>", …]}
```

Every segment carries **two integrity values, and they answer different
questions**. `pk_sha3` is the SHA3-256 of the compressed `.pk` bytes — it
authenticates the particular encoded artifact, and it is what MUST be checked
before any byte reaches a decompressor. `image_setsum` is a setsum over the
segment's **image items** — one canonical record per entry, regardless of
encoding:

```
blob  <TAB> <content sha3 hex> <LF>     # raw, zstd, zstdd, and construct alike
line  <TAB> <line id>          <LF>
dict  <TAB> <content sha3 hex> <LF>
```

A `construct` entry contributes the same `blob` item a keyframe of the same
content would: the image is *logical*, encoding-blind. The pair is the Wall
(I1) made verifiable: two segments with equal `image_setsum` and unequal
`pk_sha3` are re-encodings of the same content; unequal `image_setsum` is
content forgery. Scrub is stratified like union: arithmetic on setsums and
byte counts first; decompress-and-rehash only on suspicion. `prev` chains
manifests, so the history of swaps is itself an auditable log.

### §3.3 Compaction

Compaction rewrites segments into better segments — merged small ones, newer
dictionaries, retuned K — publishes a manifest referencing the new set and
listing the old in `retire`, and eventually deletes retired segments.

Because image items are encoding-blind, **compaction correctness is an
arithmetic proof**: across any manifest swap,

```
Σ image_setsum(segments in N+1) − Σ image_setsum(segments in N)
    = setsum(items genuinely added at N+1)
```

and for a pure compaction the right-hand side is zero. A re-encoding that
lost or invented so much as one entry fails a group equation checkable in
microseconds without touching a single `.pk`. I2's equivalence obligation
thereby has a cheap necessary condition enforced at every swap; the
sufficient condition (bit-identical unpack) remains the scrub-level check.

> **I2 (equivalence).** `unpack` is total, deterministic, and model-free, and
> unpack-before-compaction equals unpack-after, bit for bit at the loose
> level. Retired segments are the only thing tally ever deletes, and they
> are re-encodings, not information.

Retired segments MUST be retained until every manifest referencing them ages
out of the retention window (parameter R, §5) — readers may hold old
manifests. Compaction scheduling is an LSM problem and is out of scope here;
the reference implementation intends lsmtk-style triangular compaction.

## §4. Wire protocol

> **I10 (dumb server).** The wire format is the packed format. The server is
> an object store: it GETs and PUTs segments and manifests, supports
> put-if-absent, and computes nothing.

**Clone.** GET the highest-`seq` manifest; GET its segments, in parallel,
resumable, cacheable by any dumb CDN — segment names are content hashes, so
caching is trivially safe. Unpack what you need or operate on packs directly.

**Fetch.** GET the latest manifest; diff its segment set against yours; GET
the difference. There is no negotiation because there is nothing to
negotiate.

**Verification.** Fetched bytes are hostile until proven otherwise, and the
order of proof is mandated:

```
1. manifest:  size-bounded (§5) parse; verify id per §1.3
2. .pk:       sha3 the raw bytes; compare to pk_sha3 (= segid)
              BEFORE the decompressor sees a single byte
3. .idx:      sha3 the raw bytes; compare to idx_sha3; only then parse
4. frames:    decompress with hard output budgets taken from the
              authenticated idx lengths — amplification is rejected
              by arithmetic, not by running out of memory
5. entries:   verify logical identity post-decode — blob content hashes,
              line ids — and the segment's image_setsum
6. dicts:     are entries; fully verified (steps 2–5) before any bytes
              are loaded into a decompression context
7. construct: applies only verified inputs (base blob by content hash,
              line bytes by id) and verifies its output's content hash
```

Verify-then-decompress forecloses streaming decode of a segment mid-download;
the segment size target (§5) is what makes buffering acceptable, and bounded
pre-verification buffering is now a security property of that parameter, not
merely a packing convenience. The residual root-of-trust surface is the
size-bounded JSON parse of the manifest itself; v0 accepts it, and the
designated exit is an outer detached signature over the manifest's raw bytes
so that even the parse is post-authentication.

**Push.**

```
1. pack new content into segments locally
2. PUT segments                      (idempotent; existing names are no-ops)
3. build manifest seq N+1 with prev = id of N
4. put-if-absent manifest/<N+1>.json          ← LINEARIZATION POINT (wire)
5. on conflict: GET the winner, rebuild the manifest against it, retry —
   uploaded segments are content-addressed and reusable as-is
```

Manifest rebuild at step 5 is a union of segment sets plus per-fork head
updates. Pushes to different forks merge trivially. Two pushes advancing the
same fork violate single-writer (I8) and the loser MUST NOT silently splice:
it re-runs `union` at the log level and pushes the result. The put-if-absent
on the next sequence number is the server-side linearization point — the same
role the fsync'd append plays loose, and deliberately the same shape wal3
uses against object storage.

**Andon over the wire.** The emergency path is loose-first: fetch, unpack,
operate per the ANDON with an editor and stdlib Python, repack, push. With
no `tally` at all this requires `curl`, `zstd`, and Python — acceptable, and
the reason `.pk` uses standard zstd frames and `.idx` is plain text.

Authorization is storage ACL plus the `sig` requirement on andon lines;
tally adds no auth protocol of its own. Anything smarter — maintainer
policy, review gates on a fork — is the Datalog layer's job, enforced by
the maintainer agent as *a client*, never by the server.

## §5. Parameters

All encoding-side, per I1; none appear in any hash preimage; all are
defaults, not requirements.

| parameter | default | nature |
|---|---|---|
| keyframe interval K | 32 text / 1 opaque | cache policy, per path-class |
| log-line spill threshold | 65536 bytes | loose-format requirement (§2.5) |
| segment target size | 64 MiB | packing policy |
| zstd level | 19 at rest, 3 on pack-for-push | packing policy |
| retention window R | 30 days of manifests | deletion safety (§3.3) |
| manifest size bound | 16 MiB | root-of-trust parse bound (§4) |

The spill threshold is the one entry that is format, not policy: it bounds
loose log lines and so lives on the identity side of nothing — it constrains
bytes, not hashes — but changing it changes what valid loose logs look like,
so it is versioned with the format.

## §6. Invariants, collected

- **I1** — the Wall: no encoding parameter in any hash preimage.
- **I2** — loose is truth; pack is an encoding; unpack is total,
  deterministic, model-free, and equivalence-preserving.
- **I3** — the repository is append-only; `index/` is a deletable cache.
- **I4** — log lines and manifests are byte-preserved; blobs are
  re-encodable.
- **I5** — with zero models available, tally degrades to a complete,
  operable VCS; the model is a layer, never a load-bearing wall.
- **I6** — the native format is human-writable: an editor and one static
  binary suffice to operate the emergency path, and the walkthrough in the
  ANDON is that path's documentation.
- **I7** — retention is lossless: fuse appends a fuse line whose realized
  delta is empty, never a mutation, and the fused lines remain underneath;
  observed exhaust is never discarded; the only deletion is a retired
  re-encoding.
- **I8** — one writer per fork; the fsync'd log append (loose) and the
  put-if-absent of the next manifest (wire) are the sole linearization
  points.
- **I9** — every remove is membership-checked against a manifest before it
  touches a sum; placeholder debt is never given the chance to be silent.
- **I10** — the server is dumb; the wire format is the packed format; all
  intelligence is client-side, including the maintainer.
- **I11** — authenticate before interpreting: no fetched bytes reach a
  decompressor, parser, or op applier until their hash matches an
  expectation reachable from the trust root; decompression output is
  budgeted by authenticated lengths. Every decompression boundary carries
  the pair (sha3 of the compressed form, setsum of the uncompressed image),
  and the pair is the Wall made verifiable.

---

*The loose format is the stock; the pack is the foil. Settlement is `unpack`,
and the halves must align.*
