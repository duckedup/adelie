//! Kill-and-reopen crash harness: SIGKILL a child mid-workload, reopen, and check every
//! acknowledged row survived and nothing unacknowledged leaked in (SPEC §6, §13).
//!
//! This tests *process death*, not power loss: a write that reached the page cache survives
//! SIGKILL, so an fsync-ordering bug that only shows on power loss is out of reach. That is
//! also why the "acks before writing" fake buffers in memory rather than writing unsynced.

mod child;
mod verdict;
#[cfg(test)]
mod tests;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child as ChildProcess, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::rng::SplitMix64;

/// (batch id, index within batch).
pub type Row = (u64, u32);

/// A store under test. The child process drives it; the parent reopens and reads it back.
pub trait CrashTarget {
    type Store;
    fn open(dir: &Path) -> std::io::Result<Self::Store>;
    /// Returns only once the batch is durable: this is the acknowledgement.
    fn write(store: &mut Self::Store, batch: &[Row]) -> std::io::Result<()>;
    fn read_all(store: &Self::Store) -> std::io::Result<Vec<Row>>;
    /// Called between batches in the child: where compaction will run. Default no-op.
    fn between(_store: &mut Self::Store, _batch: u64) -> std::io::Result<()> {
        Ok(())
    }
}

/// A single crash test: how many runs, how big a workload, and how to kill it.
pub struct Plan {
    pub runs: u32,
    pub batches: u64,
    pub rows_per_batch: u32,
    pub seed: u64,
    pub kill: Kill,
}

/// The kill policy, derived deterministically from `plan.seed ^ run`.
#[derive(Debug, Clone, Copy)]
pub enum Kill {
    /// Lockstep; kills on the k-th ack, so exactly k batches are acked.
    AtAck,
    /// Lockstep; kills on the named failpoint's nth hit.
    Failpoint(&'static str),
    /// Not lockstep; kills after a random delay regardless of progress.
    Random { max_delay_ms: u64 },
}

/// What one `run<T>` call did across all its runs.
#[derive(Debug, Clone, Copy)]
pub struct Summary {
    pub runs: u32,
    pub killed: u32,
    pub acked_rows_checked: u64,
}

/// A defect a reopened store showed after a kill, with enough context to reproduce it.
#[derive(Debug)]
pub enum Violation {
    LostAck { run: u32, batch: u64, dir: PathBuf },
    Torn { run: u32, batch: u64, present: usize, expected: usize, dir: PathBuf },
    Phantom { run: u32, batch: u64, dir: PathBuf },
    Duplicate { run: u32, batch: u64, row: u32, dir: PathBuf },
    Child { run: u32, msg: String },
    Reopen { run: u32, err: String, dir: PathBuf },
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Violation::LostAck { run, batch, dir } => write!(
                f,
                "lost ack: run {run} batch {batch} is missing entirely (dir: {})",
                dir.display()
            ),
            Violation::Torn { run, batch, present, expected, dir } => write!(
                f,
                "torn batch: run {run} batch {batch} has {present}/{expected} rows (dir: {})",
                dir.display()
            ),
            Violation::Phantom { run, batch, dir } => write!(
                f,
                "phantom batch: run {run} batch {batch} was never sent (dir: {})",
                dir.display()
            ),
            Violation::Duplicate { run, batch, row, dir } => write!(
                f,
                "duplicate row: run {run} batch {batch} row {row} appears twice (dir: {})",
                dir.display()
            ),
            Violation::Child { run, msg } => write!(f, "child failed: run {run}: {msg}"),
            Violation::Reopen { run, err, dir } => {
                write!(f, "reopen failed: run {run}: {err} (dir: {})", dir.display())
            }
        }
    }
}

impl std::error::Error for Violation {}

static FAILPOINT_HITS: AtomicU64 = AtomicU64::new(0);

/// Fires `name`'s nth hit under `ADELIE_FAILPOINT=name:nth`: prints, flushes and blocks on
/// stdin. A no-op everywhere else, including every hit of `name` that isn't the nth.
///
/// adelie's store will call a cfg'd equivalent that reads the same env protocol at E4.
pub fn failpoint(name: &str) {
    if std::env::var("ADELIE_CRASH_ROLE").ok().as_deref() != Some("child") {
        return;
    }
    let Ok(spec) = std::env::var("ADELIE_FAILPOINT") else { return };
    let Some((fp_name, nth)) = spec.split_once(':') else { return };
    if fp_name != name {
        return;
    }
    let Ok(nth) = nth.parse::<u64>() else { return };
    if FAILPOINT_HITS.fetch_add(1, Ordering::SeqCst) + 1 != nth {
        return;
    }
    println!("fp {name}");
    let _ = std::io::stdout().flush();
    let mut discard = String::new();
    let _ = std::io::stdin().lock().read_line(&mut discard);
}

