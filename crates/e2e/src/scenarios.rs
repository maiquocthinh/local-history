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
    orphan_blobs: u64,
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
        ("rename_keeps_chain", s6_rename_keeps_chain),
        ("multi_workspace", s7_multi_workspace),
        ("wave_200_files", s8_wave_200_files),
        ("self_update", s9_self_update),
        ("daemon_resilience", s10_daemon_resilience),
        ("gc_dry_run", s11_gc_dry_run),
        ("ignored_dirs", s12_ignored_dirs),
        ("rename_collision_fallback", s13_rename_collision_fallback),
        ("debounce_coalesce", s14_debounce_coalesce),
        ("kill_mid_record", s15_kill_mid_record),
        ("corrupt_blob_recovery", s16_corrupt_blob_recovery),
        ("delta_chain_rollover", s17_delta_chain_rollover),
        ("revert_noop", s18_revert_noop),
        ("rename_out_of_ws", s19_rename_out_of_ws),
        ("rename_to_ignored", s20_rename_to_ignored),
        ("concurrent_writers", s21_concurrent_writers),
        ("stale_lock_takeover", s22_stale_lock_takeover),
        ("full_policy_big_patch", s23_full_policy_big_patch),
        ("empty_and_weird_files", s24_empty_and_weird_files),
        ("orphan_blob_gc", s25_orphan_blob_gc),
        ("max_file_size_cap", s26_max_file_size_cap),
        ("command_matrix", s27_command_matrix),
        ("update_noop", s28_update_noop),
        ("version_file_missing", s29_version_file_missing),
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

/// Pinned rename contract: `fs::rename a.txt -> b.txt` carries the WHOLE
/// version chain over to the new path (JetBrains-style) — one rename version
/// labeled with the old rel path, no delete marker, files count unchanged.
fn s6_rename_keeps_chain(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let a = ws.join("a.txt");
    let b = ws.join("b.txt");
    io(fs::write(&a, "rename contract original\n"), "failed to write a.txt")?;
    h.wait_history(&a, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    io(fs::write(&a, "rename contract revision 2\n"), "failed to write a.txt rev 2")?;
    wave_gap();
    io(fs::write(&a, "rename contract revision 3\n"), "failed to write a.txt rev 3")?;
    let before = h.wait_history(&a, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 3)
    })?;
    let n_before = before.get("count").and_then(Value::as_u64).unwrap_or(0);
    let files_before = v_u64(
        v_arr(&h.status_json()?, "workspaces")?
            .first()
            .ok_or("no workspace in status")?,
        "files",
    )?;

    io(fs::rename(&a, &b), "failed to rename a.txt -> b.txt")?;
    let got = poll_until(10_000, POLL_INTERVAL_MS, || {
        let hb = h.history_json(&b).ok()?;
        let arr = v_arr(&hb, "versions").ok()?;
        let newest = arr.first()?;
        (v_str(newest, "kind").ok()? == "rename").then_some(hb)
    });
    let hb = got.ok_or("b.txt did not gain a rename version within 10s")?;

    // (i) chain moved over intact: n_before old versions + exactly one rename, newest-first.
    let arr = v_arr(&hb, "versions")?;
    if arr.len() as u64 != n_before + 1 {
        return Err(format!(
            "b.txt has {} versions, expected {} (old chain + 1 rename)",
            arr.len(),
            n_before + 1
        ));
    }
    let newest = arr.first().unwrap();
    if v_str(newest, "label")? != "a.txt" {
        return Err(format!("rename label is not the old rel path: {newest}"));
    }
    for v in before
        .get("versions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let seq = v.get("seq").and_then(Value::as_i64).unwrap_or(0);
        if find_seq(&hb, seq).is_none() {
            return Err(format!("pre-rename seq {seq} missing from b.txt's chain"));
        }
    }

    // (ii) the files count is unchanged (rename reuses the same file row).
    let files_after = v_u64(
        v_arr(&h.status_json()?, "workspaces")?
            .first()
            .ok_or("no workspace in status")?,
        "files",
    )?;
    if files_after != files_before {
        return Err(format!("files count changed {files_before} -> {files_after}"));
    }

    // (iii) the latest version reconstructs to the on-disk content.
    let pb = b.display().to_string();
    let s = h.cli(&["show", &pb, "--at", "latest"])?;
    if s.code != 0 || s.stdout != "rename contract revision 3\n" {
        return Err(format!(
            "show latest mismatch (exit {}): {:?}",
            s.code,
            truncate(&s.stdout, 200)
        ));
    }

    // (iv) the old path has NO history at all — no delete marker either.
    let ha = h.history_json(&a)?;
    if ha.get("count").and_then(Value::as_u64).unwrap_or(1) != 0 {
        return Err(format!("a.txt still has history (delete marker?): {ha}"));
    }
    Ok(format!(
        "rename kept the chain: {n_before} old versions moved to b.txt + 1 rename (label a.txt); \
         files count {files_before} unchanged; show latest byte-exact; a.txt history empty"
    ))
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
    // same Unix requirement as the first install copy: the file must be
    // runnable for the rollback-case self-update process to start at all
    #[cfg(unix)]
    io(
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                install2.path().join(&exe_name),
                fs::Permissions::from_mode(0o755),
            )
        },
        "failed to chmod the install2 exe",
    )?;
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

