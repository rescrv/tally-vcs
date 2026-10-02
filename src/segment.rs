//! §3.1 Segments: the packed format's unit of storage.
//!
//! A segment is an immutable pair: `seg/<segid>.pk` (standard zstd frames)
//! and `seg/<segid>.idx` (a plain-text entry table).  `segid` commits to
//! both halves — see [`segid_of`] — so immutability is enforced by naming,
//! uploads are idempotent, and caches never invalidate.  Everything here
//! lives strictly on the encoding side of the Wall (I1): no parameter of the
//! encoding appears in any hash preimage.

use std::collections::BTreeMap;
use std::io::Read;

use crate::ident::{Sum, is_hex64, sha3_hex};
use crate::log::LogLine;
use crate::patch::{Op, replace_unique};
use crate::{Error, Result};

/////////////////////////////////////////////// enc ///////////////////////////////////////////////

/// SHA3-256 of the empty byte string.  The empty blob is implicitly present
/// in every pack: it is the base a `construct` names when the line it
/// replays creates its path from inline content, so a create is an edit
/// from nothing and costs no stored bytes.
pub const EMPTY_BLOB: &str = "a7ffc6f8bf1ed76651c14756a061d662f580ff4de43b49fa82d80a4b80f8434a";

/// How an entry's bytes are (or are not) stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Enc {
    /// Bytes as-is in the decompressed frame.
    Raw,
    /// Bytes are zstd-compressed within the decompressed frame.
    Zstd,
    /// zstd with a dictionary; aux1 names the dictionary blob.
    Zstdd {
        /// Content hash of the dictionary blob.
        dict: String,
    },
    /// No bytes stored: the blob is the result of applying the named log
    /// line's edit ops for this path to the base blob.  Versioned files
    /// deduplicate against the history that produced them.
    Construct {
        /// Content hash of the base blob.
        base: String,
        /// The log line whose edit ops construct this blob.
        line_id: String,
    },
    /// A byte-preserved log span (I4): exact JSONL bytes, never transcoded.
    Lines {
        /// The fork the span belongs to.
        fork: String,
        /// `first..last` line ids of the span.
        span: String,
    },
}

/// One line of the `.idx`: `<entry-sha3> <frame#> <offset> <len> <enc>
/// [<aux1>] [<aux2>]`, offsets and lengths in the decompressed frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdxEntry {
    /// Content hash of the item's bytes (blob content, span bytes) —
    /// identity, never encoding.
    pub sha3: String,
    /// Which zstd frame holds the bytes.
    pub frame: usize,
    /// Offset in the decompressed frame.
    pub offset: usize,
    /// Length in the decompressed frame.
    pub len: usize,
    /// The encoding.
    pub enc: Enc,
}

impl IdxEntry {
    fn to_line(&self) -> String {
        let (enc, aux1, aux2): (&str, Option<&str>, Option<&str>) = match &self.enc {
            Enc::Raw => ("raw", None, None),
            Enc::Zstd => ("zstd", None, None),
            Enc::Zstdd { dict } => ("zstdd", Some(dict), None),
            Enc::Construct { base, line_id } => ("construct", Some(base), Some(line_id)),
            Enc::Lines { fork, span } => ("lines", Some(fork), Some(span)),
        };
        let mut line = format!(
            "{} {} {} {} {enc}",
            self.sha3, self.frame, self.offset, self.len
        );
        if let Some(a) = aux1 {
            line.push(' ');
            line.push_str(a);
        }
        if let Some(a) = aux2 {
            line.push(' ');
            line.push_str(a);
        }
        line.push('\n');
        line
    }

