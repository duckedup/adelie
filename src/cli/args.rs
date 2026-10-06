//! The command line's shape. No default store path (SPEC §4): `<DIR>` is always required.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "adelie",
    version,
    about = "A pure-Rust columnar analytics store with SQL."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run SQL against a store directory and print the result.
    Sql(SqlArgs),
    /// Serve MCP over stdio.
    #[cfg(feature = "mcp")]
    Mcp(McpArgs),
    /// Serve HTTP: POST /query, GET /health and /mcp.
    #[cfg(feature = "serve")]
    Serve(ServeArgs),
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Format {
    Table,
    Csv,
    Json,
    Ndjson,
}

#[derive(clap::Args)]
pub struct SqlArgs {
    /// The store directory.
    pub dir: PathBuf,
    /// The SQL to run; read from --file or stdin when absent.
    pub query: Option<String>,
    /// Read the SQL from a file.
    #[arg(short, long, conflicts_with = "query")]
    pub file: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "table")]
    pub format: Format,
    /// Allow write statements (takes the writer lock).
    #[arg(long)]
    pub write: bool,
}

#[cfg(feature = "mcp")]
#[derive(clap::Args)]
pub struct McpArgs {
    /// The store directory.
    pub dir: PathBuf,
    /// Allow write statements through the sql tool.
    #[arg(long)]
    pub allow_writes: bool,
    /// Most rows one result returns.
    #[arg(long, default_value_t = 200)]
    pub max_rows: usize,
    /// Most bytes of rows one result returns.
    #[arg(long, default_value_t = 64 * 1024)]
    pub max_bytes: usize,
    /// Seconds one query may run.
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
    /// MiB of memory one query may use.
    #[arg(long, default_value_t = 256)]
    pub memory: usize,
}

#[cfg(feature = "serve")]
#[derive(clap::Args)]
pub struct ServeArgs {
    /// The store directory.
    pub dir: PathBuf,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:7878")]
    pub listen: std::net::SocketAddr,
    /// Allow write statements (takes the writer lock).
    #[arg(long)]
    pub allow_writes: bool,
    /// Most rows one result returns.
    #[arg(long, default_value_t = 10_000)]
    pub max_rows: usize,
    /// Seconds one query may run.
    #[arg(long, default_value_t = 30)]
    pub timeout: u64,
    /// MiB of memory one query may use.
    #[arg(long, default_value_t = 256)]
    pub memory: usize,
}
