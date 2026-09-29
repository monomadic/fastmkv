//! Two kinds of failure, kept apart on purpose (PROPOSAL-2 §6).
//!
//! A `Refused` is decided before anything is written: the file is exactly as
//! it was. Every rule in the supported-file boundary (§2) ends here.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Refused(Refusal),
    /// Applying a plan failed part-way. The path may be partially modified
    /// (PROPOSAL-2 §6): apply to a copy, never to the only original.
    WriteFailed(std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    NotMatroska,
    DocType,
    Version,
    UnknownSize,
    MultipleSegments,
    TrailingData,
    SegmentChecksum,
    SeekHead,
    Malformed,
    TooLarge,
    /// A checksum on an element about to be modified does not match it.
    Checksum,
    /// The name is held by a binary value, which a string edit must not guess at.
    BinaryValue,
    /// The edit would delete a node that holds nested tags or children
    /// this crate has no model of. That is a decision for the tree API.
    HasChildren,
    /// The edit cannot be made where the file is without moving something
    /// to the end that must stay at the front. `Mkv::reseat` can make it.
    NeedsReseat,
    /// The file holds a position this crate does not know how to correct
    /// when the clusters move.
    Position,
    /// No padding to take what has to grow.
    NoRoom,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: Kind,
    pub detail: String,
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn refuse<T>(kind: Kind, detail: impl Into<String>) -> Result<T> {
    Err(Error::Refused(Refusal {
        kind,
        detail: detail.into(),
    }))
}

impl Error {
    pub fn kind(&self) -> Option<Kind> {
        match self {
            Error::Refused(r) => Some(r.kind),
            Error::Io(_) | Error::WriteFailed(_) => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::WriteFailed(e) => write!(f, "write failed part-way: {e}"),
            Error::Refused(r) => write!(f, "{}", r.detail),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
