//! The planted stores are the proof: each makes `crash::run` return the matching `Violation`,
//! or, for `Journal`, prove it survives every kill policy.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::verdict::{verdict, Defect};
use super::{run, CrashTarget, Kill, Plan, Row, Violation};

// ── Journal: a correct write-ahead log ──────────────────────────────────────

struct Journal;

struct JournalStore {
    path: PathBuf,
}

/// A simple hash of the two numeric fields, just to detect a torn tail.
fn fnv1a(batch: u64, rows: u32) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in batch.to_le_bytes().into_iter().chain(rows.to_le_bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn read_checked_lines(path: &Path) -> io::Result<Vec<Row>> {
    let data = match fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut rows = Vec::new();
    for line in data.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [b, n, cs] = parts.as_slice() else { continue };
        let (Ok(b), Ok(n), Ok(cs)) = (b.parse::<u64>(), n.parse::<u32>(), cs.parse::<u64>())
        else {
            continue;
        };
        if cs != fnv1a(b, n) {
            continue; // torn tail: checksum doesn't match
        }
        rows.extend((0..n).map(|i| (b, i)));
    }
    Ok(rows)
}

impl CrashTarget for Journal {
    type Store = JournalStore;

    fn open(dir: &Path) -> io::Result<Self::Store> {
        fs::create_dir_all(dir)?;
        Ok(JournalStore { path: dir.join("journal.log") })
    }

    fn write(store: &mut Self::Store, batch: &[Row]) -> io::Result<()> {
        let b = batch[0].0;
        let n = batch.len() as u32;
        let line = format!("{b} {n} {}\n", fnv1a(b, n));
        let bytes = line.as_bytes();
        let mid = bytes.len() / 2;

        let mut f = fs::OpenOptions::new().create(true).append(true).open(&store.path)?;
        f.write_all(&bytes[..mid])?;
        super::failpoint("journal.mid_write");
        f.write_all(&bytes[mid..])?;
        f.sync_data()
    }

    fn read_all(store: &Self::Store) -> io::Result<Vec<Row>> {
        read_checked_lines(&store.path)
    }
}

// ── AckBeforeWrite: acks before it's durable ────────────────────────────────

struct AckBeforeWrite;

struct AckBeforeWriteStore {
    path: PathBuf,
    buffered: Vec<Row>,
    batches_written: u64,
}

impl CrashTarget for AckBeforeWrite {
    type Store = AckBeforeWriteStore;

    fn open(dir: &Path) -> io::Result<Self::Store> {
        fs::create_dir_all(dir)?;
        Ok(AckBeforeWriteStore {
            path: dir.join("data.log"),
            buffered: Vec::new(),
            batches_written: 0,
        })
    }

    // Buffers in memory and acks immediately: never writes unsynced, so a crash test that
    // only kills via SIGKILL still catches the bug (see the module doc on process vs power).
    fn write(store: &mut Self::Store, batch: &[Row]) -> io::Result<()> {
        store.buffered.extend_from_slice(batch);
        store.batches_written += 1;
        if store.batches_written % 1_000_000 == 0 {
            let mut f = fs::OpenOptions::new().create(true).append(true).open(&store.path)?;
            for &(b, i) in &store.buffered {
                writeln!(f, "{b} {i}")?;
            }
            store.buffered.clear();
        }
        Ok(())
    }

    fn read_all(store: &Self::Store) -> io::Result<Vec<Row>> {
        read_pair_lines(&store.path)
    }
}

fn read_pair_lines(path: &Path) -> io::Result<Vec<Row>> {
    let data = match fs::read_to_string(path) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut rows = Vec::new();
    for line in data.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(b), Some(i)) = (parts.next(), parts.next()) {
            if let (Ok(b), Ok(i)) = (b.parse(), i.parse()) {
                rows.push((b, i));
            }
        }
    }
    Ok(rows)
}

// ── TornWriter: not atomic per batch ────────────────────────────────────────

struct TornWriter;

struct TornWriterStore {
    path: PathBuf,
}

impl CrashTarget for TornWriter {
    type Store = TornWriterStore;

    fn open(dir: &Path) -> io::Result<Self::Store> {
        fs::create_dir_all(dir)?;
        Ok(TornWriterStore { path: dir.join("rows.log") })
    }

    fn write(store: &mut Self::Store, batch: &[Row]) -> io::Result<()> {
        let mut f = fs::OpenOptions::new().create(true).append(true).open(&store.path)?;
        let half = batch.len() / 2;
        for &(b, i) in &batch[..half] {
            writeln!(f, "{b} {i}")?;
        }
        f.sync_data()?;
        super::failpoint("torn.mid_batch");
        for &(b, i) in &batch[half..] {
            writeln!(f, "{b} {i}")?;
        }
        f.sync_data()
    }

    fn read_all(store: &Self::Store) -> io::Result<Vec<Row>> {
        read_pair_lines(&store.path) // returns every line, even a torn tail
    }
}

// ── PhantomStore: a correct Journal that also invents rows ──────────────────

struct PhantomStore;

struct PhantomStoreStore {
    inner: JournalStore,
}

impl CrashTarget for PhantomStore {
    type Store = PhantomStoreStore;

    fn open(dir: &Path) -> io::Result<Self::Store> {
        Ok(PhantomStoreStore { inner: Journal::open(dir)? })
    }