// ---------------------------------------------------------------- S13

/// Renaming a file ONTO a tracked path (both have history) must NOT merge the
/// chains: the engine refuses the collision, so the daemon's fallback records
/// a delete marker on the old path and new content on the destination — two
/// separate chains, nothing lost.
fn s13_rename_collision_fallback(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let a = ws.join("a.txt");
    let c = ws.join("c.txt");
    io(fs::write(&a, "a content v1\n"), "failed to write a.txt")?;
    wave_gap();
    io(fs::write(&a, "a content v2\n"), "failed to write a.txt v2")?;
    io(fs::write(&c, "c own history\n"), "failed to write c.txt")?;
    h.wait_history(&a, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|k| k >= 2)
    })?;
    h.wait_history(&c, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|k| k >= 1)
    })?;

    io(fs::rename(&a, &c), "failed to rename a.txt over c.txt")?;
    // Old path gains a delete marker; destination gains the new content.
    let got = poll_until(10_000, POLL_INTERVAL_MS, || {
        let ha = h.history_json(&a).ok()?;
        let hc = h.history_json(&c).ok()?;
        let newest_a = v_arr(&ha, "versions").ok()?.first()?.get("kind").and_then(Value::as_str)?;
        let newest_c = v_arr(&hc, "versions").ok()?.first()?.get("kind").and_then(Value::as_str)?;
        (newest_a == "delete" && newest_c == "external").then_some((ha, hc))
    });
    let (ha, hc) = got.ok_or("collision rename was not recorded as delete + external within 10s")?;

    // Old chain intact + exactly one delete marker appended.
    if v_u64(&ha, "count")? != 3 {
        return Err(format!("a.txt should have 2 old versions + 1 delete marker: {}", v_u64(&ha, "count")?));
    }
    // Destination chain intact + one external appended (its own old version stays).
    let c_count = v_u64(&hc, "count")?;
    if c_count != 2 {
        return Err(format!("c.txt should have its own version + 1 external: {c_count}"));
    }
    // The destination's latest content is what landed on disk (a.txt's bytes).
    let s = h.cli(&["show", &c.display().to_string(), "--at", "latest"])?;
    if s.code != 0 || s.stdout != "a content v2\n" {
        return Err(format!(
            "c.txt latest must be the renamed content (exit {}): {:?}",
            s.code,
            truncate(&s.stdout, 120)
        ));
    }
    // The chains stayed separate: c's seq-1 content is still its own.
    let s = h.cli(&["show", &c.display().to_string(), "--at", "1"])?;
    if s.code != 0 || s.stdout != "c own history\n" {
        return Err(format!("c.txt seq 1 must still be its own content: {s:?}"));
    }
    Ok(
        "collision rename kept the chains separate: a.txt 2 old + delete marker, c.txt own + external, contents byte-exact"
            .into(),
    )
}

// ---------------------------------------------------------------- S14

/// Ten writes of one file inside ONE debounce window coalesce into exactly
/// one version, whose content is the last write.
fn s14_debounce_coalesce(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f = ws.join("rapid.txt");
    let p = f.display().to_string();
    let mut last = String::new();
    for i in 1..=10 {
        last = format!("rapid write {i}\n");
        io(fs::write(&f, &last), "failed to write rapid.txt")?;
        sleep(Duration::from_millis(30)); // 10 × 30ms = 300ms < the 400ms debounce
    }
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    // Let every trailing event settle well past the debounce window.
    sleep(Duration::from_millis(1500));
    let j = h.history_json(&f)?;
    let count = v_u64(&j, "count")?;
    if count != 1 {
        return Err(format!(
            "10 writes within one debounce window must coalesce into 1 version, got {count}"
        ));
    }
    let s = h.cli(&["show", &p, "--at", "latest"])?;
    if s.code != 0 || s.stdout != last {
        return Err(format!(
            "coalesced version must be the LAST write (exit {}): {:?}",
            s.code,
            truncate(&s.stdout, 120)
        ));
    }
    Ok("10 writes in 300ms coalesced into exactly 1 version = the last write".into())
}

// ---------------------------------------------------------------- S15

