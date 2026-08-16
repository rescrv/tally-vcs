//! §2.5 The log: the primary artifact.
//!
//! One applied patch per line, canonical JSON plus a trailing LF.  The chain
//! orders the narrative; the arithmetic never needed it.  `sum_after` is
//! deliberately redundant: it makes every log prefix independently
//! verifiable and corruption bisectable.

use serde::{Deserialize, Serialize};

use crate::blobs::BlobStore;
use crate::ident::{Sum, canonical_json, record_id};
use crate::patch::{Intent, RealizedEntry, apply_realized_to_sum};
use crate::{Error, Result};

/// A log line MUST be under this many bytes (§5); `reads` past the limit
/// spills whole into a blob.  The one parameter that is format, not policy.
pub const SPILL_THRESHOLD: usize = 65536;

/// Who produced a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    /// An instrumented author; reads are observed, not inferred.
    #[default]
    Agent,
    /// The emergency cord (ANDON §9): degraded provenance is marked,
    /// never faked.
    Andon,
    /// Landed by union from another fork; `origin` names the source line.
    Union,
    /// A fuse (§2.6): an interpretation appended to the log.  Carries a
    /// `fuse` span and an empty realized delta — an arithmetic identity.
    Fuse,
}

/// A fuse: one interval of the log under a named interpretation (§2.6).
/// The span is by line id, inclusive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fuse {
    /// The fuse's name: the handle a view filters to.
    pub name: String,
    /// First line id of the span.
    pub from: String,
    /// Last line id of the span, inclusive.
    pub to: String,
}

/// For `provenance: union`, the source line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    /// The fork the line came from.
    pub fork: String,
    /// The id of the line on that fork.
    pub id: String,
}

/// For lines derived from a git commit (§2.4 import): the machine-readable
/// derivation facts, so `prose` can carry the commit message verbatim
/// instead of a provenance string fused to a subject.  Anyone reading the
/// log can re-derive the imported state from these facts alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitImport {
    /// The git object hash algorithm (`sha1` or `sha256`).
    pub algorithm: String,
    /// The source commit's object name.
    pub commit: String,
    /// The commit's tree object name: the import is a pure function of it.
    pub tree: String,
    /// The ref as the user passed it, when a line names one (the anchor
    /// provenance line of a single-commit import).  Per-commit lines of a
    /// linear import derive from commits directly and carry no ref.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// The annotation: the exhaust the harness stops throwing away.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Annotation {
    /// Who wrote the patch.
    pub author: String,
    /// How the patch entered the substrate.
    pub provenance: Provenance,
    /// Andon lines: why the cord was pulled.  MUST be non-empty for andon.
    pub reason: Option<String>,
    /// Andon lines: a detached signature blob hash.  MUST be present for
    /// andon; the scheme is deliberately under-specified in v0.
    pub sig: Option<String>,
    /// The harness session, if any.
    pub session: Option<String>,
    /// Narrative for humans.
    pub prose: Option<String>,
    /// The observed read set: what entered the author's context, including
    /// universally quantified negatives.  Either an array of read records or
    /// `{"reads_blob": "<hex>"}` after spilling.  Absent for andon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reads: Option<serde_json::Value>,
    /// For union lines, the source line.
    pub origin: Option<Origin>,
    /// For lines derived from a git commit, the derivation facts.  When
    /// present, `prose` carries the commit message verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import: Option<GitImport>,
    /// For fuse lines, the named span (§2.6).  A line carrying a fuse MUST
    /// have an empty realized delta: a fuse is an interpretation, never a
    /// mutation.  Union re-keys `from`/`to` when the line lands elsewhere;
    /// the name travels untouched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuse: Option<Fuse>,
}

/// One applied patch, as the log records it: intent plus realization plus
/// the annotation that carries the whole reason tally exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// SHA3-256 of the canonical JSON with `id` absent (§1.3).
    pub id: String,
    /// The id of the preceding line; `""` on the first line.
    pub prev: String,
    /// What the author meant: commutes and travels.
    pub intent: Intent,
    /// What the application did: a fact about one application.
    pub realized: Vec<RealizedEntry>,
    /// The state sum after this line; redundant on purpose.
    pub sum_after: String,
    /// Commit time: milliseconds since the Unix epoch, stamped when the
    /// line is sealed.  Time is annotation, never arithmetic: nothing
    /// orders or verifies by it (the chain orders; the sums verify), but a
    /// narrative without a clock is a poorer narrative.
    #[serde(default)]
    pub committed_ms: u64,
    /// The exhaust.
    pub annotation: Annotation,
}

