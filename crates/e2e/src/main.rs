//! `lh-e2e` — process-level end-to-end runner for local-history.
//!
//! Spawns the real daemon and the real `lh` CLI against real files in temp
//! dirs and asserts on JSON output and disk state. Cross-platform (Windows +
//! Linux): only `std::process::Command`, `PathBuf` and `Child::kill`.
//!
//! Usage:
//! ```text
//! lh-e2e [--lh PATH] [--filter SUBSTRING]
//! ```
//! - `--lh PATH` overrides the binary under test (default order: `--lh` flag >
//!   env `LH_BIN` > `target/debug/lh(.exe)` in this workspace).
//! - `--filter SUBSTRING` runs only the scenarios whose name contains it.
//!
//! Every scenario runs on its own thread with a hard 120s wall-clock deadline;
//! output is `PASS <name>` / `FAIL <name>: <reason>` and the process exits 0
//! only when everything passed. A scenario that panics or hangs becomes a
//! FAIL, never a runner crash.

mod harness;
mod scenarios;

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use harness::{kill_all_live_daemons, locate_lh, Ctx, Res};
use scenarios::{registry, ScenarioFn};

/// Hard wall-clock deadline for one scenario.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(120);

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let (lh_flag, filter) = match parse_args() {
        Ok(v) => v,
        Err(code) => return code,
    };

    let lh = match locate_lh(lh_flag.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    println!("lh binary: {}", lh.display());

    let all = registry();
    let selected: Vec<(&'static str, ScenarioFn)> = all
        .into_iter()
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();
    if selected.is_empty() {
        eprintln!("no scenario name contains {filter:?}");
        return 2;
    }

    let mut failed: Vec<String> = Vec::new();
    let mut passed = 0usize;
    for (name, func) in &selected {
        // Kill leftovers of a previously timed-out scenario, if any.
        kill_all_live_daemons();
        println!("--- running {name}");
        match run_scenario(name, *func, &lh) {
            Ok(note) => {
                passed += 1;
                println!("PASS {name}");
                if !note.is_empty() {
                    println!("      {note}");
                }
            }
            Err(reason) => {
                failed.push(format!("{name}: {reason}"));
                println!("FAIL {name}: {reason}");
            }
        }
    }

    println!();
    println!(
        "{} passed, {} failed (of {} selected)",
        passed,
        failed.len(),
        selected.len()
    );
    if failed.is_empty() {
        0
    } else {
        1
    }
}

/// Parse `--lh PATH` and `--filter SUBSTRING`; returns `(lh, filter)` or the
/// exit code to use.
fn parse_args() -> Result<(Option<String>, Option<String>), i32> {
    let mut lh: Option<String> = None;
    let mut filter: Option<String> = None;
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut it = argv.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--lh" => match it.next() {
                Some(v) => lh = Some(v.clone()),
                None => {
                    eprintln!("--lh needs a PATH argument");
                    return Err(2);
                }
            },
            "--filter" => match it.next() {
                Some(v) => filter = Some(v.clone()),
                None => {
                    eprintln!("--filter needs a SUBSTRING argument");
                    return Err(2);
                }
            },
            "--help" | "-h" => {
                println!("usage: lh-e2e [--lh PATH] [--filter SUBSTRING]");
                return Err(0);
            }
            other => {
                eprintln!("unknown argument `{other}` — usage: lh-e2e [--lh PATH] [--filter SUBSTRING]");
                return Err(2);
            }
        }
    }
    Ok((lh, filter))
}

/// Run one scenario on a worker thread with a hard deadline; a panic or a
/// deadline hit becomes a FAIL (and its daemons get killed).
fn run_scenario(name: &str, func: ScenarioFn, lh: &std::path::Path) -> Res {
    let (tx, rx) = std::sync::mpsc::channel::<Res>();
    let ctx = Ctx { lh: lh.to_path_buf() };
    let spawned = std::thread::Builder::new()
        .name(format!("e2e-{name}"))
        .spawn(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| func(&ctx)));
            let _ = tx.send(outcome.unwrap_or_else(|payload| {
                Err(format!("scenario panicked: {}", panic_message(&payload)))
            }));
        });
    let rx = match spawned {
        Ok(_) => rx,
        Err(e) => return Err(format!("failed to spawn the scenario thread: {e}")),
    };
    match rx.recv_timeout(SCENARIO_DEADLINE) {
        Ok(result) => result,
        Err(RecvTimeoutError::Timeout) => {
            kill_all_live_daemons();
            Err(format!(
                "deadline exceeded ({}s) — scenario thread left running, its daemons were killed",
                SCENARIO_DEADLINE.as_secs()
            ))
        }
        Err(RecvTimeoutError::Disconnected) => {
            kill_all_live_daemons();
            Err("scenario thread ended without a result".into())
        }
    }
}

/// Human-readable message out of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".into()
    }
}
