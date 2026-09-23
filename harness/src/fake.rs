//! A scripted `Engine`: answers keyed by SQL text, for planting known-wrong behavior.

use std::collections::HashMap;

use crate::engine::{Engine, EngineError, Outcome};

/// Collapses whitespace runs to a single space and trims both ends, so callers can key
/// answers without caring about incidental formatting differences in the SQL text.
fn normalize(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// An `Engine` whose answers are fixed in advance, for testing the harnesses themselves.
pub struct FakeEngine {
    name: String,
    answers: HashMap<String, Result<Outcome, EngineError>>,
}

impl FakeEngine {
    pub fn new(name: &str) -> Self {
        FakeEngine { name: name.to_string(), answers: HashMap::new() }
    }

    /// Keyed by SQL with whitespace runs collapsed and trimmed.
    pub fn answer(mut self, sql: &str, result: Result<Outcome, EngineError>) -> Self {
        self.answers.insert(normalize(sql), result);
        self
    }
}

impl Engine for FakeEngine {
    fn name(&self) -> &str {
        &self.name
    }

    fn run(&mut self, sql: &str) -> Result<Outcome, EngineError> {
        match self.answers.get(&normalize(sql)) {
            Some(result) => result.clone(),
            None => Err(EngineError(format!("fake: no answer for `{sql}`"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_sql_errors() {
        let mut fake = FakeEngine::new("fake");
        let err = fake.run("select 1").unwrap_err();
        assert_eq!(err, EngineError("fake: no answer for `select 1`".to_string()));
    }

    #[test]
    fn whitespace_normalized_key_matches() {
        let mut fake = FakeEngine::new("fake").answer("select   1", Ok(Outcome::Statement));
        assert_eq!(fake.run(" select 1 "), Ok(Outcome::Statement));
        assert_eq!(fake.run("select\n1"), Ok(Outcome::Statement));
    }
}