/// Runs `plan` against `T`, spawning this same test binary as the child under
/// `ADELIE_CRASH_ROLE=child`. `test_path` is `concat!(module_path!(), "::fn_name")`.
pub fn run<T: CrashTarget>(test_path: &str, plan: &Plan) -> Result<Summary, Violation> {
    if std::env::var("ADELIE_CRASH_ROLE").ok().as_deref() == Some("child") {
        let dir = std::env::var("ADELIE_CRASH_DIR").expect("ADELIE_CRASH_DIR set for the child");
        let batches = env_u64("ADELIE_CRASH_BATCHES");
        let rows_per_batch = env_u32("ADELIE_CRASH_ROWS");
        let lockstep = std::env::var("ADELIE_CRASH_LOCKSTEP").as_deref() == Ok("1");
        child::run_child::<T>(Path::new(&dir), batches, rows_per_batch, lockstep);
    }
    run_parent::<T>(test_path, plan)
}

fn env_u64(key: &str) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} must be a valid u64 for the crash child"))
}

fn env_u32(key: &str) -> u32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| panic!("{key} must be a valid u32 for the crash child"))
}

/// A fresh, run-specific temp dir: never reused across runs or processes.
fn crash_dir(test_path: &str, run: u32) -> PathBuf {
    let sanitised: String = test_path
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    std::env::temp_dir().join(format!("adelie-crash-{}-{sanitised}-{run}", std::process::id()))
}

/// One drive_* function's result: whether the child was killed, and what it acked/sent.
struct DriveOutcome {
    killed: bool,
    acked: u64,
    highest_sent: Option<u64>,
}

fn run_parent<T: CrashTarget>(test_path: &str, plan: &Plan) -> Result<Summary, Violation> {
    let exe = std::env::current_exe()
        .map_err(|e| Violation::Child { run: 0, msg: format!("current_exe: {e}") })?;
    let bin_test_path = test_path.split_once("::").map(|(_, rest)| rest).unwrap_or(test_path);

    let mut killed = 0u32;
    let mut acked_rows_checked = 0u64;

    for run in 0..plan.runs {
        let dir = crash_dir(test_path, run);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| Violation::Child { run, msg: format!("mkdir {}: {e}", dir.display()) })?;

        let mut rng = SplitMix64::new(plan.seed ^ run as u64);
        let lockstep = !matches!(plan.kill, Kill::Random { .. });

        let mut cmd = Command::new(&exe);
        cmd.args([bin_test_path, "--exact", "--nocapture", "--test-threads=1"])
            .env("ADELIE_CRASH_ROLE", "child")
            .env("ADELIE_CRASH_DIR", &dir)
            .env("ADELIE_CRASH_BATCHES", plan.batches.to_string())
            .env("ADELIE_CRASH_ROWS", plan.rows_per_batch.to_string())
            .env("ADELIE_CRASH_LOCKSTEP", if lockstep { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Kill::Failpoint(name) = plan.kill {
            let nth = rng.range(1, plan.batches + 1);
            cmd.env("ADELIE_FAILPOINT", format!("{name}:{nth}"));
        }

        let mut child =
            cmd.spawn().map_err(|e| Violation::Child { run, msg: format!("spawn: {e}") })?;

        let outcome = match plan.kill {
            Kill::AtAck => drive_at_ack(&mut child, rng.range(1, plan.batches)),
            Kill::Failpoint(_) => drive_failpoint(&mut child),
            Kill::Random { max_delay_ms } => drive_random(&mut child, &mut rng, max_delay_ms),
        }
        .map_err(|msg| Violation::Child { run, msg })?;

        let status =
            child.wait().map_err(|e| Violation::Child { run, msg: format!("wait: {e}") })?;
        if !outcome.killed && !status.success() {
            return Err(Violation::Child { run, msg: format!("child exited with {status}") });
        }
        if outcome.killed {
            killed += 1;
        }

        let store = T::open(&dir)
            .map_err(|e| Violation::Reopen { run, err: e.to_string(), dir: dir.clone() })?;
        let rows = T::read_all(&store)
            .map_err(|e| Violation::Reopen { run, err: e.to_string(), dir: dir.clone() })?;

        verdict::verdict(outcome.acked, outcome.highest_sent, plan.rows_per_batch, &rows)
            .map_err(|d| to_violation(d, run, &dir))?;

        acked_rows_checked += outcome.acked * plan.rows_per_batch as u64;
        let _ = std::fs::remove_dir_all(&dir);
    }

    Ok(Summary { runs: plan.runs, killed, acked_rows_checked })
}

fn to_violation(defect: verdict::Defect, run: u32, dir: &Path) -> Violation {
    let dir = dir.to_path_buf();
    match defect {
        verdict::Defect::LostAck { batch } => Violation::LostAck { run, batch, dir },
        verdict::Defect::Torn { batch, present, expected } => {
            Violation::Torn { run, batch, present, expected, dir }
        }
        verdict::Defect::Phantom { batch } => Violation::Phantom { run, batch, dir },
        verdict::Defect::Duplicate { batch, row } => Violation::Duplicate { run, batch, row, dir },
    }
}

/// Answers `go` to every ack except the k-th, where it kills instead.
fn drive_at_ack(child: &mut ChildProcess, k: u64) -> Result<DriveOutcome, String> {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut highest_sent = None;
    let mut acked = 0u64;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("reading child stdout: {e}"))?;
        match child::parse_line(&line) {
            child::Line::Sent(b) => highest_sent = Some(b),
            child::Line::Ack(_) => {
                acked += 1;
                if acked == k {
                    let _ = child.kill();
                    return Ok(DriveOutcome { killed: true, acked, highest_sent });
                }
                writeln!(stdin, "go").map_err(|e| format!("writing go: {e}"))?;
                stdin.flush().map_err(|e| format!("flushing stdin: {e}"))?;
            }
            child::Line::Err(msg) => return Err(msg),
            child::Line::Fp(_) | child::Line::Other => {}
        }
    }
    Ok(DriveOutcome { killed: false, acked, highest_sent })
}

