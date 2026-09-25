//! Migration ledger (SPEC §19 `adelie migrate`, D0014): applies a directory of numbered `.sql`
//! files at least once each, recording every applied file in the manifest so a later run only
//! runs what's new. Not `migrate.rs`/`Store::migrate`, which is the ALTER job runner.

use std::path::Path;

use crate::storage::manifest::Edit;
use crate::storage::segment::hash::xxh64;

use super::{Error, Store};

/// `apply_migrations`'s mode: `DryRun` checks and lists what would run, executing nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateMode {
    Apply,
    DryRun,
}

/// One migration file to run: its name, XXH64 checksum and contents.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingMigration {
    pub name: String,
    pub checksum: u64,
    pub sql: String,
}

/// One `.sql` file found in the migrations directory, parsed and hashed.
#[derive(Debug)]
struct File {
    name: String,
    number: u64,
    checksum: u64,
    sql: String,
}

/// The number before the first `_` in a `.sql` file's stem (its name without the extension): at
/// least one ASCII digit, else this isn't a migration file name.
fn parse_number(stem: &str) -> Option<u64> {
    let us = stem.find('_')?;
    let digits = &stem[..us];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// The number encoded in a recorded or pending migration's own `name` (its `.sql` file name).
fn number_of(name: &str) -> Option<u64> {
    parse_number(name.strip_suffix(".sql")?)
}

/// Lists `dir` non-recursively: ignores subdirectories and files not ending in `.sql`, parses
/// and hashes the rest, and sorts by number. A bad name or a duplicate number is
/// `MigrationInvalid`.
fn list(dir: &Path) -> Result<Vec<File>, Error> {
    let mut files: Vec<File> = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|source| super::io_err(dir, source))?;
    for entry in entries {
        let entry = entry.map_err(|source| super::io_err(dir, source))?;
        let path = entry.path();
        if entry
            .file_type()
            .map_err(|source| super::io_err(&path, source))?
            .is_dir()
        {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(stem) = name.strip_suffix(".sql") else {
            continue;
        };
        let number = parse_number(stem).ok_or_else(|| Error::MigrationInvalid {
            name: name.to_string(),
            detail: "expected <number>_<name>.sql".to_string(),
        })?;
        if files.iter().any(|f| f.number == number) {
            return Err(Error::MigrationInvalid {
                name: name.to_string(),
                detail: format!("duplicate migration number {number}"),
            });
        }
        let bytes = std::fs::read(&path).map_err(|source| super::io_err(&path, source))?;
        let checksum = xxh64(&bytes, 0);
        let sql = String::from_utf8(bytes).map_err(|_| Error::MigrationInvalid {
            name: name.to_string(),
            detail: "file is not valid UTF-8".to_string(),
        })?;
        files.push(File {
            name: name.to_string(),
            number,
            checksum,
            sql,
        });
    }
    files.sort_by_key(|f| f.number);
    Ok(files)
}

/// Every check runs before the first `exec`: lists and validates `dir`, cross-checks it against
/// what's already recorded, then applies (or, in `DryRun`, just returns) whatever is pending,
/// oldest first. A crash between one file's `exec` and its record re-runs that file on the next
/// call (at-least-once, SPEC §19, D0014): migrations should be idempotent.
pub(crate) fn apply_migrations<E>(
    store: &Store,
    dir: &Path,
    mode: MigrateMode,
    mut exec: impl FnMut(&PendingMigration) -> Result<(), E>,
) -> Result<Vec<PendingMigration>, Error>
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    // A panicking `exec` poisons this; the guard protects no data, so carry on regardless.
    let _guard = store.shared.ledger.lock().unwrap_or_else(|e| e.into_inner());
    let files = list(dir)?;
    let recorded = store.migrations();

    for r in &recorded {
        match files.iter().find(|f| f.name == r.name) {
            None => {
                return Err(Error::MigrationMissing {
                    name: r.name.clone(),
                });
            }
            Some(f) if f.checksum != r.checksum => {
                return Err(Error::MigrationChanged {
                    name: r.name.clone(),
                });
            }
            Some(_) => {}
        }
    }

    // The highest recorded number and its name, for `MigrationInvalid`'s message below.
    let highest_recorded: Option<(u64, &str)> = recorded
        .iter()
        .filter_map(|r| number_of(&r.name).map(|n| (n, r.name.as_str())))
        .max_by_key(|(n, _)| *n);

    let mut pending: Vec<&File> = Vec::new();
    for f in &files {
        if recorded.iter().any(|r| r.name == f.name) {
            continue;
        }
        if let Some((highest, name)) = highest_recorded {
            if f.number <= highest {
                return Err(Error::MigrationInvalid {
                    name: f.name.clone(),
                    detail: format!("numbered below applied migration {name}"),
                });
            }
        }
        pending.push(f);
    }

    let to_pending = |f: &File| PendingMigration {
        name: f.name.clone(),
        checksum: f.checksum,
        sql: f.sql.clone(),
    };
    if mode == MigrateMode::DryRun {
        return Ok(pending.into_iter().map(to_pending).collect());
    }

    let mut applied = Vec::new();
    for f in pending {
        let p = to_pending(f);
        exec(&p).map_err(|e| Error::MigrationFailed {
            name: p.name.clone(),
            source: e.into(),
        })?;
        crate::storage::fail::point("ledger.pre_record");
        let name = p.name.clone();
        let checksum = p.checksum;
        store
            .shared
            .commit(move |_v| vec![Edit::RecordMigration { name, checksum }], &[])?;
        applied.push(p);
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_number_accepts_digits_before_the_first_underscore() {
        assert_eq!(parse_number("12_init"), Some(12));
        assert_eq!(parse_number("0001_init"), Some(1));
    }

    #[test]
    fn parse_number_rejects_no_underscore_no_digits_or_mixed_digits() {
        assert_eq!(parse_number("init"), None);
        assert_eq!(parse_number("_init"), None);
        assert_eq!(parse_number("a1_init"), None);
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("adelie-ledger-{tag}-{}-{nanos}", std::process::id()))
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn list_ignores_directories_and_non_sql_extensions_and_sorts_numerically() {
        let dir = temp_dir("list-basic");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10_second.sql"), b"select 1;").unwrap();
        std::fs::write(dir.join("2_first.sql"), b"select 2;").unwrap();
        std::fs::write(dir.join("readme.txt"), b"ignore me").unwrap();
        std::fs::write(dir.join("3_upper.SQL"), b"ignore case").unwrap();
        std::fs::create_dir_all(dir.join("1_a_directory.sql")).unwrap();

        let files = list(&dir).unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["2_first.sql", "10_second.sql"]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    #[cfg_attr(miri, ignore)] // touches the real filesystem
    fn list_rejects_a_name_with_no_number_and_a_duplicate_number() {
        let dir = temp_dir("bad-name");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("nope.sql"), b"x").unwrap();
        let err = list(&dir).unwrap_err();
        assert!(matches!(err, Error::MigrationInvalid { .. }), "{err:?}");
        std::fs::remove_dir_all(&dir).unwrap();

        let dup = temp_dir("dup-number");
        std::fs::create_dir_all(&dup).unwrap();
        std::fs::write(dup.join("1_a.sql"), b"x").unwrap();
        std::fs::write(dup.join("01_b.sql"), b"y").unwrap();
        let err = list(&dup).unwrap_err();
        assert!(matches!(err, Error::MigrationInvalid { .. }), "{err:?}");
        std::fs::remove_dir_all(&dup).unwrap();
    }
}