/// A daemon killed hard mid-burst must not corrupt the store: after respawn
/// the store opens, previously recorded history survives, and a fresh touch
/// wave records every file byte-exactly (in-flight debounce state is allowed
/// to be lost — that is the pinned contract, not a bug).
fn s15_kill_mid_record(ctx: &Ctx) -> Res {
    let mut h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let base = ws.join("base.txt");
    io(fs::write(&base, "base before crash\n"), "failed to write base.txt")?;
    h.wait_history(&base, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    // Burst: 20 quick writes; the daemon dies right in the middle.
    for i in 0..20 {
        let f = ws.join(format!("burst_{i:02}.txt"));
        io(fs::write(&f, format!("burst {i} pre-crash\n")), "failed to write burst file")?;
        if i == 10 {
            h.kill_daemon()?;
        }
        sleep(Duration::from_millis(30));
    }
    // Respawn on the same store — the single instance must take over cleanly.
    h.spawn_daemon()?;

    // Previously recorded history survives the crash.
    let j = h.history_json(&base)?;
    if v_u64(&j, "count")? < 1 {
        return Err("base.txt history lost after the crash".into());
    }

    // Touch EVERY burst file: after respawn + quiet ≥ debounce each must be
    // recorded, and the newest version must reconstruct byte-exactly.
    for i in 0..20 {
        let f = ws.join(format!("burst_{i:02}.txt"));
        io(
            fs::write(&f, format!("burst {i} post-respawn\n")),
            "failed to touch burst file",
        )?;
    }
    for i in 0..20 {
        let f = ws.join(format!("burst_{i:02}.txt"));
        let content = format!("burst {i} post-respawn\n");
        h.wait_history(&f, 10_000, |v| {
            v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
        })?;
        let p = f.display().to_string();
        let s = h.cli(&["show", &p, "--at", "latest"])?;
        if s.code != 0 || s.stdout != content {
            return Err(format!(
                "show latest mismatch after crash for {} (exit {}): {:?}",
                p,
                s.code,
                truncate(&s.stdout, 120)
            ));
        }
    }
    Ok("kill -9 mid-burst: store reopened cleanly, old history intact, all 20 files recorded byte-exact after respawn".into())
}

// ---------------------------------------------------------------- S16

/// A corrupted blob must fail CLEANLY: show/diff exit non-zero with an error
/// message (never a panic), the rest of the store stays usable, and the
/// daemon keeps running.
fn s16_corrupt_blob_recovery(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f = ws.join("victim.txt");
    let p = f.display().to_string();
    io(fs::write(&f, "victim v1\n"), "failed to write victim.txt")?;
    wave_gap();
    io(fs::write(&f, "victim v2\n"), "failed to write victim.txt v2")?;
    let hist = h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    // Locate seq 1's blob: workspaces/<ws:016x>/blobs/v<id>-full.zst
    let ws_id = v_u64(
        v_arr(&h.status_json()?, "workspaces")?
            .first()
            .ok_or("no workspace in status")?,
        "id",
    )?;
    let v1 = find_seq(&hist, 1).ok_or("seq 1 missing from history")?;
    let v1_id = v_i64(v1, "id")?;
    let storage = v_str(v1, "storage")?.to_string();
    let blob = h
        .store
        .path()
        .join("workspaces")
        .join(format!("{ws_id:016x}"))
        .join("blobs")
        .join(format!("v{v1_id}-{storage}.zst"));
    if !blob.exists() {
        return Err(format!("blob file not found at {}", blob.display()));
    }
    io(fs::write(&blob, b"this is not zstd data"), "failed to corrupt the blob")?;

    // show on the corrupted seq → clean failure (non-zero, message, no panic).
    let s_bad = h.cli(&["show", &p, "--at", "1"])?;
    if s_bad.code == 0 {
        return Err("show --at 1 must fail on a corrupted blob".into());
    }
    if s_bad.code == 101 || s_bad.code == 134 {
        return Err(format!("show --at 1 panicked (exit {})", s_bad.code));
    }
    if s_bad.stdout.is_empty() && s_bad.stderr.is_empty() {
        return Err("show --at 1 failed without any error message".into());
    }
    // diff involving the corrupted base → clean failure too.
    let d = h.cli(&["diff", &p, "1", "current"])?;
    if d.code == 0 || d.code == 101 || d.code == 134 {
        return Err(format!("diff with a corrupted base must fail cleanly, exit {}", d.code));
    }

    // The untouched latest version still reconstructs; the daemon is alive.
    let s_ok = h.cli(&["show", &p, "--at", "latest"])?;
    if s_ok.code != 0 || s_ok.stdout != "victim v2\n" {
        return Err(format!("latest version must survive the corruption: {s_ok:?}"));
    }
    if !h.daemon_alive() {
        return Err("daemon died during corruption handling".into());
    }
    Ok(format!(
        "corrupt blob: show/diff fail cleanly (exit {} / {}), latest still byte-exact, daemon alive",
        s_bad.code, d.code
    ))
}

// ---------------------------------------------------------------- S17

/// 21 sequential edits: the delta policy must roll over to a full snapshot
/// (chain of 20 deltas → next version is a full), and EVERY seq must
/// reconstruct byte-exactly through `show --at k`.
fn s17_delta_chain_rollover(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let f = h.ws_path(0).join("chain.txt");
    let p = f.display().to_string();
    let mut contents: Vec<String> = Vec::new();
    for i in 1..=21 {
        let c: String = (0..i).map(|k| format!("chain v{i} line {k}\n")).collect();
        io(fs::write(&f, &c), "failed to write chain.txt")?;
        contents.push(c);
        h.wait_history(&f, 10_000, |v| {
            v.get("count").and_then(Value::as_u64).is_some_and(|n| n as usize >= i)
        })?;
        wave_gap();
    }
    let hist = h.history_json(&f)?;
    let count = v_u64(&hist, "count")?;
    if count != 21 {
        return Err(format!("21 edits must produce 21 versions (dedup never fires), got {count}"));
    }
    // seq 2..20 are deltas, seq 21 must be a full (chain hit the 20-delta cap).
    let v21 = find_seq(&hist, 21).ok_or("seq 21 missing")?;
    if v_str(v21, "storage")? != "full" {
        return Err(format!(
            "seq 21 must roll over to a full snapshot after 20 deltas: {}",
            v21
        ));
    }
    // Byte-exact reconstruction of every version.
    for (i, c) in contents.iter().enumerate() {
        let s = h.cli(&["show", &p, "--at", &(i + 1).to_string()])?;
        if s.code != 0 || s.stdout != *c {
            return Err(format!(
                "show --at {} mismatch (exit {}): {:?}",
                i + 1,
                s.code,
                truncate(&s.stdout, 100)
            ));
        }
    }
    Ok("21-edit chain: seq 21 rolled over to full, all 21 versions reconstruct byte-exact".into())
}

// ---------------------------------------------------------------- S18

/// Reverting when disk already equals the target is a no-op: no pre_revert
/// snapshot, no restore, history unchanged (the CLI reports pre_revert_seq
/// null).
fn s18_revert_noop(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let f = h.ws_path(0).join("noop.txt");
    let p = f.display().to_string();
    io(fs::write(&f, "noop v1\n"), "failed to write noop.txt")?;
    wave_gap();
    io(fs::write(&f, "noop v2\n"), "failed to write noop.txt v2")?;
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    // Real revert first: disk goes back to v1, pre_revert + restore recorded.
    let r = h.cli_json_value(&["revert", &p, "--yes", "--at", "1"])?;
    if !v_bool(&r, "reverted")? {
        return Err(format!("first revert reported reverted=false: {r}"));
    }
    if r.get("pre_revert_seq").map(Value::is_null).unwrap_or(true) {
        return Err(format!("first revert must record a pre_revert snapshot: {r}"));
    }
    let before = h.history_json(&f)?;
    let before_count = v_u64(&before, "count")?;

    // Reverting again to the SAME seq (disk already there) → nothing changes.
    let r2 = h.cli_json_value(&["revert", &p, "--yes", "--at", "1"])?;
    if !v_bool(&r2, "reverted")? {
        return Err(format!("no-op revert must still report reverted=true: {r2}"));
    }
    match r2.get("pre_revert_seq") {
        Some(Value::Null) | None => {}
        Some(other) => return Err(format!("no-op revert must not record pre_revert: {other}")),
    }
    let after = h.history_json(&f)?;
    if v_u64(&after, "count")? != before_count {
        return Err(format!(
            "no-op revert must not add versions: {} -> {}",
            before_count,
            v_u64(&after, "count")?
        ));
    }
    let disk = io(fs::read(&f), "failed to read after no-op revert")?;
    if disk != b"noop v1\n" {
        return Err("no-op revert must leave the disk untouched".into());
    }
    Ok(format!(
        "no-op revert: reverted=true, pre_revert_seq null, history unchanged ({before_count} versions), disk byte-exact"
    ))
}

// ---------------------------------------------------------------- S19

/// A rename whose destination is OUTSIDE every watched workspace is treated
/// as a plain removal: the old path gets a delete marker, and the file at its
/// new location is not tracked at all.
fn s19_rename_out_of_ws(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let a = ws.join("a.txt");
    io(fs::write(&a, "leaving the workspace\n"), "failed to write a.txt")?;
    wave_gap();
    io(fs::write(&a, "leaving v2\n"), "failed to write a.txt v2")?;
    h.wait_history(&a, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    // Move OUT of the workspace: the store tempdir is outside every watch dir.
    let outside = h.store.path().join("moved-out.txt");
    io(fs::rename(&a, &outside), "failed to move a.txt out of the workspace")?;

    let got = poll_until(10_000, POLL_INTERVAL_MS, || {
        let ha = h.history_json(&a).ok()?;
        let newest = v_arr(&ha, "versions").ok()?.first()?.get("kind").and_then(Value::as_str)?;
        (newest == "delete").then_some(ha)
    });
    let ha = got.ok_or("old path did not gain a delete marker within 10s")?;
    if v_u64(&ha, "count")? != 3 {
        return Err(format!(
            "old path should have 2 old versions + 1 delete marker, got {}",
            v_u64(&ha, "count")?
        ));
    }

    // The file at its new location is untracked: `lh history` outside a ws is
    // a clean error (non-zero exit), and the store never created versions.
    let p_out = outside.display().to_string();
    let s = h.cli(&["history", "--json", &p_out])?;
    if s.code == 0 {
        return Err(format!("history outside the workspace must fail, got: {}", s.stdout));
    }
    Ok("rename out of the workspace: delete marker on the old path, nothing tracked outside".into())
}

// ---------------------------------------------------------------- S20

/// Renaming a tracked file INTO an ignored directory (node_modules) records a
/// delete marker on the old path and NOTHING inside the ignored tree.
fn s20_rename_to_ignored(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    io(fs::create_dir_all(ws.join("node_modules")), "failed to create node_modules")?;
    let b = ws.join("b.txt");
    io(fs::write(&b, "moving into the void\n"), "failed to write b.txt")?;
    h.wait_history(&b, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let ignored_dest = ws.join("node_modules").join("b.txt");
    io(fs::rename(&b, &ignored_dest), "failed to move b.txt into node_modules")?;

    let got = poll_until(10_000, POLL_INTERVAL_MS, || {
        let hb = h.history_json(&b).ok()?;
        let newest = v_arr(&hb, "versions").ok()?.first()?.get("kind").and_then(Value::as_str)?;
        (newest == "delete").then_some(hb)
    });
    let hb = got.ok_or("old path did not gain a delete marker within 10s")?;
    if v_u64(&hb, "count")? != 2 {
        return Err(format!(
            "old path should have 1 old version + 1 delete marker, got {}",
            v_u64(&hb, "count")?
        ));
    }
    // Settle well past the debounce, then prove the ignored copy is untracked.
    sleep(Duration::from_millis(1800));
    let j = h.history_json(&ignored_dest)?;
    if v_u64(&j, "count")? != 0 {
        return Err(format!(
            "file inside node_modules must have zero versions, got {}",
            v_u64(&j, "count")?
        ));
    }
    Ok("rename into an ignored dir: delete marker on the old path, zero versions inside".into())
}

// ---------------------------------------------------------------- S21

/// Three writers append to their own files CONCURRENTLY (interleaved fs
/// events) — no content may be lost: each file's newest version reconstructs
/// byte-exactly to the on-disk content, with every writer's lines present.
fn s21_concurrent_writers(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let writers: Vec<String> = (0..3).map(|i| format!("writer_{i}.log")).collect();
    for name in &writers {
        io(fs::write(ws.join(name), ""), "failed to init writer file")?;
    }
    h.wait_history(&ws.join(&writers[0]), 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let handles: Vec<_> = writers
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let dir = ws.clone();
            let name = name.clone();
            std::thread::spawn(move || {
                use std::io::Write;
                for k in 0..20 {
                    let line = format!("writer {i} line {k}\n");
                    let mut f =
                        fs::OpenOptions::new().append(true).open(dir.join(&name)).unwrap();
                    f.write_all(line.as_bytes()).unwrap();
                    std::thread::sleep(Duration::from_millis(15));
                }
            })
        })
        .collect();
    for t in handles {
        t.join().map_err(|_| "writer thread panicked".to_string())?;
    }

    // Every writer's 20 lines must end up in the final version, byte-exact.
    for name in &writers {
        let f = ws.join(name);
        let expected = io(fs::read(&f), "failed to read writer file")?;
        let expected_str = String::from_utf8_lossy(&expected).into_owned();
        let line_count = expected_str.lines().count();
        if line_count != 20 {
            return Err(format!("{name} has {line_count} lines, expected 20"));
        }
        h.wait_history(&f, 15_000, |v| {
            v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
        })?;
        // Settle any trailing debounce, then compare the newest version to disk.
        sleep(Duration::from_millis(800));
        let s = h.cli(&["show", &f.display().to_string(), "--at", "latest"])?;
        if s.code != 0 || s.stdout != expected_str {
            return Err(format!(
                "{name}: newest version lost content (exit {}): {:?}",
                s.code,
                truncate(&s.stdout, 200)
            ));
        }
    }
    Ok("3 concurrent writers × 20 appends: no lost content, each file's latest version is byte-exact".into())
}

// ---------------------------------------------------------------- S22

/// After a hard kill the lock dies with the process (fd-lock) and daemon.json
/// goes stale: `lh status` must report alive=false cleanly (no crash), and a
/// respawned daemon must take over — replacing the stale status with its own
/// pid.
fn s22_stale_lock_takeover(ctx: &Ctx) -> Res {
    let mut h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f = ws.join("takeover.txt");
    io(fs::write(&f, "before takeover\n"), "failed to write takeover.txt")?;
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let status_path = h.store.path().join("daemon.json");
    let old_raw = io(fs::read_to_string(&status_path), "failed to read daemon.json")?;
    let old_pid = serde_json::from_str::<Value>(&old_raw)
        .ok()
        .and_then(|j| j.get("pid").and_then(Value::as_u64))
        .ok_or("daemon.json has no pid")?;

    // Hard kill → stale daemon.json with a dead pid.
    h.kill_daemon()?;
    let s = h.cli(&["--json", "status"])?;
    if s.code != 0 {
        return Err(format!("status on a stale daemon.json must exit 0, got {}", s.code));
    }
    let j = s.json()?;
    if status_alive(&j) {
        return Err("status must report alive=false while the pid is dead".into());
    }

    // Respawn: the OS lock is free again, the new daemon takes over.
    h.spawn_daemon()?;
    let j = h.status_json()?;
    if !status_alive(&j) {
        return Err("respawned daemon must report alive=true".into());
    }
    let new_raw = io(fs::read_to_string(&status_path), "failed to re-read daemon.json")?;
    let new_pid = serde_json::from_str::<Value>(&new_raw)
        .ok()
        .and_then(|j| j.get("pid").and_then(Value::as_u64))
        .ok_or("daemon.json after respawn has no pid")?;
    if new_pid == old_pid {
        return Err(format!("stale daemon.json was not replaced (pid {new_pid} unchanged)"));
    }
    // The store survived the takeover: history is still readable.
    let j = h.history_json(&f)?;
    if v_u64(&j, "count")? < 1 {
        return Err("history lost after the takeover".into());
    }
    Ok(format!(
        "stale lock takeover: status alive=false on the dead pid (old pid {old_pid}), respawn took over (pid {new_pid}), history intact"
    ))
}

// ---------------------------------------------------------------- S23

/// Editing ~80% of the lines at once must exceed the delta budget
/// (`patch >= content/2`) and store a FULL snapshot, not a delta.
fn s23_full_policy_big_patch(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let f = h.ws_path(0).join("big.txt");
    let p = f.display().to_string();
    let small: String = (0..100).map(|i| format!("original line {i}\n")).collect();
    io(fs::write(&f, &small), "failed to write big.txt")?;
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;

    let big: String = (0..100)
        .map(|i| {
            if i % 5 == 0 {
                format!("REWRITTEN line {i}\n")
            } else {
                format!("original line {i}\n")
            }
        })
        .collect();
    io(fs::write(&f, &big), "failed to rewrite big.txt")?;
    let hist = h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;
    let v2 = find_seq(&hist, 2).ok_or("seq 2 missing")?;
    if v_str(v2, "storage")? != "full" {
        return Err(format!("a patch this large must store a full snapshot, got: {v2}"));
    }
    let s = h.cli(&["show", &p, "--at", "latest"])?;
    if s.code != 0 || s.stdout != big {
        return Err(format!("rewritten content must reconstruct byte-exact: {s:?}"));
    }
    Ok("80%-line rewrite stored as a full snapshot (patch over the delta budget), content byte-exact".into())
}

// ---------------------------------------------------------------- S24

/// Odd files all round-trip byte-exactly: an empty file, NUL bytes in the
/// middle, CRLF line endings, and a unicode name with spaces.
fn s24_empty_and_weird_files(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let cases: Vec<(&str, &str)> = vec![
        ("empty.txt", ""),
        ("nulls.dat", "\u{0}\u{1}\u{0}data\u{0}"),
        ("crlf.txt", "line one\r\nline two\r\n"),
        ("tên có khoảng cách.txt", "nội dung tiếng Việt ✓\n"),
    ];
    for (name, content) in &cases {
        let f = ws.join(name);
        io(fs::write(&f, content), "failed to write the file")?;
        h.wait_history(&f, 10_000, |v| {
            v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
        })?;
        let p = f.display().to_string();
        let s = h.cli(&["show", &p, "--at", "latest"])?;
        if s.code != 0 || s.stdout != *content {
            return Err(format!(
                "{name}: content did not round-trip (exit {}): {:?}",
                s.code,
                truncate(&s.stdout, 120)
            ));
        }
    }
    // The empty file round-trips AND its first version is a full snapshot.
    let j = h.history_json(&ws.join("empty.txt"))?;
    let v1 = find_seq(&j, 1).ok_or("empty.txt seq 1 missing")?;
    if v_str(v1, "storage")? != "full" {
        return Err("the empty file's first version must be a full snapshot".into());
    }
    Ok("empty / NUL / CRLF / unicode-name files all recorded and reconstructed byte-exact".into())
}

// ---------------------------------------------------------------- S25

/// A blob file with no row in the db (orphan) is found and deleted by a REAL
/// `lh gc` run; live versions are untouched.
fn s25_orphan_blob_gc(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f = ws.join("gcme.txt");
    io(fs::write(&f, "gc orphan test\n"), "failed to write gcme.txt")?;
    wave_gap();
    io(fs::write(&f, "gc orphan test v2\n"), "failed to write gcme.txt v2")?;
    let hist = h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 2)
    })?;

    let ws_id = v_u64(
        v_arr(&h.status_json()?, "workspaces")?
            .first()
            .ok_or("no workspace in status")?,
        "id",
    )?;
    let blobs_dir = h
        .store
        .path()
        .join("workspaces")
        .join(format!("{ws_id:016x}"))
        .join("blobs");
    if !blobs_dir.exists() {
        return Err(format!("blobs dir missing at {}", blobs_dir.display()));
    }
    // Drop an orphan blob with an id no version references.
    let orphan = blobs_dir.join("v999999-full.zst");
    io(fs::write(&orphan, b"orphan blob body"), "failed to write the orphan blob")?;

    let g: GcJson = h.cli_json_as(&["gc"])?;
    if g.dry_run {
        return Err("real gc must not report dry_run=true".into());
    }
    if g.orphan_blobs < 1 {
        return Err(format!("gc must report the orphan blob, got {}", g.orphan_blobs));
    }
    if orphan.exists() {
        return Err("the orphan blob file was not deleted by gc".into());
    }
    // Live versions survive: seq 1 still reconstructs.
    let s = h.cli(&["show", &f.display().to_string(), "--at", "1"])?;
    if s.code != 0 || s.stdout != "gc orphan test\n" {
        return Err(format!("live version damaged by gc: {s:?}"));
    }
    let _ = find_seq(&hist, 1).ok_or("history broken after gc")?;
    Ok(format!(
        "orphan blob found ({}) and deleted; live versions intact",
        g.orphan_blobs
    ))
}