/// Answers `go` to every ack; kills on the named failpoint's line instead of on an ack.
fn drive_failpoint(child: &mut ChildProcess) -> Result<DriveOutcome, String> {
    let stdout = child.stdout.take().expect("piped stdout");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let mut highest_sent = None;
    let mut acked = 0u64;
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("reading child stdout: {e}"))?;
        match child::parse_line(&line) {
            child::Line::Sent(b) => highest_sent = Some(b),
            child::Line::Ack(_) => {
                acked += 1;
                writeln!(stdin, "go").map_err(|e| format!("writing go: {e}"))?;
                stdin.flush().map_err(|e| format!("flushing stdin: {e}"))?;
            }
            child::Line::Fp(_) => {
                let _ = child.kill();
                return Ok(DriveOutcome { killed: true, acked, highest_sent });
            }
            child::Line::Err(msg) => return Err(msg),
            child::Line::Other => {}
        }
    }
    Ok(DriveOutcome { killed: false, acked, highest_sent })
}

/// Not lockstep: a reader thread tracks progress while the main thread sleeps then kills.
fn drive_random(
    child: &mut ChildProcess,
    rng: &mut SplitMix64,
    max_delay_ms: u64,
) -> Result<DriveOutcome, String> {
    let stdout = child.stdout.take().expect("piped stdout");
    let observed = Arc::new(Mutex::new((None::<u64>, 0u64)));
    let err_msg: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let observed_reader = Arc::clone(&observed);
    let err_reader = Arc::clone(&err_msg);
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            match child::parse_line(&line) {
                child::Line::Sent(b) => observed_reader.lock().unwrap().0 = Some(b),
                child::Line::Ack(_) => observed_reader.lock().unwrap().1 += 1,
                child::Line::Err(msg) => *err_reader.lock().unwrap() = Some(msg),
                child::Line::Fp(_) | child::Line::Other => {}
            }
        }
    });

    let delay = if max_delay_ms > 0 { rng.range(0, max_delay_ms) } else { 0 };
    thread::sleep(Duration::from_millis(delay));
    let killed = child.kill().is_ok();
    reader.join().map_err(|_| "reader thread panicked".to_string())?;

    if let Some(msg) = err_msg.lock().unwrap().take() {
        return Err(msg);
    }
    let (highest_sent, acked) = *observed.lock().unwrap();
    Ok(DriveOutcome { killed, acked, highest_sent })
}
