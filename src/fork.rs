//! §2.3 Fork files: an anchor plus a log.
//!
//! A fork is what git called a branch and what a harness calls a session;
//! tally does not distinguish.  The fork file is three lines; the current
//! state of a fork is the anchor manifest plus the replay of its log.

use crate::ident::{Sum, is_hex64};
use crate::{Error, Result};

/// The first line of every fork file.
pub const FORK_HEADER: &str = "tally-fork v0";

/// A fork file: `anchor` names a state sum, `manifest` names the anchor
/// manifest `anchors/<sum>.manifest`, whose own `sum` header must equal
/// `anchor`.  The empty repository is anchor all-zeros with an empty
/// manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkFile {
    /// The anchor state sum, 64 hex.
    pub anchor: String,
    /// The anchor manifest's sum, 64 hex; names `anchors/<sum>.manifest`.
    pub manifest: String,
}

impl ForkFile {
    /// A fork anchored at the given sum, whose manifest is that same state.
    pub fn at(anchor: &Sum) -> Self {
        let hex = anchor.hexdigest();
        ForkFile { anchor: hex.clone(), manifest: hex }
    }

    /// The empty repository's fork: anchor all-zeros, empty manifest.
    pub fn empty() -> Self {
        ForkFile::at(&Sum::zero())
    }

    /// Serialize the three lines.
    pub fn to_bytes(&self) -> Vec<u8> {
        format!("{FORK_HEADER}\nanchor {}\nmanifest {}\n", self.anchor, self.manifest).into_bytes()
    }

    /// Parse and validate a fork file.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| Error::Corrupt("fork file is not UTF-8".to_string()))?;
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        if header != FORK_HEADER {
            return Err(Error::Corrupt(format!("bad fork header: {header:?}")));
        }
        let anchor = lines
            .next()
            .and_then(|l| l.strip_prefix("anchor "))
            .ok_or_else(|| Error::Corrupt("fork file missing anchor line".to_string()))?;
        let manifest = lines
            .next()
            .and_then(|l| l.strip_prefix("manifest "))
            .ok_or_else(|| Error::Corrupt("fork file missing manifest line".to_string()))?;
        if !is_hex64(anchor) || !is_hex64(manifest) {
            return Err(Error::Corrupt("fork anchor/manifest must be 64 hex".to_string()));
        }
        Ok(ForkFile { anchor: anchor.to_string(), manifest: manifest.to_string() })
    }
}

/// Validate a fork name: it becomes a directory under `forks/`, so it must
/// be a single, safe path component.
pub fn validate_fork_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\0')
        || name.contains('\n')
    {
        return Err(Error::Invalid(format!("bad fork name: {name:?}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let f = ForkFile::empty();
        assert_eq!(f.anchor, "0".repeat(64));
        let parsed = ForkFile::parse(&f.to_bytes()).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn rejects_garbage() {
        assert!(ForkFile::parse(b"not a fork\n").is_err());
        assert!(ForkFile::parse(b"tally-fork v0\nanchor xyz\nmanifest xyz\n").is_err());
    }

    #[test]
    fn fork_names() {
        assert!(validate_fork_name("main").is_ok());
        assert!(validate_fork_name("session-2026-01-01").is_ok());
        assert!(validate_fork_name("a/b").is_err());
        assert!(validate_fork_name("..").is_err());
        assert!(validate_fork_name("").is_err());
    }
}
