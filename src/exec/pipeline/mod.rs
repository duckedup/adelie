//! The morsel scheduler (SPEC §7): a pool of `std::thread::scope` workers pulling morsels
//! from a shared index and running one pipeline (operators + sink) to a partial result.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use crate::exec::operator::{Operator, Sink};
use crate::exec::{Batch, ExecContext, ExecError, MorselSource};

/// Runs `source` through a fresh operator chain and sink per worker, morsel by morsel, and
/// merges the partial sinks. One worker runs inline on the caller's thread and spawns nothing.
pub(crate) fn run<S, F>(
    source: &dyn MorselSource,
    ctx: &ExecContext,
    threads: usize,
    make: F,
) -> Result<S, ExecError>
where
    S: Sink,
    F: Fn() -> Result<(Vec<Box<dyn Operator>>, S), ExecError> + Sync,
{
    let total = source.morsels();
    let workers = threads.max(1).min(total.max(1));

    if workers == 1 {
        let (mut ops, mut sink) = make()?;
        let next = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        run_worker(source, ctx, total, &next, &stop, &mut ops, &mut sink)?;
        return Ok(sink);
    }

    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let error: Mutex<Option<ExecError>> = Mutex::new(None);

    let outcomes: Vec<std::thread::Result<Option<S>>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let built = make();
                    let (mut ops, mut sink) = match built {
                        Ok(v) => v,
                        Err(e) => {
                            record_error(&stop, &error, e);
                            return None;
                        }
                    };
                    match run_worker(source, ctx, total, &next, &stop, &mut ops, &mut sink) {
                        Ok(()) => Some(sink),
                        Err(e) => {
                            record_error(&stop, &error, e);
                            None
                        }
                    }
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join()).collect()
    });

    let mut panicked = None;
    let mut sinks = Vec::with_capacity(workers);
    for outcome in outcomes {
        match outcome {
            Ok(Some(sink)) => sinks.push(sink),
            Ok(None) => {}
            Err(payload) => {
                if panicked.is_none() {
                    panicked = Some(payload);
                }
            }
        }
    }
    if let Some(payload) = panicked {
        std::panic::resume_unwind(payload);
    }
    if let Some(e) = error.into_inner().unwrap() {
        return Err(e);
    }

    let mut iter = sinks.into_iter();
    let mut merged = iter.next().expect("at least one worker ran");
    for other in iter {
        merged.merge(ctx, other)?;
    }
    Ok(merged)
}

/// Sets the shared stop flag and records `e` only if no error is recorded yet: "the first
/// error", per worker arrival order at this lock, wins.
fn record_error(stop: &AtomicBool, error: &Mutex<Option<ExecError>>, e: ExecError) {
    stop.store(true, Ordering::Relaxed);
    let mut slot = error.lock().unwrap();
    if slot.is_none() {
        *slot = Some(e);
    }
}

/// One worker's loop: claim morsels via `next` until they run out, `stop` is set, or this
/// worker's own chain is done; then `finish` every operator, cascading into `sink`.
fn run_worker<S: Sink>(
    source: &dyn MorselSource,
    ctx: &ExecContext,
    total: usize,
    next: &AtomicUsize,
    stop: &AtomicBool,
    ops: &mut [Box<dyn Operator>],
    sink: &mut S,
) -> Result<(), ExecError> {
    let mut chain_done = false;
    while !chain_done && !stop.load(Ordering::Relaxed) {
        let i = next.fetch_add(1, Ordering::Relaxed);
        if i >= total {
            break;
        }
        ctx.check()?;
        let batches = source.read(i, ctx)?;
        for batch in batches {
            ctx.check()?;
            push_through(ctx, ops, sink, vec![batch])?;
            if sink.done() || ops.iter().any(|op| op.done()) {
                chain_done = true;
                break;
            }
        }
    }
    finish_chain(ctx, ops, sink)
}

/// Pushes `batches` through `ops` in order, each operator's `out` feeding the next, then the
/// last stage's output into `sink`.
fn push_through<S: Sink>(
    ctx: &ExecContext,
    ops: &mut [Box<dyn Operator>],
    sink: &mut S,
    batches: Vec<Batch>,
) -> Result<(), ExecError> {
    let mut current = batches;
    for op in ops.iter_mut() {
        let mut next = Vec::with_capacity(current.len());
        for batch in current {
            op.push(ctx, batch, &mut next)?;
        }
        current = next;
    }
    for batch in current {
        sink.push(ctx, batch)?;
    }
    Ok(())
}

