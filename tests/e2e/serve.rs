//! `adelie serve` e2e: the HTTP server over a real store (`/query`, `/health`, `/mcp`).
//! Every test can fail; the comment on each says how.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use adelie::sql::{SqlOutput, execute};
use adelie::storage::{Store, StoreOptions};
use adelie::types::Value;

use super::common::{bin, temp_dir};

/// A running `adelie serve`; the child is killed on drop.
struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(dir: &Path, extra: &[&str]) -> Server {
        let mut child = Command::new(bin())
            .args(["serve", dir.to_str().unwrap(), "--listen", "127.0.0.1:0"])
            .args(extra)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let _ = BufReader::new(stdout).read_line(&mut line);
            let _ = tx.send(line);
        });
        // Built before the wait so a timeout still kills the child on unwind.
        let mut server = Server {
            child,
            addr: String::new(),
        };
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("no listening line within 10s");
        let url = line
            .trim()
            .strip_prefix("listening on http://")
            .unwrap_or_else(|| panic!("unexpected first line: {line:?}"));
        server.addr = url.to_string();
        server
    }

    /// One HTTP/1.1 request with `Connection: close`; returns the status and the body.
    fn request(&self, method: &str, path: &str, headers: &[&str], body: &[u8]) -> (u16, String) {
        let mut s = TcpStream::connect(&self.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
            self.addr,
            body.len()
        );
        for h in headers {
            head.push_str(h);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        // A server that rejects early may close before reading it all; read the reply anyway.
        let _ = s
            .write_all(head.as_bytes())
            .and_then(|()| s.write_all(body));
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let status = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {text:?}"));
        let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, body.to_string())
    }

    fn query(&self, sql: &str) -> (u16, String) {
        let body = serde_json::json!({ "sql": sql }).to_string();
        self.request(
            "POST",
            "/query",
            &["Content-Type: application/json"],
            body.as_bytes(),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A store with `t (id BIGINT)` holding three rows, flushed and closed.
fn fixture(tag: &str) -> PathBuf {
    let dir = temp_dir(tag);
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    execute(&store, "CREATE TABLE t (id BIGINT)").unwrap();
    execute(&store, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
    store.close().unwrap();
    dir
}

fn count(store: &Store) -> i64 {
    let SqlOutput::Rows(rows) = execute(store, "SELECT count(*) FROM t").unwrap() else {
        panic!("expected rows")
    };
    match rows.batches[0].column(0).get(0) {
        Value::Int64(n) => n,
        other => panic!("count was {other:?}"),
    }
}

/// Fails if `/health` is not 200 or does not report this crate's version.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn health_reports_the_version() {
    let dir = fixture("serve-health");
    let server = Server::start(&dir, &[]);
    let (status, body) = server.request("GET", "/health", &[], b"");
    assert_eq!(status, 200);
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
}

/// Fails if the HTTP rows differ from what the library returns for the same SELECT.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn query_select_matches_the_library() {
    let dir = fixture("serve-select");
    let server = Server::start(&dir, &[]);
    let (status, body) = server.query("SELECT id FROM t ORDER BY id");
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();

    // Read mode holds no writer lock, so the library may open the directory too.
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    let SqlOutput::Rows(rows) = execute(&store, "SELECT id FROM t ORDER BY id").unwrap() else {
        panic!("expected rows")
    };
    let want: Vec<serde_json::Value> = (0..rows.batches[0].rows())
        .map(|r| {
            let text = rows.batches[0].column(0).get(r).to_text().unwrap();
            serde_json::json!([text.parse::<i64>().unwrap()])
        })
        .collect();
    assert_eq!(want.len(), 3);
    assert_eq!(v["rows"], serde_json::Value::Array(want));
    assert_eq!(v["columns"][0]["name"], "id");
}

/// Fails if a write is accepted without `--allow-writes`, or changes the store anyway.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn insert_without_allow_writes_is_forbidden() {
    let dir = fixture("serve-ro");
    let server = Server::start(&dir, &[]);
    let (status, body) = server.query("INSERT INTO t VALUES (99)");
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("error"), "{body}");
    drop(server);
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    assert_eq!(count(&store), 3);
}

/// Fails if a body over 1 MiB is not refused with 413, or if refusing it hurts the server.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn oversized_body_is_413_and_the_server_survives() {
    let dir = fixture("serve-413");
    let server = Server::start(&dir, &[]);
    let pad = "x".repeat((1 << 20) + 1024);
    let body = format!("{{\"sql\":\"SELECT 1 /* {pad} */\"}}");
    let (status, _) = server.request(
        "POST",
        "/query",
        &["Content-Type: application/json"],
        body.as_bytes(),
    );
    assert_eq!(status, 413);
    let (status, _) = server.request("GET", "/health", &[], b"");
    assert_eq!(status, 200);
}

/// Fails if the 200 for an INSERT is sent before the row is durable: SIGKILL right after the
/// ack must still leave the row on disk.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn acknowledged_write_survives_sigkill() {
    let dir = fixture("serve-durable");
    let mut server = Server::start(&dir, &["--allow-writes"]);
    let (status, body) = server.query("INSERT INTO t VALUES (4)");
    assert_eq!(status, 200, "{body}");
    server.child.kill().unwrap(); // SIGKILL on Unix
    server.child.wait().unwrap();
    let store = Store::open(&dir, StoreOptions::default()).unwrap();
    assert_eq!(count(&store), 4);
}

/// Fails if `adelie sql` cannot read while `serve --allow-writes` holds the writer lock.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn sql_reads_while_serve_holds_the_writer() {
    let dir = fixture("serve-coexist");
    let _server = Server::start(&dir, &["--allow-writes"]);
    let out = Command::new(bin())
        .args(["sql", dir.to_str().unwrap(), "SELECT count(*) FROM t"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains('3'));
}

/// Fails if `/mcp` is not mounted or does not answer `initialize` as the `adelie` server.
/// Unverified until the MCP handler (src/mcp) is merged.
#[test]
#[cfg_attr(miri, ignore)] // spawns the adelie binary
fn mcp_initialize_names_the_server() {
    let dir = fixture("serve-mcp");
    let server = Server::start(&dir, &[]);
    let init = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": { "name": "e2e", "version": "0" }
        }
    })
    .to_string();
    let (status, body) = server.request(
        "POST",
        "/mcp",
        &[
            "Content-Type: application/json",
            "Accept: application/json, text/event-stream",
        ],
        init.as_bytes(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"name\":\"adelie\""), "{body}");
}
