//! `.idx` companion files (SPEC §5, D0008): skip-index blobs rebuilt for an existing segment
//! and bound to its footer CRC, so a reader opening the wrong pairing gets `IdxMismatch`.

use crate::exec::Field;

use super::crc::crc32c;
use super::directory::{RawIndexEntry, decode_directory, encode_directory};
use super::error::Error;
use super::footer::{read_trailer, write_trailer};
use super::index::{self, IndexKind, SkipIndex};
use super::reader::{IndexEntry, Reader, check_header, load_entry, resolve_entries, trailer_err};
use super::wire::{Cursor, Sink};
use super::{FORMAT_VERSION, IDX_MAGIC};

const IDX_HEADER_LEN: usize = 12;

pub struct IdxWriter;

impl IdxWriter {
    /// Builds `requests` in order, and for each request every row group in order
    /// (`read_column` then `index::build`). Deterministic: the same segment and requests
    /// always give the same bytes.
    pub fn build<B: AsRef<[u8]>>(
        seg: &Reader<B>,
        requests: &[(usize, IndexKind)],
    ) -> Result<Vec<u8>, Error> {
        for &(column, kind) in requests {
            let field = seg
                .fields()
                .get(column)
                .ok_or_else(|| Error::Usage(format!("index names unknown column {column}")))?;
            if !kind.applies_to(&field.ty) {
                return Err(Error::Usage(format!(
                    "index kind {kind:?} does not apply to column {}",
                    field.name
                )));
            }
        }

        let mut out = vec![0u8; IDX_HEADER_LEN];
        out[..6].copy_from_slice(&IDX_MAGIC);
        out[6..8].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&seg.footer_crc().to_le_bytes());
        let mut pos = out.len() as u64;

        let mut entries = Vec::new();
        for &(column, kind) in requests {
            for row_group in 0..seg.row_groups().len() {
                let col = seg.read_column(row_group, column)?;
                let Some(blob) = index::build(kind, &col) else {
                    continue;
                };
                let crc = crc32c(&blob);
                let offset = pos;
                out.extend_from_slice(&blob);
                pos += blob.len() as u64;
                entries.push(RawIndexEntry {
                    kind: kind.id(),
                    row_group: Some(row_group),
                    columns: vec![column],
                    offset,
                    len: blob.len() as u64,
                    crc,
                    params: Vec::new(),
                });
            }
        }

        let mut footer = Sink::new();
        footer.record(|r| encode_directory(&entries, r));
        write_trailer(&mut out, IDX_MAGIC, &footer.into_vec());
        Ok(out)
    }
}

/// An open, validated `.idx` file, bound to the `Reader` it was opened against.
pub struct IdxReader<B: AsRef<[u8]>> {
    name: String,
    bytes: B,
    fields: Vec<Field>,
    indexes: Vec<IndexEntry>,
}

impl<B: AsRef<[u8]>> IdxReader<B> {
    /// `IdxMismatch` if the bound CRC in the header no longer matches `seg.footer_crc()`.
    pub fn open<S: AsRef<[u8]>>(
        name: impl Into<String>,
        bytes: B,
        seg: &Reader<S>,
    ) -> Result<Self, Error> {
        let name = name.into();
        let buf = bytes.as_ref();
        let footer_range =
            read_trailer(buf, IDX_MAGIC, IDX_HEADER_LEN).map_err(|e| trailer_err(e, &name))?;
        check_header(buf, IDX_MAGIC, &name)?;

        let bound_crc = u32::from_le_bytes(buf[8..12].try_into().unwrap());
        if bound_crc != seg.footer_crc() {
            return Err(Error::IdxMismatch { segment: name });
        }

        let footer_start = footer_range.start as u64;
        let mut cur = Cursor::new(&buf[footer_range]);
        let mut body = cur.record().map_err(|e| e.at(&name, None))?;
        let raw = decode_directory(&mut body).map_err(|e| e.at(&name, None))?;
        let indexes = resolve_entries(
            raw,
            seg.fields(),
            seg.row_groups().len(),
            IDX_HEADER_LEN as u64..footer_start,
            &name,
        )?;

        Ok(IdxReader {
            name,
            bytes,
            fields: seg.fields().to_vec(),
            indexes,
        })
    }

    pub fn indexes(&self) -> &[IndexEntry] {
        &self.indexes
    }

    pub fn load_index(&self, entry: &IndexEntry) -> Result<SkipIndex, Error> {
        load_entry(self.bytes.as_ref(), entry, &self.fields, &self.name)
    }
}