/// End of input: `finish` each operator in turn, cascading its output through the operators
/// after it (a normal `push`) and finally into `sink`.
fn finish_chain<S: Sink>(
    ctx: &ExecContext,
    ops: &mut [Box<dyn Operator>],
    sink: &mut S,
) -> Result<(), ExecError> {
    for i in 0..ops.len() {
        let mut produced = Vec::new();
        ops[i].finish(ctx, &mut produced)?;
        push_through(ctx, &mut ops[i + 1..], sink, produced)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::run;
    use crate::exec::operator::{Operator, Sink};
    use crate::exec::{
        Batch, BatchSource, CancelToken, Column, ExecContext, ExecError, ExecOptions, Field,
        MorselSource,
    };
    use crate::types::{DataType, Value};

    fn opts(threads: usize) -> ExecOptions {
        ExecOptions {
            memory_limit: 1 << 30,
            threads,
            timeout: None,
            cancel: CancelToken::new(),
        }
    }

    fn int_field() -> Field {
        Field {
            name: "v".to_string(),
            ty: DataType::Int64,
        }
    }

    fn int_batch(vals: &[i64]) -> Batch {
        let values: Vec<Value> = vals.iter().map(|v| Value::Int64(*v)).collect();
        let col = Column::from_values(&DataType::Int64, &values).unwrap();
        Batch::new(vec![int_field()], vec![col]).unwrap()
    }

    /// Sums the single INT64 column across every pushed batch; `merge` adds the two totals.
    #[derive(Debug, Default)]
    struct SumSink {
        total: i64,
        pushes: usize,
    }

    impl Sink for SumSink {
        fn push(&mut self, _ctx: &ExecContext, batch: Batch) -> Result<(), ExecError> {
            let col = batch.column(0);
            for i in 0..batch.rows() {
                if let Value::Int64(v) = col.get(i) {
                    self.total += v;
                }
            }
            self.pushes += 1;
            Ok(())
        }

        fn merge(&mut self, _ctx: &ExecContext, other: Self) -> Result<(), ExecError> {
            self.total += other.total;
            self.pushes += other.pushes;
            Ok(())
        }

        fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
            Ok(Vec::new())
        }
    }

    /// A source that counts its own `read` calls, for tests that need to see how far a run
    /// got. Optionally fails at one morsel and/or sleeps per read.
    struct CountingSource {
        fields: Vec<Field>,
        batches: Vec<Batch>,
        reads: AtomicUsize,
        fail_at: Option<usize>,
        sleep: Duration,
    }

    impl CountingSource {
        fn new(batches: Vec<Batch>) -> Self {
            CountingSource {
                fields: vec![int_field()],
                batches,
                reads: AtomicUsize::new(0),
                fail_at: None,
                sleep: Duration::ZERO,
            }
        }
    }

    impl MorselSource for CountingSource {
        fn fields(&self) -> &[Field] {
            &self.fields
        }

        fn morsels(&self) -> usize {
            self.batches.len()
        }

        fn read(&self, morsel: usize, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if !self.sleep.is_zero() {
                std::thread::sleep(self.sleep);
            }
            if self.fail_at == Some(morsel) {
                return Err(ExecError::Invalid(format!("morsel {morsel} fails")));
            }
            Ok(vec![self.batches[morsel].clone()])
        }
    }

    fn make_batches(n: usize) -> Vec<Batch> {
        (0..n).map(|i| int_batch(&[i as i64])).collect()
    }

    #[test]
    fn one_and_many_threads_agree_on_the_total() {
        let fields = vec![int_field()];
        let batches = make_batches(20);
        let expected: i64 = (0..20).sum();
        for threads in [1, 2, 8] {
            let source = BatchSource::new(fields.clone(), batches.clone());
            let ctx = ExecContext::new(&opts(threads));
            let sink: SumSink =
                run(&source, &ctx, threads, || Ok((Vec::new(), SumSink::default()))).unwrap();
            assert_eq!(sink.total, expected, "threads={threads}");
        }
    }

    /// A source whose `read` waits to observe a second concurrent `read`, or falsifies by
    /// timing out — proof the scheduler actually overlaps work, not just accepts N threads.
    struct ConcurrentSource {
        fields: Vec<Field>,
        morsels: usize,
        in_flight: AtomicUsize,
        seen_two: AtomicBool,
    }

    impl MorselSource for ConcurrentSource {
        fn fields(&self) -> &[Field] {
            &self.fields
        }

        fn morsels(&self) -> usize {
            self.morsels
        }

        fn read(&self, _morsel: usize, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            let start = Instant::now();
            while self.in_flight.load(Ordering::SeqCst) < 2 && start.elapsed() < Duration::from_secs(5)
            {
                std::thread::yield_now();
            }
            if self.in_flight.load(Ordering::SeqCst) >= 2 {
                self.seen_two.store(true, Ordering::SeqCst);
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![int_batch(&[1])])
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads
    fn parallelism_is_real() {
        let source = ConcurrentSource {
            fields: vec![int_field()],
            morsels: 4,
            in_flight: AtomicUsize::new(0),
            seen_two: AtomicBool::new(false),
        };
        let ctx = ExecContext::new(&opts(2));
        let _: SumSink = run(&source, &ctx, 2, || Ok((Vec::new(), SumSink::default()))).unwrap();
        assert!(source.seen_two.load(Ordering::SeqCst), "no overlap observed");
    }

    #[test]
    fn one_thread_runs_inline_on_the_caller() {
        struct ThreadIdSource {
            fields: Vec<Field>,
            seen: Mutex<Option<std::thread::ThreadId>>,
        }
        impl MorselSource for ThreadIdSource {
            fn fields(&self) -> &[Field] {
                &self.fields
            }
            fn morsels(&self) -> usize {
                3
            }
            fn read(&self, _morsel: usize, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
                *self.seen.lock().unwrap() = Some(std::thread::current().id());
                Ok(vec![int_batch(&[1])])
            }
        }

        let source = ThreadIdSource {
            fields: vec![int_field()],
            seen: Mutex::new(None),
        };
        let ctx = ExecContext::new(&opts(1));
        let _: SumSink = run(&source, &ctx, 1, || Ok((Vec::new(), SumSink::default()))).unwrap();
        assert_eq!(
            *source.seen.lock().unwrap(),
            Some(std::thread::current().id())
        );
    }

    /// Cancels the shared token after its first batch, to test the check between batches.
    struct CancelAfterFirst {
        token: CancelToken,
        fired: bool,
    }

    impl Operator for CancelAfterFirst {
        fn push(
            &mut self,
            _ctx: &ExecContext,
            batch: Batch,
            out: &mut Vec<Batch>,
        ) -> Result<(), ExecError> {
            if !self.fired {
                self.token.cancel();
                self.fired = true;
            }
            out.push(batch);
            Ok(())
        }
    }

    #[test]
    fn cancel_between_batches_stops_the_run() {
        let token = CancelToken::new();
        let source = CountingSource::new(make_batches(10));
        let ctx = ExecContext::new(&ExecOptions {
            memory_limit: 1 << 30,
            threads: 1,
            timeout: None,
            cancel: token.clone(),
        });
        let result: Result<SumSink, ExecError> = run(&source, &ctx, 1, || {
            let op: Box<dyn Operator> = Box::new(CancelAfterFirst {
                token: token.clone(),
                fired: false,
            });
            Ok((vec![op], SumSink::default()))
        });
        assert!(matches!(result, Err(ExecError::Cancelled)));
        assert!(source.reads.load(Ordering::SeqCst) < 10);
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads
    fn cancel_between_batches_stops_every_worker() {
        let token = CancelToken::new();
        let source = CountingSource::new(make_batches(10));
        let ctx = ExecContext::new(&ExecOptions {
            memory_limit: 1 << 30,
            threads: 4,
            timeout: None,
            cancel: token.clone(),
        });
        let result: Result<SumSink, ExecError> = run(&source, &ctx, 4, || {
            let op: Box<dyn Operator> = Box::new(CancelAfterFirst {
                token: token.clone(),
                fired: false,
            });
            Ok((vec![op], SumSink::default()))
        });
        assert!(matches!(result, Err(ExecError::Cancelled)));
    }

    #[test]
    fn zero_timeout_stops_before_any_read() {
        let source = CountingSource::new(make_batches(5));
        let ctx = ExecContext::new(&ExecOptions {
            memory_limit: 1 << 30,
            threads: 1,
            timeout: Some(Duration::ZERO),
            cancel: CancelToken::new(),
        });
        let result: Result<SumSink, ExecError> =
            run(&source, &ctx, 1, || Ok((Vec::new(), SumSink::default())));
        assert!(matches!(result, Err(ExecError::Timeout { .. })));
        assert_eq!(source.reads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn a_failing_morsel_gives_exactly_that_error() {
        let mut source = CountingSource::new(make_batches(5));
        source.fail_at = Some(2);
        let ctx = ExecContext::new(&opts(1));
        let result: Result<SumSink, ExecError> =
            run(&source, &ctx, 1, || Ok((Vec::new(), SumSink::default())));
        match result {
            Err(ExecError::Invalid(msg)) => assert_eq!(msg, "morsel 2 fails"),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads, sleeps
    fn a_failing_morsel_stops_the_other_workers() {
        let mut source = CountingSource::new(make_batches(1000));
        source.fail_at = Some(2);
        source.sleep = Duration::from_millis(1);
        let ctx = ExecContext::new(&opts(4));
        let result: Result<SumSink, ExecError> =
            run(&source, &ctx, 4, || Ok((Vec::new(), SumSink::default())));
        assert!(result.is_err());
        assert!(source.reads.load(Ordering::SeqCst) < 500);
    }

    /// Reports `done()` once it has absorbed 2 batches, to test that a worker stops pulling
    /// morsels as soon as its sink is satisfied.
    #[derive(Default)]
    struct DoneAfterTwo {
        pushes: usize,
    }

    impl Sink for DoneAfterTwo {
        fn push(&mut self, _ctx: &ExecContext, _batch: Batch) -> Result<(), ExecError> {
            self.pushes += 1;
            Ok(())
        }
        fn merge(&mut self, _ctx: &ExecContext, other: Self) -> Result<(), ExecError> {
            self.pushes += other.pushes;
            Ok(())
        }
        fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
            Ok(Vec::new())
        }
        fn done(&self) -> bool {
            self.pushes >= 2
        }
    }

    #[test]
    fn early_stop_when_the_sink_is_done() {
        let source = CountingSource::new(make_batches(10));
        let ctx = ExecContext::new(&opts(1));
        let _: DoneAfterTwo =
            run(&source, &ctx, 1, || Ok((Vec::new(), DoneAfterTwo::default()))).unwrap();
        assert!(source.reads.load(Ordering::SeqCst) <= 2);
    }

    #[test]
    fn zero_morsels_calls_make_once_and_returns_an_empty_sink() {
        let source = BatchSource::new(vec![int_field()], Vec::new());
        let ctx = ExecContext::new(&opts(4));
        let calls = AtomicUsize::new(0);
        let sink: SumSink = run(&source, &ctx, 4, || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok((Vec::new(), SumSink::default()))
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.total, 0);
        assert_eq!(sink.pushes, 0);
    }

    /// Tags itself with a unique id on construction and logs the id of every sink it
    /// absorbs, so a run can be checked for exactly-once, complete merging.
    struct TaggedSink {
        id: u32,
        log: std::sync::Arc<Mutex<Vec<u32>>>,
    }

    impl Sink for TaggedSink {
        fn push(&mut self, _ctx: &ExecContext, _batch: Batch) -> Result<(), ExecError> {
            Ok(())
        }
        fn merge(&mut self, _ctx: &ExecContext, other: Self) -> Result<(), ExecError> {
            self.log.lock().unwrap().push(other.id);
            Ok(())
        }
        fn finish(self, _ctx: &ExecContext) -> Result<Vec<Batch>, ExecError> {
            Ok(Vec::new())
        }
    }

    #[test]
    #[cfg_attr(miri, ignore)] // spawns threads
    fn merge_absorbs_every_worker_exactly_once() {
        let source = BatchSource::new(vec![int_field()], make_batches(4));
        let ctx = ExecContext::new(&opts(4));
        let next_id = AtomicUsize::new(0);
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let merged: TaggedSink = run(&source, &ctx, 4, || {
            let id = next_id.fetch_add(1, Ordering::SeqCst) as u32;
            Ok((
                Vec::new(),
                TaggedSink {
                    id,
                    log: log.clone(),
                },
            ))
        })
        .unwrap();

        let log = log.lock().unwrap();
        assert_eq!(log.len(), 3, "expected 3 merges for 4 workers");
        let mut all: Vec<u32> = log.clone();
        all.push(merged.id);
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2, 3]);
    }
}
