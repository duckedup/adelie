//! U10 (adelie-44e): public-API-only tests for `adelie::segment` (SPEC §5, D0008) — round
//! trip across row groups, stats, determinism, index pruning, corruption and fuzzing.

use std::cmp::Ordering;
use std::io::{BufWriter, Write};
use std::net::IpAddr;
use std::ops::Range;

use adelie::exec::{Batch, Column, Field, coalesce_companion};
use adelie::segment::*;
use adelie::types::{DataType, Decimal, Ip, Value, companion_name, total_cmp};
use adelie_harness::rng::SplitMix64;

const STRING_VALUES: [&str; 5] = ["alpha", "bravo", "charlie", "delta", "echo"];
const TS_BASE_NS: i64 = 1_700_000_000_000_000_000;

fn fields() -> Vec<Field> {
    vec![
        Field {
            name: "b".to_string(),
            ty: DataType::Bool,
        },
        Field {
            name: "i".to_string(),
            ty: DataType::Int64,
        },
        Field {
            name: "u".to_string(),
            ty: DataType::UInt64,
        },
        Field {
            name: "f".to_string(),
            ty: DataType::Float64,
        },
        Field {
            name: "d".to_string(),
            ty: DataType::decimal(18, 4).unwrap(),
        },
        Field {
            name: "s".to_string(),
            ty: DataType::String,
        },
        Field {
            name: "bytes".to_string(),
            ty: DataType::Bytes,
        },
        Field {
            name: "ts".to_string(),
            ty: DataType::Timestamp,
        },
        Field {
            name: "date".to_string(),
            ty: DataType::Date,
        },
        Field {
            name: "id".to_string(),
            ty: DataType::Uuid,
        },
        Field {
            name: "ip".to_string(),
            ty: DataType::Ip,
        },
        Field {
            name: "tags".to_string(),
            ty: DataType::list(DataType::String).unwrap(),
        },
        Field {
            name: "x".to_string(),
            ty: DataType::Int64,
        },
        Field {
            name: companion_name("x"),
            ty: DataType::String,
        },
    ]
}

fn field_index(name: &str, flds: &[Field]) -> usize {
    flds.iter()
        .position(|f| f.name == name)
        .unwrap_or_else(|| panic!("no field named {name}"))
}

fn null_roll(rng: &mut SplitMix64) -> bool {
    rng.range(0, 10) == 0
}

fn gen_string(rng: &mut SplitMix64) -> String {
    STRING_VALUES[rng.range(0, STRING_VALUES.len() as u64) as usize].to_string()
}

fn gen_bytes(rng: &mut SplitMix64) -> Vec<u8> {
    let len = rng.range(0, 16);
    (0..len).map(|_| rng.next_u64() as u8).collect()
}

/// A random FLOAT64 that regularly lands on NaN or -0.0, so the codec's exact-bits promise
/// (SPEC §5) is actually exercised.
fn gen_float(rng: &mut SplitMix64) -> f64 {
    match rng.range(0, 20) {
        0 => f64::NAN,
        1 => -0.0,
        2 => 0.0,
        _ => (rng.f64() - 0.5) * 1.0e6,
    }
}

/// Stays well under `DECIMAL(18,4)`'s bound (`|unscaled| < 10^18`) with room to spare.
fn gen_decimal(rng: &mut SplitMix64) -> Value {
    let magnitude = rng.range(0, 999_999_999_999_999_999) as i128;
    let unscaled = if rng.next_u64().is_multiple_of(2) {
        magnitude
    } else {
        -magnitude
    };
    Value::Decimal(Decimal::new(unscaled, 4).unwrap())
}

fn gen_uuid(rng: &mut SplitMix64) -> [u8; 16] {
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(8) {
        chunk.copy_from_slice(&rng.next_u64().to_le_bytes());
    }
    out
}

fn gen_ip(rng: &mut SplitMix64) -> Ip {
    let octets: [u8; 4] = [
        rng.range(0, 256) as u8,
        rng.range(0, 256) as u8,
        rng.range(0, 256) as u8,
        rng.range(0, 256) as u8,
    ];
    Ip::from(IpAddr::from(octets))
}

