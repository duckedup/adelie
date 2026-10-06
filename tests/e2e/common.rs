//! Helpers shared by the spawned-binary suites (`cli`, `mcp`, `serve`).

use std::path::PathBuf;

/// The built `adelie` binary.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_adelie")
}

pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("adelie-e2e-{tag}-{}-{nanos}", std::process::id()))
}
