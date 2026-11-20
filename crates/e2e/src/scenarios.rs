//! The 12 e2e scenarios of `lh-e2e`. Each one drives REAL processes only: a
//! real daemon (`Harness`), real `lh` CLI invocations and real files on disk —
//! no in-process engine calls. Assertions follow the actual code contracts
//! (crates/cli, crates/daemon, crates/history, crates/updater), not guesses.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

use crate::harness::{
    canon_str, poll_until, sha256_hex, spawn_second_daemon, truncate, v_arr, v_bool, v_i64, v_str,
    v_u64, CliOut, Ctx, DAEMON_DIED_PREFIX, Harness, Res, POLL_INTERVAL_MS,
};

/// Pause between two writes that must land in separate debounce waves (the
/// daemon's fixed debounce is 400ms; 700ms is comfortably above it).
fn wave_gap() {
    sleep(Duration::from_millis(700));
}

/// Map an `std::io::Result` to the scenario error type with context.
fn io<T>(r: std::io::Result<T>, what: &str) -> Result<T, String> {
    r.map_err(|e| format!("{what}: {e}"))
}

/// Read a file from disk as bytes.
fn disk_bytes(path: &Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}

/// Poll until the disk content of `path` equals `expected` (byte-exact).
fn wait_disk_eq(path: &Path, expected: &[u8], deadline_ms: u64) -> Result<(), String> {
    let got = poll_until(deadline_ms, POLL_INTERVAL_MS, || {
        disk_bytes(path).filter(|b| b.as_slice() == expected)
    });
    if got.is_some() {
        return Ok(());
    }
    let preview = disk_bytes(path)
        .map(|b| truncate(&String::from_utf8_lossy(&b), 120))
        .unwrap_or_else(|| "(file missing)".into());
    Err(format!(
        "disk content of {} did not become the expected {} bytes within {deadline_ms}ms — last: {preview}",
        path.display(),
        expected.len()
    ))
}

/// Find a version by seq in a `lh history --json` payload (versions are
/// newest-first per the engine's `ORDER BY v.seq DESC`).
fn find_seq(hist: &Value, seq: i64) -> Option<&Value> {
    v_arr(hist, "versions")
        .ok()?
        .iter()
        .find(|v| v.get("seq").and_then(Value::as_i64) == Some(seq))
}

/// True when the history payload contains a version of the given kind.
fn hist_has_kind(hist: &Value, kind: &str) -> bool {
    v_arr(hist, "versions").is_ok_and(|a| {
        a.iter().any(|v| v.get("kind").and_then(Value::as_str) == Some(kind))
    })
}

