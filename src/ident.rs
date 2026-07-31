//! §1 Identity layer: element records, state sums, canonical JSON, record ids.
//!
//! Identity is computed over uncompressed canonical bytes, always, everywhere
//! (I1, the Wall).  Nothing in this module knows that encodings exist.

use std::fmt;

use sha3::{Digest, Sha3_256};

use crate::{Error, Result};

///////////////////////////////////////////// hashing /////////////////////////////////////////////

/// The lowercase-hex SHA3-256 of `bytes`.
pub fn sha3_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha3_256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex(&digest)
}

/// Render bytes as lowercase hex.
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// True iff `s` is exactly 64 lowercase hex characters.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/////////////////////////////////////////////// Sum ///////////////////////////////////////////////

/// State identity: a setsum over element records (§1.2).
///
/// 256 bits organized as eight little-endian u32 columns, each in its own
/// prime field.  Insert hashes the record with SHA3-256 and adds columnwise;
/// remove adds the columnwise inverse.  The empty state is all zeros.
///
/// The sum lives in the free abelian group; valid states do not.  Removing an
/// absent record accrues placeholder debt silently, which is why no remove is
/// ever applied without a membership check against a manifest (I9): the sum
/// attests; the manifest adjudicates.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Sum(setsum::Setsum);

impl Sum {
    /// The identity of the empty state: all zeros.
    pub fn zero() -> Self {
        Sum::default()
    }

    /// Insert one item's canonical bytes.
    pub fn insert(&mut self, item: &[u8]) {
        self.0.insert(item);
    }

    /// Remove one item's canonical bytes.  The caller is responsible for the
    /// membership check (I9); this is pure arithmetic.
    pub fn remove(&mut self, item: &[u8]) {
        self.0.remove(item);
    }

    /// The 64-hex-character rendering: eight columns, little-endian,
    /// concatenated.
    pub fn hexdigest(&self) -> String {
        self.0.hexdigest()
    }

    /// Parse the 64-hex-character rendering.
    pub fn from_hexdigest(s: &str) -> Result<Self> {
        if !is_hex64(s) {
            return Err(Error::Corrupt(format!("not a 64-hex sum: {s:?}")));
        }
        setsum::Setsum::from_hexdigest(s)
            .map(Sum)
            .ok_or_else(|| Error::Corrupt(format!("unparseable sum: {s:?}")))
    }
}

impl std::ops::Add for Sum {
    type Output = Sum;

    fn add(self, rhs: Sum) -> Sum {
        Sum(self.0 + rhs.0)
    }
}

impl std::ops::Sub for Sum {
    type Output = Sum;

    fn sub(self, rhs: Sum) -> Sum {
        Sum(self.0 - rhs.0)
    }
}

impl fmt::Debug for Sum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hexdigest())
    }
}

impl fmt::Display for Sum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hexdigest())
    }
}

////////////////////////////////////////// ElementRecord //////////////////////////////////////////

/// An element is a file, fully qualified (§1.1).  Its record is the canonical
/// bytes `mode <TAB> path <TAB> lowercase-hex(sha3-256(blob)) <LF>`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ElementRecord {
    /// Octal ASCII mode: `100644`, `100755`, or `120000`.
    pub mode: String,
    /// Absolute from the repository root, beginning with `/`.
    pub path: String,
    /// Lowercase hex SHA3-256 of the blob.
    pub blob: String,
}

/// Modes an element may carry.
pub const MODES: &[&str] = &["100644", "100755", "120000"];

/// Validate a path per §1.1: absolute, UTF-8 (guaranteed by &str), no NUL,
/// no newline, no tab, and no `.` or `..` components.  NFC normalization is
/// the writer's obligation; it is not checked here.
pub fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/') {
        return Err(Error::Invalid(format!("path must begin with '/': {path:?}")));
    }
    if path.contains('\0') || path.contains('\n') || path.contains('\t') {
        return Err(Error::Invalid(format!(
            "path must not contain NUL, LF, or TAB: {path:?}"
        )));
    }
    for component in path.split('/') {
        if component == "." || component == ".." {
            return Err(Error::Invalid(format!(
                "path must not contain '.' or '..' components: {path:?}"
            )));
        }
    }
    Ok(())
}

