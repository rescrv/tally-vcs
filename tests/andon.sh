#!/usr/bin/env bash
#
# tests/andon.sh — assert the ANDON.md workflow, byte for byte.
#
# ANDON.md teaches abelian by hand — "a text editor, coreutils, and a few
# lines of Python" — naming at each step the `abelian` command that
# automates it.  This script walks the document end to end:
#
#   §1  elements        abelian sum --records
#   §2  the sum         abelian sum, abelian check
#   §3  snapshots       abelian commit, abelian snapshot
#   §4  patches         abelian apply (including the refused preconditions)
#   §5  the log         abelian show, abelian log --raw
#   §6  fork and union  abelian fork, abelian union (strata 2 and 3)
#   §7  fuse            abelian fuse (a lossless view)
#   §9  the Andon cord  abelian apply --provenance=andon — and then the
#                       same pull performed with NO abelian binary at all:
#                       a hand-computed, hand-appended log line that the
#                       binary must verify and accept.
#
# Every step is performed twice: once by hand, exactly as the document
# prescribes, and once by the command the document names.  The two results
# must be BYTE-IDENTICAL, and both must equal the test vectors embedded
# below wherever the result is a pure function of the fixed inputs (every
# blob hash, element record, state sum, and on-disk format is such a pure
# function).  The only values that are not pure are commit timestamps — and
# therefore log-line ids — for which §5 defines a re-derivation rule; that
# rule is checked by hand on every line instead of pinning the ids.
#
# Requires: bash, python3, coreutils.  No network, no model — that is the
# point of the document.
#
# Usage: tests/andon.sh [path-to-abelian]

set -euo pipefail
export LC_ALL=C        # §3 sorts records bytewise; pin coreutils to bytes.
umask 022

################################## rails ##################################

fail() { echo "andon.sh: FAIL: $*" >&2; exit 1; }
step() { echo; echo "== $*"; }
note() { echo "    $*"; }

assert_eq() { # <label> <expected> <actual>
    [ "$2" = "$3" ] || fail "$1: expected [$2], got [$3]"
    note "$1: ok"
}

assert_bytes() { # <label> <hand-made file> <abelian-made file>
    if ! cmp -s "$2" "$3"; then
        echo "andon.sh: FAIL: $1: not byte-identical:" >&2
        cmp "$2" "$3" >&2 || true
        exit 1
    fi
    note "$1: byte-identical"
}

# `abelian check`'s whole output is a byte-assertable function of the sum.
assert_check() { # <expected sum>
    { printf 'log expects   %s\n' "$1"
      printf 'working tree  %s\n' "$1"
      printf 'ok\n'; } > "$ANDON_WORK/expect-check"
    "$ABELIAN" check > "$ANDON_WORK/got-check"
    assert_bytes "abelian check output" "$ANDON_WORK/expect-check" "$ANDON_WORK/got-check"
}

# Canonical-JSON of stdin: one canonicalization, shared by both sides of a
# comparison, so the comparison tests content rather than key order.
canon() {
    python3 -c 'import json, sys; print(json.dumps(json.load(sys.stdin), sort_keys=True, separators=(",", ":")))'
}

############################## prerequisites ##############################

ABELIAN="${1:-${ABELIAN:-}}"
if [ -z "$ABELIAN" ]; then
    SELF_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
    ABELIAN="$SELF_DIR/../target/debug/abelian"