/// True when `lh status --json` reports a live daemon.
fn status_alive(j: &Value) -> bool {
    j.get("daemon")
        .and_then(|d| d.get("alive"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The batches list of the store as a plain array (`{"batches": [...]}`).
fn batches_list(h: &Harness) -> Result<Vec<Value>, String> {
    Ok(v_arr(&h.batches_json()?, "batches")?.clone())
}

/// Poll until some batch summary matches `pred`; returns that summary.
fn wait_batch(
    h: &Harness,
    deadline_ms: u64,
    mut pred: impl FnMut(&Value) -> bool,
) -> Result<Value, String> {
    let mut last: String = String::new();
    let found = poll_until(deadline_ms, POLL_INTERVAL_MS, || {
        match batches_list(h) {
            Ok(list) => {
                last = serde_json::to_string(&list).unwrap_or_default();
                list.iter().find(|b| pred(b)).cloned().map(Ok)
            }
            Err(e) => {
                if e.starts_with(DAEMON_DIED_PREFIX) {
                    return Some(Err(e));
                }
                None
            }
        }
    });
    match found {
        Some(Ok(v)) => Ok(v),
        Some(Err(e)) => Err(e),
        None => Err(format!(
            "timeout ({deadline_ms}ms) waiting for a matching batch — batches seen: {}",
            truncate(&last, 800)
        )),
    }
}

/// A revert report must carry no per-file errors.
fn no_errors(report: &Value) -> Result<(), String> {
    let errs = v_arr(report, "errors").cloned().unwrap_or_default();
    if errs.is_empty() {
        Ok(())
    } else {
        Err(format!("revert report carries errors: {errs:?}"))
    }
}

/// Typed view of `lh gc --json` (parsed via `run_cli_json<T: DeserializeOwned>`).
#[derive(Debug, serde::Deserialize)]
struct GcJson {
    dry_run: bool,
    files_scanned: u64,
    versions_deleted: u64,
}

/// One scenario function: the whole flow, returning a note or a failure reason.
pub type ScenarioFn = fn(&Ctx) -> Res;

/// The registry: (scenario name, function). `--filter` matches substrings of
/// the name.
pub fn registry() -> Vec<(&'static str, ScenarioFn)> {
    vec![
        ("base_flow", s1_base_flow),
        ("batch_waves", s2_batch_waves),
        ("delete_restore", s3_delete_restore),
        ("label_flow", s4_label_flow),
        ("binary_file", s5_binary_file),
        ("rename_contract_delete_plus_create", s6_rename_contract),
        ("multi_workspace", s7_multi_workspace),
        ("wave_200_files", s8_wave_200_files),
        ("self_update", s9_self_update),
        ("daemon_resilience", s10_daemon_resilience),
        ("gc_dry_run", s11_gc_dry_run),
        ("ignored_dirs", s12_ignored_dirs),
    ]
}

// ---------------------------------------------------------------- S1

/// Three writes spaced above the debounce window; verify history, diff hunk,
/// byte-exact `show`, revert to seq 1 and `gc --dry-run`.
fn s1_base_flow(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let notes = h.ws_path(0).join("notes.txt");
    let p = notes.display().to_string();
    let (c1, c2, c3) = ("notes revision one\n", "notes revision two\n", "notes revision three\n");
    io(fs::write(&notes, c1), "failed to write revision 1")?;
    wave_gap();
    io(fs::write(&notes, c2), "failed to write revision 2")?;
    wave_gap();
    io(fs::write(&notes, c3), "failed to write revision 3")?;

    let hist = h.wait_history(&notes, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 3)
    })?;
    for seq in 1..=3 {
        let v = find_seq(&hist, seq)
            .ok_or_else(|| format!("seq {seq} missing from the history: {}", truncate(&hist.to_string(), 500)))?;
        if v.get("kind").and_then(Value::as_str) != Some("external") {
            return Err(format!("seq {seq} is not kind=external: {v}"));
        }
    }

    let d = h.cli_json_value(&["diff", &p, "1", "3"])?;
    let diff = v_str(&d, "diff")?;
    if !diff.contains("@@") || !diff.contains("revision three") {
        return Err(format!(
            "diff 1->3 has no @@ hunk or no new content: {}",
            truncate(diff, 400)
        ));
    }

    let s1 = h.cli(&["show", &p, "--at", "1"])?;
    if s1.code != 0 || s1.stdout != c1 {
        return Err(format!(
            "show --at 1 mismatch (exit {}): {:?}",
            s1.code,
            truncate(&s1.stdout, 200)
        ));
    }
    let s3 = h.cli(&["show", &p, "--at", "latest"])?;
    if s3.code != 0 || s3.stdout != c3 {
        return Err(format!(
            "show --at latest mismatch (exit {}): {:?}",
            s3.code,
            truncate(&s3.stdout, 200)
        ));
    }

    let r = h.cli_json_value(&["revert", &p, "--yes", "--at", "1"])?;
    if !v_bool(&r, "reverted")? {
        return Err(format!("revert reported reverted=false: {r}"));
    }
    if v_i64(&r, "target_seq")? != 1 {
        return Err(format!("revert target_seq is not 1: {r}"));
    }
    wait_disk_eq(&notes, c1.as_bytes(), 3_000)?;

    let g = h.cli_json_value(&["gc", "--dry-run"])?;
    if !v_bool(&g, "dry_run")? {
        return Err(format!("gc --dry-run reports dry_run=false: {g}"));
    }
    Ok(format!(
        "3 external versions (seq 1..3); diff @@ hunk, byte-exact show, revert to seq 1 and gc dry-run all verified (history now has {} versions)",
        v_u64(&hist, "count")?
    ))
}

// ---------------------------------------------------------------- S2

/// The M6 batch contract, end to end:
/// wave 1 (6 creates) -> batch with file_count=6;
/// wave 2 (6 edits) -> new batch, `lh batch` lists 6 entries;
/// wave 3 (2 deletes) -> new batch with 2 delete entries;
/// `lh revert-batch` restores 6 files, then restores the 2 deleted ones;
/// the revert's own disk writes produce a further new batch.
fn s2_batch_waves(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let svc = |i: usize| ws.join(format!("svc{i}.txt"));
    let wave1 = |i: usize| format!("svc{i} wave-one content\n");
    let wave2 = |i: usize| format!("svc{i} wave-two content edited\n");

    // Wave 1: six creates inside one batch window (writes <300ms apart).
    for i in 1..=6 {
        io(fs::write(svc(i), wave1(i)), "failed to create svc file")?;
    }
    let batch_a = wait_batch(&h, 10_000, |b| {
        v_u64(b, "files").is_ok_and(|f| f == 6)
    })?;
    let id_a = v_i64(&batch_a, "id")?;

    // Leave batch A's window so wave 2 opens a fresh batch.
    sleep(Duration::from_millis(500));
    for i in 1..=6 {
        io(fs::write(svc(i), wave2(i)), "failed to edit svc file")?;
    }
    let batch_b = wait_batch(&h, 10_000, |b| {
        v_u64(b, "files").is_ok_and(|f| f == 6)
            && v_i64(b, "id").is_ok_and(|id| id != id_a)
    })?;
    let id_b = v_i64(&batch_b, "id")?;

    let entries_b = h.cli_json_value(&["batch", &id_b.to_string()])?;
    let arr_b = v_arr(&entries_b, "entries")?;
    if arr_b.len() != 6 {
        return Err(format!("batch {id_b} has {} entries (expected 6): {entries_b}", arr_b.len()));
    }
    let mut rels_b: Vec<String> = arr_b
        .iter()
        .map(|e| v_str(e, "rel_path").map(str::to_string))
        .collect::<Result<_, _>>()?;
    rels_b.sort();
    let expected_rels: Vec<String> = (1..=6).map(|i| format!("svc{i}.txt")).collect();
    if rels_b != expected_rels {
        return Err(format!("batch {id_b} rel_paths are {rels_b:?} (expected {expected_rels:?})"));
    }

    // Revert the edit batch: all 6 files go back to their wave-1 content.
    let rb = h.cli_json_value(&["revert-batch", &id_b.to_string(), "--yes"])?;
    if v_u64(&rb, "reverted")? != 6 {
        return Err(format!("revert-batch {id_b}: expected reverted=6, got {rb}"));
    }
    no_errors(&rb)?;
    for i in 1..=6 {
        wait_disk_eq(&svc(i), wave1(i).as_bytes(), 3_000)?;
    }

    // Wave 3: delete two of the six files inside one window.
    sleep(Duration::from_millis(500));
    io(fs::remove_file(svc(5)), "failed to delete svc5.txt")?;
    io(fs::remove_file(svc(6)), "failed to delete svc6.txt")?;
    let batch_c = wait_batch(&h, 10_000, |b| {
        v_u64(b, "files").is_ok_and(|f| f == 2)
            && v_i64(b, "id").is_ok_and(|id| id != id_a && id != id_b)
    })?;
    let id_c = v_i64(&batch_c, "id")?;
    let entries_c = h.cli_json_value(&["batch", &id_c.to_string()])?;
    let arr_c = v_arr(&entries_c, "entries")?;
    if arr_c.len() != 2 {
        return Err(format!("batch {id_c} has {} entries (expected 2): {entries_c}", arr_c.len()));
    }
    for e in arr_c {
        if v_str(e, "kind")? != "delete" {
            return Err(format!("batch {id_c} contains a non-delete entry: {e}"));
        }
    }
    let mut rels_c: Vec<String> = arr_c
        .iter()
        .map(|e| v_str(e, "rel_path").map(str::to_string))
        .collect::<Result<_, _>>()?;
    rels_c.sort();
    if rels_c != ["svc5.txt".to_string(), "svc6.txt".to_string()] {
        return Err(format!("batch {id_c} rel_paths are {rels_c:?} (expected svc5.txt + svc6.txt)"));
    }

    let batches_before_reverts = batches_list(&h)?.len();

    // Revert the delete batch: the two files are restored (their content right
    // before the deletion is the wave-1 content — disk had just been reverted
    // to it by revert-batch B).
    let rc = h.cli_json_value(&["revert-batch", &id_c.to_string(), "--yes"])?;
    if v_u64(&rc, "restored")? != 2 {
        return Err(format!("revert-batch {id_c}: expected restored=2, got {rc}"));
    }
    no_errors(&rc)?;
    wait_disk_eq(&svc(5), wave1(5).as_bytes(), 3_000)?;
    wait_disk_eq(&svc(6), wave1(6).as_bytes(), 3_000)?;

    // The revert's own disk writes must produce a further new batch once the
    // window has passed.
    sleep(Duration::from_millis(500));
    let grew = poll_until(8_000, POLL_INTERVAL_MS, || {
        batches_list(&h)
            .ok()
            .map(|l| l.len())
            .filter(|n| *n > batches_before_reverts)
    });
    if grew.is_none() {
        return Err(format!(
            "batch count did not grow after the revert writes (still {batches_before_reverts})"
        ));
    }
    Ok(format!(
        "waves recorded as batches A(id={id_a}, 6 files), B(id={id_b}, 6 entries), C(id={id_c}, 2 deletes); revert-batch B reverted=6, revert-batch C restored=2; batch count grew {} -> {}",
        batches_before_reverts,
        batches_list(&h)?.len()
    ))
}

// ---------------------------------------------------------------- S3

/// Two content versions, then a deletion; the delete marker appears in the
/// history and reverting to the newest non-delete version restores the exact
/// pre-delete content.
fn s3_delete_restore(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let doc = h.ws_path(0).join("doc.txt");
    let p = doc.display().to_string();
    let (v1, v2) = ("document version one\n", "document version two\n");
    io(fs::write(&doc, v1), "failed to write v1")?;
    wave_gap();
    io(fs::write(&doc, v2), "failed to write v2")?;
    h.wait_history(&doc, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    io(fs::remove_file(&doc), "failed to delete doc.txt")?;
    let hist = h.wait_history(&doc, 10_000, |v| hist_has_kind(v, "delete"))?;

    // The version right before the delete marker = the newest non-delete entry.
    let target = v_arr(&hist, "versions")?
        .iter()
        .find(|x| v_str(x, "kind").is_ok_and(|k| k != "delete"))
        .ok_or_else(|| "no non-delete version before the delete marker".to_string())?
        .clone();
    let seq = v_i64(&target, "seq")?;
    if v_str(&target, "kind")? != "external" {
        return Err(format!("the pre-delete version has unexpected kind: {target}"));
    }

    let r = h.cli_json_value(&["revert", &p, "--yes", "--at", &seq.to_string()])?;
    if !v_bool(&r, "reverted")? {
        return Err(format!("revert reported reverted=false: {r}"));
    }
    if v_i64(&r, "target_seq")? != seq {
        return Err(format!("revert target_seq is not {seq}: {r}"));
    }
    wait_disk_eq(&doc, v2.as_bytes(), 3_000)?;
    Ok(format!(
        "delete marker recorded; revert --at {seq} restored the file with the byte-exact pre-delete content"
    ))
}

// ---------------------------------------------------------------- S4

/// `lh label <file> "checkpoint"` creates a kind=label version carrying the
/// label text, visible in both the JSON and the text history table.
fn s4_label_flow(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let f = h.ws_path(0).join("labeled.txt");
    let p = f.display().to_string();
    io(fs::write(&f, "content before the label\n"), "failed to write v1")?;
    wave_gap();
    io(fs::write(&f, "content after the edit\n"), "failed to write v2")?;
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    let l = h.cli_json_value(&["label", &p, "checkpoint"])?;
    if v_str(&l, "label")? != "checkpoint" {
        return Err(format!("label command returned a different text: {l}"));
    }
    let seq = v_i64(&l, "seq")?;
    if seq <= 0 {
        return Err(format!("label command returned no seq: {l}"));
    }
    h.wait_history(&f, 10_000, |v| {
        v_arr(v, "versions").is_ok_and(|a| {
            a.iter().any(|x| {
                v_str(x, "kind").is_ok_and(|k| k == "label")
                    && x.get("label").and_then(Value::as_str) == Some("checkpoint")
            })
        })
    })?;
    let t = h.cli(&["history", &p])?;
    if t.code != 0 || !t.stdout.contains("checkpoint") {
        return Err(format!(
            "text history output does not show the label (exit {}): {}",
            t.code,
            truncate(&t.stdout, 400)
        ));
    }
    Ok(format!("label version recorded at seq {seq} and visible in the history table"))
}

// ---------------------------------------------------------------- S5

/// A binary file (NUL inside the first 8 KiB — the record-side rule — plus an
/// invalid UTF-8 byte, the diff-side rule in diff.rs) is recorded and `lh
/// diff` refuses it with exit code 2 and the "binary" message.
fn s5_binary_file(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let bin = h.ws_path(0).join("blob.bin");
    let p = bin.display().to_string();
    let mut content = vec![b'A'; 4096];
    content.push(0);
    content.push(0xFF);
    content.extend_from_slice(&[b'B'; 4096]);
    io(fs::write(&bin, &content), "failed to write blob.bin")?;
    h.wait_history(&bin, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let d = h.cli_json(&["diff", &p, "1", "current"])?;
    if d.code != 2 {
        return Err(format!(
            "binary diff exit code is {} (expected 2): {}",
            d.code,
            truncate(&d.stdout, 300)
        ));
    }
    if !d.stdout.contains("binary") {
        return Err(format!(
            "binary diff output does not mention binary: {}",
            truncate(&d.stdout, 400)
        ));
    }
    Ok("binary version recorded (full storage); diff exits 2 with the binary message".into())
}

// ---------------------------------------------------------------- S6

/// Pinned rename contract: `fs::rename a.txt -> b.txt` is recorded as
/// delete(a.txt) + create(b.txt) — the daemon does NOT track renames.
fn s6_rename_contract(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let a = ws.join("a.txt");
    let b = ws.join("b.txt");
    io(fs::write(&a, "rename contract original\n"), "failed to write a.txt")?;
    h.wait_history(&a, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    io(fs::rename(&a, &b), "failed to rename a.txt -> b.txt")?;
    let pair = poll_until(10_000, POLL_INTERVAL_MS, || {
        let ha = h.history_json(&a).ok()?;
        let hb = h.history_json(&b).ok()?;
        (hist_has_kind(&ha, "delete") && hist_has_kind(&hb, "external")).then_some((ha, hb))
    });
    match pair {
        Some((_, hb)) => {
            let newest = v_arr(&hb, "versions")?.first().cloned().unwrap_or_default();
            if v_str(&newest, "kind")? != "external" {
                return Err(format!("b.txt newest version is not external: {newest}"));
            }
            Ok("rename recorded as delete(a.txt) + create(b.txt) — pinned delete+create behavior".into())
        }
        None => Err("rename was not recorded as delete(a.txt) + create(b.txt) within 10s".into()),
    }
}

// ---------------------------------------------------------------- S7

/// One daemon watching two workspaces: both are registered, and each file's
/// history stays scoped to its own workspace.
fn s7_multi_workspace(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 2)?;
    let a_dir = h.ws_path(0);
    let b_dir = h.ws_path(1);
    io(fs::write(a_dir.join("a.txt"), "workspace A file\n"), "failed to write a.txt")?;
    io(fs::write(b_dir.join("b.txt"), "workspace B file\n"), "failed to write b.txt")?;

    let wss = poll_until(10_000, POLL_INTERVAL_MS, || {
        h.cli_json_value(&["workspaces"]).ok().filter(|j| {
            j.as_array().is_some_and(|a| {
                a.len() == 2
                    && a.iter()
                        .any(|w| v_u64(w, "versions").is_ok_and(|n| n >= 1))
            })
        })
    })
    .ok_or_else(|| "workspaces never reached 2 registered entries with versions".to_string())?;
    let arr = wss
        .as_array()
        .ok_or_else(|| "workspaces JSON is not an array".to_string())?;
    let roots: Vec<String> = arr
        .iter()
        .filter_map(|w| w.get("root").and_then(Value::as_str).map(str::to_string))
        .collect();
    for d in [&a_dir, &b_dir] {
        let want = canon_str(d).to_ascii_lowercase();
        if !roots.iter().any(|r| r.to_ascii_lowercase() == want) {
            return Err(format!("workspace root for {} not registered (roots: {roots:?})", d.display()));
        }
    }

    let ha = h.wait_history(&a_dir.join("a.txt"), 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    let hb = h.wait_history(&b_dir.join("b.txt"), 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    let ws_a = v_str(&ha, "workspace")?.to_string();
    let ws_b = v_str(&hb, "workspace")?.to_string();
    if !ws_a.eq_ignore_ascii_case(&canon_str(&a_dir)) {
        return Err(format!("a.txt resolved to workspace {ws_a} (expected {})", canon_str(&a_dir)));
    }
    if ws_b.eq_ignore_ascii_case(&ws_a) {
        return Err("history of b.txt resolved to the same workspace as a.txt".into());
    }
    let count_a = v_u64(&ha, "count")?;
    if count_a != 1 {
        return Err(format!("a.txt history has {count_a} versions (expected exactly 1 — workspace B must not leak in)"));
    }
    Ok("2 workspaces registered by one daemon; per-file history is workspace-scoped".into())
}

// ---------------------------------------------------------------- S8

/// 200 new files written inside one batch window; all 200 versions recorded
/// within 60s, and the new batches cover at least 150 distinct files.
fn s8_wave_200_files(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let start = Instant::now();
    for i in 0..200 {
        io(
            fs::write(ws.join(format!("wave_{i:03}.txt")), format!("wave file {i} content\n")),
            "failed to write a wave file",
        )?;
    }
    let write_ms = start.elapsed().as_millis();

    let st = poll_until(55_000, POLL_INTERVAL_MS, || {
        h.status_json().ok().filter(|j| {
            j.get("workspaces").and_then(Value::as_array).is_some_and(|a| {
                a.iter().any(|w| v_u64(w, "versions").is_ok_and(|n| n >= 200))
            })
        })
    });
    let st = match st {
        Some(j) => j,
        None => {
            return Err(format!(
                "200 versions were not recorded within 55s (writes took {write_ms}ms)"
            ))
        }
    };
    let elapsed_ms = start.elapsed().as_millis();
    if elapsed_ms >= 60_000 {
        return Err(format!("recording took {elapsed_ms}ms (>= 60s)"));
    }
    let arr = st
        .get("workspaces")
        .and_then(Value::as_array)
        .ok_or_else(|| "status JSON has no workspaces array".to_string())?;
    let w = arr
        .first()
        .ok_or_else(|| "status workspaces array is empty".to_string())?;
    let files = v_u64(w, "files")?;
    if files != 200 {
        return Err(format!("workspace file count is {files} (expected 200)"));
    }
    let versions = v_u64(w, "versions")?;
    if versions != 200 {
        return Err(format!("workspace version count is {versions} (expected 200)"));
    }
    let batches = batches_list(&h)?;
    let sum: u64 = batches
        .iter()
        .filter_map(|b| b.get("files").and_then(Value::as_u64))
        .sum();
    if sum < 150 {
        return Err(format!("batch file_count sum is {sum} (< 150; {} batches)", batches.len()));
    }
    Ok(format!(
        "200 files written in {write_ms}ms, all recorded in {elapsed_ms}ms across {} batch(es), file_count sum={sum}",
        batches.len()
    ))
}

// ---------------------------------------------------------------- S9

/// Self-update against a real distribution directory, using the manifest
/// schema of scripts/publish-update.ps1 / crates/updater:
/// - success: version differs, sha256 correct -> exit 0, artifacts swapped
///   (the updater only swaps artifacts whose target file already exists in the
///   install dir, so VERSION.txt is pre-created there and its content change
///   is the observable swap proof);
/// - rollback: wrong sha256 -> non-zero exit, install untouched.
fn s9_self_update(ctx: &Ctx) -> Res {
    let lh = &ctx.lh;
    let exe_name = format!("lh{}", std::env::consts::EXE_SUFFIX);
    let lh_bytes = io(fs::read(lh), "failed to read the lh binary")?;
    let sha_lh = sha256_hex(&lh_bytes);

    // ---- success case -------------------------------------------------
    let install = temp_dir("install dir")?;
    let dist = temp_dir("dist dir")?;
    io(fs::write(install.path().join(&exe_name), &lh_bytes), "failed to write the install exe")?;
    // the copy must be runnable on Unix (fs::write creates a 0644 file)
    #[cfg(unix)]
    io(
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                install.path().join(&exe_name),
                fs::Permissions::from_mode(0o755),
            )
        },
        "failed to chmod the install exe",
    )?;
    io(
        fs::write(install.path().join("VERSION.txt"), "install version 1\n"),
        "failed to write the install VERSION.txt",
    )?;
    io(fs::write(dist.path().join(&exe_name), &lh_bytes), "failed to write the dist exe")?;
    let new_version_txt = "dist version 2\n";
    io(
        fs::write(dist.path().join("VERSION.txt"), new_version_txt),
        "failed to write the dist VERSION.txt",
    )?;
    let manifest = serde_json::json!({
        "version": "999.0.0",
        "artifacts": [
            { "name": exe_name, "sha256": sha_lh },
            { "name": "VERSION.txt", "sha256": sha256_hex(new_version_txt.as_bytes()) },
        ],
    });
    io(
        fs::write(
            dist.path().join("manifest.json"),
            serde_json::to_vec(&manifest).map_err(|e| format!("manifest: {e}"))?,
        ),
        "failed to write manifest.json",
    )?;

    let out = run_self_update(&install.path().join(&exe_name), dist.path())?;
    if out.code != 0 {
        return Err(format!(
            "self-update exited {} (expected 0)\nstdout: {}\nstderr: {}",
            out.code,
            truncate(&out.stdout, 800),
            truncate(&out.stderr, 400)
        ));
    }
    let j = out.json()?;
    if v_str(&j, "to")? != "999.0.0" {
        return Err(format!("self-update target version is not 999.0.0: {j}"));
    }
    if v_bool(&j, "up_to_date")? {
        return Err(format!("self-update reported up_to_date=true: {j}"));
    }
    let updated: Vec<String> = v_arr(&j, "updated")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    for expected in [exe_name.as_str(), "VERSION.txt"] {
        if !updated.iter().any(|u| u == expected) {
            return Err(format!("self-update did not report `{expected}` as updated: {updated:?}"));
        }
    }
    let ver_now = io(
        fs::read(install.path().join("VERSION.txt")),
        "failed to read the swapped VERSION.txt",
    )?;
    if ver_now != new_version_txt.as_bytes() {
        return Err("VERSION.txt in the install dir did not change — the swap did not happen".into());
    }
    if !install.path().join(&exe_name).exists() {
        return Err("the lh binary is missing after a successful self-update".into());
    }

    // ---- rollback case (wrong sha256) ---------------------------------
    let install2 = temp_dir("second install dir")?;
    let dist2 = temp_dir("second dist dir")?;
    io(fs::write(install2.path().join(&exe_name), &lh_bytes), "failed to write install2 exe")?;
    io(
        fs::write(install2.path().join("VERSION.txt"), "original\n"),
        "failed to write install2 VERSION.txt",
    )?;
    io(fs::write(dist2.path().join(&exe_name), &lh_bytes), "failed to write dist2 exe")?;
    io(fs::write(dist2.path().join("VERSION.txt"), "newer\n"), "failed to write dist2 VERSION.txt")?;
    let manifest_bad = serde_json::json!({
        "version": "999.0.0",
        "artifacts": [
            { "name": exe_name, "sha256": "0".repeat(64) },
            { "name": "VERSION.txt", "sha256": sha256_hex(b"newer\n") },
        ],
    });
    io(
        fs::write(
            dist2.path().join("manifest.json"),
            serde_json::to_vec(&manifest_bad).map_err(|e| format!("manifest: {e}"))?,
        ),
        "failed to write the bad manifest.json",
    )?;
    let out2 = run_self_update(&install2.path().join(&exe_name), dist2.path())?;
    if out2.code == 0 {
        return Err(format!(
            "self-update with a wrong sha256 exited 0 (expected failure)\nstdout: {}",
            truncate(&out2.stdout, 800)
        ));
    }
    let j2 = out2.json()?;
    if j2.get("error").is_none() {
        return Err(format!("the failed self-update did not report an error: {j2}"));
    }
    let exe_after = io(fs::read(install2.path().join(&exe_name)), "failed to re-read install2 exe")?;
    if exe_after != lh_bytes {
        return Err("the install exe changed despite the failed update — rollback is broken".into());
    }
    let ver2 = io(
        fs::read(install2.path().join("VERSION.txt")),
        "failed to re-read install2 VERSION.txt",
    )?;
    if ver2 != b"original\n" {
        return Err("install2 VERSION.txt changed despite the failed update".into());
    }
    Ok(format!(
        "swap succeeded (lh.exe + VERSION.txt -> 999.0.0, updated={updated:?}); wrong-sha256 run failed with exit {} and left the install untouched",
        out2.code
    ))
}

/// Run `<install>/lh self-update --source <dist>` as a real process.
fn run_self_update(exe: &Path, dist: &Path) -> Result<CliOut, String> {
    let dist_str = dist.to_string_lossy().into_owned();
    let mut cmd = Command::new(exe);
    cmd.args(["--json", "--lang", "en", "self-update", "--source", &dist_str]);
    cmd.env("LH_LANG", "en");
    let out = cmd
        .output()
        .map_err(|e| format!("failed to run {}: {e}", exe.display()))?;
    Ok(CliOut {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn temp_dir(what: &str) -> Result<TempDir, String> {
    TempDir::new().map_err(|e| format!("failed to create {what}: {e}"))
}

// ---------------------------------------------------------------- S10

/// Kill the daemon -> status reports alive=false within 5s; respawn -> alive=
/// true; a second daemon on the same store exits non-zero (single-instance
/// lock) without hanging, and the first daemon survives it.
fn s10_daemon_resilience(ctx: &Ctx) -> Res {
    let mut h = Harness::new(&ctx.lh, 1)?;
    h.kill_daemon()?;
    let dead = poll_until(5_000, POLL_INTERVAL_MS, || {
        h.status_json().ok().filter(|j| !status_alive(j))
    });
    if dead.is_none() {
        return Err("lh status still reports the daemon alive 5s after the kill".into());
    }

    h.spawn_daemon()?;
    if !h.daemon_alive() {
        return Err("the respawned daemon process already exited".into());
    }
    let j = h.status_json()?;
    if !status_alive(&j) {
        return Err(format!("the respawned daemon is not reported alive: {j}"));
    }

    let code = spawn_second_daemon(&h)?;
    if code == 0 {
        return Err(
            "a second daemon on the same store exited 0 — the single-instance lock is not enforced"
                .into(),
        );
    }
    let j2 = h.status_json()?;
    if !status_alive(&j2) {
        return Err("the first daemon died while the second one was being rejected".into());
    }
    Ok(format!(
        "kill -> alive=false; respawn -> alive=true; second daemon rejected with exit {code} while the first kept running"
    ))
}

// ---------------------------------------------------------------- S11

/// `lh gc --dry-run` exits 0, says "dry-run" in text mode, reports
/// dry_run=true / zero deletions in JSON mode, and removes nothing.
fn s11_gc_dry_run(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f1 = ws.join("gc-one.txt");
    let f2 = ws.join("gc-two.txt");
    io(fs::write(&f1, "gc target one\n"), "failed to write gc-one.txt")?;
    io(fs::write(&f2, "gc target two\n"), "failed to write gc-two.txt")?;
    h.wait_history(&f1, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    h.wait_history(&f2, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let g = h.cli(&["gc", "--dry-run"])?;
    if g.code != 0 {
        return Err(format!("gc --dry-run exited {}: {}", g.code, truncate(&g.stderr, 300)));
    }
    if !g.stdout.contains("dry-run") {
        return Err(format!(
            "gc --dry-run text output does not contain 'dry-run': {}",
            truncate(&g.stdout, 400)
        ));
    }
    let gj: GcJson = h.cli_json_as(&["gc", "--dry-run"])?;
    if !gj.dry_run {
        return Err(format!("gc --dry-run JSON reports dry_run=false: {gj:?}"));
    }
    if gj.versions_deleted != 0 {
        return Err(format!("gc --dry-run deleted {} versions", gj.versions_deleted));
    }
    if gj.files_scanned < 1 {
        return Err(format!("gc --dry-run scanned no files: {gj:?}"));
    }
    for (p, content) in [(&f1, "gc target one\n"), (&f2, "gc target two\n")] {
        let now = io(fs::read(p), "failed to read after gc")?;
        if now != content.as_bytes() {
            return Err(format!("{} was changed/removed by gc --dry-run", p.display()));
        }
    }
    Ok("gc --dry-run exits 0, reports dry-run in text and JSON, deletes nothing".into())
}

// ---------------------------------------------------------------- S12

/// `.git/HEAD` and `node_modules/x.js` are never recorded (ignore filter),
/// proven against a live daemon that DOES record a tracked file.
fn s12_ignored_dirs(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    io(fs::create_dir_all(ws.join(".git")), "failed to create .git")?;
    io(fs::create_dir_all(ws.join("node_modules")), "failed to create node_modules")?;
    let started = Instant::now();
    io(
        fs::write(ws.join(".git").join("HEAD"), "ref: refs/heads/main\n"),
        "failed to write .git/HEAD",
    )?;
    io(
        fs::write(ws.join("node_modules").join("x.js"), "console.log('ignored');\n"),
        "failed to write node_modules/x.js",
    )?;
    io(fs::write(ws.join("tracked.txt"), "tracked content\n"), "failed to write tracked.txt")?;

    // The tracked file MUST be recorded — proves the daemon is alive and the
    // zero counts below are not just a dead watcher.
    h.wait_history(&ws.join("tracked.txt"), 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    // Wait well over the debounce window for the ignored files too (>1.5s).
    let elapsed = started.elapsed();
    if elapsed < Duration::from_millis(1500) {
        sleep(Duration::from_millis(1500) - elapsed);
    }
    for f in [".git/HEAD", "node_modules/x.js"] {
        let j = h.history_json(&ws.join(f))?;
        let c = v_u64(&j, "count")?;
        if c != 0 {
            return Err(format!("{f} recorded {c} version(s) — the watcher must ignore it"));
        }
    }
    Ok("tracked.txt recorded; .git/HEAD and node_modules/x.js have zero versions".into())
}