impl LogLine {
    /// Validate provenance-dependent requirements (§2.5).
    pub fn validate(&self) -> Result<()> {
        if self.annotation.fuse.is_some() && !self.realized.is_empty() {
            return Err(Error::Invalid(
                "fuse lines carry an empty realized delta: a fuse is an interpretation, \
                 never a mutation"
                    .to_string(),
            ));
        }
        match self.annotation.provenance {
            Provenance::Agent => Ok(()),
            Provenance::Fuse => {
                let Some(fuse) = &self.annotation.fuse else {
                    return Err(Error::Invalid("fuse lines require a fuse span".to_string()));
                };
                if fuse.name.is_empty() {
                    return Err(Error::Invalid(
                        "fuse lines require a non-empty name".to_string(),
                    ));
                }
                if !self.intent.ops.is_empty() {
                    return Err(Error::Invalid("fuse lines carry no intent ops".to_string()));
                }
                Ok(())
            }
            Provenance::Andon => {
                if self.annotation.reason.as_deref().unwrap_or("").is_empty() {
                    return Err(Error::Invalid(
                        "andon lines require a non-empty reason".to_string(),
                    ));
                }
                if self.annotation.sig.is_none() {
                    return Err(Error::Invalid("andon lines require sig".to_string()));
                }
                Ok(())
            }
            Provenance::Union => {
                if self.annotation.origin.is_none() {
                    return Err(Error::Invalid(
                        "union lines require origin naming the source line".to_string(),
                    ));
                }
                Ok(())
            }
        }
    }

    /// Compute this line's id (§1.3): canonical JSON with `id` absent.
    pub fn compute_id(&self) -> Result<String> {
        record_id(&serde_json::to_value(self)?)
    }

    /// Seal the line: stamp the commit time, apply the spill rule, then
    /// compute and set `id`.  Returns the canonical bytes to append,
    /// trailing LF included.
    pub fn seal(&mut self, blobs: &BlobStore) -> Result<Vec<u8>> {
        self.validate()?;
        if self.committed_ms == 0 {
            self.committed_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
        }
        self.spill_reads_if_needed(blobs)?;
        self.id = self.compute_id()?;
        let mut bytes = canonical_json(&serde_json::to_value(&*self)?)?.into_bytes();
        bytes.push(b'\n');
        if bytes.len() > SPILL_THRESHOLD {
            return Err(Error::Invalid(format!(
                "log line is {} bytes even after spilling reads; the limit is {SPILL_THRESHOLD}",
                bytes.len()
            )));
        }
        Ok(bytes)
    }

    /// The spill rule: if the line would exceed the threshold and `reads` is
    /// an array, the array moves whole into a blob and the field becomes
    /// `{"reads_blob": "<hex>"}`.  The log stays line-scannable; the exhaust
    /// stays kept.
    fn spill_reads_if_needed(&mut self, blobs: &BlobStore) -> Result<()> {
        let approx = canonical_json(&serde_json::to_value(&*self)?)?.len() + 1;
        if approx <= SPILL_THRESHOLD {
            return Ok(());
        }
        let Some(reads) = &self.annotation.reads else {
            return Ok(());
        };
        if !reads.is_array() {
            return Ok(());
        }
        let spilled = canonical_json(reads)?;
        let hash = blobs.put(spilled.as_bytes())?;
        self.annotation.reads = Some(serde_json::json!({ "reads_blob": hash }));
        Ok(())
    }

    /// Parse one line and verify its id.  Byte preservation (I4) means the
    /// stored bytes are authoritative; the id must re-verify against them.
    pub fn parse(line: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(line)?;
        crate::ident::verify_record_id(&value)?;
        let parsed: LogLine = serde_json::from_value(value)?;
        parsed.validate()?;
        Ok(parsed)
    }
}

/// The result of leniently parsing a log: verified lines plus the byte
/// length of the valid prefix.  A torn final line (invalid JSON, or id fails
/// re-verification) is not an error; it is truncated as never-committed
/// (§2.7 crash recovery).
pub struct ParsedLog {
    /// The verified lines.
    pub lines: Vec<LogLine>,
    /// Bytes of valid prefix; anything past this is torn.
    pub valid_prefix: usize,
}

