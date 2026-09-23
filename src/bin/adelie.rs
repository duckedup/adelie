//! The `adelie` binary. A placeholder until the CLI lands: it reports its version so the
//! release pipeline has a real artifact to build, package, and install.

fn main() {
    let version = env!("CARGO_PKG_VERSION");
    match std::env::args().nth(1).as_deref() {
        Some("--version" | "-V") => println!("adelie {version}"),
        _ => {
            eprintln!("adelie {version}: no commands yet");
            std::process::exit(2);
        }
    }
}
