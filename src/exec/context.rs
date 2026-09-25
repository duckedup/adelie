//! Per-query budget, cancellation and timeout (SPEC §7, §13): `ExecContext` is the one thing
//! every operator holds, cheap to clone (`Arc`s inside).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::types::DataType;

use super::{BatchError, ColumnError};

/// Per-query knobs: memory budget, thread count, timeout and cancellation.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    pub memory_limit: usize,
    pub threads: usize,
    pub timeout: Option<Duration>,
    pub cancel: CancelToken,
}

impl Default for ExecOptions {
    fn default() -> Self {
        ExecOptions {
            memory_limit: 1 << 30,
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
            timeout: None,
            cancel: CancelToken::new(),
        }
    }
}

/// A shared cancel flag; `clone()` shares the same underlying flag.
#[derive(Debug, Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        CancelToken(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// A byte budget shared by every `Reservation` drawn from one `ExecContext`.
#[derive(Debug)]
struct MemoryBudget {
    limit: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}

impl MemoryBudget {
    fn new(limit: usize) -> Self {
        MemoryBudget {
            limit,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Fails, leaving `used` unchanged, if `used + bytes` would pass `limit`. `usize::MAX` is
    /// a real limit, so the addition is checked, not wrapping.
    fn grow(&self, bytes: usize) -> Result<(), ExecError> {
        let limit = self.limit;
        match self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                used.checked_add(bytes).filter(|&next| next <= limit)
            }) {
            Ok(prev) => {
                self.peak.fetch_max(prev + bytes, Ordering::SeqCst);
                Ok(())
            }
            Err(used) => Err(ExecError::BudgetExceeded {
                requested: bytes,
                used,
                limit,
            }),
        }
    }

    /// Releases `bytes`, saturating at zero (a caller never releases more than it holds, but
    /// this stays safe if rounding ever does).
    fn shrink(&self, bytes: usize) {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                Some(used.saturating_sub(bytes))
            })
            .expect("the closure above always returns Some");
    }
}

/// Per-query state: the memory budget, cancellation and an optional deadline. Cheap to
/// clone (`Arc`s inside); `Send + Sync`.
#[derive(Debug, Clone)]
pub struct ExecContext {
    budget: Arc<MemoryBudget>,
    cancel: CancelToken,
    deadline: Option<Instant>,
    timeout: Option<Duration>,
}

impl ExecContext {
    pub fn new(opts: &ExecOptions) -> ExecContext {
        ExecContext {
            budget: Arc::new(MemoryBudget::new(opts.memory_limit)),
            cancel: opts.cancel.clone(),
            deadline: opts.timeout.map(|d| Instant::now() + d),
            timeout: opts.timeout,
        }
    }

    /// `usize::MAX` budget, no deadline: for callers outside a user query (`View::scan`, the
    /// flush sort) that still want the same operator machinery.
    pub fn unlimited() -> ExecContext {
        ExecContext {
            budget: Arc::new(MemoryBudget::new(usize::MAX)),
            cancel: CancelToken::new(),
            deadline: None,
            timeout: None,
        }
    }

    /// `Cancelled` first, then `Timeout` once `Instant::now() >= deadline`. A `Duration::ZERO`
    /// timeout therefore times out on the very first call: deliberate (tests rely on it).
    pub fn check(&self) -> Result<(), ExecError> {
        if self.cancel.is_cancelled() {
            return Err(ExecError::Cancelled);
        }
        if let Some(deadline) = self.deadline
            && Instant::now() >= deadline
        {
            return Err(ExecError::Timeout {
                after: self.timeout.expect("a deadline implies a timeout"),
            });
        }
        Ok(())
    }

    pub fn reserve(&self, bytes: usize) -> Result<Reservation, ExecError> {
        self.budget.grow(bytes)?;
        Ok(Reservation {
            budget: Arc::clone(&self.budget),
            bytes,
        })
    }

    pub fn memory_used(&self) -> usize {
        self.budget.used.load(Ordering::SeqCst)
    }

    pub fn memory_limit(&self) -> usize {
        self.budget.limit
    }

    pub fn peak_memory(&self) -> usize {
        self.budget.peak.load(Ordering::SeqCst)
    }
}

/// A held slice of a query's memory budget. `Drop` returns its bytes.
#[derive(Debug)]
pub struct Reservation {
    budget: Arc<MemoryBudget>,
    bytes: usize,
}

impl Reservation {
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn grow(&mut self, bytes: usize) -> Result<(), ExecError> {
        self.budget.grow(bytes)?;
        self.bytes += bytes;
        Ok(())
    }

    /// Saturating: releasing more than is held just releases everything held.
    pub fn shrink(&mut self, bytes: usize) {
        let bytes = bytes.min(self.bytes);
        self.budget.shrink(bytes);
        self.bytes -= bytes;
    }