fn gen_tags(rng: &mut SplitMix64) -> Vec<Value> {
    let len = rng.range(0, 4);
    (0..len)
        .map(|_| {
            if rng.range(0, 10) == 0 {
                Value::Null
            } else {
                Value::String(gen_string(rng))
            }
        })
        .collect()
}

/// Generates `rows` fresh rows over the schema `fields()` returns, with about 10% nulls per
/// column except the Q2 pair, where exactly one of `x`/`x::string` is non-null per row.
fn batch(rng: &mut SplitMix64, rows: usize) -> Batch {
    let flds = fields();
    let (mut b, mut i, mut u, mut f) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut d, mut s, mut by, mut ts) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut date, mut id, mut ip, mut tags) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut x, mut xs) = (Vec::new(), Vec::new());

    for r in 0..rows {
        b.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Bool(rng.next_u64().is_multiple_of(2))
        });
        i.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Int64(rng.next_u64() as i64)
        });
        u.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::UInt64(rng.next_u64())
        });
        f.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Float64(gen_float(rng))
        });
        d.push(if null_roll(rng) {
            Value::Null
        } else {
            gen_decimal(rng)
        });
        s.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::String(gen_string(rng))
        });
        by.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Bytes(gen_bytes(rng))
        });
        let jitter = rng.range(0, 6_000_001) as i64 - 3_000_000;
        ts.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Timestamp(TS_BASE_NS + r as i64 * 1_000_000_000 + jitter)
        });
        date.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Date(rng.range(0, 40_000) as i32 - 20_000)
        });
        id.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Uuid(gen_uuid(rng))
        });
        ip.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::Ip(gen_ip(rng))
        });
        tags.push(if null_roll(rng) {
            Value::Null
        } else {
            Value::List(gen_tags(rng))
        });
        if rng.next_u64().is_multiple_of(2) {
            x.push(Value::Int64(rng.next_u64() as i64));
            xs.push(Value::Null);
        } else {
            x.push(Value::Null);
            xs.push(Value::String(gen_string(rng)));
        }
    }

    let cols = vec![
        Column::from_values(&flds[0].ty, &b).unwrap(),
        Column::from_values(&flds[1].ty, &i).unwrap(),
        Column::from_values(&flds[2].ty, &u).unwrap(),
        Column::from_values(&flds[3].ty, &f).unwrap(),
        Column::from_values(&flds[4].ty, &d).unwrap(),
        Column::from_values(&flds[5].ty, &s).unwrap(),
        Column::from_values(&flds[6].ty, &by).unwrap(),
        Column::from_values(&flds[7].ty, &ts).unwrap(),
        Column::from_values(&flds[8].ty, &date).unwrap(),
        Column::from_values(&flds[9].ty, &id).unwrap(),
        Column::from_values(&flds[10].ty, &ip).unwrap(),
        Column::from_values(&flds[11].ty, &tags).unwrap(),
        Column::from_values(&flds[12].ty, &x).unwrap(),
        Column::from_values(&flds[13].ty, &xs).unwrap(),
    ];
    Batch::new(flds, cols).unwrap()
}

/// Two values are equal when `total_cmp` says so; FLOAT64 additionally requires identical
/// `to_bits` (NaN payloads and -0.0 are distinct on the wire even though `total_cmp` folds them).
fn value_eq(a: &Value, b: &Value) -> bool {
    if let (Value::Float64(x), Value::Float64(y)) = (a, b) {
        return x.to_bits() == y.to_bits();
    }
    total_cmp(a, b) == Some(Ordering::Equal)
}

fn value_opt_eq(a: &Option<Value>, b: &Option<Value>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => value_eq(x, y),
        _ => false,
    }
}

