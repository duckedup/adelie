//! `adelie mcp` e2e: the stdio MCP server over a real store, with the guardrails on.
//! Every test can fail; the comment on each says how.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::Duration;

use adelie::sql::{SqlOutput, execute};
use adelie::storage::{Store, StoreOptions};
use serde_json::{Value, json};

use super::common::{bin, temp_dir};

/// A spawned `adelie mcp` speaking newline-delimited JSON-RPC over its pipes.
struct Client {
    child: Child,
    stdin: ChildStdin,
    lines: Receiver<String>,
    next_id: u64,
}

impl Client {
    fn start(dir: &Path, flags: &[&str]) -> Client {
        let mut child = Command::new(bin())
            .arg("mcp")
            .arg(dir)
            .args(flags)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut client = Client {
            child,
            stdin,
            lines,
            next_id: 0,
        };
        let init = client.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "e2e", "version": "0" }
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "adelie", "{init}");
        client.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        client
    }

    fn send(&mut self, msg: &Value) {
        writeln!(self.stdin, "{msg}").unwrap();
        self.stdin.flush().unwrap();
    }

    /// Sends a request and returns the response with the matching id; a 10s silence fails
    /// the test rather than stalling it.
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let line = match self.lines.recv_timeout(Duration::from_secs(10)) {
                Ok(line) => line,
                Err(RecvTimeoutError::Timeout) => panic!("no response to {method} in 10s"),
                Err(RecvTimeoutError::Disconnected) => panic!("server closed during {method}"),
            };
            let msg: Value = serde_json::from_str(&line).expect("stdout carries only JSON-RPC");
            if msg["id"] == json!(id) {
                return msg;
            }
        }
    }

    /// The `result` of `tools/call`: `(isError, text)`.
    fn call(&mut self, tool: &str, args: Value) -> (bool, String) {
        let resp = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        let result = &resp["result"];
        assert!(
            result.is_object(),
            "protocol error, not a tool result: {resp}"
        );
        let text = result["content"][0]["text"].as_str().unwrap().to_string();
        (result["isError"].as_bool().unwrap_or(false), text)
    }

    fn call_json(&mut self, tool: &str, args: Value) -> Value {
        let (is_error, text) = self.call(tool, args);
        assert!(!is_error, "{tool} failed: {text}");
        serde_json::from_str(&text).unwrap()
    }

    fn count(&mut self, table: &str) -> i64 {
        let out = self.call_json(
            "sql",
            json!({ "query": format!("SELECT count(*) FROM {table}") }),
        );
        out["rows"][0][0].as_i64().unwrap()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `t (id, v)` with 500 rows, `v` NULL on every even id.
fn store(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    execute(&store, "CREATE TABLE t (id BIGINT, v BIGINT)").unwrap();
    let rows: Vec<String> = (0..500)
        .map(|i| {
            let v = if i % 2 == 0 {
                "NULL".to_string()
            } else {
                i.to_string()
            };
            format!("({i}, {v})")
        })
        .collect();
    execute(&store, &format!("INSERT INTO t VALUES {}", rows.join(", "))).unwrap();
    store.close().unwrap();
    dir
}

fn library_count(dir: &Path) -> i64 {
    let store = Store::open(dir, StoreOptions::default()).unwrap();
    let SqlOutput::Rows(rows) = execute(&store, "SELECT count(*) FROM t").unwrap() else {
        panic!("expected rows")
    };
    match rows.batches[0].column(0).get(0) {
        adelie::types::Value::Int64(n) => n,
        other => panic!("count was {other:?}"),
    }
}

/// Fails if the server names a tool that is not built, or misses one that is.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn lists_exactly_the_four_tools() {
    let dir = store("mcp-tools");
    let mut c = Client::start(&dir, &[]);
    let resp = c.request("tools/list", json!({}));
    let mut names: Vec<&str> = resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["describe_table", "list_tables", "sample", "sql"]);
    let listed = c.call_json("list_tables", json!({}));
    assert_eq!(listed[0]["name"], "t");
    assert_eq!(listed[0]["rows"], 500);
}

/// Fails if the row cap is not applied: 500 rows come back, or the note does not say 500.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn sql_results_are_capped_with_a_note() {
    let dir = store("mcp-cap");
    let mut c = Client::start(&dir, &["--max-rows", "50"]);
    let out = c.call_json("sql", json!({ "query": "SELECT id FROM t" }));
    assert_eq!(out["rows"].as_array().unwrap().len(), 50);
    let note = out["truncated"].as_str().expect("truncation note");
    assert!(note.contains("500"), "{note}");
    let sample = c.call_json("sample", json!({ "table": "t", "n": 400 }));
    assert_eq!(sample["rows"].as_array().unwrap().len(), 50);
}

/// Fails if a write runs without `--allow-writes`, or stops working with it.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn writes_need_allow_writes() {
    let dir = store("mcp-writes");
    {
        let mut c = Client::start(&dir, &[]);
        let (is_error, text) = c.call("sql", json!({ "query": "INSERT INTO t VALUES (1000, 1)" }));
        assert!(is_error, "{text}");
        assert!(text.contains("--allow-writes"), "{text}");
        let (is_error, _) = c.call("sql", json!({ "query": "SELECT 1; DELETE FROM t" }));
        assert!(is_error);
        assert_eq!(c.count("t"), 500);
    }
    assert_eq!(library_count(&dir), 500);
    let mut c = Client::start(&dir, &["--allow-writes"]);
    let (is_error, text) = c.call("sql", json!({ "query": "INSERT INTO t VALUES (1000, 1)" }));
    assert!(!is_error, "{text}");
    assert_eq!(c.count("t"), 501);
}

/// Fails if an unknown table does not name the near one, or `present_fraction` is not
/// `non_null / rows` (half of `v` is NULL, so 0.5).
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn describe_table_reports_presence_and_suggests() {
    let dir = store("mcp-describe");
    let mut c = Client::start(&dir, &[]);
    let (is_error, text) = c.call("describe_table", json!({ "table": "tt" }));
    assert!(is_error);
    assert!(text.contains("did you mean \"t\"?"), "{text}");
    let out = c.call_json("describe_table", json!({ "table": "t" }));
    let cols = out["columns"].as_array().unwrap();
    let v = cols.iter().find(|c| c["name"] == "v").unwrap();
    assert_eq!(v["present_fraction"], 0.5);
    let id = cols.iter().find(|c| c["name"] == "id").unwrap();
    assert_eq!(id["present_fraction"], 1.0);
}

/// Fails if exceeding the memory budget kills the server instead of erroring one call.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn memory_budget_is_an_error_not_a_crash() {
    let dir = store("mcp-memory");
    {
        let store = Store::open(&dir, StoreOptions::default()).unwrap();
        execute(&store, "CREATE TABLE big (id BIGINT)").unwrap();
        for chunk in 0..10 {
            let rows: Vec<String> = (0..10_000)
                .map(|i| format!("({})", chunk * 10_000 + i))
                .collect();
            execute(
                &store,
                &format!("INSERT INTO big VALUES {}", rows.join(", ")),
            )
            .unwrap();
        }
        store.close().unwrap();
    }
    let mut c = Client::start(&dir, &["--memory", "1"]);
    let (is_error, text) = c.call(
        "sql",
        json!({ "query": "SELECT id, count(*) FROM big GROUP BY id" }),
    );
    assert!(is_error, "{text}");
    assert!(text.contains("budget"), "{text}");
    let listed = c.call_json("list_tables", json!({}));
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["name"] == "big")
    );
}
