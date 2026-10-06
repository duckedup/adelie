//! The `adelie` command line: `sql`, `mcp` and `serve` over one directory.

mod args;
mod render;

use std::io::{Read, Write};
use std::process::ExitCode;

use clap::Parser;

use crate::sql::{self, SqlError, SqlOutput};
use crate::surface::Handle;
use args::{Cli, Command, Format, SqlArgs};

pub fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Sql(args) => run_sql(&args),
        #[cfg(feature = "mcp")]
        Command::Mcp(a) => {
            let guard = crate::mcp::Guardrails {
                allow_writes: a.allow_writes,
                limits: crate::surface::Limits {
                    max_rows: a.max_rows,
                    max_bytes: a.max_bytes,
                },
                timeout: std::time::Duration::from_secs(a.timeout),
                memory_limit: a.memory.saturating_mul(1 << 20),
            };
            crate::mcp::run_stdio(&a.dir, guard).map_err(|e| e.to_string())
        }
        #[cfg(feature = "serve")]
        Command::Serve(a) => {
            let guard = crate::mcp::Guardrails {
                allow_writes: a.allow_writes,
                limits: crate::surface::Limits {
                    max_rows: a.max_rows,
                    max_bytes: 16 << 20,
                },
                timeout: std::time::Duration::from_secs(a.timeout),
                memory_limit: a.memory.saturating_mul(1 << 20),
            };
            let cfg = crate::server::ServeConfig {
                dir: a.dir,
                listen: a.listen,
                guard,
            };
            crate::server::run(cfg).map_err(|e| e.to_string())
        }
    }
}

fn read_query(args: &SqlArgs) -> Result<String, String> {
    let text = if let Some(q) = &args.query {
        q.clone()
    } else if let Some(path) = &args.file {
        std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .map_err(|e| format!("stdin: {e}"))?;
        buf
    };
    if text.trim().is_empty() {
        return Err("no SQL given: pass a query, --file, or pipe it on stdin".to_string());
    }
    Ok(text)
}

fn run_sql(args: &SqlArgs) -> Result<(), String> {
    let query = read_query(args)?;
    let handle = Handle::open(&args.dir, args.write).map_err(|e| e.to_string())?;
    let out = handle
        .run(&query, &sql::Options::default())
        .map_err(|e| match e {
            SqlError::ReadOnly(_) => format!("{e} (pass --write to allow writes)"),
            other => other.to_string(),
        });
    // Close a writer before reporting so its flush errors are not lost.
    let closed = match handle {
        Handle::Write(store) => store.close().map_err(|e| e.to_string()),
        Handle::Read(_) => Ok(()),
    };
    let out = out?;
    print_output(&out, args.format)?;
    closed
}

fn print_output(out: &SqlOutput, format: Format) -> Result<(), String> {
    let rows = match out {
        SqlOutput::Statement { rows_affected } => {
            println!("{rows_affected} rows affected");
            return Ok(());
        }
        SqlOutput::Rows(rows) => rows,
    };
    for w in &rows.warnings {
        eprintln!("warning: {w}");
    }
    let text = match format {
        Format::Table => format!("{}\n", render::table(rows)),
        Format::Csv => render::csv(rows).map_err(|e| e.to_string())?,
        Format::Json => render::json(rows),
        Format::Ndjson => render::ndjson(rows),
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|e| e.to_string())
}