/// Not derived `PartialEq`: NaN is not equal to itself, so a real comparison goes row by row.
fn columns_equal(a: &Column, b: &Column) -> bool {
    a.len() == b.len()
        && (0..a.len()).all(|i| {
            a.is_null(i) == b.is_null(i) && (a.is_null(i) || value_eq(&a.get(i), &b.get(i)))
        })
}

/// Global row `g` to `(batch index, row within that batch)`, in push order.
fn row_index_map(batches: &[Batch]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (bi, b) in batches.iter().enumerate() {
        out.extend((0..b.rows()).map(|ri| (bi, ri)));
    }
    out
}

fn concat_values(
    batches: &[Batch],
    col: usize,
    range: Range<usize>,
    index: &[(usize, usize)],
) -> Vec<Value> {
    range
        .map(|g| {
            let (bi, ri) = index[g];
            batches[bi].column(col).get(ri)
        })
        .collect()
}

/// Mirrors `Writer`'s own flush rule (SPEC §5): a row group is flushed, taking every
/// pending row, as soon as pushing crosses `row_group_rows`; `finish` flushes whatever remains.
fn expected_row_group_ranges(push_sizes: &[usize], row_group_rows: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut pending = 0usize;
    for &n in push_sizes {
        pending += n;
        if pending >= row_group_rows {
            out.push(start..start + pending);
            start += pending;
            pending = 0;
        }
    }
    if pending > 0 {
        out.push(start..start + pending);
    }
    out
}

fn write_segment(flds: &[Field], batches: &[Batch], opts: WriterOptions) -> Vec<u8> {
    let mut w = Writer::new(Vec::new(), flds.to_vec(), opts).unwrap();
    for b in batches {
        w.push(b).unwrap();
    }
    w.finish().unwrap().0
}

fn read_full_column(seg: &Reader<Vec<u8>>, col: usize, flds: &[Field]) -> Column {
    let mut values = Vec::new();
    for rg in 0..seg.row_groups().len() {
        let c = seg.read_column(rg, col).unwrap();
        values.extend((0..c.len()).map(|i| c.get(i)));
    }
    Column::from_values(&flds[col].ty, &values).unwrap()
}

fn push_sizes_500() -> Vec<usize> {
    let mut sizes = vec![37; 13];
    sizes.push(19);
    sizes
}

struct RoundTripFixture {
    flds: Vec<Field>,
    batches: Vec<Batch>,
    index: Vec<(usize, usize)>,
    ranges: Vec<Range<usize>>,
    bytes: Vec<u8>,
    meta: Meta,
    rows_before_finish: u64,
}

fn build_round_trip_fixture(
    seed: u64,
    row_group_rows: usize,
    push_sizes: &[usize],
) -> RoundTripFixture {
    let flds = fields();
    let mut rng = SplitMix64::new(seed);
    let batches: Vec<Batch> = push_sizes.iter().map(|&n| batch(&mut rng, n)).collect();
    let opts = WriterOptions {
        row_group_rows,
        ..Default::default()
    };
    let mut w = Writer::new(Vec::new(), flds.clone(), opts).unwrap();
    for b in &batches {
        w.push(b).unwrap();
    }
    let rows_before_finish = w.rows();
    let (bytes, meta) = w.finish().unwrap();
    let index = row_index_map(&batches);
    let ranges = expected_row_group_ranges(push_sizes, row_group_rows);
    RoundTripFixture {
        flds,
        batches,
        index,
        ranges,
        bytes,
        meta,
        rows_before_finish,
    }
}