    fn parse(line: &str) -> Result<Self> {
        let fields: Vec<&str> = line.split(' ').collect();
        if fields.len() < 5 {
            return Err(Error::Corrupt(format!("short idx line: {line:?}")));
        }
        let sha3 = fields[0].to_string();
        if !is_hex64(&sha3) {
            return Err(Error::Corrupt(format!("bad idx entry hash: {sha3:?}")));
        }
        let parse_num = |s: &str| -> Result<usize> {
            s.parse()
                .map_err(|_| Error::Corrupt(format!("bad idx number: {s:?}")))
        };
        let frame = parse_num(fields[1])?;
        let offset = parse_num(fields[2])?;
        let len = parse_num(fields[3])?;
        let aux1 = fields.get(5).map(|s| s.to_string());
        let aux2 = fields.get(6).map(|s| s.to_string());
        let enc = match (fields[4], aux1, aux2) {
            ("raw", None, None) => Enc::Raw,
            ("zstd", None, None) => Enc::Zstd,
            ("zstdd", Some(dict), None) => Enc::Zstdd { dict },
            ("construct", Some(base), Some(line_id)) => Enc::Construct { base, line_id },
            ("lines", Some(fork), Some(span)) => Enc::Lines { fork, span },
            (enc, _, _) => {
                return Err(Error::Corrupt(format!(
                    "bad idx enc/aux for {enc:?}: {line:?}"
                )));
            }
        };
        Ok(IdxEntry {
            sha3,
            frame,
            offset,
            len,
            enc,
        })
    }
}

//////////////////////////////////////////// image items //////////////////////////////////////////

/// One canonical record in a segment's logical image, regardless of
/// encoding.  A `construct` entry contributes the same `blob` item a
/// keyframe of the same content would: the image is logical,
/// encoding-blind.  The (pk_sha3, image_setsum) pair is the Wall made
/// verifiable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ImageItem {
    /// A blob by content hash — raw, zstd, zstdd, and construct alike.
    Blob(String),
    /// A log line by fork and id.  Fork-qualified so the image is a set:
    /// two forks that each carry a line with the same id carry two items.
    Line {
        /// The fork whose log the line belongs to.
        fork: String,
        /// The line's id.
        id: String,
    },
    /// A dictionary blob by content hash.
    Dict(String),
}

impl ImageItem {
    /// The canonical record bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            ImageItem::Blob(h) => format!("blob\t{h}\n"),
            ImageItem::Line { fork, id } => format!("line\t{fork}\t{id}\n"),
            ImageItem::Dict(h) => format!("dict\t{h}\n"),
        }
        .into_bytes()
    }
}

/// Fold image items into a setsum.
pub fn image_setsum<'a>(items: impl IntoIterator<Item = &'a ImageItem>) -> Sum {
    let mut sum = Sum::zero();
    for item in items {
        sum.insert(&item.to_bytes());
    }
    sum
}

////////////////////////////////////////////// writer /////////////////////////////////////////////

/// A segment's id: SHA3-256 of `pk_sha3 ‖ idx_sha3`, the two hashes as 128
/// lowercase hex characters.  The id must commit to the `.idx` as well as
/// the `.pk`: equal frame bytes can carry different entry tables — a
/// `construct` stores no bytes, and the same bytes can be cut into entries
/// differently — and both objects are named by the id, so an id over the
/// `.pk` alone would let two segments claim one `.idx` name.
pub fn segid_of(pk_sha3: &str, idx_sha3: &str) -> String {
    sha3_hex(format!("{pk_sha3}{idx_sha3}").as_bytes())
}

/// What to put in a segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentInput {
    /// A blob, stored raw in the frame.
    Blob(Vec<u8>),
    /// A blob that is a zstd dictionary; contributes a `dict` image item.
    Dict(Vec<u8>),
    /// A blob stored as no bytes at all: the result of replaying a log
    /// line's ops for the target's path over a base blob (§3.1).  The
    /// writer is responsible for having verified that the replay
    /// reproduces `sha3`; every reader re-verifies.
    Construct {
        /// Content hash of the blob this entry reproduces.
        sha3: String,
        /// Content hash of the base blob, or [`EMPTY_BLOB`].
        base: String,
        /// The log line whose ops construct the blob.
        line_id: String,
    },
    /// A byte-preserved log span: the exact JSONL bytes plus the parsed ids
    /// for the image.
    LogSpan {
        /// The fork the span belongs to.
        fork: String,
        /// The exact bytes of the span.
        bytes: Vec<u8>,
        /// The ids of the lines in the span, in order.
        line_ids: Vec<String>,
    },
}

impl SegmentInput {
    /// The image items this input contributes, computed without building
    /// anything: what a segment holding it adds to the logical image.
    pub fn image_items(&self) -> Vec<ImageItem> {
        match self {
            SegmentInput::Blob(content) => vec![ImageItem::Blob(sha3_hex(content))],
            SegmentInput::Dict(content) => vec![ImageItem::Dict(sha3_hex(content))],
            SegmentInput::Construct { sha3, .. } => vec![ImageItem::Blob(sha3.clone())],
            SegmentInput::LogSpan { fork, line_ids, .. } => line_ids
                .iter()
                .map(|id| ImageItem::Line {
                    fork: fork.clone(),
                    id: id.clone(),
                })
                .collect(),
        }
    }

