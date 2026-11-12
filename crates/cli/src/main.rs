//! Thin entry point for `lh` — the multi-call binary (plan M4). All logic
//! lives in `lib.rs`.
//!
//! Startup order matters:
//! 1. `updater::cleanup_stale` — best-effort recovery of a previous self-update
//!    (auto-restore `lh.exe` after a crash between the two renames, then delete
//!    leftover `.lh-old-*`/`.lh-new-*`; plan §3.3 steps 12-13). A file still
//!    locked by a running process just retries on the next startup.
//! 2. bare `lh` (argv empty) → open the GUI detached — checked BEFORE
//!    `Cli::parse` so clap cannot turn empty argv into a usage error (this is a
//!    deliberate behavior change from help+exit 2; plan §3.1).
//! 3. otherwise parse + dispatch as before (`lh --help` stays exit 0).

use clap::Parser;

fn main() {
    // Self-update recovery first (best-effort — never blocks startup).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        updater::cleanup_stale(dir);
    }

    // No args → the GUI (exit code from the spawn result; 0 on success).
    // `nth(1)` == `skip(1).next()` — true when argv holds nothing after the
    // program name (written via nth to satisfy clippy::iter_skip_next).
    if std::env::args_os().nth(1).is_none() {
        std::process::exit(cli::run_default_gui());
    }

    let cli = cli::Cli::parse();
    std::process::exit(cli::run(cli));
}