// ---------------------------------------------------------------- S26

/// A file larger than the 10MB cap is skipped entirely — no version, daemon
/// stays healthy — while normal files keep recording afterwards.
fn s26_max_file_size_cap(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let huge = ws.join("huge.bin");
    let mut body = Vec::with_capacity(11 * 1024 * 1024);
    for i in 0..11 * 1024 * 256 {
        body.extend_from_slice(format!("chunk {i}\n").as_bytes());
        if body.len() > 11 * 1024 * 1024 {
            break;
        }
    }
    body.truncate(11 * 1024 * 1024);
    io(fs::write(&huge, &body), "failed to write the oversized file")?;
    // Well past the debounce: the cap must have skipped it.
    sleep(Duration::from_millis(1800));
    let j = h.history_json(&huge)?;
    if v_u64(&j, "count")? != 0 {
        return Err(format!(
            "file over the 10MB cap must not be recorded, got {} version(s)",
            v_u64(&j, "count")?
        ));
    }
    if !h.daemon_alive() {
        return Err("daemon died handling the oversized file".into());
    }
    // The daemon still records normal files afterwards.
    let ok = ws.join("after.txt");
    io(fs::write(&ok, "still alive\n"), "failed to write after.txt")?;
    h.wait_history(&ok, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    Ok("file over the 10MB cap skipped (0 versions, daemon alive); normal recording continues".into())
}

// ---------------------------------------------------------------- S27

/// Every headless CLI subcommand runs against a live store: correct exit code
/// and output shape for text AND --json mode (daemon/gui/ui are covered by
/// the rest of the suite, self-update --check pins its clean error).
fn s27_command_matrix(ctx: &Ctx) -> Res {
    let h = Harness::new(&ctx.lh, 1)?;
    let ws = h.ws_path(0);
    let f = ws.join("matrix.txt");
    let p = f.display().to_string();
    io(fs::write(&f, "matrix v1\n"), "failed to write matrix.txt")?;
    wave_gap();
    io(fs::write(&f, "matrix v2\n"), "failed to write matrix.txt v2")?;
    wave_gap();
    io(fs::write(&f, "matrix v3\n"), "failed to write matrix.txt v3")?;
    h.wait_history(&f, 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 3)
    })?;
    // A second wave of files -> a batch to inspect/revert.
    for i in 0..3 {
        io(
            fs::write(ws.join(format!("batch_{i}.txt")), format!("batch file {i}\n")),
            "failed to write batch file",
        )?;
    }
    h.wait_history(&ws.join("batch_0.txt"), 10_000, |v| {
        v.get("count").and_then(Value::as_u64).is_some_and(|c| c >= 1)
    })?;
    let batches = v_arr(&h.batches_json()?, "batches")?.clone();
    // batches is newest-first; the newest batch (id = max) holds the 3 created
    // files — reverting it deletes them (they had no pre-batch state).
    let newest_id = batches
        .iter()
        .map(|b| v_u64(b, "id").unwrap_or(0))
        .max()
        .ok_or("no batch recorded")?;
    let batch_id = newest_id.to_string();

    // Text mode: exit 0 + non-empty output.
    for args in [
        vec!["status"],
        vec!["workspaces"],
        vec!["history", &p],
        vec!["show", &p, "--at", "latest"],
        vec!["diff", &p, "1", "2"],
        vec!["label", &p, "e2e matrix label"],
        vec!["gc", "--dry-run"],
        vec!["batches", "--limit", "5"],
        vec!["batch", &batch_id],
    ] {
        let out = h.cli(&args)?;
        if out.code != 0 {
            return Err(format!("`lh {}` exited {}: {}", args.join(" "), out.code, out.stderr));
        }
        if out.stdout.trim().is_empty() {
            return Err(format!("`lh {}` produced no output", args.join(" ")));
        }
    }
    // JSON mode: parses and has the essential field (history needs its file arg).
    let j = h.cli_json_value(&["status"])?;
    v_arr(&j, "workspaces").map_err(|e| format!("`lh status --json`: {e}"))?;
    let j = h.cli_json_value(&["history", &p])?;
    v_arr(&j, "versions").map_err(|e| format!("`lh history --json`: {e}"))?;
    let j = h.cli_json_value(&["batches"])?;
    v_arr(&j, "batches").map_err(|e| format!("`lh batches --json`: {e}"))?;
    let _ = h.cli_json_value(&["batch", &batch_id])?;
    // revert-batch on the newest batch (created files): the engine deletes
    // them (no pre-batch state to restore) — the report counts them under
    // `deleted`; anything reverted/restored/deleted ≥ 1 proves it worked.
    let r = h.cli_json_value(&["revert-batch", &batch_id, "--yes"])?;
    let acted = v_u64(&r, "reverted")? + v_u64(&r, "restored")? + v_u64(&r, "deleted")?;
    if acted < 1 {
        return Err(format!("revert-batch acted on 0 files: {r}"));
    }
    if ws.join("batch_0.txt").exists() {
        return Err("reverting the creation batch must delete batch_0.txt".into());
    }
    // self-update --check with an empty source dir: clean failure (no panic).
    let src = h.store.path().join("update-src");
    io(fs::create_dir_all(&src), "failed to create update source dir")?;
    let src_s = src.display().to_string();
    let s = h.cli(&["self-update", "--check", "--source", &src_s])?;
    if s.code == 101 || s.code == 134 {
        return Err(format!("self-update --check panicked (exit {})", s.code));
    }
    Ok(format!(
        "command matrix: status/workspaces/history/show/diff/label/gc/batches/batch/revert-batch (id {batch_id}) all exit 0, JSON shapes verified, self-update --check fails cleanly"
    ))
}