    /// Bytes this input contributes to the decompressed frame.
    pub fn plain_len(&self) -> usize {
        match self {
            SegmentInput::Blob(content) | SegmentInput::Dict(content) => content.len(),
            SegmentInput::Construct { .. } => 0,
            SegmentInput::LogSpan { bytes, .. } => bytes.len(),
        }
    }
}

/// A built segment, ready to be named and stored.
#[derive(Clone, Debug)]
pub struct BuiltSegment {
    /// `segid`: see [`segid_of`].
    pub segid: String,
    /// SHA3-256 of the `.pk` bytes.
    pub pk_sha3: String,
    /// The `.pk` payload: one standard zstd frame.
    pub pk: Vec<u8>,
    /// The `.idx` entry table.
    pub idx: Vec<u8>,
    /// SHA3-256 of the `.idx` bytes.
    pub idx_sha3: String,
    /// The number of entries.
    pub entries: usize,
    /// The setsum of the segment's logical image.
    pub image_setsum: Sum,
    /// First and last log line ids in the segment, if any.
    pub log_span: Option<(String, String)>,
    /// The forks with a log span in this segment, in entry order.
    pub forks: Vec<String>,
}

/// Build a segment from inputs.  The payload is a single standard zstd
/// frame so a raw segment yields to `zstd -d` without `tally` present;
/// `level` is a packing policy (§5), on the encoding side of the Wall.
pub fn build_segment(inputs: &[SegmentInput], level: i32) -> Result<BuiltSegment> {
    let mut frame_plain = Vec::new();
    let mut entries = Vec::new();
    let mut items = Vec::new();
    let mut log_span: Option<(String, String)> = None;
    let mut forks: Vec<String> = Vec::new();
    for input in inputs {
        let offset = frame_plain.len();
        match input {
            SegmentInput::Blob(content) | SegmentInput::Dict(content) => {
                frame_plain.extend_from_slice(content);
                entries.push(IdxEntry {
                    sha3: sha3_hex(content),
                    frame: 0,
                    offset,
                    len: content.len(),
                    enc: Enc::Raw,
                });
            }
            SegmentInput::Construct {
                sha3,
                base,
                line_id,
            } => {
                entries.push(IdxEntry {
                    sha3: sha3.clone(),
                    frame: 0,
                    offset: 0,
                    len: 0,
                    enc: Enc::Construct {
                        base: base.clone(),
                        line_id: line_id.clone(),
                    },
                });
            }
            SegmentInput::LogSpan {
                fork,
                bytes,
                line_ids,
            } => {
                let (Some(first), Some(last)) = (line_ids.first(), line_ids.last()) else {
                    continue;
                };
                frame_plain.extend_from_slice(bytes);
                entries.push(IdxEntry {
                    sha3: sha3_hex(bytes),
                    frame: 0,
                    offset,
                    len: bytes.len(),
                    enc: Enc::Lines {
                        fork: fork.clone(),
                        span: format!("{first}..{last}"),
                    },
                });
                log_span = match log_span.take() {
                    None => Some((first.clone(), last.clone())),
                    Some((f, _)) => Some((f, last.clone())),
                };
                if !forks.contains(fork) {
                    forks.push(fork.clone());
                }
            }
        }
        items.extend(input.image_items());
    }
    let pk = zstd::stream::encode_all(&frame_plain[..], level)
        .map_err(crate::ioerr("zstd-encoding segment frame"))?;
    let mut idx = Vec::new();
    for entry in &entries {
        idx.extend_from_slice(entry.to_line().as_bytes());
    }
    let pk_sha3 = sha3_hex(&pk);
    let idx_sha3 = sha3_hex(&idx);
    Ok(BuiltSegment {
        segid: segid_of(&pk_sha3, &idx_sha3),
        pk_sha3,
        idx_sha3,
        entries: entries.len(),
        image_setsum: image_setsum(&items),
        pk,
        idx,
        log_span,
        forks,
    })
}

