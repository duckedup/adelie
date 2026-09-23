# D0009: Manifest format v1

**Status:** accepted · 2026-09-23
**Rule:** Manifest v1 is the store's one mutable object: CRC32C-framed like a segment,
record-length-prefixed and additive-only, published by temp, fsync, rename and fsync-dir. A
commit conflicts only per table, and appends never conflict.

## Why

- SPEC §5: the manifest and the format rule (additive-only, a version bump is one-way and
  recorded in a decision).
- SPEC §6: the durability order — a write is acked only after its segment and its manifest are
  both durable.
- SPEC §18 hook 3: the commit sequence, and that engine and side-file state live in the
  manifest, not in segments.
- The scope gate answers of 2026-09-23: record-only tombstones, minimal append-only compaction,
  per-table OCC, and a recorded-IO sequence as the fsync proof.

## Representation choices

- The manifest reuses the segment format's own trailer framing (`footer::write_trailer` /
  `read_trailer`): same CRC32C, same length-then-magic-then-version trailer shape, one codec to
  maintain.
- Every record is length-prefixed (`Sink::record` / `Cursor::record`), so a reader skips fields
  or whole records it doesn't recognise; this is what keeps the format additive without a
  version bump.
- `MANIFEST_VERSION` is asserted equal to the segment's `FORMAT_VERSION`, since the trailer
  writer hard-codes the segment's version into the trailer; the two only diverge on purpose.
- A garbage entry carries no column stats, since nothing but a GC needs a removed segment's
  id, partition, seq and rows.
- v1 defines no side-file kinds. A side file present on decode is `UnknownSideFile`, not
  skipped, because silently ignoring one could resurrect rows a deletion vector meant to hide.

## Format tables

### File layout

```
HEADER  "ADLMAN" (6 bytes) + MANIFEST_VERSION u16 (= 1)                     8 bytes
BODY    record(header_fields) · record(tables) · record(garbage)
TRAILER footer_len u32 · footer_crc32c u32 · "ADLMAN" · MANIFEST_VERSION u16
```

### `header_fields` record

| field | type |
|---|---|
| version | uvarint |
| next_segment_id | uvarint |

### `tables` record: uvarint count, then one record per `TableEntry`

| field | type |
|---|---|
| db | str |
| name | str |
| engine | str |
| schema | record (see below) |
| segments | record (see below) |
| tombstones | record (see below) |

`schema` record: uvarint count, then per field a record of `(str name, type_desc)`, using the
segment format's own `type_id::encode_type` / `decode_type`.

### `segments` record: uvarint count, then one record per `SegmentEntry`

| field | type |
|---|---|
| id | uvarint |
| partition | str |
| seq | uvarint |
| rows | uvarint |
| bytes | uvarint |
| footer_crc | u32 |
| columns | record (see below) |
| side_files | record (see below) |

`columns` record: uvarint count (must equal the table's schema length), then one record per
schema field of `(uvarint rows, uvarint null_count, value_opt min, value_opt max)`, using
`segment::value::encode_opt` / `decode_opt`. A LIST field always encodes both bounds absent.

`side_files` record: uvarint count, then one record per entry of `(uvarint kind, str path,
u32 crc)`. v1 defines no kinds: a decoder that sees any entry returns `UnknownSideFile` naming
the first one's kind, without decoding the rest.

### `tombstones` record: uvarint count, then one record per `Tombstone`

| field | type |
|---|---|
| seq | uvarint |
| predicates | uvarint count, then one record per `Predicate` |

`Predicate` record: `(str column, u8 CmpOp id, value)`, the value encoded by the column's type
from the table's schema (a predicate's column always resolves there; `Commit::apply` validated
it).

### `CmpOp` byte ids

| id | op |
|---|---|
| 0 | Eq |
| 1 | Ne |
| 2 | Lt |
| 3 | Le |
| 4 | Gt |
| 5 | Ge |

### Side-file kind registry

Empty in v1. Any kind value that appears in an encoded manifest is rejected on decode
(`Error::UnknownSideFile`); there is no forward-compatible way to ignore one, because a side
file no reader understands may be a deletion vector, and skipping it would resurrect rows.

### `garbage` record: uvarint count, then one record per `Garbage`