    /// Grows or shrinks so `bytes()` becomes exactly `bytes`.
    pub fn resize(&mut self, bytes: usize) -> Result<(), ExecError> {
        if bytes > self.bytes {
            self.grow(bytes - self.bytes)
        } else {
            self.shrink(self.bytes - bytes);
            Ok(())
        }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        self.budget.shrink(self.bytes);
    }
}

/// Everything a query can fail with (SPEC §7, §13).
#[derive(Debug)]
pub enum ExecError {
    BudgetExceeded {
        requested: usize,
        used: usize,
        limit: usize,
    },
    Cancelled,
    Timeout {
        after: Duration,
    },
    /// An ill-typed or malformed plan: a binder bug, not the data's fault.
    Plan(String),
    Cast {
        value: String,
        to: DataType,
    },
    /// Names the operation, e.g. "sum(INT64)", "INT64 * INT64".
    Overflow(String),
    /// A bad runtime argument or malformed encoded state.
    Invalid(String),
    /// The `TableSource`'s own error.
    Source(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::BudgetExceeded {
                requested,
                used,
                limit,
            } => write!(
                f,
                "memory budget exceeded: requested {requested} bytes, {used} used of {limit}"
            ),
            ExecError::Cancelled => write!(f, "query cancelled"),
            ExecError::Timeout { after } => write!(f, "query timed out after {after:?}"),
            ExecError::Plan(msg) => write!(f, "invalid plan: {msg}"),
            ExecError::Cast { value, to } => write!(f, "cannot cast {value} to {to}"),
            ExecError::Overflow(op) => write!(f, "overflow in {op}"),
            ExecError::Invalid(msg) => write!(f, "invalid: {msg}"),
            ExecError::Source(e) => write!(f, "source error: {e}"),
        }
    }
}

impl std::error::Error for ExecError {}

impl From<ColumnError> for ExecError {
    fn from(e: ColumnError) -> Self {
        ExecError::Plan(e.to_string())
    }
}

impl From<BatchError> for ExecError {
    fn from(e: BatchError) -> Self {
        ExecError::Plan(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(limit: usize) -> ExecOptions {
        ExecOptions {
            memory_limit: limit,
            ..ExecOptions::default()
        }
    }

    fn budget_numbers(e: &ExecError) -> (usize, usize, usize) {
        match e {
            ExecError::BudgetExceeded {
                requested,
                used,
                limit,
            } => (*requested, *used, *limit),
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
    }

    #[test]
    fn reserve_up_to_the_limit_succeeds_one_byte_more_is_budget_exceeded() {
        let ctx = ExecContext::new(&opts(100));
        let r = ctx.reserve(100).unwrap();
        assert_eq!(ctx.memory_used(), 100);
        let err = ctx.reserve(1).unwrap_err();
        assert_eq!(budget_numbers(&err), (1, 100, 100));
        drop(r);
    }

    #[test]
    fn dropping_a_reservation_returns_its_bytes() {
        let ctx = ExecContext::new(&opts(100));
        {
            let _r = ctx.reserve(50).unwrap();
            assert_eq!(ctx.memory_used(), 50);
        }
        assert_eq!(ctx.memory_used(), 0);
    }

    #[test]
    fn resize_down_then_up() {
        let ctx = ExecContext::new(&opts(100));
        let mut r = ctx.reserve(50).unwrap();
        r.resize(20).unwrap();
        assert_eq!(ctx.memory_used(), 20);
        r.resize(80).unwrap();
        assert_eq!(ctx.memory_used(), 80);
        assert_eq!(r.bytes(), 80);
    }

    #[test]
    fn peak_memory_is_the_high_water_mark() {
        let ctx = ExecContext::new(&opts(100));
        let mut r = ctx.reserve(80).unwrap();
        r.shrink(60);
        assert_eq!(ctx.memory_used(), 20);
        assert_eq!(ctx.peak_memory(), 80);
    }

    #[test]
    fn check_after_cancel_is_cancelled() {
        let ctx = ExecContext::new(&opts(100));
        ctx.cancel.cancel();
        assert!(matches!(ctx.check(), Err(ExecError::Cancelled)));
    }

    #[test]
    fn zero_timeout_times_out_immediately() {
        let mut o = opts(100);
        o.timeout = Some(Duration::ZERO);
        let ctx = ExecContext::new(&o);
        assert!(matches!(ctx.check(), Err(ExecError::Timeout { .. })));
    }

    #[test]
    fn no_timeout_never_times_out() {
        let ctx = ExecContext::new(&opts(100));
        assert!(ctx.check().is_ok());
    }

    #[test]
    fn unlimited_never_fails_a_one_gib_reservation() {
        let ctx = ExecContext::unlimited();
        let r = ctx.reserve(1 << 30).unwrap();
        assert_eq!(r.bytes(), 1 << 30);
    }
}