// ---------------------------------------------------------------- S28

/// Self-update against a manifest whose version EQUALS the running build:
/// exit 0, up_to_date=true, nothing swapped, install untouched.
fn s28_update_noop(ctx: &Ctx) -> Res {
    let lh = &ctx.lh;
    let exe_name = format!("lh{}", std::env::consts::EXE_SUFFIX);
    let lh_bytes = io(fs::read(lh), "failed to read the lh binary")?;

    let install = temp_dir("noop install dir")?;
    let dist = temp_dir("noop dist dir")?;
    io(fs::write(install.path().join(&exe_name), &lh_bytes), "failed to write the install exe")?;
    io(
        fs::write(install.path().join("VERSION.txt"), "same version\n"),
        "failed to write the install VERSION.txt",
    )?;
    io(fs::write(dist.path().join(&exe_name), &lh_bytes), "failed to write the dist exe")?;
    io(fs::write(dist.path().join("VERSION.txt"), "same version\n"), "failed to write the dist VERSION.txt")?;
    let manifest = serde_json::json!({
        // the cli crate version — equal to current_version() in the binary
        "version": "0.1.0",
        "artifacts": [
            { "name": exe_name, "sha256": sha256_hex(&lh_bytes) },
            { "name": "VERSION.txt", "sha256": sha256_hex(b"same version\n") },
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
            "equal-version self-update exited {} (expected 0)\nstdout: {}\nstderr: {}",
            out.code,
            truncate(&out.stdout, 600),
            truncate(&out.stderr, 300)
        ));
    }
    let j = out.json()?;
    if !v_bool(&j, "up_to_date")? {
        return Err(format!("equal-version self-update must report up_to_date=true: {j}"));
    }
    let updated: Vec<String> = v_arr(&j, "updated")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    if !updated.is_empty() {
        return Err(format!("equal-version self-update must swap nothing, updated={updated:?}"));
    }
    let ver = io(fs::read(install.path().join("VERSION.txt")), "failed to re-read VERSION.txt")?;
    if ver != b"same version\n" {
        return Err("VERSION.txt changed on an up-to-date run".into());
    }
    Ok("equal-version self-update: exit 0, up_to_date=true, nothing swapped, install untouched".into())
}