/// Build segments from inputs in order, cutting whenever a segment's
/// decompressed frame reaches `target` bytes.  An input is never split, so
/// a single input larger than `target` makes a segment of its own.  The cut
/// is a pure function of the inputs, so the same inputs always yield the
/// same segids: an interrupted upload resumes by skipping what landed.
pub fn build_segments(
    inputs: &[SegmentInput],
    target: usize,
    level: i32,
) -> Result<Vec<BuiltSegment>> {
    let mut built = Vec::new();
    let mut start = 0;
    let mut size = 0usize;
    for (i, input) in inputs.iter().enumerate() {
        size = size.saturating_add(input.plain_len());
        if size >= target {
            built.push(build_segment(&inputs[start..=i], level)?);
            start = i + 1;
            size = 0;
        }
    }
    if start < inputs.len() {
        built.push(build_segment(&inputs[start..], level)?);
    }
    Ok(built)
}

/// Parse an `.idx` entry table.  The caller MUST have authenticated the
/// bytes against an `idx_sha3` first (I11).
pub fn parse_idx(idx: &[u8]) -> Result<Vec<IdxEntry>> {
    let idx_text = std::str::from_utf8(idx)
        .map_err(|_| Error::Corrupt("segment .idx is not UTF-8".to_string()))?;
    idx_text.lines().map(IdxEntry::parse).collect()
}

////////////////////////////////////////////// reader /////////////////////////////////////////////

/// A verified, opened segment.
pub struct Segment {
    /// The parsed entry table.
    pub entries: Vec<IdxEntry>,
    frames: Vec<Vec<u8>>,
}

impl Segment {
    /// Open a segment from hostile bytes, in the mandated order (§4, I11):
    /// hash the `.pk` before the decompressor sees a single byte; hash the
    /// `.idx` before parsing it; decompress with hard output budgets taken
    /// from the authenticated idx lengths, so amplification is rejected by
    /// arithmetic, not by running out of memory.
    pub fn open(
        pk: &[u8],
        idx: &[u8],
        expect_pk_sha3: &str,
        expect_idx_sha3: &str,
    ) -> Result<Self> {
        if sha3_hex(pk) != expect_pk_sha3 {
            return Err(Error::Corrupt("segment .pk hash mismatch".to_string()));
        }
        if sha3_hex(idx) != expect_idx_sha3 {
            return Err(Error::Corrupt("segment .idx hash mismatch".to_string()));
        }
        let entries = parse_idx(idx)?;
        // Budget per frame: the maximal extent any authenticated entry
        // claims.  v0 writes a single frame; readers accept only frame 0.
        let mut budgets: BTreeMap<usize, usize> = BTreeMap::new();
        for entry in &entries {
            if entry.frame != 0 {
                return Err(Error::Corrupt(
                    "v0 reads single-frame segments only".to_string(),
                ));
            }
            let end = entry
                .offset
                .checked_add(entry.len)
                .ok_or_else(|| Error::Corrupt("idx entry extent overflows".to_string()))?;
            let budget = budgets.entry(entry.frame).or_insert(0);
            *budget = (*budget).max(end);
        }
        let budget = budgets.get(&0).copied().unwrap_or(0);
        let mut decoder = zstd::stream::read::Decoder::new(pk)
            .map_err(crate::ioerr("initializing zstd decoder"))?;
        let mut plain = Vec::with_capacity(budget);
        let read = decoder
            .by_ref()
            .take(budget as u64 + 1)
            .read_to_end(&mut plain)
            .map_err(crate::ioerr("decompressing segment frame"))?;
        if read > budget {
            return Err(Error::Corrupt(format!(
                "frame decompresses past its authenticated budget of {budget} bytes"
            )));
        }
        Ok(Segment {
            entries,
            frames: vec![plain],
        })
    }

    /// The stored bytes of an entry (pre-item-decoding).
    fn stored(&self, entry: &IdxEntry) -> Result<&[u8]> {
        let frame = self
            .frames
            .get(entry.frame)
            .ok_or_else(|| Error::Corrupt(format!("no frame {}", entry.frame)))?;
        frame
            .get(entry.offset..entry.offset + entry.len)
            .ok_or_else(|| {
                Error::Corrupt(format!(
                    "entry extent {}+{} exceeds frame of {} bytes",
                    entry.offset,
                    entry.len,
                    frame.len()
                ))
            })
    }