fi
case "$ABELIAN" in /*) ;; *) ABELIAN="$(pwd)/$ABELIAN" ;; esac
[ -f "$ABELIAN" ] || fail "abelian binary not found: $ABELIAN (hint: cargo build --locked)"
[ -x "$ABELIAN" ] || fail "abelian binary not executable: $ABELIAN"
command -v python3 >/dev/null 2>&1 || fail "python3 not found; the ANDON's eleven lines run on it"

# One scratch world.  Everything this script creates lives under it; the
# trap removes it, but only if the path still carries our marker.
ANDON_WORK=""
cleanup() {
    if [ -n "$ANDON_WORK" ] && [ -d "$ANDON_WORK" ]; then
        case "$ANDON_WORK" in
            *abelian-andon.*) rm -rf "$ANDON_WORK" ;;
            *) echo "andon.sh: refusing to clean unexpected dir: $ANDON_WORK" >&2 ;;
        esac
    fi
}
trap cleanup EXIT
ANDON_WORK=$(mktemp -d "${TMPDIR:-/tmp}/abelian-andon.XXXXXX")
[ -d "$ANDON_WORK" ] || fail "mktemp -d failed"
export ANDON_WORK
REPO="$ANDON_WORK/repo"
EXPECT="$ANDON_WORK/expect"
mkdir -p "$REPO" "$EXPECT"

############################### test vectors ##############################
# Everything pinned here is a pure function of the fixed inputs written
# below: the same bytes always hash, fold, and serialize the same way.
# These constants were derived by hand once and pin the ANDON's formats
# and arithmetic against regressions in either the document or the binary.

# The empty state (§2: "The empty state is all zeros").
S_EMPTY=0000000000000000000000000000000000000000000000000000000000000000

# §1: three fixed blobs (written just below) and their SHA3-256.
H_README=490c2896fdde41efac3393f8459557db33071ba863d1adbd4642031565a34870
H_MAIN0=c69a7d95fc5925b4fa65474c43154392074d7ee6785032bc050a2069293896d7
H_TOOL=95e791bffc3c30febfeb0d2b0e31617f5c2bb4d4515e03b8ef3b254326d8a1a9

# §2: the setsum of the three element records, folded by hand once, pinned.
S_1=10ca11a44b07381e9a34680c430011619fd7dd6ae11d3715efa26570819473bc

# §4: one patch exercising all four span ops (edit, create, delete, chmod).
H_MAIN1=c510d3ee18b39ba4128092936a41b573dec40a59a68261451fbfb7da09533046
H_LIB0=05114450eb289e964e733dc0ae3626df08665b0721cf98c507dff5c6e3ce4789
S_2=7a6b2e42b2ad4e6a279eabf8ad62478275e3c46b5710c5b9aaf3c988d4faae40

# §6: a fork, a disjoint edit on each side (stratum 2: realized replay)...
H_LIB1=87ffd1c93a80e4da7889dbda511e78ddc1d3a084dfb2097a606865f13f28aac7
S_DEV=edb8be2f63469afc04f9a9de75c563b9ed2ecae2fb0409ae5a1c1161d608a39a
H_MAIN2=665420e13f70f60df70ce98351ef1c4742a49208f955cb40747cf7215ae19568
S_3=4be97f408c0703f5bc6ee1ba09ec94d0db97fb506771e78a063d17d852b658ef
S_UNION2=be36102e4ea04e8799c9dfa0344fb10753e300c80b662b7fb6655eb00dc54c49
# ...then two disjoint-span edits to the SAME file (stratum 3: intent replay).
H_MAIN3=79db01d70a373cd6c723b4d9a4123ee8952b26fe662f783d6b1df7c69bbf59bd
S_DEV2=081c67b1fc1d712967487344c0cd52a63bd1db505ef909852428fd9ea822e023
H_MAIN4=7c7c44fc7f4e443976a3ec335f2a0aba6c0748baabb7812fa9b6b26ecbe9e9dd
S_4=2e597246a35dcf281ca9e3297771d13568f8d4d7dc9c02fa5f2b34266b6eaa51
H_MAIN5=a79f32b1d5ca60cb7fa1985495de2ada556490ec43a11818c5968d415c1533dc
S_UNION3=15bf57da4561a55b54eb2e5ec93014cdbb5bb891a5156b3b5fa961107d63029e

# §9: the cord, pulled first with the binary...
H_MAIN6=e87fb58c7c11b01b22646d1c2c968e2c98670a7caff064830cca0cddd0cc4dd7
S_5=bf001c2f43bdb09811066ed0015740c5c3ebc8e78bea0cbfad33190e33c64510
# ...and then by hand, with zero abelian binaries in the loop.
H_MAIN7=41246b1ca32e9d787c9abafe56eb77d4e0895c4f8e331d407d8218d2efb4abcd
S_CORD=d830b8cab1fed5cb20c5523ad9a6dbb9bd60481aa691d87b1bd45eee7f3870c2
H_SIG=00cc442ebc35ed53e32bc155a6d9e2e1fc20ab11e7c7b6e5943edb1cf15eedb7
V_CORD_MS=1780000000000

export S_EMPTY H_README H_MAIN0 H_TOOL S_1 H_MAIN1 H_LIB0 S_2 \
       H_LIB1 S_DEV H_MAIN2 S_3 S_UNION2 H_MAIN3 S_DEV2 H_MAIN4 S_4 \
       H_MAIN5 S_UNION3 H_MAIN6 S_5 H_MAIN7 S_CORD H_SIG V_CORD_MS

############################ fixed input bytes ############################
# Every blob the walkthrough touches, written once.  Python asserts each
# file's SHA3-256 against the vectors above every time it uses one, so a
# typo here fails loudly at the step that consumes it, not mysteriously.

printf '# andon test\n'                                            > "$EXPECT/README.md"
printf 'fn main() {\n    println!("hello, andon");\n}\n'           > "$EXPECT/main0.rs"
printf '#!/bin/sh\necho apply\n'                                   > "$EXPECT/tools_apply"
printf 'fn main() {\n    println!("hello, abelian");\n}\n'         > "$EXPECT/main1.rs"
printf 'pub fn answer() -> u32 {\n    42\n}\n'                     > "$EXPECT/lib0.rs"
printf 'pub fn answer() -> u32 {\n    137\n}\n'                    > "$EXPECT/lib1.rs"
printf 'fn main() {\n    println!("hello, union");\n}\n'           > "$EXPECT/main2.rs"
printf 'fn main() {\n    // dev2 was here\n    println!("hello, union");\n}\n' > "$EXPECT/main3.rs"
printf 'fn main() {\n    println!("hello, union");\n    println!("done");\n}\n' > "$EXPECT/main4.rs"
printf 'fn main() {\n    // dev2 was here\n    println!("hello, union");\n    println!("done");\n}\n' > "$EXPECT/main5.rs"
printf 'fn main() {\n    // dev2 was here\n    println!("hello, union");\n    println!("done: CVE-2026-0001 patched");\n}\n' > "$EXPECT/main6.rs"
printf 'fn main() {\n    // dev2 was here\n    println!("hello, union");\n    println!("done: CVE-2026-0001 patched by hand");\n}\n' > "$EXPECT/main7.rs"
printf 'abelian detached signature v0\nsigned-by: oncall-human\n'  > "$EXPECT/sig-oncall"

# The patches of §4, §6, and §9, as an author writes them (ANDON §4 shows
# this exact shape).  Written outside the repository so the tree walk of
# §1 never sees them.
cat > "$ANDON_WORK/patch1.json" <<'EOF'
{"ops": [
  {"edit":   {"path": "/src/main.rs",
              "old_str": "println!(\"hello, andon\");",
              "new_str": "println!(\"hello, abelian\");"}},
  {"create": {"path": "/src/lib.rs", "mode": "100644",
              "content_b64": "cHViIGZuIGFuc3dlcigpIC0+IHUzMiB7CiAgICA0Mgp9Cg=="}},
  {"delete": {"path": "/README.md", "blob": "490c2896fdde41efac3393f8459557db33071ba863d1adbd4642031565a34870"}},
  {"chmod":  {"path": "/tools/apply", "old_mode": "100755",
              "new_mode": "100644"}}
]}
EOF
cat > "$ANDON_WORK/patch-dev.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/lib.rs", "old_str": "42", "new_str": "137"}}]}
EOF
cat > "$ANDON_WORK/patch-main.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "println!(\"hello, abelian\");", "new_str": "println!(\"hello, union\");"}}]}
EOF
cat > "$ANDON_WORK/patch-dev2.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "fn main() {", "new_str": "fn main() {\n    // dev2 was here"}}]}
EOF
cat > "$ANDON_WORK/patch-main2.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "println!(\"hello, union\");", "new_str": "println!(\"hello, union\");\n    println!(\"done\");"}}]}
EOF
cat > "$ANDON_WORK/patch-andon.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "println!(\"done\");", "new_str": "println!(\"done: CVE-2026-0001 patched\");"}}]}
EOF
# Patches the ANDON says must NOT apply (§4: "Anything but one: stop";
# §2: no remove without a membership check; §4: create requires absence).
cat > "$ANDON_WORK/bad-zero.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "NO SUCH SPAN", "new_str": "x"}}]}
EOF
cat > "$ANDON_WORK/bad-many.json" <<'EOF'
{"ops": [{"edit": {"path": "/src/main.rs", "old_str": "l", "new_str": "x"}}]}
EOF
cat > "$ANDON_WORK/bad-delete.json" <<EOF
{"ops": [{"delete": {"path": "/src/lib.rs", "blob": "$S_EMPTY"}}]}
EOF
cat > "$ANDON_WORK/bad-create.json" <<'EOF'
{"ops": [{"create": {"path": "/src/lib.rs", "mode": "100644", "content_b64": "eA=="}}]}
EOF

######################### the eleven lines, and kin ########################

# The §2 construction, as the document gives it, plus the three helpers
# the later steps need (parse a sum hex, canonical JSON per §1.3, the §4
# occurrence count).
cat > "$ANDON_WORK/andon.py" <<'EOF'
import hashlib
import json

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

def from_hex(h):  # the inverse rendering: sum hex back to eight columns
    b = bytes.fromhex(h)
    return [int.from_bytes(b[4*i:4*i+4], 'little') for i in range(8)]

def sha3_hex(data: bytes):
    return hashlib.sha3_256(data).hexdigest()

def canonical(obj):  # §1.3 canonical JSON: sorted keys, no whitespace
    return json.dumps(obj, sort_keys=True, separators=(',', ':'),
                      ensure_ascii=False).encode()

def count_occurrences(haystack: bytes, needle: bytes) -> int:
    # §4's precondition count, sliding-window, overlaps included — the
    # same contract the substrate's own count_occurrences implements.
    if not needle or len(needle) > len(haystack):
        return 0
    return sum(1 for i in range(len(haystack) - len(needle) + 1)
               if haystack[i:i+len(needle)] == needle)

def record(mode, path, blob_hex):  # §1: an element's canonical bytes
    return f"{mode}\t{path}\t{blob_hex}\n".encode()
EOF

cat > "$ANDON_WORK/mkrecord.py" <<'EOF'
"""§1 by hand: the document's one-liner with an assertion grown onto it.

Emit the canonical element record for a file, asserting the blob hash
equals the embedded test vector."""
import hashlib
import sys

mode, path, content_file, want_hash = sys.argv[1:5]
content = open(content_file, "rb").read()
got = hashlib.sha3_256(content).hexdigest()
assert got == want_hash, \
    f"{path}: sha3-256 {got} disagrees with the test vector {want_hash}"
sys.stdout.write(f"{mode}\t{path}\t{got}\n")
EOF

cat > "$ANDON_WORK/fold.py" <<'EOF'
"""§2 by hand: fold records into a state sum — order does not matter —
and check by hand the laws the document says to check by hand."""
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import add, neg, state_of, sum_hex

records = open(sys.argv[1], "rb").read().splitlines(keepends=True)
assert records, "no records to fold"
ZERO = [0] * 8

def fold(recs):
    s = list(ZERO)
    for r in recs:
        s = add(s, state_of(r))
    return s

forward = fold(records)

# Law 1: commutativity and associativity — any fold order, same sum.
assert fold(list(reversed(records))) == forward
assert fold(records[1:] + records[:1]) == forward

# Law 2: identity and inverses — zeros is the empty state; insert then
# remove is a no-op; add(s, neg(s)) is zeros.
assert fold([]) == ZERO
r0 = state_of(records[0])
assert add(add(ZERO, r0), neg(r0)) == ZERO
assert add(forward, neg(forward)) == ZERO

# (Law 3 — equality means equality — is probabilistic; the substrate
# property-tests it, and every byte comparison in this script leans on it.)

print(sum_hex(forward))
EOF

cat > "$ANDON_WORK/check_manifest.py" <<'EOF'
"""§3 by hand: verify a manifest — header, sum line, every record sorted
bytewise; a manifest whose sum line disagrees with the fold of its
records is corrupt, full stop."""
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import add, state_of, sum_hex

data = open(sys.argv[1], "rb").read()
lines = data.split(b"\n")
assert lines[0] == b"abelian-manifest v0", "bad manifest header"
assert lines[-1] == b"", "manifest must end with LF"
claimed = lines[1].decode()
assert claimed.startswith("sum ") and len(claimed) == 4 + 64, "bad sum line"
records = [line + b"\n" for line in lines[2:-1]]
assert records == sorted(records), "manifest records not sorted bytewise"
s = [0] * 8
for r in records:
    s = add(s, state_of(r))
assert sum_hex(s) == claimed[4:], \
    "manifest sum line disagrees with the fold of its records: corrupt"
EOF

cat > "$ANDON_WORK/count.py" <<'EOF'
"""§4's precondition count, by hand: occurrences of a span in a blob."""
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import count_occurrences

print(count_occurrences(open(sys.argv[1], "rb").read(), sys.argv[2].encode()))
EOF

cat > "$ANDON_WORK/step4_pre.py" <<'EOF'
"""§4 by hand: the document's five steps of applying a patch, numbered
exactly as the document numbers them.  Prints the hand-computed sum
after the patch."""
import base64
import json
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import (add, count_occurrences, from_hex, neg, record,
                   sha3_hex, state_of, sum_hex)

W = os.environ["ANDON_WORK"]
E = os.environ
repo, patch_path = sys.argv[1], sys.argv[2]

intent = json.load(open(patch_path, "rb"))
ops = intent["ops"]
assert len(ops) == 4, "this walkthrough's patch has one op of each kind"

# Step 1: for each edit, count occurrences of old_str in the current
# blob.  Anything but one: stop.
edit = ops[0]["edit"]
blob = open(os.path.join(repo, edit["path"].lstrip("/")), "rb").read()
old = edit["old_str"].encode()
new = edit["new_str"].encode()
assert count_occurrences(blob, old) == 1, "old_str not unique: stop"

# Step 2: make the edit (the editor's part; count == 1 makes replace-all
# replace-one).
edited = blob.replace(old, new)
assert edited == open(os.path.join(W, "expect", "main1.rs"), "rb").read()

# Step 3: rehash each touched blob; write the realized delta.
h_main1 = sha3_hex(edited)
assert h_main1 == E["H_MAIN1"]
create = ops[1]["create"]
lib_content = base64.b64decode(create["content_b64"])
assert lib_content == open(os.path.join(W, "expect", "lib0.rs"), "rb").read()
assert sha3_hex(lib_content) == E["H_LIB0"]
chmod = ops[3]["chmod"]
sans_lf = lambda r: r.decode().rstrip("\n")
realized = [
    {"remove": sans_lf(record("100644", "/src/main.rs", E["H_MAIN0"])),
     "add":    sans_lf(record("100644", "/src/main.rs", h_main1))},
    {"add":    sans_lf(record(create["mode"], "/src/lib.rs", E["H_LIB0"]))},
    {"remove": sans_lf(record("100644", "/README.md", E["H_README"]))},
    {"remove": sans_lf(record(chmod["old_mode"], "/tools/apply", E["H_TOOL"])),
     "add":    sans_lf(record(chmod["new_mode"], "/tools/apply", E["H_TOOL"]))},
]

# Step 5: check each removed record against the manifest FIRST — the
# placeholder-debt rule is enforced here, at application, always.  The
# manifest is the §3 snapshot's, built by hand there.
with open(os.path.join(W, "hand-manifest"), "rb") as f:
    present = set(f.read().splitlines(keepends=True)[2:])
for entry in realized:
    if "remove" in entry:
        assert (entry["remove"] + "\n").encode() in present, \
            f"placeholder debt refused: {entry['remove']}"

# Step 4: new sum = old sum, add the neg of each removed record, add
# each added record.
s = from_hex(E["S_1"])
for entry in realized:
    if "remove" in entry:
        s = add(s, neg(state_of((entry["remove"] + "\n").encode())))
    if "add" in entry:
        s = add(s, state_of((entry["add"] + "\n").encode()))

# Save the hand-computed delta for comparison against the log line.
with open(os.path.join(W, "hand-realized.json"), "w") as f:
    json.dump(realized, f)
print(sum_hex(s))
EOF

cat > "$ANDON_WORK/step4_post.py" <<'EOF'
"""§4 against the log: the applied form records the intent AND the
realized delta; both must equal the hand-computed ones."""
import json
import os
import sys

W = os.environ["ANDON_WORK"]
log_path, patch_path = sys.argv[1], sys.argv[2]
line = json.loads(open(log_path, "rb").read().splitlines()[-1])
hand_realized = json.load(open(os.path.join(W, "hand-realized.json")))
assert line["realized"] == hand_realized, \
    "the logged realized delta differs from the hand computation"
assert line["intent"] == json.load(open(patch_path, "rb")), \
    "the log must record the intent as written"
assert line["sum_after"] == os.environ["S_2"]
print(line["id"])
EOF

cat > "$ANDON_WORK/arith.py" <<'EOF'
"""§2/§4 step 4 as pure arithmetic: fold a realized delta (the log's own
shape, as JSON on stdin) into a base sum."""
import json
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import add, from_hex, neg, state_of, sum_hex

s = from_hex(sys.argv[1])
for entry in json.load(sys.stdin):
    if "remove" in entry:
        s = add(s, neg(state_of(entry["remove"].encode() + b"\n")))
    if "add" in entry:
        s = add(s, state_of(entry["add"].encode() + b"\n"))
print(sum_hex(s))
EOF

cat > "$ANDON_WORK/verify_log.py" <<'EOF'
"""§5 by hand: sweep a whole log.  For every line, check the canonical
byte form (§1.3 — the stored bytes ARE the canonical JSON), re-derive
the id (§5: sha3-256 of the canonical bytes with id absent), check the
prev chain, and replay every sum by pure arithmetic from the anchor.
Prints the line ids, space-separated."""
import hashlib
import json
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import add, canonical, from_hex, neg, state_of, sum_hex

log_path, anchor_hex, want_count, want_final = sys.argv[1:5]
raw = open(log_path, "rb").read()
parts = [line + b"\n" for line in raw.split(b"\n") if line]
assert len(parts) == int(want_count), \
    f"expected {want_count} log lines, found {len(parts)}"

s = from_hex(anchor_hex)
prev = ""
ids = []
for i, rawline in enumerate(parts):
    obj = json.loads(rawline)
    assert canonical(obj) + b"\n" == rawline, \
        f"line {i}: stored bytes are not the canonical JSON of their content"
    stripped = dict(obj)
    del stripped["id"]
    assert hashlib.sha3_256(canonical(stripped)).hexdigest() == obj["id"], \
        f"line {i}: id does not re-derive per §5"
    assert obj["prev"] == prev, f"line {i}: prev does not chain"
    prev = obj["id"]
    for entry in obj["realized"]:
        if "remove" in entry:
            s = add(s, neg(state_of(entry["remove"].encode() + b"\n")))
        if "add" in entry:
            s = add(s, state_of(entry["add"].encode() + b"\n"))
    assert sum_hex(s) == obj["sum_after"], \
        f"line {i}: sum_after disagrees with pure arithmetic"
    ids.append(obj["id"])
assert sum_hex(s) == want_final, \
    f"final sum {sum_hex(s)} disagrees with {want_final}"
print(" ".join(ids))
EOF

cat > "$ANDON_WORK/canon_realized.py" <<'EOF'
"""Print the canonical JSON of a log's last line's realized delta."""
import json
import sys

line = json.loads(open(sys.argv[1], "rb").read().splitlines()[-1])
print(json.dumps(line["realized"], sort_keys=True, separators=(",", ":")))
EOF

cat > "$ANDON_WORK/unionline.py" <<'EOF'
"""§6: a union-landed line carries union provenance, names its origin
line, and lands at the hand-computed sum."""
import json
import sys

log_path, want_fork, want_origin, want_sum = sys.argv[1:5]
line = json.loads(open(log_path, "rb").read().splitlines()[-1])
a = line["annotation"]
assert a["provenance"] == "union", "a landed line carries union provenance"
assert a["origin"] == {"fork": want_fork, "id": want_origin}, \
    "a landed line names the source line it re-enacts mechanically"
assert line["sum_after"] == want_sum
print(line["id"])
EOF

cat > "$ANDON_WORK/viewline.py" <<'EOF'
"""§7: a fuse is a view — empty intent, empty realized delta, the
arithmetic identity; a rendering, never a mutation."""
import json
import sys

log_path, want_from, want_to, want_sum = sys.argv[1:5]
line = json.loads(open(log_path, "rb").read().splitlines()[-1])
a = line["annotation"]
assert a["provenance"] == "view"
assert a["view"] == {"from": want_from, "to": want_to}
assert line["intent"] == {"ops": []}, "a view carries no intent ops"
assert line["realized"] == [], "a view carries an empty realized delta"
assert line["sum_after"] == want_sum, "a view is the arithmetic identity"
print(line["id"])
EOF

cat > "$ANDON_WORK/andonline.py" <<'EOF'
"""§9 step 3: an andon line carries a non-empty reason and a signature,
and does not fake instrumented reads — the reads field is absent."""
import json
import sys

log_path, want_reason = sys.argv[1], sys.argv[2]
line = json.loads(open(log_path, "rb").read().splitlines()[-1])
a = line["annotation"]
assert a["provenance"] == "andon"
assert a["reason"] and a["reason"] == want_reason, \
    "andon lines require a non-empty reason"
assert a.get("sig"), "andon lines require a signature"
assert "reads" not in a, "an andon line honestly lacks reads; it must not fake them"
print(a["sig"])
EOF

cat > "$ANDON_WORK/showcheck.py" <<'EOF'
"""§5: `abelian show` renders one line; re-canonicalized, the rendering
must be byte-identical to the stored line — the log's bytes are the
authoritative form (I4)."""
import json
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import canonical

shown = json.load(open(sys.argv[1], "rb"))
raw = open(sys.argv[2], "rb").read()
target = None
for line in raw.split(b"\n"):
    if line and json.loads(line)["id"] == shown["id"]:
        target = line + b"\n"
assert target is not None, "the shown id is not in the log"
assert canonical(shown) + b"\n" == target, \
    "show does not re-canonicalize to the stored line's bytes"
EOF

cat > "$ANDON_WORK/step6b.py" <<'EOF'
"""§6 stratum 3 by hand: the consumed record is missing because the
target drifted, so re-validate the SPAN precondition against the
target's current blob and realize a fresh delta.  Prints the union sum."""
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import (add, count_occurrences, from_hex, neg, record,
                   sha3_hex, state_of, sum_hex)

W = os.environ["ANDON_WORK"]
E = os.environ
repo = sys.argv[1]

# The target's current blob at /src/main.rs: main's own edit landed
# meanwhile, so dev2's realized delta no longer applies.
blob = open(os.path.join(repo, "src", "main.rs"), "rb").read()
assert sha3_hex(blob) == E["H_MAIN4"], "the target drifted as expected"

# old_str still unique?  (This is where disjoint-span edits to the same
# file sail through.)
old = b"fn main() {"
new = b"fn main() {\n    // dev2 was here"
assert count_occurrences(blob, old) == 1, \
    "span precondition is dead; only re-enactment (stratum 4) remains"
merged = blob.replace(old, new)
assert merged == open(os.path.join(W, "expect", "main5.rs"), "rb").read()
h = sha3_hex(merged)
assert h == E["H_MAIN5"]

# A fresh delta against the drifted target; then pure arithmetic (§2).
removed = record("100644", "/src/main.rs", E["H_MAIN4"])
added = record("100644", "/src/main.rs", h)
s = from_hex(E["S_4"])
s = add(s, neg(state_of(removed)))
s = add(s, state_of(added))
print(sum_hex(s))
EOF

cat > "$ANDON_WORK/step9.py" <<'EOF'
"""§9 with zero abelian binaries: the five steps of §4 and a hand-written
log line with "provenance": "andon".  The binary is used only afterwards,
to verify — that verification is the entire guarantee the document makes.
Prints the hand-computed line id."""
import hashlib
import json
import os
import sys

sys.path.insert(0, os.environ["ANDON_WORK"])
from andon import (add, canonical, count_occurrences, from_hex, neg,
                   record, sha3_hex, state_of, sum_hex)

W = os.environ["ANDON_WORK"]
E = os.environ
repo = sys.argv[1]
log_path = os.path.join(repo, ".abelian", "forks", "main", "log.jsonl")

# §4 step 1: count occurrences of old_str in the current blob.
main_rs = os.path.join(repo, "src", "main.rs")
blob = open(main_rs, "rb").read()
old = b'patched");'
new = b'patched by hand");'
assert count_occurrences(blob, old) == 1, "the cord obeys the same precondition"

# §4 step 2: make the edit.  An editor; this script is standing in for one.
edited = blob.replace(old, new)
assert edited == open(os.path.join(W, "expect", "main7.rs"), "rb").read()
with open(main_rs, "wb") as f:
    f.write(edited)

# §4 step 3: rehash the touched blob; write the realized delta.
h_new = sha3_hex(edited)
assert h_new == E["H_MAIN7"]
removed = record("100644", "/src/main.rs", E["H_MAIN6"]).decode().rstrip("\n")
added = record("100644", "/src/main.rs", h_new).decode().rstrip("\n")

# §4 step 5: check the removed record against the manifest FIRST — the
# placeholder-debt rule.  Build the current manifest by hand; check the
# hand manifest really is the current state, then check membership.
manifest_records = [
    record("100644", "/src/lib.rs", E["H_LIB1"]),
    record("100644", "/src/main.rs", E["H_MAIN6"]),
    record("100644", "/tools/apply", E["H_TOOL"]),
]
s = [0] * 8
for r in manifest_records:
    s = add(s, state_of(r))
assert sum_hex(s) == E["S_5"], "the hand-built manifest must be the current state"
assert (removed + "\n").encode() in manifest_records, "placeholder debt refused"

# §4 step 4: new sum = old sum, add the neg of the removed record, add
# the added record.
s = from_hex(E["S_5"])
s = add(s, neg(state_of((removed + "\n").encode())))
s = add(s, state_of((added + "\n").encode()))
assert sum_hex(s) == E["S_CORD"]

# The v0 signature scheme is a detached signature blob; the pool is
# content-addressed, so write it by hand.
sig = b"abelian detached signature v0\nsigned-by: cord-puller\n"
assert sha3_hex(sig) == E["H_SIG"]
blob_dir = os.path.join(repo, ".abelian", "blobs", E["H_SIG"][:2])
os.makedirs(blob_dir, exist_ok=True)
with open(os.path.join(blob_dir, E["H_SIG"][2:]), "wb") as f:
    f.write(sig)

# §5: the log line, hand-written; the id is §5's rule applied by hand.
raw = open(log_path, "rb").read()
prev = json.loads(raw.splitlines()[-1])["id"]
line = {
    "annotation": {
        "author": "cord-puller",
        "origin": None,
        "prose": "CVE-2026-0001: pulled by hand, with the document and "
                 "eleven lines of Python",
        "provenance": "andon",
        "reason": "CVE-2026-0001",
        "session": None,
        "sig": E["H_SIG"],
    },
    "committed_ms": int(E["V_CORD_MS"]),
    "id": "",
    "intent": {"ops": [{"edit": {"path": "/src/main.rs",
                                 "old_str": old.decode(),
                                 "new_str": new.decode()}}]},
    "prev": prev,
    "realized": [{"add": added, "remove": removed}],
    "sum_after": E["S_CORD"],
}
line["id"] = hashlib.sha3_256(
    canonical({k: v for k, v in line.items() if k != "id"})).hexdigest()
with open(log_path, "ab") as f:
    f.write(canonical(line) + b"\n")
print(line["id"])
EOF

############################### the walk #################################

step "§0 setup: abelian init"
"$ABELIAN" init "$REPO" > /dev/null
cd "$REPO"
printf 'abelian v0\n' > "$EXPECT/version"
assert_bytes "repository version file" "$EXPECT/version" .abelian/version
EMPTY_SUM_GOT=$("$ABELIAN" sum)
assert_eq "empty tree sums to zeros" "$S_EMPTY" "$EMPTY_SUM_GOT"
printf 'abelian-manifest v0\nsum %s\n' "$S_EMPTY" > "$EXPECT/empty.manifest"
assert_bytes "empty anchor manifest" "$EXPECT/empty.manifest" \
    ".abelian/anchors/$S_EMPTY.manifest"

step "§1 Elements — abelian sum --records"
cp "$EXPECT/README.md" README.md
mkdir -p src tools
cp "$EXPECT/main0.rs" src/main.rs
cp "$EXPECT/tools_apply" tools/apply
chmod 755 tools/apply
# By hand: the document's one-liner per file (hash asserted against the
# embedded vector inside mkrecord.py), then sorted bytewise per §3.
: > "$ANDON_WORK/hand-records"
for spec in "100644 /README.md README.md $H_README" \
            "100644 /src/main.rs src/main.rs $H_MAIN0" \
            "100755 /tools/apply tools/apply $H_TOOL"; do
    # shellcheck disable=SC2086
    python3 "$ANDON_WORK/mkrecord.py" $spec >> "$ANDON_WORK/hand-records"
done
sort "$ANDON_WORK/hand-records" > "$ANDON_WORK/hand-records.sorted"
"$ABELIAN" sum --records | sed '$d' > "$ANDON_WORK/abelian-records"
assert_bytes "§1 element records" "$ANDON_WORK/hand-records.sorted" \
    "$ANDON_WORK/abelian-records"

step "§2 The sum — abelian sum"
HAND_SUM1=$(python3 "$ANDON_WORK/fold.py" "$ANDON_WORK/hand-records.sorted")
assert_eq "§2 hand fold == embedded vector" "$S_1" "$HAND_SUM1"
ABELIAN_SUM1=$("$ABELIAN" sum)
assert_eq "§2 abelian sum == hand fold" "$HAND_SUM1" "$ABELIAN_SUM1"

step "§3 Snapshots — abelian commit, abelian snapshot"
COMMIT_OUT=$("$ABELIAN" commit --author andon-tester --prose 'initial: three elements')
COMMIT_LINE1=${COMMIT_OUT%%$'\n'*}
assert_eq "§3 commit lands at the working-tree sum" "$S_1" "${COMMIT_LINE1##* }"
"$ABELIAN" snapshot > /dev/null
# By hand: a header, the sum line, and every record, sorted bytewise.
{ printf 'abelian-manifest v0\n'
  printf 'sum %s\n' "$S_1"
  cat "$ANDON_WORK/hand-records.sorted"; } > "$ANDON_WORK/hand-manifest"
assert_bytes "§3 manifest" "$ANDON_WORK/hand-manifest" ".abelian/anchors/$S_1.manifest"
python3 "$ANDON_WORK/check_manifest.py" "$ANDON_WORK/hand-manifest"
note "§3 manifest verifies against its own sum line"
assert_check "$S_1"

step "§4 Patches — abelian apply"
HAND_SUM2=$(python3 "$ANDON_WORK/step4_pre.py" "$REPO" "$ANDON_WORK/patch1.json")
assert_eq "§4 hand five-step sum == embedded vector" "$S_2" "$HAND_SUM2"
APPLY_OUT=$("$ABELIAN" apply --author andon-tester \
    --prose 'span ops: edit, create, delete, chmod' "$ANDON_WORK/patch1.json")
assert_eq "§4 abelian apply == hand arithmetic" "$HAND_SUM2" "${APPLY_OUT##* }"
python3 "$ANDON_WORK/step4_post.py" .abelian/forks/main/log.jsonl \
    "$ANDON_WORK/patch1.json" > /dev/null
note "§4 logged intent and realized delta match the hand computation"
assert_bytes "§4 edited blob" "$EXPECT/main1.rs" src/main.rs
assert_bytes "§4 created blob" "$EXPECT/lib0.rs" src/lib.rs
[ ! -e README.md ] || fail "§4: delete op left README.md in the working tree"
note "§4 deleted blob is gone from the working tree"
python3 -c 'import os, sys; m = os.stat("tools/apply").st_mode & 0o777; \
            sys.exit(0 if m == 0o644 else f"mode {m:o} != 644")'
note "§4 chmod landed (tools/apply is 0644)"
assert_check "$S_2"

# "Anything but one: stop."  Every refused patch must leave state and log
# untouched — the document's five steps are atomic.
expect_refused() { # <label> <patch.json>
    head_before=$("$ABELIAN" rev-parse HEAD)
    lines_before=$(wc -l < .abelian/forks/main/log.jsonl | tr -d ' ')
    if "$ABELIAN" apply --author andon-tester --prose 'must refuse' "$2" \
            > "$ANDON_WORK/refused.out" 2>&1; then
        fail "$1: abelian applied a patch the ANDON says to stop"
    fi
    head_after=$("$ABELIAN" rev-parse HEAD)
    assert_eq "$1: state untouched" "$head_before" "$head_after"
    lines_after=$(wc -l < .abelian/forks/main/log.jsonl | tr -d ' ')
    assert_eq "$1: log untouched" "$lines_before" "$lines_after"
    note "$1: refused, atomically"
}
ZERO_COUNT=$(python3 "$ANDON_WORK/count.py" src/main.rs 'NO SUCH SPAN')
assert_eq "§4 precondition: zero occurrences counted by hand" "0" "$ZERO_COUNT"
expect_refused "§4 old_str absent" "$ANDON_WORK/bad-zero.json"
MANY=$(python3 "$ANDON_WORK/count.py" src/main.rs 'l')
[ "$MANY" -gt 1 ] || fail "§4 test vector: expected 'l' to occur many times, got $MANY"
expect_refused "§4 old_str not unique ($MANY occurrences)" "$ANDON_WORK/bad-many.json"
expect_refused "§4 delete with wrong blob (placeholder debt refused)" \
    "$ANDON_WORK/bad-delete.json"
expect_refused "§4 create of a present path" "$ANDON_WORK/bad-create.json"

step "§5 The log — canonical bytes, chained ids, replayed sums"
IDS=$(python3 "$ANDON_WORK/verify_log.py" .abelian/forks/main/log.jsonl \
    "$S_EMPTY" 2 "$S_2")
note "§5 both lines: canonical bytes, ids re-derived, prev chained, sums replayed"
set -- $IDS
"$ABELIAN" show "$2" > "$ANDON_WORK/show.json"
python3 "$ANDON_WORK/showcheck.py" "$ANDON_WORK/show.json" .abelian/forks/main/log.jsonl
note "§5 abelian show re-canonicalizes to the stored line, byte for byte"
"$ABELIAN" log --raw > "$ANDON_WORK/log-raw"
for id in $IDS; do
    grep -F -q "$id" "$ANDON_WORK/log-raw" || fail "§5: $id missing from abelian log --raw"
done
note "§5 abelian log --raw renders every line"

step "§6 Fork and union — abelian fork, abelian union"
FORK_OUT=$("$ABELIAN" fork dev)
assert_eq "§6 abelian fork" "fork dev anchored at $S_2" "$FORK_OUT"
{ printf 'abelian-fork v0\n'
  printf 'anchor %s\n' "$S_2"
  printf 'manifest %s\n' "$S_2"; } > "$ANDON_WORK/hand-fork"
assert_bytes "§6 fork file" "$ANDON_WORK/hand-fork" .abelian/forks/dev/fork
[ -f .abelian/forks/dev/log.jsonl ] && [ ! -s .abelian/forks/dev/log.jsonl ] \
    || fail "§6: a fresh fork is an anchor and an EMPTY log"
note "§6 an anchor and an empty log — that is the whole file"
# The fork anchors a manifest of the current state; byte-check it too.
{ printf 'abelian-manifest v0\n'
  printf 'sum %s\n' "$S_2"
  python3 "$ANDON_WORK/mkrecord.py" 100644 /src/lib.rs "$EXPECT/lib0.rs" "$H_LIB0"
  python3 "$ANDON_WORK/mkrecord.py" 100644 /src/main.rs "$EXPECT/main1.rs" "$H_MAIN1"
  python3 "$ANDON_WORK/mkrecord.py" 100644 /tools/apply "$EXPECT/tools_apply" "$H_TOOL"
} > "$ANDON_WORK/hand-manifest-2"
python3 "$ANDON_WORK/check_manifest.py" "$ANDON_WORK/hand-manifest-2"
assert_bytes "§6 anchored manifest" "$ANDON_WORK/hand-manifest-2" \
    ".abelian/anchors/$S_2.manifest"

# A disjoint edit on each side; union lands it at stratum 2.
DELTA_LIB='[{"remove":"100644\t/src/lib.rs\t'$H_LIB0'","add":"100644\t/src/lib.rs\t'$H_LIB1'"}]'
DEV_OUT=$("$ABELIAN" apply --fork dev --author andon-tester \
    --prose 'dev: answer 137' "$ANDON_WORK/patch-dev.json")
DEV_LINE_ID=${DEV_OUT%% *}
HAND_DEV=$(printf '%s' "$DELTA_LIB" | python3 "$ANDON_WORK/arith.py" "$S_2")
assert_eq "§6 dev sum, by hand" "$S_DEV" "$HAND_DEV"
assert_eq "§6 dev sum, abelian == hand" "$HAND_DEV" "${DEV_OUT##* }"
DEV_IDS=$(python3 "$ANDON_WORK/verify_log.py" .abelian/forks/dev/log.jsonl \
    "$S_2" 1 "$S_DEV")
assert_eq "§6 dev log verifies against its anchor" "$DEV_LINE_ID" "$DEV_IDS"
HAND_DELTA_CANON=$(printf '%s' "$DELTA_LIB" | canon)
DEV_DELTA_CANON=$(python3 "$ANDON_WORK/canon_realized.py" .abelian/forks/dev/log.jsonl)
assert_eq "§6 dev realized delta == hand delta" "$HAND_DELTA_CANON" "$DEV_DELTA_CANON"

DELTA_MAIN='[{"remove":"100644\t/src/main.rs\t'$H_MAIN1'","add":"100644\t/src/main.rs\t'$H_MAIN2'"}]'
MAIN_OUT=$("$ABELIAN" apply --author andon-tester \
    --prose 'main: hello union' "$ANDON_WORK/patch-main.json")
HAND_S3=$(printf '%s' "$DELTA_MAIN" | python3 "$ANDON_WORK/arith.py" "$S_2")
assert_eq "§6 main sum, by hand" "$S_3" "$HAND_S3"
assert_eq "§6 main sum, abelian == hand" "$HAND_S3" "${MAIN_OUT##* }"

UNION_OUT=$("$ABELIAN" union --author andon-tester dev)
printf '%s\n' "$UNION_OUT" | grep -F -q "(stratum 2: realized replay)" \
    || fail "§6: expected stratum 2 (realized replay), got: $UNION_OUT"
printf '%s\n' "$UNION_OUT" | grep -F -q " <- $DEV_LINE_ID " \
    || fail "§6: union must name the origin line, got: $UNION_OUT"
HAND_UNION2=$(printf '%s' "$DELTA_LIB" | python3 "$ANDON_WORK/arith.py" "$S_3")
assert_eq "§6 union sum, by hand (stratum 2)" "$S_UNION2" "$HAND_UNION2"
HEAD_AFTER_UNION=$("$ABELIAN" rev-parse HEAD)
assert_eq "§6 union sum, abelian == hand" "$HAND_UNION2" "$HEAD_AFTER_UNION"
python3 "$ANDON_WORK/unionline.py" .abelian/forks/main/log.jsonl dev \
    "$DEV_LINE_ID" "$S_UNION2" > /dev/null
note "§6 landed line carries union provenance and names its origin"
DEV_AFTER_UNION=$("$ABELIAN" rev-parse --fork dev HEAD)
assert_eq "§6 union does not mutate the source fork" "$S_DEV" "$DEV_AFTER_UNION"

# Now two disjoint-span edits to the SAME file; union lands at stratum 3.
"$ABELIAN" fork dev2 > /dev/null
DELTA_DEV2='[{"remove":"100644\t/src/main.rs\t'$H_MAIN2'","add":"100644\t/src/main.rs\t'$H_MAIN3'"}]'
DEV2_OUT=$("$ABELIAN" apply --fork dev2 --author andon-tester \
    --prose 'dev2: comment at top' "$ANDON_WORK/patch-dev2.json")
DEV2_LINE_ID=${DEV2_OUT%% *}
HAND_DEV2=$(printf '%s' "$DELTA_DEV2" | python3 "$ANDON_WORK/arith.py" "$S_UNION2")
assert_eq "§6 dev2 sum, by hand" "$S_DEV2" "$HAND_DEV2"
assert_eq "§6 dev2 sum, abelian == hand" "$HAND_DEV2" "${DEV2_OUT##* }"
python3 "$ANDON_WORK/verify_log.py" .abelian/forks/dev2/log.jsonl \
    "$S_UNION2" 1 "$S_DEV2" > /dev/null
note "§6 dev2 log verifies against its anchor"
DELTA_MAIN2='[{"remove":"100644\t/src/main.rs\t'$H_MAIN2'","add":"100644\t/src/main.rs\t'$H_MAIN4'"}]'
MAIN2_OUT=$("$ABELIAN" apply --author andon-tester \
    --prose 'main: add done' "$ANDON_WORK/patch-main2.json")
HAND_S4=$(printf '%s' "$DELTA_MAIN2" | python3 "$ANDON_WORK/arith.py" "$S_UNION2")
assert_eq "§6 main sum, by hand" "$S_4" "$HAND_S4"
assert_eq "§6 main sum, abelian == hand" "$HAND_S4" "${MAIN2_OUT##* }"

# By hand FIRST: stratum 2's consumed record is missing (the target
# drifted), so re-validate the span and compute the fresh delta by hand.
HAND_UNION3=$(python3 "$ANDON_WORK/step6b.py" "$REPO")
assert_eq "§6 union sum, by hand (stratum 3)" "$S_UNION3" "$HAND_UNION3"
UNION2_OUT=$("$ABELIAN" union --author andon-tester dev2)
printf '%s\n' "$UNION2_OUT" | grep -F -q "(stratum 3: intent replay)" \
    || fail "§6: expected stratum 3 (intent replay), got: $UNION2_OUT"
HEAD_AFTER_UNION3=$("$ABELIAN" rev-parse HEAD)
assert_eq "§6 union sum, abelian == hand" "$HAND_UNION3" "$HEAD_AFTER_UNION3"
python3 "$ANDON_WORK/unionline.py" .abelian/forks/main/log.jsonl dev2 \
    "$DEV2_LINE_ID" "$S_UNION3" > /dev/null
assert_bytes "§6 disjoint spans, same file: both edits present" \
    "$EXPECT/main5.rs" src/main.rs
assert_check "$S_UNION3"

step "§7 Fuse — a lossless view"
IDS_BEFORE=$(python3 "$ANDON_WORK/verify_log.py" .abelian/forks/main/log.jsonl \
    "$S_EMPTY" 6 "$S_UNION3")
set -- $IDS_BEFORE
FROM_ID=$2
TO_ID=$6
cp .abelian/forks/main/log.jsonl "$ANDON_WORK/log-before-fuse"
BYTES_BEFORE=$(wc -c < .abelian/forks/main/log.jsonl | tr -d ' ')
FUSE_OUT=$("$ABELIAN" fuse --author andon-tester \
    --prose 'the span-ops beat' "$FROM_ID" "$TO_ID")
printf '%s\n' "$FUSE_OUT" | grep -F -q \
    "fused $FROM_ID..$TO_ID (lossless: the fine structure remains underneath)" \
    || fail "§7: unexpected fuse output: $FUSE_OUT"
VIEW_ID=$(python3 "$ANDON_WORK/viewline.py" .abelian/forks/main/log.jsonl \
    "$FROM_ID" "$TO_ID" "$S_UNION3")
note "§7 view line $VIEW_ID: empty intent, empty realized, identity arithmetic"
# Lossless means the covered lines' bytes are still there, untouched.
head -c "$BYTES_BEFORE" .abelian/forks/main/log.jsonl \
    > "$ANDON_WORK/log-prefix-after-fuse"
assert_bytes "§7 fuse is a view, never a mutation" \
    "$ANDON_WORK/log-before-fuse" "$ANDON_WORK/log-prefix-after-fuse"
"$ABELIAN" log --raw > "$ANDON_WORK/log-raw"
for id in $IDS_BEFORE; do
    grep -F -q "$id" "$ANDON_WORK/log-raw" \
        || fail "§7: line $id missing from abelian log --raw"
done
note "§7 abelian log --raw still renders every covered line"
"$ABELIAN" log > "$ANDON_WORK/log-fused"
grep -E -q 'fuse\([0-9]+ lines\)' "$ANDON_WORK/log-fused" \
    || fail "§7: the fused view does not render the beat"
note "§7 abelian log renders the fused beat at the default zoom"

step "§9 The Andon cord — pulled with the binary"
DELTA_ANDON='[{"remove":"100644\t/src/main.rs\t'$H_MAIN5'","add":"100644\t/src/main.rs\t'$H_MAIN6'"}]'
HAND_ANDON=$(printf '%s' "$DELTA_ANDON" | python3 "$ANDON_WORK/arith.py" "$S_UNION3")
assert_eq "§9 cord sum, by hand" "$S_5" "$HAND_ANDON"
ANDON_OUT=$("$ABELIAN" apply --author oncall-human --provenance=andon \
    --reason="CVE-2026-0001" --sign \
    --prose 'emergency: patch the greeting' "$ANDON_WORK/patch-andon.json")
assert_eq "§9 cord sum, abelian == hand" "$HAND_ANDON" "${ANDON_OUT##* }"
SIG=$(python3 "$ANDON_WORK/andonline.py" .abelian/forks/main/log.jsonl \
    "CVE-2026-0001")
note "§9 andon line: reason and sig present; reads honestly absent"
assert_bytes "§9 detached signature blob" "$EXPECT/sig-oncall" \
    ".abelian/blobs/${SIG:0:2}/${SIG:2}"
assert_bytes "§9 cord blob" "$EXPECT/main6.rs" src/main.rs
assert_check "$S_5"

step "§9 The Andon cord — pulled by hand, with zero abelian binaries"
CORD_ID=$(python3 "$ANDON_WORK/step9.py" "$REPO")
note "§9 hand-written log line: $CORD_ID"
# The binary's only role now is verification — the document's guarantee.
assert_check "$S_CORD"
CORD_HEAD=$("$ABELIAN" rev-parse HEAD)
assert_eq "§9 abelian rev-parse accepts the hand line" "$S_CORD" "$CORD_HEAD"
CORD_HEAD_LINE=$("$ABELIAN" rev-parse --line HEAD)
assert_eq "§9 the hand line is the head" "$CORD_ID" "$CORD_HEAD_LINE"
"$ABELIAN" show "$CORD_ID" > "$ANDON_WORK/show-cord.json"
python3 "$ANDON_WORK/showcheck.py" "$ANDON_WORK/show-cord.json" \
    .abelian/forks/main/log.jsonl
note "§9 abelian show re-canonicalizes to the hand-written bytes"
assert_bytes "§9 cord blob" "$EXPECT/main7.rs" src/main.rs

step "§5 final sweep — the whole chain, by hand"
python3 "$ANDON_WORK/verify_log.py" .abelian/forks/main/log.jsonl \
    "$S_EMPTY" 9 "$S_CORD" > /dev/null
note "§5 all nine lines: canonical bytes, ids re-derived, prev chained, sums replayed"

echo
echo "andon.sh: PASS — the ANDON.md workflow is byte-identical to abelian's commands"