| field | type |
|---|---|
| table.db | str |
| table.name | str |
| removed_at_ms | uvarint |
| segment | record, `encode_segment` with an empty schema (no column stats) |

## The commit rules

- **`seq`.** A flush's `AddSegments` sets every new segment's `seq` to the commit's new version
  (`current.version + 1`). A compaction's sets it to the max `seq` among its input segments. A
  tombstone's `seq` is the commit's new version. `TableEntry::tombstones_for(seg)` returns the
  tombstones with `t.seq > seg.seq` — the exact contract E5 scans against.
- **The straddle rule.** A compaction plan is rejected (`Error::Usage`) if its input segments'
  seq range straddles a tombstone: `min(input seq) < t.seq <= max(input seq)`. Compaction never
  merges across a delete boundary.
- **Conflict table** (OCC is per table; `base` is recorded but not checked — only the edits
  below decide):

| edit | conflicts when |
|---|---|
| `CreateTable` | a table of that name already exists; `Usage` if `db` or `name` is not one path component (empty, `.`, `..`, or holding `/`, `\` or NUL) |
| `AddSegments` | never (appends never conflict); `Usage` if a segment's `columns` length doesn't match the table's schema length |
| `RemoveSegments` | any named id is not live in the current manifest |
| `AddTombstone` | never, once the table exists; `Usage` for a null value, an unknown column, or a value that does not fit the column's type. The value is stored coerced to that exact type (a DECIMAL rescaled to its scale), because the codec writes a DECIMAL's unscaled digits and reads them back at the column's scale |
| `ForgetGarbage` | never; missing ids are ignored |
| `ReserveSegmentIds` | never; only ever raises `next_segment_id` |

Every edit except `ForgetGarbage` and `ReserveSegmentIds` also conflicts if it names a table
that doesn't exist.

A decoder applies the same path-component rule to every `db`, `name` and `partition` it reads:
one that could escape the store root is `Corrupt`, never followed.

## The durability order

A write is acked only once both its segment and the manifest naming it are durable (SPEC §6):

1. Per table, write each output segment's bytes and fsync the file (`flush.pre_segment_sync`
   fires just before the sync).
2. Fsync each distinct partition directory the flush touched.
3. Fire `flush.pre_publish`, then commit and publish the new manifest:
   a. write `manifest.tmp`, fsync it;
   b. fire `manifest.pre_rename`, then rename `manifest.tmp` to `manifest`;
   c. fire `manifest.pre_dir_sync`, then fsync the store's root directory;
   d. hard-link `manifest` to `manifest.<version>`, then prune links older than
      `retain_manifests`. Both are best-effort: the commit already happened at (c), so a
      failure here must not tell the caller it did not.
4. Fire `flush.pre_ack`, then ack the write.

Compaction follows the same segment-then-manifest shape (write + sync each output, sync its
partition dir, `compact.pre_publish`, commit `RemoveSegments` then `AddSegments`), and GC fires
`compact.pre_gc` before deleting a garbage file's bytes. A SIGKILL test cannot observe a sync
that silently didn't happen, which is why every operation goes through the recorded `Io` and an
inline test asserts the exact sequence above rather than trusting a kill to catch a dropped
fsync.

## Consequences

- **Reader processes versus deleted files.** In-process, a live `Snapshot` is tracked exactly:
  GC never deletes a segment one still names. A reader *process* has no such handle and is
  protected only by `gc_grace` (default 5 minutes): a scan against a manifest older than
  `gc_grace` may find a segment already gone and gets `Error::SnapshotExpired`, and must reopen.
  This narrows SPEC §6's earlier claim that no reader can still hold a manifest naming a deleted
  file — that claim now holds only for in-process snapshots. SPEC §5, §6 and §18 were amended to
  match in this same PR (adelie-70j); this record does not itself edit SPEC.md.
- Tombstones are recorded in the manifest but not applied to any scan in E4: `Store::delete`
  appends a `Tombstone`, and `View::scan` returns every live segment's rows unfiltered. E5 makes
  a scan apply `tombstones_for`; E7 rewrites segments to drop the rows outright.
- The `manifest.<version>` hard links exist to let a human or a test inspect history; they are
  not part of the durability path (only `manifest` itself is renamed-into and fsynced), and only
  the last `retain_manifests` are kept.