    /// Materialize an entry's item bytes and verify their identity
    /// post-decode.  `fetch_blob` resolves construct bases and zstdd
    /// dictionaries by content hash; `fetch_line` resolves log lines by id.
    pub fn materialize(
        &self,
        entry: &IdxEntry,
        fetch_blob: &dyn Fn(&str) -> Result<Vec<u8>>,
        fetch_line: &dyn Fn(&str) -> Result<LogLine>,
    ) -> Result<Vec<u8>> {
        let bytes = match &entry.enc {
            Enc::Raw | Enc::Lines { .. } => self.stored(entry)?.to_vec(),
            Enc::Zstd => zstd::stream::decode_all(self.stored(entry)?)
                .map_err(crate::ioerr("zstd-decoding entry"))?,
            Enc::Zstdd { dict } => {
                // Dictionaries are entries; fully verified before any bytes
                // are loaded into a decompression context (§4 step 6).
                let dict_bytes = fetch_blob(dict)?;
                if sha3_hex(&dict_bytes) != *dict {
                    return Err(Error::Corrupt(format!("dictionary {dict} fails its hash")));
                }
                let mut decoder = zstd::stream::read::Decoder::with_dictionary(
                    std::io::BufReader::new(self.stored(entry)?),
                    &dict_bytes,
                )
                .map_err(crate::ioerr("initializing dictionary decoder"))?;
                let mut out = Vec::new();
                decoder
                    .read_to_end(&mut out)
                    .map_err(crate::ioerr("zstdd-decoding entry"))?;
                out
            }
            Enc::Construct { base, line_id } => {
                // construct applies only verified inputs (§4 step 7).  The
                // empty blob is implicitly present: a create replays from
                // nothing.
                let base_bytes = if base == EMPTY_BLOB {
                    Vec::new()
                } else {
                    fetch_blob(base)?
                };
                if sha3_hex(&base_bytes) != *base {
                    return Err(Error::Corrupt(format!(
                        "construct base {base} fails its hash"
                    )));
                }
                let line = fetch_line(line_id)?;
                construct_blob(&base_bytes, &line, &entry.sha3)?
            }
        };
        if sha3_hex(&bytes) != entry.sha3 {
            return Err(Error::Corrupt(format!(
                "entry {} fails its content hash after decode",
                entry.sha3
            )));
        }
        Ok(bytes)
    }

    /// Compute the logical image of this segment by materializing every
    /// entry — the scrub-level check backing the arithmetic one.
    pub fn image_items(
        &self,
        fetch_blob: &dyn Fn(&str) -> Result<Vec<u8>>,
        fetch_line: &dyn Fn(&str) -> Result<LogLine>,
    ) -> Result<Vec<ImageItem>> {
        let mut items = Vec::new();
        for entry in &self.entries {
            match &entry.enc {
                Enc::Raw | Enc::Zstd | Enc::Zstdd { .. } | Enc::Construct { .. } => {
                    self.materialize(entry, fetch_blob, fetch_line)?;
                    items.push(ImageItem::Blob(entry.sha3.clone()));
                }
                Enc::Lines { fork, .. } => {
                    let bytes = self.materialize(entry, fetch_blob, fetch_line)?;
                    let text = std::str::from_utf8(&bytes)
                        .map_err(|_| Error::Corrupt("log span is not UTF-8".to_string()))?;
                    for line in text.lines() {
                        // Overflowed lines name their blob; the image
                        // records the resolved line's id either way.
                        let parsed = LogLine::parse_with(line, fetch_blob)?;
                        items.push(ImageItem::Line {
                            fork: fork.clone(),
                            id: parsed.id,
                        });
                    }
                }
            }
        }
        Ok(items)
    }
}