/// Validate a mode per §1.1.
pub fn validate_mode(mode: &str) -> Result<()> {
    if MODES.contains(&mode) {
        Ok(())
    } else {
        Err(Error::Invalid(format!("mode must be one of {MODES:?}: {mode:?}")))
    }
}

impl ElementRecord {
    /// Construct a validated element record.
    pub fn new(mode: &str, path: &str, blob: &str) -> Result<Self> {
        validate_mode(mode)?;
        validate_path(path)?;
        if !is_hex64(blob) {
            return Err(Error::Invalid(format!("blob must be 64 lowercase hex: {blob:?}")));
        }
        Ok(ElementRecord {
            mode: mode.to_string(),
            path: path.to_string(),
            blob: blob.to_string(),
        })
    }

    /// The canonical record bytes, trailing newline included.
    pub fn to_bytes(&self) -> Vec<u8> {
        format!("{}\t{}\t{}\n", self.mode, self.path, self.blob).into_bytes()
    }

    /// The canonical record as a string sans trailing newline, as claims
    /// store their inputs (§2.7).
    pub fn to_line(&self) -> String {
        format!("{}\t{}\t{}", self.mode, self.path, self.blob)
    }

    /// Parse a record line; the trailing newline is optional.
    pub fn parse(line: &str) -> Result<Self> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = line.split('\t');
        let (mode, path, blob) = match (fields.next(), fields.next(), fields.next(), fields.next())
        {
            (Some(m), Some(p), Some(b), None) => (m, p, b),
            _ => {
                return Err(Error::Corrupt(format!(
                    "element record must be mode\\tpath\\tblob: {line:?}"
                )));
            }
        };
        ElementRecord::new(mode, path, blob)
    }
}

impl fmt::Display for ElementRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_line())
    }
}

/// Fold element records into a state sum, in any order (the order does not
/// matter; that sentence is the entire design).
pub fn sum_of_records<'a>(records: impl IntoIterator<Item = &'a ElementRecord>) -> Sum {
    let mut sum = Sum::zero();
    for record in records {
        sum.insert(&record.to_bytes());
    }
    sum
}

/////////////////////////////////////////// canonical JSON ////////////////////////////////////////

/// Canonical JSON (§1.3): UTF-8, object keys sorted bytewise, separators `,`
/// and `:` with no whitespace, no floats.  serde_json's default map is a
/// BTreeMap, so serializing a Value yields sorted keys; compact output has
/// the required separators; non-ASCII passes through unescaped.
pub fn canonical_json(value: &serde_json::Value) -> Result<String> {
    reject_floats(value)?;
    serde_json::to_string(value).map_err(Error::from)
}

fn reject_floats(value: &serde_json::Value) -> Result<()> {
    match value {
        serde_json::Value::Number(n) => {
            if n.is_f64() && n.as_i64().is_none() && n.as_u64().is_none() {
                return Err(Error::Invalid(format!("canonical JSON forbids floats: {n}")));
            }
            Ok(())
        }
        serde_json::Value::Array(a) => a.iter().try_for_each(reject_floats),
        serde_json::Value::Object(o) => o.values().try_for_each(reject_floats),
        _ => Ok(()),
    }
}

/// The id of an identified record (§1.3): the lowercase hex SHA3-256 of the
/// canonical JSON of the record with the `id` key absent.
pub fn record_id(value: &serde_json::Value) -> Result<String> {
    let mut value = value.clone();
    if let serde_json::Value::Object(map) = &mut value {
        map.remove("id");
    } else {
        return Err(Error::Invalid("identified records are JSON objects".to_string()));
    }
    Ok(sha3_hex(canonical_json(&value)?.as_bytes()))
}

/// Verify that a record's stored `id` matches its content (§1.3): strip
/// `id`, re-canonicalize, re-hash.
pub fn verify_record_id(value: &serde_json::Value) -> Result<()> {
    let claimed = value
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::Corrupt("record has no string id".to_string()))?;
    let actual = record_id(value)?;
    if claimed == actual {
        Ok(())
    } else {
        Err(Error::Corrupt(format!("id mismatch: claimed {claimed}, actual {actual}")))
    }
}

