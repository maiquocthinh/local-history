//! `lh-e2e` — process-level end-to-end runner for local-history.
//!
//! Spawns the real daemon and the real `lh` CLI against real files in temp
//! dirs and asserts on JSON output and disk state. Cross-platform (Windows +
//! Linux): only `std::process::Command`, `PathBuf` and `Child::kill`.
//!
//! Usage:
//! ```text
//! lh-e2e [--lh PATH] [--filter SUBSTRING] [--suite smoke|full] [--json]
//! ```
//! - `--lh PATH` overrides the binary under test (default order: `--lh` flag >
//!   env `LH_BIN` > `target/debug/lh(.exe)` in this workspace).
//! - `--filter SUBSTRING` runs only the scenarios whose name contains it.
//! - `--suite smoke` runs the fast 12-scenario set (per-commit), `full` runs
//!   everything (default; pre-release).
//! - `--json` prints progress to stderr and one machine-readable summary
//!   object to stdout (CI-friendly).
//! - `LH_E2E_KEEP=1` keeps every scenario's temp dirs on disk (paths printed
//!   to stderr) — artifact retention for debugging failures.
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

/// The fast smoke suite: the original 12 scenarios (per-commit set).
const SMOKE_SCENARIOS: [&str; 12] = [
    "base_flow",
    "batch_waves",
    "delete_restore",
    "label_flow",
    "binary_file",
    "rename_keeps_chain",
    "multi_workspace",
    "wave_200_files",
    "self_update",
    "daemon_resilience",
    "gc_dry_run",
    "ignored_dirs",
];

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let (lh_flag, filter, suite, as_json) = match parse_args() {
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

    let all = registry();
    let selected: Vec<(&'static str, ScenarioFn)> = all
        .into_iter()
        .filter(|(name, _)| match suite.as_deref() {
            Some("smoke") => SMOKE_SCENARIOS.contains(name),
            _ => true,
        })
        .filter(|(name, _)| filter.as_deref().is_none_or(|f| name.contains(f)))
        .collect();
    if selected.is_empty() {
        eprintln!("no scenario matches filter={filter:?} suite={suite:?}");
        return 2;
    }

    // --json: progress goes to stderr, the final summary to stdout.
    let out = |s: &str| {
        if as_json {
            eprintln!("{s}");
        } else {
            println!("{s}");
        }
    };

    let binary = lh.display().to_string();
    let suite_label = suite.clone().unwrap_or_else(|| "full".into());
    out(&format!("lh binary: {binary}"));

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    let mut passed = 0usize;
    for (name, func) in &selected {
        // Kill leftovers of a previously timed-out scenario, if any.
        kill_all_live_daemons();
        out(&format!("--- running {name}"));
        match run_scenario(name, *func, &lh) {
            Ok(note) => {
                passed += 1;
                out(&format!("PASS {name}"));
                if !note.is_empty() {
                    out(&format!("      {note}"));
                }
                results.push(serde_json::json!({ "name": name, "status": "pass", "note": note }));
            }
            Err(reason) => {
                failed.push(format!("{name}: {reason}"));
                out(&format!("FAIL {name}: {reason}"));
                results.push(serde_json::json!({ "name": name, "status": "fail", "note": reason }));
            }
        }
    }

    out("");
    out(&format!(
        "{} passed, {} failed (of {} selected)",
        passed,
        failed.len(),
        selected.len()
    ));
    if as_json {
        println!(
            "{}",
            serde_json::json!({
                "binary": binary,
                "suite": suite_label,
                "total": selected.len(),
                "passed": passed,
                "failed": failed.len(),
                "results": results,
            })
        );
    }
    if failed.is_empty() {
        0
    } else {
        1
    }
}

/// Parse `--lh PATH`, `--filter SUBSTRING`, `--suite smoke|full` and `--json`;
/// returns `(lh, filter, suite, json)` or the exit code to use.
fn parse_args() -> Result<(Option<String>, Option<String>, Option<String>, bool), i32> {
    let mut lh: Option<String> = None;
    let mut filter: Option<String> = None;
    let mut suite: Option<String> = None;
    let mut as_json = false;
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
            "--suite" => match it.next() {
                Some(v) if v == "smoke" || v == "full" => suite = Some(v.clone()),
                Some(other) => {
                    eprintln!("--suite must be `smoke` or `full`, got `{other}`");
                    return Err(2);
                }
                None => {
                    eprintln!("--suite needs smoke|full");
                    return Err(2);
                }
            },
            "--json" => as_json = true,
            "--help" | "-h" => {
                println!("usage: lh-e2e [--lh PATH] [--filter SUBSTRING] [--suite smoke|full] [--json]");
                println!("env: LH_E2E_KEEP=1 keeps every scenario's temp dirs on disk");
                return Err(0);
            }
            other => {
                eprintln!(
                    "unknown argument `{other}` — usage: lh-e2e [--lh PATH] [--filter SUBSTRING] [--suite smoke|full] [--json]"
                );
                return Err(2);
            }
        }
    }
    Ok((lh, filter, suite, as_json))
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