/// Parse `log.jsonl` bytes, stopping at the first torn line.  Leniency
/// covers exactly one shape of damage: a torn *tail*, the residue of a
/// crash mid-append.  If any line past the sheer point parses as a valid
/// log line, the damage is in the middle of the log — that is corruption,
/// never truncated, always an error.
pub fn parse_log_lenient(bytes: &[u8]) -> Result<ParsedLog> {
    let mut lines = Vec::new();
    let mut valid_prefix = 0;
    let mut offset = 0;
    while offset < bytes.len() {
        let end = match bytes[offset..].iter().position(|&b| b == b'\n') {
            Some(i) => offset + i + 1,
            None => break, // no trailing LF: torn
        };
        let Ok(text) = std::str::from_utf8(&bytes[offset..end - 1]) else {
            break;
        };
        match LogLine::parse(text) {
            Ok(line) => lines.push(line),
            Err(_) => break,
        }
        valid_prefix = end;
        offset = end;
    }
    // Verify there are no valid lines after the sheered line.
    let torn = &bytes[valid_prefix..];
    for (i, chunk) in torn.split(|&b| b == b'\n').enumerate().skip(1) {
        let Ok(text) = std::str::from_utf8(chunk) else {
            continue;
        };
        if LogLine::parse(text).is_ok() {
            return Err(Error::Corrupt(format!(
                "valid log line follows a torn line {i} past byte {valid_prefix}: \
                 corruption mid-log, not a torn tail"
            )));
        }
    }
    Ok(ParsedLog {
        lines,
        valid_prefix,
    })
}

/// Strictly parse a log: any torn or trailing garbage is corruption.
pub fn parse_log_strict(bytes: &[u8]) -> Result<Vec<LogLine>> {
    let parsed = parse_log_lenient(bytes)?;
    if parsed.valid_prefix != bytes.len() {
        return Err(Error::Corrupt(format!(
            "log has {} bytes of garbage after byte {}",
            bytes.len() - parsed.valid_prefix,
            parsed.valid_prefix
        )));
    }
    Ok(parsed.lines)
}

/// The last position in `lines` whose `sum_after` names the state
/// `sum_hex`, if any.  Anchors and replay starts are found this way
/// everywhere: snapshots repoint anchors at states mid-log, and a state
/// can recur (apply, then undo), so the *last* occurrence is the one that
/// the chain's suffix extends.
pub fn last_state_position(lines: &[LogLine], sum_hex: &str) -> Option<usize> {
    lines.iter().rposition(|l| l.sum_after == sum_hex)
}

