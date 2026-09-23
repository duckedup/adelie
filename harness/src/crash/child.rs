//! The child side of the crash harness: the workload loop, and the stdout/stdin line
//! protocol the parent's readers in `mod.rs` parse.

use std::io::{self, BufRead, Write};
use std::path::Path;

use super::{CrashTarget, Row};

/// Prefixes every protocol line. libtest prints `test <name> ... ` with no newline before the
/// test body runs, so the child's first line shares a physical line with it: the parser looks
/// for the marker anywhere in the line, never at its start.
pub(super) const MARK: &str = "@@adelie-crash ";

/// One parsed protocol line. Anything else (libtest's own output) is `Other` and ignored.
pub(super) enum Line {
    Sent(u64),
    Ack,
    Err(String),
    Fp,
    Other,
}

pub(super) fn parse_line(line: &str) -> Line {
    let Some(at) = line.find(MARK) else {
        return Line::Other;
    };
    let line = &line[at + MARK.len()..];
    if let Some(rest) = line.strip_prefix("sent ") {
        return rest.trim().parse().map(Line::Sent).unwrap_or(Line::Other);
    }
    if let Some(rest) = line.strip_prefix("ack ") {
        return rest
            .trim()
            .parse::<u64>()
            .map_or(Line::Other, |_| Line::Ack);
    }
    if let Some(rest) = line.strip_prefix("err ") {
        return Line::Err(rest.to_string());
    }
    if line.starts_with("fp ") {
        return Line::Fp;
    }
    Line::Other
}

/// Runs the workload in-process and exits: the child never returns into libtest.
pub(super) fn run_child<T: CrashTarget>(
    dir: &Path,
    batches: u64,
    rows_per_batch: u32,
    lockstep: bool,
) -> ! {
    match workload::<T>(dir, batches, rows_per_batch, lockstep) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            println!("{MARK}err {e}");
            let _ = io::stdout().flush();
            std::process::exit(3);
        }
    }
}

fn workload<T: CrashTarget>(
    dir: &Path,
    batches: u64,
    rows_per_batch: u32,
    lockstep: bool,
) -> io::Result<()> {
    let mut store = T::open(dir)?;
    let stdin = io::stdin();
    let mut input = stdin.lock().lines();

    for b in 0..batches {
        println!("{MARK}sent {b}");
        io::stdout().flush()?;
        let batch: Vec<Row> = (0..rows_per_batch).map(|i| (b, i)).collect();
        T::write(&mut store, &batch)?;
        println!("{MARK}ack {b}");
        io::stdout().flush()?;
        if lockstep {
            match input.next() {
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => break, // parent closed stdin (already killing us)
            }
        }
        T::between(&mut store, b)?;
    }
    Ok(())
}