// Criterion 1: round trip across several row groups.
#[test]
fn round_trip_across_several_row_groups() {
    let sizes = push_sizes_500();
    let fx = build_round_trip_fixture(0xA11CE, 100, &sizes);
    assert_eq!(fx.rows_before_finish, 500);
    assert_eq!(fx.meta.rows, 500);

    let seg = Reader::open("e2e", fx.bytes.clone()).unwrap();
    assert_eq!(seg.rows(), 500);
    assert_eq!(seg.fields(), fx.flds.as_slice());

    let rgs = seg.row_groups();
    assert_eq!(rgs.len(), fx.ranges.len());
    assert_eq!(rgs.iter().map(|rg| rg.rows).sum::<u64>(), 500);
    for rg in &rgs[..rgs.len() - 1] {
        assert!(
            rg.rows >= 100,
            "every row group but the last has >= row_group_rows rows"
        );
    }

    let projection: Vec<usize> = (0..fx.flds.len()).collect();
    for (rg_idx, range) in fx.ranges.iter().enumerate() {
        assert_eq!(rgs[rg_idx].rows as usize, range.len());
        let read_batch = seg.read_row_group(rg_idx, &projection).unwrap();
        assert_eq!(read_batch.rows(), range.len());
        assert_eq!(read_batch.fields(), fx.flds.as_slice());
        for (col, field) in fx.flds.iter().enumerate() {
            let want = Column::from_values(
                &field.ty,
                &concat_values(&fx.batches, col, range.clone(), &fx.index),
            )
            .unwrap();
            assert!(
                columns_equal(read_batch.column(col), &want),
                "row group {rg_idx} column {}",
                field.name
            );
            let direct = seg.read_column(rg_idx, col).unwrap();
            assert!(
                columns_equal(&direct, &want),
                "row group {rg_idx} column {} (read_column)",
                field.name
            );
        }
    }
}

// Criterion 2: per-row-group and whole-segment stats.
#[test]
fn stats_match_column_stats_per_row_group_and_for_the_whole_segment() {
    let sizes = push_sizes_500();
    let fx = build_round_trip_fixture(0xA11CE, 100, &sizes);
    let seg = Reader::open("e2e", fx.bytes.clone()).unwrap();

    for (rg_idx, range) in fx.ranges.iter().enumerate() {
        let rg_meta = &seg.row_groups()[rg_idx];
        for (col, field) in fx.flds.iter().enumerate() {
            let want = Column::from_values(
                &field.ty,
                &concat_values(&fx.batches, col, range.clone(), &fx.index),
            )
            .unwrap()
            .stats();
            let chunk = &rg_meta.chunks[col];
            assert_eq!(
                chunk.null_count, want.null_count as u64,
                "column {}",
                field.name
            );
            if matches!(field.ty, DataType::List(_)) {
                assert!(
                    chunk.min.is_none() && chunk.max.is_none(),
                    "LIST stats are always absent"
                );
            } else {
                assert!(
                    value_opt_eq(&chunk.min, &want.min),
                    "column {}: min mismatch",
                    field.name
                );
                assert!(
                    value_opt_eq(&chunk.max, &want.max),
                    "column {}: max mismatch",
                    field.name
                );
            }
        }
    }

    for (col, field) in fx.flds.iter().enumerate() {
        let want = Column::from_values(
            &field.ty,
            &concat_values(&fx.batches, col, 0..500, &fx.index),
        )
        .unwrap()
        .stats();
        let got = &fx.meta.columns[col];
        assert_eq!(got.null_count, want.null_count, "column {}", field.name);
        assert!(
            value_opt_eq(&got.min, &want.min),
            "column {}: segment min mismatch",
            field.name
        );
        assert!(
            value_opt_eq(&got.max, &want.max),
            "column {}: segment max mismatch",
            field.name
        );
    }
}

fn flip_one_value(flds: &[Field], batches: &[Batch], field_name: &str) -> Vec<Batch> {
    let col = field_index(field_name, flds);
    let mut out = batches.to_vec();
    let rows = out[0].rows();
    let mut values: Vec<Value> = (0..rows).map(|r| out[0].column(col).get(r)).collect();
    values[0] = match &values[0] {
        Value::Int64(x) => Value::Int64(x.wrapping_add(1)),
        _ => Value::Int64(1),
    };
    let mut cols = out[0].columns().to_vec();
    cols[col] = Column::from_values(&flds[col].ty, &values).unwrap();
    out[0] = Batch::new(flds.to_vec(), cols).unwrap();
    out
}

