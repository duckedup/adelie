//! Migration jobs and retired table entries (SPEC §19, D0012, D0013): store-wide manifest state.

use super::{TableEntry, TableId};

/// One running migration (SPEC §19 "Rebuild and swap"). Lives in the manifest, so it survives a
/// crash; `Store::run_job` advances it one bounded step at a time.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Store-wide, from `Manifest::next_job_id`; never reused.
    pub id: u64,
    /// The live table being migrated (its name is `target.name`).
    pub source: TableId,
    /// The manifest version the job was planned against: SPEC §19 step 1's V.
    pub snapshot: u64,
    /// The new definition under its own freshly reserved table id. `segments` holds what has
    /// been built so far: reused source segments (projected onto the new schema) and rewritten
    /// ones (files under `target.dir()`). `tombstones` stays empty until the swap copies them.
    pub target: TableEntry,
    /// Source segment ids already carried into `target`, ascending.
    pub handled: Vec<u64>,
    pub reused: u64,
    pub rewritten: u64,
}

/// Why a table entry left the live list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireReason {
    /// Replaced by a migration's swap; `ALTER TABLE … REVERT` brings it back.
    Swapped,
    /// Replaced by a REVERT; kept only so its rewritten files outlive the grace period.
    Reverted,
    /// `DROP TABLE`; `UNDROP TABLE` brings it back.
    Dropped,
}

/// A table entry kept for the grace period (SPEC §19, default 24 hours), then expired by GC.
#[derive(Debug, Clone, PartialEq)]
pub struct Retired {
    pub entry: TableEntry,
    pub reason: RetireReason,
    pub retired_at_ms: u64,
    /// The manifest version of the commit that retired it.
    pub version: u64,
    /// `Swapped` only: the successor's segment ids at the swap, ascending. REVERT is refused
    /// once any of them is no longer live (compacted or truncated since). Empty otherwise.
    pub successor_segments: Vec<u64>,
}
