//! Thin binary entry point of `lh-daemon` — all logic (args parsing, tray/console run,
//! exit codes) lives in the lib: `daemon::run_app` takes the CLI args and returns the
//! process exit code.

fn main() {
    std::process::exit(daemon::run_app(std::env::args().skip(1)));
}
