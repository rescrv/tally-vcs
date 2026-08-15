//! tally: a version control substrate in which states are sums, patches
//! commute, and history is arithmetic.
//!
//! State identity forms an abelian group: patches are elements, composition
//! is addition, undo is the inverse, and order never matters.  The loose
//! format is the logical content and the definition of truth; the packed
//! format is an encoding of it; the wire format is the packed format.

use std::fmt;

use handled::SError;

pub mod b64;
pub mod blame;
pub mod blobs;
pub mod diff;
pub mod fork;
pub mod git;
pub mod ident;
pub mod ignore;
pub mod log;
pub mod manifest;
pub mod patch;
pub mod repo;
pub mod revision;
pub mod segment;
pub mod serve;
pub mod union;
pub mod views;
pub mod wire;

pub use ident::{ElementRecord, Sum, canonical_json, record_id, sha3_hex, verify_record_id};

/// Errors across the substrate.
///
/// `Corrupt`, `Invalid`, `Precondition`, and `NeedsReenactment` are the API
/// boundary: the substrate's own vocabulary, matched on by callers.  `Io`
/// and `Json` are incidental — foreign failures wrapped in transit — so
/// they carry a structured [`handled::SError`] instead of leaking
/// `std::io::Error` or `serde_json::Error` through the public type.
#[derive(Debug)]
pub enum Error {
    /// An I/O error, annotated with what was being attempted.
    Io(SError),
    /// Bytes that should have been well-formed were not: the repository (or
    /// a fetched artifact) is corrupt.
    Corrupt(String),
    /// A caller-supplied value violates the format.
    Invalid(String),
    /// A patch precondition did not hold; nothing was written.
    Precondition(String),
    /// A conflict that mechanical strata cannot resolve; re-enactment (union
    /// stratum 4) is required and costs tokens, so it is never automatic.
    NeedsReenactment(String),
    /// JSON (de)serialization failed.
    Json(SError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "io: {err}"),
            Error::Corrupt(what) => write!(f, "corrupt: {what}"),
            Error::Invalid(what) => write!(f, "invalid: {what}"),
            Error::Precondition(what) => write!(f, "precondition: {what}"),
            Error::NeedsReenactment(what) => write!(f, "needs re-enactment: {what}"),
            Error::Json(err) => write!(f, "json: {err}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(err: serde_json::Error) -> Self {
        Error::Json(
            SError::new("json")
                .with_atom_field("line", err.line())
                .with_atom_field("column", err.column())
                .with_message(&err.to_string()),
        )
    }
}

/// Annotate an io::Error with what was being attempted:
/// `.map_err(ioerr("writing the log"))`.
pub fn ioerr(what: impl fmt::Display) -> impl FnOnce(std::io::Error) -> Error {
    let what = what.to_string();
    move |err| {
        Error::Io(
            SError::new("io")
                .with_atom_field("kind", format_args!("{:?}", err.kind()))
                .with_message(&format!("{what}: {err}")),
        )
    }
}

/// Result alias for the substrate.
pub type Result<T> = std::result::Result<T, Error>;
