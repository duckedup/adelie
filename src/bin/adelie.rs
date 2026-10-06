//! The `adelie` binary: everything lives in `adelie::cli`.
#![deny(unsafe_code)]

use std::process::ExitCode;

fn main() -> ExitCode {
    adelie::cli::main()
}
