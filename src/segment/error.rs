//! `Error` (the public error) and `DecodeError` (the internal, segment-less error
//! decoders return), per the segment format v1 (SPEC §5, D0008).

use std::fmt;

use super::IndexKind;

/// A segment- or index-file operation gone wrong. Every variant names the segment it
/// happened to, and the column too where one applies.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    BadMagic {
        segment: String,
    },
    UnsupportedVersion {
        segment: String,
        version: u16,
    },
    Truncated {
        segment: String,
    },
    CorruptFooter {
        segment: String,
    },
    CorruptChunk {
        segment: String,
        column: String,
        row_group: usize,
    },
    CorruptIndex {
        segment: String,
        column: String,
        kind: IndexKind,
    },
    UnknownTypeId {
        segment: String,
        column: String,
        id: u64,
    },
    UnknownEncoding {
        segment: String,
        column: String,
        id: u64,
    },
    Malformed {
        segment: String,
        column: Option<String>,
        detail: String,
    },
    IdxMismatch {
        segment: String,
    },
    /// Writer/reader misuse: schema mismatch, bad pin or index request, index out of range.
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::BadMagic { segment } => write!(f, "segment {segment}: bad magic"),
            Error::UnsupportedVersion { segment, version } => {
                write!(f, "segment {segment}: unsupported format version {version}")
            }
            Error::Truncated { segment } => write!(f, "segment {segment}: truncated"),
            Error::CorruptFooter { segment } => {
                write!(f, "segment {segment}: corrupt footer")
            }
            Error::CorruptChunk {
                segment,
                column,
                row_group,
            } => {
                write!(
                    f,
                    "segment {segment}: column {column}: corrupt chunk in row group {row_group}"
                )
            }
            Error::CorruptIndex {
                segment,
                column,
                kind,
            } => {
                write!(
                    f,
                    "segment {segment}: column {column}: corrupt {kind:?} index"
                )
            }
            Error::UnknownTypeId {
                segment,
                column,
                id,
            } => {
                write!(
                    f,
                    "segment {segment}: column {column}: unknown logical type id {id}"
                )
            }
            Error::UnknownEncoding {
                segment,
                column,
                id,
            } => {
                write!(
                    f,
                    "segment {segment}: column {column}: unknown encoding id {id}"
                )
            }
            Error::Malformed {
                segment,
                column,
                detail,
            } => match column {
                Some(c) => write!(f, "segment {segment}: column {c}: {detail}"),
                None => write!(f, "segment {segment}: {detail}"),
            },
            Error::IdxMismatch { segment } => {
                write!(
                    f,
                    "segment {segment}: index file does not match this segment"
                )
            }
            Error::Usage(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// A decode failure with no segment or column attached yet (decoders work below that level).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DecodeError {
    Truncated,
    Malformed(&'static str),
    UnknownTypeId(u64),
    UnknownEncoding(u64),
}

/// A `DecodeError` plus the column it happened in, when known (footer parsing knows it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    pub err: DecodeError,
    pub column: Option<String>,
}

impl From<DecodeError> for Located {
    fn from(err: DecodeError) -> Self {
        Located { err, column: None }
    }
}

impl DecodeError {
    /// Lifts to `Error`. Truncated inside a CRC-verified region is Malformed, not
    /// Truncated: the CRC already proved the bytes are exactly what was written.
    pub(crate) fn at(self, segment: &str, column: Option<&str>) -> Error {
        let segment = segment.to_string();
        match self {
            DecodeError::UnknownTypeId(id) => Error::UnknownTypeId {
                segment,
                column: column.unwrap_or("?").to_string(),
                id,
            },
            DecodeError::UnknownEncoding(id) => Error::UnknownEncoding {
                segment,
                column: column.unwrap_or("?").to_string(),
                id,
            },
            DecodeError::Truncated => Error::Malformed {
                segment,
                column: column.map(str::to_string),
                detail: "truncated".to_string(),
            },
            DecodeError::Malformed(detail) => Error::Malformed {
                segment,
                column: column.map(str::to_string),
                detail: detail.to_string(),
            },
        }
    }
}