// Criterion 3: determinism, plus a control that proves the comparison sees content.
#[test]
fn determinism_same_seed_gives_identical_bytes_and_a_control_flip_differs() {
    let flds = fields();
    let mut gen_rng = SplitMix64::new(0xD3D3);
    let batches: Vec<Batch> = (0..5).map(|_| batch(&mut gen_rng, 37)).collect();
    let opts = WriterOptions {
        row_group_rows: 100,
        pins: Vec::new(),
        indexes: vec![
            ("s".to_string(), IndexKind::Bloom),
            ("s".to_string(), IndexKind::ValueSet),
            ("s".to_string(), IndexKind::Ngram),
            ("id".to_string(), IndexKind::Bloom),
        ],
    };

    let out1 = write_segment(&flds, &batches, opts.clone());
    let out2 = write_segment(&flds, &batches, opts.clone());
    assert_eq!(
        out1, out2,
        "two writes of the same seeded batches must be byte-identical"
    );

    let seg = Reader::open("e2e", out1.clone()).unwrap();
    let requests = [
        (field_index("s", &flds), IndexKind::Bloom),
        (field_index("s", &flds), IndexKind::ValueSet),
        (field_index("s", &flds), IndexKind::Ngram),
        (field_index("id", &flds), IndexKind::Bloom),
    ];
    let idx1 = IdxWriter::build(&seg, &requests).unwrap();
    let idx2 = IdxWriter::build(&seg, &requests).unwrap();
    assert_eq!(
        idx1, idx2,
        "IdxWriter::build run twice must be byte-identical"
    );

    let mutated = flip_one_value(&flds, &batches, "i");
    let out3 = write_segment(&flds, &mutated, opts);
    assert_ne!(
        out1, out3,
        "changing one value in one row must change the bytes"
    );
}

fn find_index(entries: &[IndexEntry], rg: usize, kind: IndexKind, col: usize) -> &IndexEntry {
    entries
        .iter()
        .find(|e| e.row_group == Some(rg) && e.kind == kind && e.columns == vec![col])
        .unwrap_or_else(|| panic!("missing {kind:?} index for row group {rg} column {col}"))
}

// Criterion 4: requested indexes prune correctly in every row group.
#[test]
fn indexes_prune_known_values_and_reject_absent_ones() {
    let flds = fields();
    let opts = WriterOptions {
        row_group_rows: 50,
        pins: Vec::new(),
        indexes: vec![
            ("s".to_string(), IndexKind::Bloom),
            ("s".to_string(), IndexKind::ValueSet),
            ("s".to_string(), IndexKind::Ngram),
            ("id".to_string(), IndexKind::Bloom),
        ],
    };
    let mut rng = SplitMix64::new(0x1DE5);
    let batches: Vec<Batch> = (0..6).map(|_| batch(&mut rng, 50)).collect();
    let bytes = write_segment(&flds, &batches, opts);
    let seg = Reader::open("e2e", bytes).unwrap();

    let s_idx = field_index("s", &flds);
    let id_idx = field_index("id", &flds);

    for rg in 0..seg.row_groups().len() {
        let s_col = seg.read_column(rg, s_idx).unwrap();
        let present: Vec<String> = (0..s_col.len())
            .filter(|&i| !s_col.is_null(i))
            .map(|i| match s_col.get(i) {
                Value::String(v) => v,
                other => panic!("expected STRING, got {other:?}"),
            })
            .collect();

        let vs = seg
            .load_index(find_index(seg.indexes(), rg, IndexKind::ValueSet, s_idx))
            .unwrap();
        for v in &present {
            assert!(
                vs.might_contain(&Value::String(v.clone())),
                "row group {rg}: {v} should be in the value set"
            );
        }
        assert!(!vs.might_contain(&Value::String("absent-value".to_string())));

        let ng = seg
            .load_index(find_index(seg.indexes(), rg, IndexKind::Ngram, s_idx))
            .unwrap();
        assert!(!ng.might_contain_substring("zzzq"));
        let sample = present
            .first()
            .expect("row group has at least one non-null s value");
        assert!(ng.might_contain_substring(&sample[..3]));

        let id_col = seg.read_column(rg, id_idx).unwrap();
        let bloom = seg
            .load_index(find_index(seg.indexes(), rg, IndexKind::Bloom, id_idx))
            .unwrap();
        for i in 0..id_col.len() {
            if let Value::Uuid(u) = id_col.get(i) {
                assert!(bloom.might_contain(&Value::Uuid(u)));
            }
        }
    }
}