/// Verify a chain: prev linkage from `""`, and each line's arithmetic
/// against its own `sum_after`, starting from the anchor.  Returns the final
/// sum.  A line whose arithmetic disagrees with its `sum_after` marks where
/// history stops being trustworthy.
pub fn verify_chain(anchor: &Sum, lines: &[LogLine]) -> Result<Sum> {
    let mut sum = anchor.clone();
    let mut prev = String::new();
    for (i, line) in lines.iter().enumerate() {
        if line.prev != prev {
            return Err(Error::Corrupt(format!(
                "line {i} ({}) has prev {}, expected {prev:?}",
                line.id, line.prev
            )));
        }
        sum = apply_realized_to_sum(&sum, &line.realized)?;
        if sum.hexdigest() != line.sum_after {
            return Err(Error::Corrupt(format!(
                "line {i} ({}) claims sum_after {} but arithmetic yields {}; \
                 history stops being trustworthy here",
                line.id,
                line.sum_after,
                sum.hexdigest()
            )));
        }
        prev = line.id.clone();
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::{ElementRecord, sha3_hex};

    fn blobs(name: &str) -> BlobStore {
        let dir = std::env::temp_dir().join(format!("tally-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        BlobStore::init(dir).unwrap()
    }

    fn line_adding(path: &str, content: &[u8], prev: &str, sum: &mut Sum) -> LogLine {
        let record = ElementRecord::new("100644", path, &sha3_hex(content)).unwrap();
        sum.insert(&record.to_bytes());
        LogLine {
            id: String::new(),
            prev: prev.to_string(),
            intent: Intent::default(),
            realized: vec![RealizedEntry {
                remove: None,
                add: Some(record.to_line()),
            }],
            sum_after: sum.hexdigest(),
            committed_ms: 0,
            annotation: Annotation {
                author: "test".to_string(),
                ..Annotation::default()
            },
        }
    }

    #[test]
    fn seal_parse_verify_round_trip() {
        let store = blobs("roundtrip");
        let mut sum = Sum::zero();
        let mut line = line_adding("/a", b"a", "", &mut sum);
        let bytes = line.seal(&store).unwrap();
        assert!(bytes.ends_with(b"\n"));
        assert!(line.committed_ms > 0, "seal stamps the commit time");
        let parsed =
            LogLine::parse(std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap()).unwrap();
        assert_eq!(parsed, line);
        assert_eq!(parsed.committed_ms, line.committed_ms);
        let final_sum = verify_chain(&Sum::zero(), &[parsed]).unwrap();
        assert_eq!(final_sum, sum);
    }

    #[test]
    fn chain_linkage_and_arithmetic_are_verified() {
        let store = blobs("chain");
        let mut sum = Sum::zero();
        let mut first = line_adding("/a", b"a", "", &mut sum);
        first.seal(&store).unwrap();
        let mut second = line_adding("/b", b"b", &first.id, &mut sum);
        second.seal(&store).unwrap();
        verify_chain(&Sum::zero(), &[first.clone(), second.clone()]).unwrap();

        // Broken linkage.
        let mut bad = second.clone();
        bad.prev = "wrong".to_string();
        assert!(verify_chain(&Sum::zero(), &[first.clone(), bad]).is_err());

        // Broken arithmetic.
        let mut bad = second;
        bad.sum_after = "0".repeat(64);
        assert!(verify_chain(&Sum::zero(), &[first, bad]).is_err());
    }

    #[test]
    fn torn_tail_is_truncated_not_fatal() {
        let store = blobs("torn");
        let mut sum = Sum::zero();
        let mut line = line_adding("/a", b"a", "", &mut sum);
        let mut bytes = line.seal(&store).unwrap();
        let good_len = bytes.len();
        bytes.extend_from_slice(b"{\"id\":\"torn");
        let parsed = parse_log_lenient(&bytes).unwrap();
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.valid_prefix, good_len);
        assert!(parse_log_strict(&bytes).is_err());
    }

    #[test]
    fn corruption_mid_log_is_an_error_not_a_truncation() {
        let store = blobs("midlog");
        let mut sum = Sum::zero();
        let mut first = line_adding("/a", b"a", "", &mut sum);
        let mut bytes = first.seal(&store).unwrap();
        let mut second = line_adding("/b", b"b", &first.id, &mut sum);
        let second_bytes = second.seal(&store).unwrap();
        // Corrupt the middle: a torn fragment with a valid line after it.
        bytes.extend_from_slice(b"{\"id\":\"torn\n");
        bytes.extend_from_slice(&second_bytes);
        assert!(
            matches!(parse_log_lenient(&bytes), Err(Error::Corrupt(_))),
            "a valid line after the sheer point is corruption, not a torn tail"
        );
    }

    #[test]
    fn tampered_id_is_rejected() {
        let store = blobs("tamper");
        let mut sum = Sum::zero();
        let mut line = line_adding("/a", b"a", "", &mut sum);
        let bytes = line.seal(&store).unwrap();
        let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).unwrap();
        let tampered = text.replace("\"author\":\"test\"", "\"author\":\"evil\"");
        assert!(LogLine::parse(&tampered).is_err());
    }

    #[test]
    fn andon_and_union_provenance_rules() {
        let store = blobs("provenance");
        let mut sum = Sum::zero();
        let mut line = line_adding("/a", b"a", "", &mut sum);
        line.annotation.provenance = Provenance::Andon;
        assert!(
            line.clone().seal(&store).is_err(),
            "andon requires reason and sig"
        );
        line.annotation.reason = Some("CVE-2026-0001".to_string());
        line.annotation.sig = Some("00".repeat(32));
        line.seal(&store).unwrap();

        let mut line = line_adding("/b", b"b", "", &mut sum);
        line.annotation.provenance = Provenance::Union;
        assert!(line.clone().seal(&store).is_err(), "union requires origin");
        line.annotation.origin = Some(Origin {
            fork: "session-1".to_string(),
            id: "ab".repeat(32),
        });
        line.seal(&store).unwrap();
    }

    #[test]
    fn oversized_reads_spill_to_a_blob() {
        let store = blobs("spill");
        let mut sum = Sum::zero();
        let mut line = line_adding("/a", b"a", "", &mut sum);
        let big: Vec<serde_json::Value> = (0..4096)
            .map(|i| serde_json::json!({"path": format!("/file-{i}"), "blob": "ab".repeat(32)}))
            .collect();
        let reads = serde_json::Value::Array(big);
        line.annotation.reads = Some(reads.clone());
        let bytes = line.seal(&store).unwrap();
        assert!(bytes.len() < SPILL_THRESHOLD);
        let spilled = line.annotation.reads.as_ref().unwrap();
        let hash = spilled.get("reads_blob").and_then(|v| v.as_str()).unwrap();
        // Lossless: the blob holds the whole array.
        let recovered: serde_json::Value =
            serde_json::from_slice(&store.get(hash).unwrap()).unwrap();
        assert_eq!(recovered, reads);
    }
}