// ---------------------------------------------------------------- S29

/// A manifest referencing an artifact that is MISSING from the dist dir fails
/// cleanly (error JSON, no panic) and leaves the install untouched.
fn s29_version_file_missing(ctx: &Ctx) -> Res {
    let lh = &ctx.lh;
    let exe_name = format!("lh{}", std::env::consts::EXE_SUFFIX);
    let lh_bytes = io(fs::read(lh), "failed to read the lh binary")?;

    let install = temp_dir("missing install dir")?;
    let dist = temp_dir("missing dist dir")?;
    io(fs::write(install.path().join(&exe_name), &lh_bytes), "failed to write the install exe")?;
    io(
        fs::write(install.path().join("VERSION.txt"), "original\n"),
        "failed to write the install VERSION.txt",
    )?;
    // the dist has the exe but NOT the VERSION.txt the manifest promises
    io(fs::write(dist.path().join(&exe_name), &lh_bytes), "failed to write the dist exe")?;
    let manifest = serde_json::json!({
        "version": "999.0.0",
        "artifacts": [
            { "name": exe_name, "sha256": sha256_hex(&lh_bytes) },
            { "name": "VERSION.txt", "sha256": sha256_hex(b"never written\n") },
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
    if out.code == 0 {
        return Err(format!(
            "self-update with a missing artifact exited 0 (expected failure)\nstdout: {}",
            truncate(&out.stdout, 600)
        ));
    }
    if out.code == 101 || out.code == 134 {
        return Err(format!("self-update panicked on the missing artifact (exit {})", out.code));
    }
    let j = out.json()?;
    if j.get("error").is_none() {
        return Err(format!("the failed self-update did not report an error: {j}"));
    }
    let exe_after = io(fs::read(install.path().join(&exe_name)), "failed to re-read the install exe")?;
    if exe_after != lh_bytes {
        return Err("the install exe changed despite the missing-artifact failure".into());
    }
    let ver = io(fs::read(install.path().join("VERSION.txt")), "failed to re-read VERSION.txt")?;
    if ver != b"original\n" {
        return Err("VERSION.txt changed despite the missing-artifact failure".into());
    }
    Ok(format!(
        "missing artifact: self-update failed cleanly (exit {}), error reported, install untouched",
        out.code
    ))
}
