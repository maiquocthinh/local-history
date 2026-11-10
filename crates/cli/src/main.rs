//! Thin entry point for `lh` — parses args and calls the lib. All logic lives in `lib.rs`.

use clap::Parser;

fn main() {
    let cli = cli::Cli::parse();
    std::process::exit(cli::run(cli));
}
