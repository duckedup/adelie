//! `TableSpec` (SPEC §18, D0012): one builder method per `CREATE TABLE` clause, and the pure
//! `validate` every one of those clauses' rules is checked against — driven by a static table of
//! all five engines, so the KEY rules are reachable before any keyed engine is built.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use crate::exec::Field;
use crate::types::DataType;

use super::{TableName, error::Error, is_path_component};

/// What SPEC §18 says about each engine, whether or not it is built yet.
pub struct EngineRules {
    pub name: &'static str,
    pub keyed: bool,
    pub version: bool,
}

pub const ENGINES: &[EngineRules] = &[
    EngineRules {
        name: "append",
        keyed: false,
        version: false,
    },
    EngineRules {
        name: "latest",
        keyed: true,
        version: true,
    },
    EngineRules {
        name: "rollup",
        keyed: true,
        version: false,
    },
    EngineRules {
        name: "vector",
        keyed: false,
        version: false,
    },
    EngineRules {
        name: "ledger",
        keyed: true,
        version: false,
    },
];

fn engine_rules(name: &str) -> Option<&'static EngineRules> {
    ENGINES.iter().find(|e| e.name == name)
}

/// A `CREATE TABLE` in waiting: every SPEC §18 clause, columns named rather than id'd (ids are
/// assigned only at commit). Build with the fluent methods below, then `validate`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSpec {
    pub name: TableName,
    pub columns: Vec<Field>,
    pub engine: String,
    pub key: Vec<String>,
    pub version: Option<String>,
    pub order_by: Vec<String>,
    pub partition_by: Option<(String, Duration)>,
    pub ttl: Option<(String, Duration)>,
    pub options: BTreeMap<String, String>,
}

impl TableSpec {
    pub fn new(name: TableName, columns: Vec<Field>) -> TableSpec {
        TableSpec {
            name,
            columns,
            engine: "append".to_string(),
            key: Vec::new(),
            version: None,
            order_by: Vec::new(),
            partition_by: None,
            ttl: None,
            options: BTreeMap::new(),
        }
    }

    pub fn engine(mut self, engine: &str) -> Self {
        self.engine = engine.to_string();
        self
    }

    pub fn key<I, S>(mut self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.key = cols.into_iter().map(Into::into).collect();
        self
    }

    pub fn version(mut self, col: impl Into<String>) -> Self {
        self.version = Some(col.into());
        self
    }

    pub fn order_by<I, S>(mut self, cols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.order_by = cols.into_iter().map(Into::into).collect();
        self
    }

    pub fn partition_by(mut self, col: impl Into<String>, bucket: Duration) -> Self {
        self.partition_by = Some((col.into(), bucket));
        self
    }