    fn write(store: &mut Self::Store, batch: &[Row]) -> io::Result<()> {
        Journal::write(&mut store.inner, batch)
    }

    fn read_all(store: &Self::Store) -> io::Result<Vec<Row>> {
        let mut rows = Journal::read_all(&store.inner)?;
        let phantom_batch = u64::MAX / 2;
        rows.extend((0..4).map(|i| (phantom_batch, i)));
        Ok(rows)
    }
}

// ── Process tests: each proves the harness against a planted store ─────────

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn journal_survives_ack_kills() {
    let plan = Plan { runs: 6, batches: 20, rows_per_batch: 8, seed: 1, kill: Kill::AtAck };
    let summary = run::<Journal>(concat!(module_path!(), "::journal_survives_ack_kills"), &plan)
        .expect("journal must survive ack kills");
    assert_eq!(summary.killed, 6);
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn journal_survives_failpoint_mid_write() {
    let plan = Plan {
        runs: 6,
        batches: 10,
        rows_per_batch: 4,
        seed: 2,
        kill: Kill::Failpoint("journal.mid_write"),
    };
    let summary = run::<Journal>(
        concat!(module_path!(), "::journal_survives_failpoint_mid_write"),
        &plan,
    )
    .expect("journal must survive a mid-write kill");
    assert!(summary.killed >= 1);
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn journal_survives_random_kills() {
    let plan = Plan {
        runs: 4,
        batches: 200,
        rows_per_batch: 4,
        seed: 3,
        kill: Kill::Random { max_delay_ms: 30 },
    };
    run::<Journal>(concat!(module_path!(), "::journal_survives_random_kills"), &plan)
        .expect("journal must survive random kills");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn ack_before_write_loses_acked_rows() {
    let plan = Plan { runs: 2, batches: 10, rows_per_batch: 4, seed: 4, kill: Kill::AtAck };
    let err = run::<AckBeforeWrite>(
        concat!(module_path!(), "::ack_before_write_loses_acked_rows"),
        &plan,
    )
    .expect_err("acking before writing must be caught");
    assert!(matches!(err, Violation::LostAck { .. }), "expected LostAck, got {err:?}");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn torn_writer_is_caught() {
    let plan = Plan {
        runs: 3,
        batches: 10,
        rows_per_batch: 4,
        seed: 5,
        kill: Kill::Failpoint("torn.mid_batch"),
    };
    let err = run::<TornWriter>(concat!(module_path!(), "::torn_writer_is_caught"), &plan)
        .expect_err("a torn batch must be caught");
    assert!(matches!(err, Violation::Torn { .. }), "expected Torn, got {err:?}");
}

#[test]
#[cfg_attr(miri, ignore)] // spawns and kills a child process
fn phantom_rows_are_caught() {
    let plan = Plan { runs: 1, batches: 10, rows_per_batch: 4, seed: 6, kill: Kill::AtAck };
    let err = run::<PhantomStore>(concat!(module_path!(), "::phantom_rows_are_caught"), &plan)
        .expect_err("a phantom batch must be caught");
    assert!(matches!(err, Violation::Phantom { .. }), "expected Phantom, got {err:?}");
}

// ── Pure tests: verdict directly, no process involved, runs under Miri ─────

#[test]
fn verdict_clean_input_is_ok() {
    let rows: Vec<Row> = (0..3u64).flat_map(|b| (0..4u32).map(move |i| (b, i))).collect();
    assert_eq!(verdict(3, Some(2), 4, &rows), Ok(()));
}

#[test]
fn verdict_catches_lost_ack() {
    let rows: Vec<Row> = (0..4u32).map(|i| (1, i)).collect(); // batch 0 acked, missing
    assert_eq!(verdict(2, Some(1), 4, &rows), Err(Defect::LostAck { batch: 0 }));
}

#[test]
fn verdict_torn_beats_lost_for_partial_acked_batch() {
    let rows: Vec<Row> = vec![(0, 0), (0, 1)]; // batch 0 acked, only half present
    assert_eq!(
        verdict(1, Some(0), 4, &rows),
        Err(Defect::Torn { batch: 0, present: 2, expected: 4 })
    );
}

#[test]
fn verdict_catches_phantom() {
    let rows: Vec<Row> = vec![(0, 0), (5, 0)]; // batch 5 was never sent
    assert_eq!(verdict(1, Some(0), 1, &rows), Err(Defect::Phantom { batch: 5 }));
}

#[test]
fn verdict_catches_duplicate() {
    let rows: Vec<Row> = vec![(0, 0), (0, 0)];
    assert_eq!(verdict(1, Some(0), 1, &rows), Err(Defect::Duplicate { batch: 0, row: 0 }));
}

#[test]
fn verdict_unacked_empty_batch_is_fine() {
    // Batch 1 was sent but never acked; nothing durable yet is expected, not a violation.
    let rows: Vec<Row> = vec![(0, 0)];
    assert_eq!(verdict(1, Some(1), 1, &rows), Ok(()));
}

#[test]
fn verdict_unacked_torn_batch_is_still_caught() {
    // Batch 0 acked and complete; batch 1 sent but never acked, and only half-written — a
    // partial write is still non-atomic, so still Torn even though nothing acked it.
    let rows: Vec<Row> = vec![(0, 0), (0, 1), (1, 0)];
    assert_eq!(
        verdict(1, Some(1), 2, &rows),
        Err(Defect::Torn { batch: 1, present: 1, expected: 2 })
    );
}
