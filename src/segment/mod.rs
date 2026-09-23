//! Segment format v1 (SPEC §5, D0008): writer, reader, encodings, CRC framing, skip structures.
mod crc;
mod directory;
pub mod encode;
mod error;
mod footer;
mod hash;
mod idx;
pub mod index;
mod reader;
mod type_id;
mod value;
mod wire;
mod writer;

pub use encode::Encoding;
pub use error::Error;
pub use idx::{IdxReader, IdxWriter};
pub use index::{IndexKind, SkipIndex};
pub use reader::{ChunkMeta, IndexEntry, Reader, RowGroupMeta};
pub use writer::{Meta, Writer, WriterOptions};

pub(crate) const SEGMENT_MAGIC: [u8; 6] = *b"ADLSEG";
pub(crate) const IDX_MAGIC: [u8; 6] = *b"ADLIDX";
pub(crate) const FORMAT_VERSION: u16 = 1;
pub(crate) const HEADER_LEN: usize = 8;
pub(crate) const TRAILER_LEN: usize = 16;
pub const DEFAULT_ROW_GROUP_ROWS: usize = 65_536;
pub(crate) const MAX_DECODE_ROWS: usize = 1 << 22; // a claimed row count above this is Malformed
pub(crate) const STATS_MAX_BYTES: usize = 128;
pub(crate) const SAMPLE_ROWS: usize = 4096;
pub(crate) const VALUE_SET_MAX: usize = 256;
