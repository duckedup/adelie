//! The `adelie` command line: `sql`, `mcp` and `serve` over one directory.

use std::process::ExitCode;

pub fn main() -> ExitCode {
    let version = env!("CARGO_PKG_VERSION");
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => {
            println!("adelie {version}");
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("adelie {version}: not built yet");
            ExitCode::from(2)
        }
    }
}
