//! §2.4 Manifests: materialized states.
//!
//! A manifest is a header plus every element record, sorted bytewise.
//! Sorting serves humans and diff tools; the sum is order-blind.  Manifests
//! are compactions — derived, never primary — but they carry the one duty
//! arithmetic cannot: adjudicating membership (I9).  The sum attests; the
//! manifest adjudicates.

use std::collections::BTreeMap;

use crate::ident::{ElementRecord, Sum, sum_of_records};
use crate::{Error, Result};

/// The first line of every manifest.
pub const MANIFEST_HEADER: &str = "abelian-manifest v0";

/// A materialized state: elements keyed by path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    elements: BTreeMap<String, ElementRecord>,
}

impl Manifest {
    /// The empty state.
    pub fn new() -> Self {
        Manifest::default()
    }

    /// Build from records; duplicate paths are corrupt.
    pub fn from_records(records: impl IntoIterator<Item = ElementRecord>) -> Result<Self> {
        let mut manifest = Manifest::new();
        for record in records {
            if manifest.elements.insert(record.path.clone(), record.clone()).is_some() {
                return Err(Error::Corrupt(format!("duplicate path in manifest: {}", record.path)));
            }
        }
        Ok(manifest)
    }

    /// The element at `path`, if present.
    pub fn get(&self, path: &str) -> Option<&ElementRecord> {
        self.elements.get(path)
    }

    /// Membership check for an exact record: the adjudication I9 requires
    /// before any remove touches a sum.
    pub fn contains(&self, record: &ElementRecord) -> bool {
        self.elements.get(&record.path) == Some(record)
    }

    /// Insert an element; the path must be absent.
    pub fn insert(&mut self, record: ElementRecord) -> Result<()> {
        if self.elements.contains_key(&record.path) {
            return Err(Error::Precondition(format!("path already present: {}", record.path)));
        }
        self.elements.insert(record.path.clone(), record);
        Ok(())
    }

    /// Remove an exact record; membership is checked here, always (I9).
    pub fn remove(&mut self, record: &ElementRecord) -> Result<()> {
        if !self.contains(record) {
            return Err(Error::Precondition(format!(
                "record not present (placeholder debt refused): {}",
                record.to_line()
            )));
        }
        self.elements.remove(&record.path);
        Ok(())
    }

    /// Iterate records in bytewise path order.
    pub fn records(&self) -> impl Iterator<Item = &ElementRecord> {
        self.elements.values()
    }

    /// The number of elements.
    pub fn len(&self) -> usize {
        self.elements.len()
    }

    /// True iff the state is empty.
    pub fn is_empty(&self) -> bool {
        self.elements.is_empty()
    }

    /// Fold every record into a state sum.
    pub fn sum(&self) -> Sum {
        sum_of_records(self.records())
    }

    /// Serialize: header, sum line, records sorted bytewise.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = String::new();
        out.push_str(MANIFEST_HEADER);
        out.push('\n');
        out.push_str(&format!("sum {}\n", self.sum().hexdigest()));
        let mut lines: Vec<Vec<u8>> = self.records().map(|r| r.to_bytes()).collect();
        lines.sort();
        let mut bytes = out.into_bytes();
        for line in lines {
            bytes.extend_from_slice(&line);
        }
        bytes
    }

    /// Parse and verify: a manifest whose `sum` disagrees with the fold of
    /// its records is corrupt, full stop.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Corrupt("manifest is not UTF-8".to_string()))?;
        let mut lines = text.split_inclusive('\n');
        let header = lines.next().unwrap_or("");
        if header.strip_suffix('\n').unwrap_or(header) != MANIFEST_HEADER {
            return Err(Error::Corrupt(format!("bad manifest header: {header:?}")));
        }
        let sum_line = lines.next().unwrap_or("");
        let sum_line = sum_line.strip_suffix('\n').unwrap_or(sum_line);
        let claimed = sum_line
            .strip_prefix("sum ")
            .ok_or_else(|| Error::Corrupt(format!("bad manifest sum line: {sum_line:?}")))?;
        let claimed = Sum::from_hexdigest(claimed)?;
        let mut records = Vec::new();
        for line in lines {
            records.push(ElementRecord::parse(line)?);
        }
        let manifest = Manifest::from_records(records)?;
        let actual = manifest.sum();
        if actual != claimed {
            return Err(Error::Corrupt(format!(
                "manifest sum {} disagrees with fold of records {}",
                claimed.hexdigest(),
                actual.hexdigest()
            )));
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::sha3_hex;

    fn rec(mode: &str, path: &str, content: &[u8]) -> ElementRecord {
        ElementRecord::new(mode, path, &sha3_hex(content)).unwrap()
    }

    #[test]
    fn round_trip_and_sorting() {
        let m = Manifest::from_records([
            rec("100755", "/tools/apply", b"#!/bin/sh\n"),
            rec("100644", "/README.md", b"# hi\n"),
        ])
        .unwrap();
        let bytes = m.to_bytes();
        let text = String::from_utf8(bytes.clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], MANIFEST_HEADER);
        assert!(lines[1].starts_with("sum "));
        assert!(lines[2] < lines[3], "records must be sorted bytewise");
        assert_eq!(Manifest::parse(&bytes).unwrap(), m);
    }

    #[test]
    fn corrupt_sum_is_rejected() {
        let m = Manifest::from_records([rec("100644", "/a", b"a")]).unwrap();
        let mut bytes = m.to_bytes();
        // Flip a hex digit in the sum line.
        let idx = MANIFEST_HEADER.len() + 1 + 4;
        bytes[idx] = if bytes[idx] == b'0' { b'1' } else { b'0' };
        assert!(matches!(Manifest::parse(&bytes), Err(Error::Corrupt(_))));
    }

    #[test]
    fn membership_adjudicates_removes() {
        let r = rec("100644", "/a", b"a");
        let mut m = Manifest::from_records([r.clone()]).unwrap();
        let absent = rec("100644", "/b", b"b");
        assert!(m.remove(&absent).is_err(), "placeholder debt must be refused");
        let wrong_blob = rec("100644", "/a", b"different");
        assert!(m.remove(&wrong_blob).is_err());
        m.remove(&r).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn empty_manifest_sums_to_zero() {
        let m = Manifest::new();
        assert_eq!(m.sum().hexdigest(), "0".repeat(64));
        assert_eq!(Manifest::parse(&m.to_bytes()).unwrap(), m);
    }

    #[test]
    fn duplicate_paths_are_corrupt() {
        let a = rec("100644", "/a", b"1");
        let b = rec("100644", "/a", b"2");
        assert!(Manifest::from_records([a, b]).is_err());
    }
}
