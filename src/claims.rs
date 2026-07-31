//! §2.7 Claims: attested executions.
//!
//! A claim is a fact about a state, not an assertion by an author — a cached
//! function evaluation keyed by its input elements.  Its validity transfers:
//! it holds at any state in which its input elements are unchanged.  Drift
//! makes it stale, never silently wrong, and staleness is arithmetic.

use serde::{Deserialize, Serialize};

use crate::ident::{ElementRecord, Sum, canonical_json, record_id, verify_record_id};
use crate::manifest::Manifest;
use crate::{Error, Result};

/// An attested execution, stored at `claims/<id>.json` as canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    /// Record id per §1.3.
    pub id: String,
    /// The state sum at which the command ran.
    pub at_sum: String,
    /// The command.
    pub cmd: String,
    /// The element records the command read, sans trailing LF.
    pub inputs: Vec<String>,
    /// The setsum of the input records; staleness at any state is the
    /// comparison of these inputs against that state's manifest.
    pub input_sum: String,
    /// The command's exit status.
    pub exit: i64,
    /// SHA3-256 of the transcript blob.
    pub transcript_sha3: String,
}

impl Claim {
    /// Build a claim, computing `input_sum` and `id` from the parts.
    pub fn new(
        at_sum: &Sum,
        cmd: &str,
        inputs: Vec<ElementRecord>,
        exit: i64,
        transcript_sha3: &str,
    ) -> Result<Self> {
        let mut input_sum = Sum::zero();
        for record in &inputs {
            input_sum.insert(&record.to_bytes());
        }
        let mut claim = Claim {
            id: String::new(),
            at_sum: at_sum.hexdigest(),
            cmd: cmd.to_string(),
            inputs: inputs.iter().map(|r| r.to_line()).collect(),
            input_sum: input_sum.hexdigest(),
            exit,
            transcript_sha3: transcript_sha3.to_string(),
        };
        claim.id = record_id(&serde_json::to_value(&claim)?)?;
        Ok(claim)
    }

    /// The canonical JSON bytes, as stored (byte-preserved per I4).
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(canonical_json(&serde_json::to_value(self)?)?.into_bytes())
    }

    /// Parse and verify a claim's id and input arithmetic.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(bytes)?;
        verify_record_id(&value)?;
        let claim: Claim = serde_json::from_value(value)?;
        let mut input_sum = Sum::zero();
        for record in claim.input_records()? {
            input_sum.insert(&record.to_bytes());
        }
        if input_sum.hexdigest() != claim.input_sum {
            return Err(Error::Corrupt(format!(
                "claim {} input_sum {} disagrees with fold of inputs {}",
                claim.id,
                claim.input_sum,
                input_sum.hexdigest()
            )));
        }
        Ok(claim)
    }

    /// The parsed input records.
    pub fn input_records(&self) -> Result<Vec<ElementRecord>> {
        self.inputs.iter().map(|line| ElementRecord::parse(line)).collect()
    }

    /// Whether this claim is stale at `state`: any input element changed or
    /// absent.  When an input changed, the claim is not violated; it is
    /// stale, and re-execution is CPU, not tokens.
    pub fn is_stale_at(&self, state: &Manifest) -> Result<bool> {
        for record in self.input_records()? {
            if !state.contains(&record) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ident::sha3_hex;

    fn rec(path: &str, content: &[u8]) -> ElementRecord {
        ElementRecord::new("100644", path, &sha3_hex(content)).unwrap()
    }

    #[test]
    fn round_trip_and_id_verification() {
        let inputs = vec![rec("/src/retry.rs", b"fn backoff() {}\n")];
        let claim =
            Claim::new(&Sum::zero(), "cargo test -p retry", inputs, 0, &sha3_hex(b"ok\n"))
                .unwrap();
        let bytes = claim.to_bytes().unwrap();
        let parsed = Claim::parse(&bytes).unwrap();
        assert_eq!(parsed, claim);

        // Tampering with the command breaks the id.
        let tampered =
            String::from_utf8(bytes).unwrap().replace("cargo test", "cargo bless");
        assert!(Claim::parse(tampered.as_bytes()).is_err());
    }

    #[test]
    fn staleness_is_arithmetic_on_inputs() {
        let input = rec("/src/retry.rs", b"v1");
        let claim = Claim::new(&Sum::zero(), "true", vec![input.clone()], 0, &sha3_hex(b""))
            .unwrap();

        let fresh = Manifest::from_records([input, rec("/unrelated", b"x")]).unwrap();
        assert!(!claim.is_stale_at(&fresh).unwrap());

        // The input drifted: stale, not violated.
        let drifted = Manifest::from_records([rec("/src/retry.rs", b"v2")]).unwrap();
        assert!(claim.is_stale_at(&drifted).unwrap());

        // The input vanished: stale.
        let gone = Manifest::new();
        assert!(claim.is_stale_at(&gone).unwrap());
    }

    #[test]
    fn corrupt_input_sum_is_rejected() {
        let claim =
            Claim::new(&Sum::zero(), "true", vec![rec("/a", b"a")], 0, &sha3_hex(b"")).unwrap();
        let mut value = serde_json::to_value(&claim).unwrap();
        value["input_sum"] = serde_json::Value::String("0".repeat(64));
        value["id"] = serde_json::Value::String(record_id(&value).unwrap());
        let bytes = canonical_json(&value).unwrap();
        assert!(Claim::parse(bytes.as_bytes()).is_err());
    }
}
