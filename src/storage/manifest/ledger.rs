//! `MigrationRecord`: one applied versioned-migration file (SPEC §19 `adelie migrate`, D0014).
//! The manifest-level stand-in for `adelie.migrations` until system tables exist.

#[derive(Debug, Clone, PartialEq)]
pub struct MigrationRecord {
    /// The file name, e.g. `0001_init.sql`.
    pub name: String,
    /// XXH64 (seed 0) of the file's bytes.
    pub checksum: u64,
    pub applied_at_ms: u64,
}