/// Reproduce a blob from a base blob plus a log line's edit ops for the
/// target's path.  Since the blob's name is its content hash, every
/// materialization is self-verifying; chains bottom out at keyframes.
pub fn construct_blob(base: &[u8], line: &LogLine, target_sha3: &str) -> Result<Vec<u8>> {
    // Find the path this construct targets: the realized add carrying the
    // target hash.
    let mut target_path = None;
    for entry in &line.realized {
        if let Some(added) = entry.added()?
            && added.blob == target_sha3
        {
            target_path = Some(added.path);
        }
    }
    let target_path = target_path.ok_or_else(|| {
        Error::Corrupt(format!("line {} realizes no blob {target_sha3}", line.id))
    })?;
    let mut content = base.to_vec();
    for op in &line.intent.ops {
        match op {
            Op::Edit {
                path,
                old_str,
                new_str,
            } if *path == target_path => {
                content = replace_unique(&content, old_str.as_bytes(), new_str.as_bytes())
                    .map_err(|n| {
                        Error::Corrupt(format!(
                            "construct: old_str occurs {n} times replaying line {}",
                            line.id
                        ))
                    })?;
            }
            // A create for the target path restarts its content: whatever
            // base the entry named, the bytes from here on derive from the
            // create (a delete-then-recreate line), never from the base.
            Op::Create {
                path,
                content_b64,
                blob,
                ..
            } if *path == target_path => match (content_b64, blob) {
                (Some(b64_content), None) => {
                    content = crate::b64::decode(b64_content)?;
                }
                _ => {
                    return Err(Error::Corrupt(format!(
                        "construct: line {} creates {target_path} by blob \
                             reference; the blob is a keyframe, not a construct",
                        line.id
                    )));
                }
            },
            _ => {}
        }
    }
    if sha3_hex(&content) != target_sha3 {
        return Err(Error::Corrupt(format!(
            "construct of {target_sha3} produced different bytes"
        )));
    }
    Ok(content)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_blob(_: &str) -> Result<Vec<u8>> {
        Err(Error::Corrupt("no blob fetcher".to_string()))
    }
    fn no_line(_: &str) -> Result<LogLine> {
        Err(Error::Corrupt("no line fetcher".to_string()))
    }

    #[test]
    fn build_open_materialize_round_trip() {
        let built = build_segment(
            &[
                SegmentInput::Blob(b"hello".to_vec()),
                SegmentInput::Blob(b"world, longer".to_vec()),
            ],
            3,
        )
        .unwrap();
        assert_eq!(built.entries, 2);
        let seg = Segment::open(&built.pk, &built.idx, &built.pk_sha3, &built.idx_sha3).unwrap();
        let a = seg
            .materialize(&seg.entries[0], &no_blob, &no_line)
            .unwrap();
        assert_eq!(a, b"hello");
        let b = seg
            .materialize(&seg.entries[1], &no_blob, &no_line)
            .unwrap();
        assert_eq!(b, b"world, longer");
        // The image is logical: recomputing it matches the built setsum.
        let items = seg.image_items(&no_blob, &no_line).unwrap();
        assert_eq!(image_setsum(&items), built.image_setsum);
    }

    #[test]
    fn hostile_bytes_never_reach_the_decompressor() {
        let built = build_segment(&[SegmentInput::Blob(b"x".to_vec())], 3).unwrap();
        let mut evil = built.pk.clone();
        evil[0] ^= 1;
        match Segment::open(&evil, &built.idx, &built.pk_sha3, &built.idx_sha3) {
            Err(Error::Corrupt(msg)) => assert_eq!(msg, "segment .pk hash mismatch"),
            other => panic!(
                "tampered .pk must fail its hash check, got {:?}",
                other.err()
            ),
        }
        let mut evil_idx = built.idx.clone();
        evil_idx[0] = b'f';
        match Segment::open(&built.pk, &evil_idx, &built.pk_sha3, &built.idx_sha3) {
            Err(Error::Corrupt(msg)) => assert_eq!(msg, "segment .idx hash mismatch"),
            other => panic!(
                "tampered .idx must fail its hash check, got {:?}",
                other.err()
            ),
        }
    }

    #[test]
    fn amplification_is_rejected_by_arithmetic() {
        // A frame that decompresses past the idx's authenticated budget.
        let big = vec![0u8; 1 << 16];
        let honest = build_segment(&[SegmentInput::Blob(big)], 3).unwrap();
        // Lie in the idx: claim the entry is 8 bytes.
        let lying_idx_text = {
            let entry = &honest.idx;
            let text = std::str::from_utf8(entry).unwrap();
            let mut fields: Vec<String> = text.trim_end().split(' ').map(String::from).collect();
            fields[3] = "8".to_string();
            format!("{}\n", fields.join(" "))
        };
        let idx_sha3 = sha3_hex(lying_idx_text.as_bytes());
        match Segment::open(
            &honest.pk,
            lying_idx_text.as_bytes(),
            &honest.pk_sha3,
            &idx_sha3,
        ) {
            Err(Error::Corrupt(msg)) => assert_eq!(
                msg, "frame decompresses past its authenticated budget of 8 bytes",
                "budget from the idx must reject the oversized frame",
            ),
            other => panic!("amplification must be rejected, got {:?}", other.err()),
        }
    }

    #[test]
    fn idx_round_trips_all_encs() {
        for entry in [
            IdxEntry {
                sha3: "ab".repeat(32),
                frame: 0,
                offset: 4,
                len: 9,
                enc: Enc::Raw,
            },
            IdxEntry {
                sha3: "ab".repeat(32),
                frame: 0,
                offset: 0,
                len: 1,
                enc: Enc::Zstd,
            },
            IdxEntry {
                sha3: "ab".repeat(32),
                frame: 0,
                offset: 0,
                len: 1,
                enc: Enc::Zstdd {
                    dict: "cd".repeat(32),
                },
            },
            IdxEntry {
                sha3: "ab".repeat(32),
                frame: 0,
                offset: 0,
                len: 0,
                enc: Enc::Construct {
                    base: "cd".repeat(32),
                    line_id: "ef".repeat(32),
                },
            },
            IdxEntry {
                sha3: "ab".repeat(32),
                frame: 0,
                offset: 0,
                len: 5,
                enc: Enc::Lines {
                    fork: "main".to_string(),
                    span: "a..b".to_string(),
                },
            },
        ] {
            let line = entry.to_line();
            assert_eq!(IdxEntry::parse(line.trim_end()).unwrap(), entry, "{line}");
        }
    }

    #[test]
    fn construct_reproduces_exact_bytes() {
        use crate::log::Annotation;
        use crate::patch::{Intent, RealizedEntry};
        let base = b"fn main() { println!(\"hello\"); }\n".to_vec();
        let new_content = b"fn main() { println!(\"hello, tally\"); }\n".to_vec();
        let target = sha3_hex(&new_content);
        let line = LogLine {
            id: "test-line".to_string(),
            prev: String::new(),
            intent: Intent {
                ops: vec![Op::Edit {
                    path: "/src/main.rs".to_string(),
                    old_str: "hello".to_string(),
                    new_str: "hello, tally".to_string(),
                }],
            },
            realized: vec![RealizedEntry {
                remove: Some(format!("100644\t/src/main.rs\t{}", sha3_hex(&base))),
                add: Some(format!("100644\t/src/main.rs\t{target}")),
            }],
            sum_after: "0".repeat(64),
            committed_ms: 0,
            annotation: Annotation::default(),
        };
        let out = construct_blob(&base, &line, &target).unwrap();
        assert_eq!(out, new_content);
        // A wrong target hash is caught: every materialization self-verifies.
        assert!(construct_blob(&base, &line, &sha3_hex(b"other")).is_err());
    }

    #[test]
    fn construct_replays_a_create_preceding_an_edit() {
        use crate::log::Annotation;
        use crate::patch::{Intent, RealizedEntry};
        // A delete-then-recreate-then-edit line: the target derives from
        // the create's content, never from the base blob.
        let base = b"the old file, wholly unrelated\n".to_vec();
        let created = b"fresh start: hello\n";
        let new_content = b"fresh start: hello, tally\n".to_vec();
        let target = sha3_hex(&new_content);
        let intent_with_create = |blob: Option<String>, content_b64: Option<String>| Intent {
            ops: vec![
                Op::Delete {
                    path: "/f".to_string(),
                    blob: sha3_hex(&base),
                },
                Op::Create {
                    path: "/f".to_string(),
                    mode: "100644".to_string(),
                    blob,
                    content_b64,
                },
                Op::Edit {
                    path: "/f".to_string(),
                    old_str: "hello".to_string(),
                    new_str: "hello, tally".to_string(),
                },
            ],
        };
        let line = |intent: Intent| LogLine {
            id: "test-line".to_string(),
            prev: String::new(),
            intent,
            realized: vec![RealizedEntry {
                remove: Some(format!("100644\t/f\t{}", sha3_hex(&base))),
                add: Some(format!("100644\t/f\t{target}")),
            }],
            sum_after: "0".repeat(64),
            committed_ms: 0,
            annotation: Annotation::default(),
        };
        // Create by inline content: the construct restarts from it.
        let inline = line(intent_with_create(None, Some(crate::b64::encode(created))));
        assert_eq!(
            construct_blob(&base, &inline, &target).unwrap(),
            new_content
        );
        // Create by blob reference: not constructible from this entry.
        let by_ref = line(intent_with_create(Some(sha3_hex(created)), None));
        assert!(matches!(
            construct_blob(&base, &by_ref, &target),
            Err(Error::Corrupt(_)),
        ));
    }

    #[test]
    fn empty_blob_is_the_hash_of_nothing() {
        assert_eq!(sha3_hex(b""), EMPTY_BLOB);
    }

    #[test]
    fn construct_entries_store_no_bytes_and_cuts_are_deterministic() {
        let inputs = vec![
            SegmentInput::Blob(vec![b'a'; 300]),
            SegmentInput::Construct {
                sha3: "ab".repeat(32),
                base: EMPTY_BLOB.to_string(),
                line_id: "cd".repeat(32),
            },
            SegmentInput::Blob(vec![b'b'; 300]),
            SegmentInput::Blob(vec![b'c'; 300]),
        ];
        let segments = build_segments(&inputs, 500, 3).unwrap();
        assert_eq!(segments.len(), 2, "cut once the frame reaches the target");
        let again = build_segments(&inputs, 500, 3).unwrap();
        let ids = |s: &[BuiltSegment]| s.iter().map(|b| b.segid.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&segments), ids(&again), "same inputs, same segids");
        let entries = parse_idx(&segments[1].idx).unwrap();
        assert_eq!(entries.len(), 1);
        let first = parse_idx(&segments[0].idx).unwrap();
        assert_eq!(first[1].len, 0, "a construct stores no bytes");
        let mut total = Sum::zero();
        for s in &segments {
            total = total + s.image_setsum.clone();
        }
        let items: Vec<ImageItem> = inputs.iter().flat_map(|i| i.image_items()).collect();
        assert_eq!(
            total,
            image_setsum(&items),
            "cutting never changes the image"
        );
    }

    #[test]
    fn equal_frames_with_different_tables_are_different_segments() {
        let span = SegmentInput::LogSpan {
            fork: "main".to_string(),
            bytes: b"{\"fake\":\"line\"}\n".to_vec(),
            line_ids: vec!["id-1".to_string()],
        };
        let construct = SegmentInput::Construct {
            sha3: "ab".repeat(32),
            base: EMPTY_BLOB.to_string(),
            line_id: "id-1".to_string(),
        };
        let bare = build_segment(std::slice::from_ref(&span), 3).unwrap();
        let with = build_segment(&[construct, span], 3).unwrap();
        assert_eq!(bare.pk, with.pk, "a construct adds no frame bytes");
        assert_ne!(bare.idx_sha3, with.idx_sha3);
        assert_ne!(bare.segid, with.segid, "so the id must commit to the table");
    }

    #[test]
    fn pk_is_standard_zstd() {
        // The emergency path: a raw segment yields to plain zstd decoding
        // without tally present.
        let built = build_segment(&[SegmentInput::Blob(b"emergency".to_vec())], 19).unwrap();
        let plain = zstd::stream::decode_all(&built.pk[..]).unwrap();
        assert_eq!(plain, b"emergency");
    }

    #[test]
    fn log_spans_are_byte_preserved() {
        let span_bytes = b"{\"fake\":\"line\"}\n".to_vec();
        let built = build_segment(
            &[SegmentInput::LogSpan {
                fork: "main".to_string(),
                bytes: span_bytes.clone(),
                line_ids: vec!["id-1".to_string()],
            }],
            3,
        )
        .unwrap();
        let seg = Segment::open(&built.pk, &built.idx, &built.pk_sha3, &built.idx_sha3).unwrap();
        let out = seg
            .materialize(&seg.entries[0], &no_blob, &no_line)
            .unwrap();
        assert_eq!(out, span_bytes, "exact bytes, never transcoded");
        assert_eq!(
            built.log_span,
            Some(("id-1".to_string(), "id-1".to_string()))
        );
    }
}
