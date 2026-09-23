//! The write buffer's flush trigger and `FlushTicket` (SPEC §6). The pending/in-flight buffers
//! themselves are plain `BTreeMap<TableName, Vec<Arc<Batch>>>`s, held in `Shared::state`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::StoreOptions;

/// Pure: whether a flush should run now, given the buffer's current shape. `force` is set by
/// an explicit `Store::flush()` call or by `close`/`Drop`.
pub(crate) fn should_flush(
    rows: usize,
    bytes: usize,
    since_first: Option<Duration>,
    opts: &StoreOptions,
    force: bool,
) -> bool {
    force
        || rows >= opts.max_rows
        || bytes >= opts.max_bytes
        || since_first.is_some_and(|elapsed| elapsed >= opts.flush_interval)
}

/// One flush's outcome, shared by every writer whose rows land in it (group commit). A writer
/// clones the `Arc<FlushTicket>` under `state`'s lock, then calls `wait()` outside it.
pub(crate) struct FlushTicket {
    done: Mutex<Option<Result<u64, String>>>,
    cv: Condvar,
}

impl FlushTicket {
    pub(crate) fn new() -> Arc<FlushTicket> {
        Arc::new(FlushTicket {
            done: Mutex::new(None),
            cv: Condvar::new(),
        })
    }

    /// Blocks until `resolve` is called, then returns its result.
    pub(crate) fn wait(&self) -> Result<u64, String> {
        let mut guard = self.done.lock().unwrap();
        while guard.is_none() {
            guard = self.cv.wait(guard).unwrap();
        }
        guard.clone().unwrap()
    }

    /// Sets the result and wakes every waiter. Called exactly once per ticket.
    pub(crate) fn resolve(&self, result: Result<u64, String>) {
        *self.done.lock().unwrap() = Some(result);
        self.cv.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> StoreOptions {
        StoreOptions {
            max_rows: 100,
            max_bytes: 1000,
            flush_interval: Duration::from_millis(250),
            ..StoreOptions::default()
        }
    }

    #[test]
    fn none_of_the_triggers_is_false() {
        assert!(!should_flush(
            1,
            1,
            Some(Duration::from_millis(1)),
            &opts(),
            false
        ));
    }

    #[test]
    fn the_rows_limit_triggers() {
        assert!(should_flush(100, 1, None, &opts(), false));
        assert!(!should_flush(99, 1, None, &opts(), false));
    }

    #[test]
    fn the_bytes_limit_triggers() {
        assert!(should_flush(1, 1000, None, &opts(), false));
        assert!(!should_flush(1, 999, None, &opts(), false));
    }

    #[test]
    fn the_interval_triggers() {
        assert!(should_flush(
            1,
            1,
            Some(Duration::from_millis(250)),
            &opts(),
            false
        ));
        assert!(!should_flush(
            1,
            1,
            Some(Duration::from_millis(249)),
            &opts(),
            false
        ));
        assert!(!should_flush(1, 1, None, &opts(), false));
    }

    #[test]
    fn force_triggers_regardless_of_shape() {
        assert!(should_flush(0, 0, None, &opts(), true));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns a thread
    fn a_ticket_delivers_its_result_to_every_waiter() {
        let ticket = FlushTicket::new();
        let t = {
            let ticket = ticket.clone();
            std::thread::spawn(move || ticket.wait())
        };
        std::thread::sleep(Duration::from_millis(10));
        ticket.resolve(Ok(7));
        assert_eq!(t.join().unwrap(), Ok(7));
        assert_eq!(ticket.wait(), Ok(7));
    }
}