/// The trailer's `footer_len` field, per the documented wire layout (SPEC §5): the last 16
/// bytes are `footer_len u32 · footer_crc32c u32 · magic(6) · version u16`.
fn footer_len(bytes: &[u8]) -> usize {
    let n = bytes.len();
    u32::from_le_bytes(bytes[n - 16..n - 12].try_into().unwrap()) as usize
}

// Criterion 5: a single-byte body flip either leaves every read Ok, or corrupts exactly one
// column's chunk (never silently returns different data).
#[test]
fn single_byte_flips_in_the_body_either_stay_ok_or_name_the_corrupt_chunk() {
    let flds = fields();
    let opts = WriterOptions {
        row_group_rows: 100,
        pins: Vec::new(),
        indexes: vec![
            ("s".to_string(), IndexKind::Bloom),
            ("s".to_string(), IndexKind::ValueSet),
            ("s".to_string(), IndexKind::Ngram),
            ("id".to_string(), IndexKind::Bloom),
        ],
    };
    let mut rng = SplitMix64::new(0xC07E);
    let batches: Vec<Batch> = (0..5).map(|_| batch(&mut rng, 37)).collect();
    let bytes = write_segment(&flds, &batches, opts);

    // A 0-row segment has an empty body by construction, so this derives HEADER_LEN (8) rather
    // than hardcoding it, while `footer_len` is read straight off each segment's own trailer.
    let empty = write_segment(&flds, &[], WriterOptions::default());
    let header_len = empty.len() - footer_len(&empty) - 16;
    assert_eq!(header_len, 8, "SPEC §5: an 8-byte header");

    let body_start = header_len;
    let body_end = bytes.len() - 16 - footer_len(&bytes);
    assert!(
        body_end > body_start,
        "a segment with rows has a non-empty body"
    );

    let reference = Reader::open("e2e", bytes.clone()).unwrap();
    let mut flip_rng = SplitMix64::new(0xF11D);
    for _ in 0..50 {
        let pos = body_start + flip_rng.range(0, (body_end - body_start) as u64) as usize;
        let bit = 1u8 << flip_rng.range(0, 8);
        let mut corrupted = bytes.clone();
        corrupted[pos] ^= bit;

        let seg = Reader::open("e2e", corrupted).expect("a body flip never breaks the footer");
        let mut corrupt_count = 0;
        for rg in 0..seg.row_groups().len() {
            for col in 0..flds.len() {
                match seg.read_column(rg, col) {
                    Ok(got) => {
                        let want = reference.read_column(rg, col).unwrap();
                        assert!(
                            columns_equal(&got, &want),
                            "an unaffected chunk must decode to the original data"
                        );
                    }
                    Err(e @ Error::CorruptChunk { .. }) => {
                        let msg = e.to_string();
                        assert!(msg.contains("e2e"), "error should name the segment: {msg}");
                        assert!(
                            flds.iter().any(|f| msg.contains(&f.name)),
                            "error should name a column: {msg}"
                        );
                        corrupt_count += 1;
                    }
                    Err(other) => panic!("unexpected error variant: {other}"),
                }
            }
        }
        assert!(
            corrupt_count <= 1,
            "a single-byte flip corrupts at most one chunk, got {corrupt_count}"
        );
    }
}

