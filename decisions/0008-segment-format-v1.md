# D0008: Segment format v1

**Status:** accepted · 2026-09-23
**Rule:** Segment format v1 is CRC32C-framed, record-length-prefixed and additive-only. Its
type ids, encoding ids and index kinds are fixed numbers, and a reader rejects an unknown type
or encoding and skips an unknown index kind.

## Why

- SPEC §5's format rule: on-disk formats are additive only, and a version bump is one-way and
  recorded in a decision.
- §18's three hooks (answered in #12). None of the hooks can be added after v1 without a
  version bump.
- Readers ignore trailing bytes inside a record, which is how later fields stay additive.

## Representation choices

- CRC32C, not IEEE, because it detects errors better and has hardware support. Hand-rolled,
  for D0004.
- xxh64 hand-rolled for blooms, because `std::hash` is not stable.
- Stats are bounds: long string mins are truncated and long maxes dropped.
- Null placeholders are kept in the value stream, so decoding is exact.
- Selection samples four windows of 1024 rows, and ties go to the lower id.
- No engine metadata, time or ids appear in a segment, so the same input gives byte-identical
  output.
- `.idx` is bound to its segment by the segment's footer CRC.
- Q11 is not answered: structures are built only when requested.

## Format tables

### Logical type ids (never derived from enum order; 0 is reserved)

| id | type | params |
|---|---|---|
| 1 | BOOL | — |
| 2 | INT64 | — |
| 3 | UINT64 | — |
| 4 | FLOAT64 | — |
| 5 | DECIMAL | u8 precision, u8 scale (validated by `DecimalType::new`) |
| 6 | STRING | — |
| 7 | BYTES | — |
| 8 | TIMESTAMP | — |
| 9 | DATE | — |
| 10 | UUID | — |
| 11 | IP | — |
| 12 | LIST | type_desc of the element (validated by `ListType::new`) |

### Encodings (ids are stable; an unknown or inapplicable id is `UnknownEncoding`)

| id | name | types | values payload |
|---|---|---|---|
| 0 | PLAIN | all | fixed-width LE per row (BOOL as Bitmap words, DECIMAL as i128, DATE as i32). STRING/BYTES: (rows+1) × u32 offsets, then `offsets[rows]` data bytes. LIST: (rows+1) × u32 offsets, then uvarint child_encoding_id, bytes child_chunk (a full chunk for the element column; its row count is `offsets[rows]`) |
| 1 | DICT | STRING, BYTES | uvarint ndict, ndict × bytes (distinct, first-occurrence order over **valid** rows), then FoR-packed codes, one per row (a null row gets code 0 and decodes as an empty range) |
| 2 | RLE | BOOL, INT64, UINT64, TIMESTAMP, DATE | uvarint nruns, nruns × (value, uvarint len), with runs summing to rows. The value is a u8 for BOOL, svarint for signed types, uvarint for UINT64 |
| 3 | FOR | INT64, UINT64, TIMESTAMP, DATE | map to an order-preserving u64 (signed: `(v as i64 as u64) ^ (1<<63)`), then a FoR block |
| 4 | DELTA | same as FOR | u64 first (mapped), then a FoR block of `zigzag(x[i].wrapping_sub(x[i-1]) as i64)` for i ≥ 1 |
| 5 | DELTA_OF_DELTA | same as FOR | u64 first, svarint first delta, then a FoR block of zigzag(delta[i] − delta[i−1]), all wrapping |
| 6 | XOR | FLOAT64 | Gorilla bitstream: 64 raw bits for the first value; then per value `0` = same, `10` = meaningful bits inside the previous window, `11` + 6-bit leading + 7-bit length (1..=64) + bits |

### Skip structures (blob formats; `params` are empty in v1)

| kind | name | applies to | blob |
|---|---|---|---|
| 1 | BLOOM | every type except LIST | u8 k (= 7), u8 log2_bits, 2^log2_bits/64 × u64 words. bits = next_pow2(max(512, 10·distinct)), capped at 2^20. The bit for probe i is `(h1 + i·h2) mod bits`, where h1 = xxh64(value_bytes, 0) and h2 = xxh64(value_bytes, 1) \| 1 |
| 2 | VALUE_SET | every type except LIST | uvarint n, n × value, sorted by `total_cmp`, distinct. Built only when there are at most `VALUE_SET_MAX` (256) distinct non-null values; otherwise nothing is written |
| 3 | NGRAM | STRING | a BLOOM blob over every byte trigram of every valid value (the value_bytes are the 3 raw bytes) |

### File layout: segment `<id>.seg`

```
HEADER   "ADLSEG" (6 bytes) + FORMAT_VERSION u16 (= 1)            8 bytes
BODY     for each row group: every column chunk in schema order, then that row group's
         inline index blobs
FOOTER   footer bytes
TRAILER  footer_len u32 · footer_crc32c u32 · "ADLSEG" · FORMAT_VERSION u16   16 bytes
```

### File layout: index `<id>.idx` (rebuildable; built after the segment)

```
HEADER   "ADLIDX" + FORMAT_VERSION u16 + u32 segment_footer_crc32c    (binds it to one segment)
BODY     index blobs
FOOTER   record(index_directory)                                      (same codec as above)
TRAILER  footer_len u32 · footer_crc32c u32 · "ADLIDX" · FORMAT_VERSION u16
```

## Evidence

- adelie-44e, SPEC §5 and §18, `src/format/`.
