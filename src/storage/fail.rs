//! `point`: adelie's own failpoint, the E4 equivalent of `harness::crash::failpoint`
//! (`harness/src/crash/mod.rs:152-180`). Same env protocol, so the crash harness drives it.

use std::io::{BufRead, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Prefixes the protocol line the parent's reader looks for (`harness/src/crash/child.rs:11`).
const MARK: &str = "@@adelie-crash ";

static HITS: AtomicU64 = AtomicU64::new(0);
static SPEC: OnceLock<Option<(String, u64)>> = OnceLock::new();

/// Parses `ADELIE_CRASH_ROLE`/`ADELIE_FAILPOINT` once per process and caches the result, so a
/// release build pays one env load per call instead of two.
fn spec() -> &'static Option<(String, u64)> {
    SPEC.get_or_init(|| {
        if std::env::var("ADELIE_CRASH_ROLE").ok().as_deref() != Some("child") {
            return None;
        }
        let raw = std::env::var("ADELIE_FAILPOINT").ok()?;
        let (name, nth) = raw.split_once(':')?;
        let nth: u64 = nth.parse().ok()?;
        Some((name.to_string(), nth))
    })
}

/// Fires `name`'s nth hit under `ADELIE_FAILPOINT=name:nth`: prints the marker line, flushes
/// stdout, then blocks on one stdin line. A no-op everywhere else, including other hits of
/// `name` and every hit of a different name.
pub(crate) fn point(name: &str) {
    let Some((fp_name, nth)) = spec() else {
        return;
    };
    if fp_name.as_str() != name {
        return;
    }
    if HITS.fetch_add(1, Ordering::SeqCst) + 1 != *nth {
        return;
    }
    println!("{MARK}fp {name}");
    let _ = std::io::stdout().flush();
    let mut discard = String::new();
    let _ = std::io::stdin().lock().read_line(&mut discard);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outside a crash child (the normal test environment), every call is a no-op: it must
    /// return immediately and never block on stdin.
    #[test]
    fn is_a_no_op_outside_a_crash_child() {
        point("anything");
        point("manifest.pre_rename");
    }
}