fn mutate_bytes(bytes: &[u8], rng: &mut SplitMix64) -> Vec<u8> {
    match rng.range(0, 4) {
        0 => {
            let mut out = bytes.to_vec();
            if !out.is_empty() {
                for _ in 0..rng.range(1, 5) {
                    let pos = rng.range(0, out.len() as u64) as usize;
                    out[pos] ^= 1u8 << rng.range(0, 8);
                }
            }
            out
        }
        1 => bytes[..rng.range(0, bytes.len() as u64 + 1) as usize].to_vec(),
        2 => {
            let mut out = bytes.to_vec();
            if !out.is_empty() {
                let start = rng.range(0, out.len() as u64) as usize;
                let len = rng.range(0, (out.len() - start) as u64 + 1) as usize;
                for b in &mut out[start..start + len] {
                    *b = rng.next_u64() as u8;
                }
            }
            out
        }
        _ => {
            let len = rng.range(0, 2049);
            (0..len).map(|_| rng.next_u64() as u8).collect()
        }
    }
}

/// Whenever `open` succeeds the footer's own CRC has passed, so the schema and row-group
/// shape are exactly the original; only body bytes (chunks, index blobs) may be corrupted.
fn check_fuzzed_bytes(bytes: &[u8], reference: &Reader<Vec<u8>>, flds: &[Field]) {
    let Ok(seg) = Reader::open("e2e", bytes.to_vec()) else {
        return;
    };
    let rg_count = seg.row_groups().len().min(reference.row_groups().len());
    for rg in 0..rg_count {
        for col in 0..flds.len() {
            if let Ok(got) = seg.read_column(rg, col) {
                let want = reference
                    .read_column(rg, col)
                    .expect("footer identical when open() succeeds");
                assert!(
                    columns_equal(&got, &want),
                    "row group {rg} column {col} decoded to wrong data"
                );
            }
        }
    }
    let idx_count = seg.indexes().len().min(reference.indexes().len());
    for (i, entry) in seg.indexes().iter().enumerate().take(idx_count) {
        if let Ok(got) = seg.load_index(entry) {
            let want = reference
                .load_index(&reference.indexes()[i])
                .expect("footer identical when open() succeeds");
            assert_eq!(got, want, "index {i} decoded to wrong data");
        }
    }
}

// Criterion 6: fuzzed bytes never panic and never decode to the wrong data.
#[test]
fn fuzzed_bytes_never_panic_and_never_decode_to_wrong_data() {
    let iters: usize = std::env::var("ADELIE_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if cfg!(miri) { 6 } else { 300 });

    let flds = fields();
    let opts = WriterOptions {
        row_group_rows: 100,
        pins: Vec::new(),
        indexes: vec![
            ("s".to_string(), IndexKind::Bloom),
            ("s".to_string(), IndexKind::ValueSet),
            ("s".to_string(), IndexKind::Ngram),
            ("id".to_string(), IndexKind::Bloom),
        ],
    };
    let mut gen_rng = SplitMix64::new(0xFE55);
    let batches: Vec<Batch> = (0..5).map(|_| batch(&mut gen_rng, 37)).collect();
    let original = write_segment(&flds, &batches, opts);
    let reference = Reader::open("e2e", original.clone()).unwrap();

    for i in 0..iters {
        let seed = 0x5EED_0000_u64.wrapping_add(i as u64);
        let mut rng = SplitMix64::new(seed);
        let mutated = mutate_bytes(&original, &mut rng);
        let flds_ref = &flds;
        let reference_ref = &reference;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            check_fuzzed_bytes(&mutated, reference_ref, flds_ref);
        }));
        if result.is_err() {
            panic!("seed {seed} caused a panic while decoding fuzzed bytes");
        }
    }
}