    pub fn ttl(mut self, col: impl Into<String>, after: Duration) -> Self {
        self.ttl = Some((col.into(), after));
        self
    }

    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.insert(key.into(), value.into());
        self
    }

    /// `order_by` with SPEC §18's default applied: a keyed engine with no ORDER BY sorts by KEY.
    pub fn resolved_order_by(&self) -> Vec<String> {
        if self.order_by.is_empty()
            && engine_rules(&self.engine).is_some_and(|r| r.keyed)
            && !self.key.is_empty()
        {
            return self.key.clone();
        }
        self.order_by.clone()
    }

    fn column(&self, name: &str) -> Option<&Field> {
        self.columns.iter().find(|c| c.name == name)
    }

    fn check_names_exist(&self, table: &str, clause: &str, cols: &[String]) -> Result<(), Error> {
        for c in cols {
            if self.column(c).is_none() {
                return Err(Error::Usage(format!(
                    "table {table}: {clause} names unknown column {c}"
                )));
            }
        }
        Ok(())
    }

    fn check_no_duplicates(&self, table: &str, clause: &str, cols: &[String]) -> Result<(), Error> {
        let mut seen = HashSet::new();
        for c in cols {
            if !seen.insert(c.as_str()) {
                return Err(Error::Usage(format!(
                    "table {table}: {clause} names column {c} twice"
                )));
            }
        }
        Ok(())
    }

    fn check_timestamp_column(&self, table: &str, clause: &str, col: &str) -> Result<(), Error> {
        // Existence was already checked by rule 4; `expect` only documents that invariant.
        let field = self.column(col).expect("column existence checked already");
        if field.ty != DataType::Timestamp {
            return Err(Error::Usage(format!(
                "table {table}: {clause} column {col} must be TIMESTAMP, not {}",
                field.ty
            )));
        }
        Ok(())
    }

    /// Every rule below, in this order. Pure: no engine registry, no IO (Miri-clean).
    pub fn validate(&self) -> Result<(), Error> {
        let table = self.name.to_string();

        // 1. `db` and `name` are each one path component.
        if !is_path_component(&self.name.db) || !is_path_component(&self.name.name) {
            return Err(Error::Usage(format!(
                "table {table}: db and name must each be one path component"
            )));
        }

        // 2. Column names are unique.
        let mut seen = HashSet::new();
        for c in &self.columns {
            if !seen.insert(c.name.as_str()) {
                return Err(Error::Usage(format!(
                    "table {table}: column {} is declared twice",
                    c.name
                )));
            }
        }

        // 3. The engine is known (built or not).
        let Some(rules) = engine_rules(&self.engine) else {
            return Err(Error::UnknownEngine {
                table,
                engine: self.engine.clone(),
            });
        };

        // 4. Every clause names a real column, and KEY/ORDER BY name none twice.
        self.check_names_exist(&table, "KEY", &self.key)?;
        self.check_no_duplicates(&table, "KEY", &self.key)?;
        if let Some(v) = &self.version {
            self.check_names_exist(&table, "VERSION", std::slice::from_ref(v))?;
        }
        self.check_names_exist(&table, "ORDER BY", &self.order_by)?;
        self.check_no_duplicates(&table, "ORDER BY", &self.order_by)?;
        if let Some((col, _)) = &self.partition_by {
            self.check_names_exist(&table, "PARTITION BY", std::slice::from_ref(col))?;
        }
        if let Some((col, _)) = &self.ttl {
            self.check_names_exist(&table, "TTL", std::slice::from_ref(col))?;
        }

        // 5. KEY on a non-keyed engine.
        if !self.key.is_empty() && !rules.keyed {
            return Err(Error::KeyNotAllowed {
                table,
                engine: self.engine.clone(),
                key: self.key.clone(),
            });
        }

        // 6. A keyed engine with no KEY.
        if self.key.is_empty() && rules.keyed {
            return Err(Error::KeyRequired {
                table,
                engine: self.engine.clone(),
            });
        }

        // 7. VERSION on an engine that doesn't take one.
        if self.version.is_some() && !rules.version {
            return Err(Error::Usage(format!(
                "table {table}: VERSION is only for ENGINE = latest"
            )));
        }

        // 8. KEY must be a prefix of the resolved ORDER BY.
        if !self.key.is_empty() {
            let resolved = self.resolved_order_by();
            let is_prefix = resolved.len() >= self.key.len()
                && resolved.iter().zip(&self.key).all(|(a, b)| a == b);
            if !is_prefix {
                return Err(Error::KeyNotSortPrefix {
                    table,
                    key: self.key.clone(),
                    order_by: resolved,
                });
            }
        }

        // 9. PARTITION BY / TTL columns are TIMESTAMP, and their durations are non-zero.
        if let Some((col, bucket)) = &self.partition_by {
            self.check_timestamp_column(&table, "PARTITION BY", col)?;
            if bucket.is_zero() {
                return Err(Error::Usage(format!(
                    "table {table}: PARTITION BY bucket must not be zero"
                )));
            }
        }
        if let Some((col, after)) = &self.ttl {
            self.check_timestamp_column(&table, "TTL", col)?;
            if after.is_zero() {
                return Err(Error::Usage(format!("table {table}: TTL must not be zero")));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols() -> Vec<Field> {
        vec![
            Field {
                name: "id".to_string(),
                ty: DataType::Int64,
            },
            Field {
                name: "ts".to_string(),
                ty: DataType::Timestamp,
            },
        ]
    }

    fn spec() -> TableSpec {
        TableSpec::new(TableName::new("d", "t"), cols())
    }

    #[test]
    fn a_fresh_spec_defaults_to_append_with_no_clauses() {
        let s = spec();
        assert_eq!(s.engine, "append");
        assert!(s.key.is_empty());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn duplicate_column_names_are_usage() {
        let s = TableSpec::new(
            TableName::new("d", "t"),
            vec![
                Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
                Field {
                    name: "a".to_string(),
                    ty: DataType::Int64,
                },
            ],
        );
        assert!(matches!(s.validate(), Err(Error::Usage(_))));
    }

    #[test]
    fn an_unknown_engine_is_rejected_by_name() {
        let s = spec().engine("nope");
        assert!(matches!(
            s.validate(),
            Err(Error::UnknownEngine { engine, .. }) if engine == "nope"
        ));
    }

    #[test]
    fn a_clause_naming_an_unknown_column_is_usage() {
        for bad in [
            spec().key(["missing"]),
            spec().engine("latest").key(["id"]).version("missing"),
            spec().order_by(["missing"]),
        ] {
            assert!(matches!(bad.validate(), Err(Error::Usage(_))), "{bad:?}");
        }
    }

    #[test]
    fn a_clause_naming_the_same_column_twice_is_usage() {
        assert!(matches!(
            spec().key(["id", "id"]).validate(),
            Err(Error::Usage(_))
        ));
        assert!(matches!(
            spec().order_by(["id", "id"]).validate(),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn key_on_append_names_latest() {
        let err = spec().key(["id"]).validate().unwrap_err();
        assert!(matches!(err, Error::KeyNotAllowed { .. }));
        assert!(err.to_string().contains("latest"));
        assert!(err.to_string().contains("KEY (id)"));
    }

    #[test]
    fn a_keyed_engine_with_no_key_is_key_required() {
        let err = spec().engine("latest").validate().unwrap_err();
        assert!(matches!(err, Error::KeyRequired { .. }));
        assert!(err.to_string().contains("latest"));
    }

    #[test]
    fn version_on_a_non_latest_engine_is_usage() {
        assert!(matches!(
            spec().version("ts").validate(),
            Err(Error::Usage(_))
        ));
    }

    #[test]
    fn a_keyed_engine_with_no_order_by_stores_order_by_equal_to_key() {
        let s = spec().engine("latest").key(["id"]);
        assert!(s.validate().is_ok());
        assert_eq!(s.resolved_order_by(), vec!["id".to_string()]);
    }

    #[test]
    fn key_not_a_prefix_of_order_by_names_both_lists_and_suggests_the_fix() {
        let err = spec()
            .engine("latest")
            .key(["id"])
            .order_by(["ts", "id"])
            .validate()
            .unwrap_err();
        assert!(matches!(err, Error::KeyNotSortPrefix { .. }));
        let msg = err.to_string();
        assert!(msg.contains("KEY (id)"));
        assert!(msg.contains("ORDER BY (ts, id)"));
        assert!(msg.contains("try ORDER BY (id, ts)"));
    }

    #[test]
    fn append_never_reaches_the_sort_prefix_check() {
        // Rule 5 (KeyNotAllowed) stops append before rule 8 ever runs.
        let err = spec().key(["id"]).order_by(["ts"]).validate().unwrap_err();
        assert!(matches!(err, Error::KeyNotAllowed { .. }));
    }

    #[test]
    fn partition_by_a_non_timestamp_column_is_usage() {
        let s = spec().partition_by("id", Duration::from_secs(1));
        assert!(matches!(s.validate(), Err(Error::Usage(_))));
    }

    #[test]
    fn a_zero_partition_bucket_is_usage() {
        let s = spec().partition_by("ts", Duration::ZERO);
        assert!(matches!(s.validate(), Err(Error::Usage(_))));
    }

    #[test]
    fn ttl_on_a_non_timestamp_column_is_usage() {
        let s = spec().ttl("id", Duration::from_secs(1));
        assert!(matches!(s.validate(), Err(Error::Usage(_))));
    }

    #[test]
    fn a_zero_ttl_is_usage() {
        let s = spec().ttl("ts", Duration::ZERO);
        assert!(matches!(s.validate(), Err(Error::Usage(_))));
    }

    #[test]
    fn a_valid_partition_by_and_ttl_pass() {
        let s = spec()
            .partition_by("ts", Duration::from_secs(3600))
            .ttl("ts", Duration::from_secs(86_400));
        assert!(s.validate().is_ok());
    }

    #[test]
    fn with_stores_options_sorted_by_key() {
        let s = spec().with("b", "2").with("a", "1");
        assert_eq!(
            s.options.keys().collect::<Vec<_>>(),
            vec![&"a".to_string(), &"b".to_string()]
        );
    }
}