/////////////////////////////////////////////// tests /////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> ElementRecord {
        ElementRecord::new(
            "100644",
            "/src/main.rs",
            &sha3_hex(b"fn main() {}\n"),
        )
        .unwrap()
    }

    #[test]
    fn record_round_trips() {
        let r = record();
        let bytes = r.to_bytes();
        assert!(bytes.ends_with(b"\n"));
        let parsed = ElementRecord::parse(std::str::from_utf8(&bytes).unwrap()).unwrap();
        assert_eq!(r, parsed);
    }

    #[test]
    fn paths_are_validated() {
        assert!(validate_path("/ok/path").is_ok());
        assert!(validate_path("relative").is_err());
        assert!(validate_path("/has\ttab").is_err());
        assert!(validate_path("/has\nnewline").is_err());
        assert!(validate_path("/has/../dotdot").is_err());
        assert!(validate_path("/has/./dot").is_err());
    }

    #[test]
    fn modes_are_validated() {
        assert!(validate_mode("100644").is_ok());
        assert!(validate_mode("100755").is_ok());
        assert!(validate_mode("120000").is_ok());
        assert!(validate_mode("040000").is_err());
    }

    #[test]
    fn law_1_commutativity_and_associativity() {
        let a = record();
        let b = ElementRecord::new("100755", "/tools/apply", &sha3_hex(b"#!/bin/sh\n")).unwrap();
        let c = ElementRecord::new("120000", "/link", &sha3_hex(b"target")).unwrap();
        let fold = |order: &[&ElementRecord]| {
            let mut s = Sum::zero();
            for r in order {
                s.insert(&r.to_bytes());
            }
            s.hexdigest()
        };
        let abc = fold(&[&a, &b, &c]);
        assert_eq!(abc, fold(&[&c, &b, &a]));
        assert_eq!(abc, fold(&[&b, &a, &c]));
    }

    #[test]
    fn law_2_identity_and_inverses() {
        let r = record();
        assert_eq!(Sum::zero().hexdigest(), "0".repeat(64));
        let mut s = Sum::zero();
        s.insert(&r.to_bytes());
        s.remove(&r.to_bytes());
        assert_eq!(s, Sum::zero());
    }

    #[test]
    fn placeholder_debt_is_silent() {
        // The central soundness warning: removing an element that was never
        // present does not fail; a future insert silently consumes the debt.
        let r = record();
        let mut s = Sum::zero();
        s.remove(&r.to_bytes());
        assert_ne!(s, Sum::zero());
        s.insert(&r.to_bytes());
        assert_eq!(s, Sum::zero());
    }

    #[test]
    fn sum_matches_readme_python() {
        // Cross-checked against the eleven lines of Python in README §2.
        let mut s = Sum::zero();
        s.insert(b"100644\t/src/main.rs\tab12\n");
        // python3: sum_hex(state_of(b"100644\t/src/main.rs\tab12\n")) from
        // the README's eleven lines of Python produced this constant.
        assert_eq!(
            s.hexdigest(),
            "b1a65b56c64f592d7632225e56240ff6014b090c9679e879609c9ef001acf75e"
        );
    }

    #[test]
    fn sum_hex_round_trips() {
        let mut s = Sum::zero();
        s.insert(&record().to_bytes());
        let h = s.hexdigest();
        assert_eq!(Sum::from_hexdigest(&h).unwrap(), s);
    }

    #[test]
    fn canonical_json_sorts_and_compacts() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"b": 1, "a": {"z": [2, 3], "y": "é"}}"#).unwrap();
        assert_eq!(canonical_json(&v).unwrap(), r#"{"a":{"y":"é","z":[2,3]},"b":1}"#);
    }

    #[test]
    fn canonical_json_rejects_floats() {
        let v: serde_json::Value = serde_json::from_str(r#"{"x": 1.5}"#).unwrap();
        assert!(canonical_json(&v).is_err());
    }

    #[test]
    fn record_ids_verify() {
        let mut v: serde_json::Value = serde_json::from_str(r#"{"id":"","x":1}"#).unwrap();
        let id = record_id(&v).unwrap();
        // The id hashes the record with the id key absent.
        assert_eq!(id, sha3_hex(br#"{"x":1}"#));
        v["id"] = serde_json::Value::String(id);
        verify_record_id(&v).unwrap();
        v["x"] = serde_json::Value::from(2);
        assert!(verify_record_id(&v).is_err());
    }
}