// Criterion 7: the Q2 companion pair is ordinary columns after the round trip.
#[test]
fn companion_columns_round_trip_and_coalesce_matches_the_input() {
    let flds = fields();
    assert_eq!(companion_name("x"), "x::string");
    assert!(flds.iter().any(|f| f.name == companion_name("x")));
    let x_idx = field_index("x", &flds);
    let xs_idx = field_index(&companion_name("x"), &flds);

    let mut rng = SplitMix64::new(0xC0FFEE);
    let batches: Vec<Batch> = (0..4).map(|_| batch(&mut rng, 37)).collect();
    let bytes = write_segment(
        &flds,
        &batches,
        WriterOptions {
            row_group_rows: 50,
            ..Default::default()
        },
    );
    let seg = Reader::open("e2e", bytes).unwrap();

    let index = row_index_map(&batches);
    let total = batches.iter().map(Batch::rows).sum::<usize>();
    let input_x = Column::from_values(
        &flds[x_idx].ty,
        &concat_values(&batches, x_idx, 0..total, &index),
    )
    .unwrap();
    let input_xs = Column::from_values(
        &flds[xs_idx].ty,
        &concat_values(&batches, xs_idx, 0..total, &index),
    )
    .unwrap();
    let input_coalesced = coalesce_companion(&input_x, &input_xs).unwrap();

    let got_x = read_full_column(&seg, x_idx, &flds);
    let got_xs = read_full_column(&seg, xs_idx, &flds);
    let got_coalesced = coalesce_companion(&got_x, &got_xs).unwrap();

    assert!(columns_equal(&input_coalesced, &got_coalesced));
}

// Criterion 8: writing through a real file round-trips.
#[test]
#[cfg_attr(miri, ignore)] // touches the real filesystem
fn writing_through_a_buffered_file_round_trips() {
    let flds = fields();
    let mut rng = SplitMix64::new(0xF11E);
    let batches: Vec<Batch> = (0..3).map(|_| batch(&mut rng, 20)).collect();

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "adelie-e2e-format-{}-{nanos}.seg",
        std::process::id()
    ));

    {
        let file = std::fs::File::create(&path).unwrap();
        let mut w =
            Writer::new(BufWriter::new(file), flds.clone(), WriterOptions::default()).unwrap();
        for b in &batches {
            w.push(b).unwrap();
        }
        let (mut out, _meta) = w.finish().unwrap();
        out.flush().unwrap();
    }

    let bytes = std::fs::read(&path).unwrap();
    let seg = Reader::open("e2e", bytes).unwrap();
    assert_eq!(seg.rows(), 60);
    let index = row_index_map(&batches);
    for (col, field) in flds.iter().enumerate() {
        let want =
            Column::from_values(&field.ty, &concat_values(&batches, col, 0..60, &index)).unwrap();
        assert!(
            columns_equal(&read_full_column(&seg, col, &flds), &want),
            "column {}",
            field.name
        );
    }
    std::fs::remove_file(&path).unwrap();
}

// Criterion 9: full-size row groups (the default row_group_rows, at scale) round-trip.
#[test]
#[cfg_attr(miri, ignore)] // 70k rows across 14 columns takes minutes under Miri
fn full_size_row_groups_round_trip() {
    let flds = fields();
    let mut rng = SplitMix64::new(0x4040);
    let sizes = [DEFAULT_ROW_GROUP_ROWS, 70_000 - DEFAULT_ROW_GROUP_ROWS];
    let batches: Vec<Batch> = sizes.iter().map(|&n| batch(&mut rng, n)).collect();

    let bytes = write_segment(&flds, &batches, WriterOptions::default());
    let seg = Reader::open("e2e", bytes).unwrap();
    assert_eq!(seg.rows(), 70_000);
    assert_eq!(seg.row_groups().len(), 2);
    assert!(seg.row_groups()[0].rows >= DEFAULT_ROW_GROUP_ROWS as u64);

    let index = row_index_map(&batches);
    let mut start = 0usize;
    for (rg_idx, rg) in seg.row_groups().iter().enumerate() {
        let range = start..start + rg.rows as usize;
        for (col, field) in flds.iter().enumerate() {
            let want = Column::from_values(
                &field.ty,
                &concat_values(&batches, col, range.clone(), &index),
            )
            .unwrap();
            let got = seg.read_column(rg_idx, col).unwrap();
            assert!(
                columns_equal(&got, &want),
                "row group {rg_idx} column {}",
                field.name
            );
        }
        start += rg.rows as usize;
    }
}
