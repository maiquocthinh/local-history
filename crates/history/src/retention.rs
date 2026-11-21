//! Retention / GC — prunes old versions under the "working days" policy (JetBrains-style)
//! and sweeps orphan blobs (blobs on disk that no row references anymore, e.g. after a
//! crash mid-record).
//!
//! Flow: `gc_scan()` groups versions per file → `deletion_plan()` computes the deletion run
//! per file (prefix older than the cutoff, outside the newest `min_versions`, cascading
//! through deltas when a full is deleted) → `delete_versions()` removes the rows in one
//! transaction → the matching blob files are removed → orphan sweep.

use std::collections::HashSet;
use std::io::ErrorKind;

use time::{OffsetDateTime, Time, Weekday};

use crate::model::{GcFile, GcReport, Result, RetentionPolicy, Storage, VersionId, WorkspaceId};
use crate::store::{now_micros, LocalHistory};

/// Computes the cutoff (unix micros UTC): steps back `working_days` working days (Mon–Fri)
/// from `now`'s date, skipping Saturday/Sunday; the cutoff is 00:00 UTC of that day.
///
/// `working_days == 0` → 00:00 UTC of `now`'s own day (even if `now` falls on a weekend).
/// TODO(semantics): `0` days + a weekend `now` is not pinned down in the spec — the
/// simplest choice above was taken.
pub(crate) fn working_day_cutoff(now_micros: i64, working_days: u32) -> i64 {
    let now = OffsetDateTime::from_unix_timestamp_nanos(now_micros as i128 * 1000)
        .expect("unix micros nằm ngoài khoảng time crate biểu diễn được");
    let mut day = now.date();
    let mut remaining = working_days;
    while remaining > 0 {
        day = day
            .previous_day()
            .expect("lùi ngày vượt khoảng time crate biểu diễn được");
        if !matches!(day.weekday(), Weekday::Saturday | Weekday::Sunday) {
            remaining -= 1;
        }
    }
    day.with_time(Time::MIDNIGHT).assume_utc().unix_timestamp() * 1_000_000
}

/// PURE (no db/disk access) — the deletion plan for one file: the VersionIds to delete,
/// in ascending seq order.
///
/// - Candidates: the prefix of `versions` (ascending seq) with `timestamp_micros < cutoff`
///   that also lies outside the newest `min_versions` versions
///   (index < versions.len() - min_versions).
/// - CASCADE: after the run is picked, it is extended over the following versions while
///   they are deltas (losing a base full makes the rest of the chain unreconstructable);
///   it stops at the protected zone (`run.len() < versions.len() - min_versions`) or at
///   a full.
///
/// Accepted edge per spec: a delta inside the protected zone can lose its base (deleted)
/// when `min_versions` is small — the row survives but reconstruction fails; GC does not
/// repair this.
pub(crate) fn deletion_plan(
    gc_file: &GcFile,
    cutoff_micros: i64,
    min_versions: u32,
) -> Vec<VersionId> {
    let versions = &gc_file.versions;
    // Protected zone: the newest min_versions versions (index >= boundary).
    // len < min_versions → boundary = 0 → nothing is deleted.
    let boundary = versions.len().saturating_sub(min_versions as usize);

    let mut run: Vec<VersionId> = Vec::new();
    // 1) Candidate prefix: older than the cutoff and outside the protected zone — stop at
    // the first version that fails.
    for v in versions.iter().take(boundary) {
        if v.timestamp_micros >= cutoff_micros {
            break;
        }
        run.push(v.id);
    }
    // 2) Cascade: extend over following deltas; a full or the protected zone ends the run.
    while run.len() < boundary && versions[run.len()].storage != Storage::Full {
        run.push(versions[run.len()].id);
    }
    run
}

/// Parses a blob file name `v<id>-full.zst` / `v<id>-delta.zst` → version id.
/// Anything else (odd names, directories...) → None — GC never touches what it does not
/// recognize.
fn parse_blob_file_name(name: &str) -> Option<VersionId> {
    let rest = name.strip_prefix('v')?;
    let (id, tail) = rest.split_once('-')?;
    if tail != "full.zst" && tail != "delta.zst" {
        return None;
    }
    id.parse().ok()
}

impl LocalHistory {
    /// Full GC. `ws = None` scans every workspace. `dry_run` only counts, touching neither
    /// db nor blobs. The orphan sweep always runs at the end (not filtered by `ws` — an
    /// orphan blob belongs to no row, so every workspace directory must be scanned).
    pub fn gc(
        &self,
        ws: Option<WorkspaceId>,
        policy: &RetentionPolicy,
        now_micros: i64,
        dry_run: bool,
    ) -> Result<GcReport> {
        let cutoff = working_day_cutoff(now_micros, policy.working_days);
        let scan = self.gc_scan()?;

        let mut report = GcReport { dry_run, ..GcReport::default() };
        let mut ids: Vec<VersionId> = Vec::new();
        // (version_id, ws, storage) — lets dry_run compute the blob bytes that would be freed
        let mut planned: Vec<(VersionId, WorkspaceId, Storage)> = Vec::new();

        for gf in &scan {
            if let Some(w) = ws
                && gf.ws_id != w
            {
                continue;
            }
            report.files_scanned += 1;
            let plan = deletion_plan(gf, cutoff, policy.min_versions);
            if plan.is_empty() {
                continue;
            }
            let selected: HashSet<VersionId> = plan.into_iter().collect();
            for v in &gf.versions {
                if selected.contains(&v.id) {
                    ids.push(v.id);
                    planned.push((v.id, gf.ws_id, v.storage));
                }
            }
        }
        report.versions_deleted = ids.len() as u64;

        if dry_run {
            // No db/disk writes: bytes_freed = size of the blobs that would be freed
            // (from metadata, when available).
            for (vid, w, storage) in &planned {
                let path = self.blob_path(*w, *vid, *storage);
                if let Ok(meta) = std::fs::metadata(&path) {
                    report.bytes_freed += meta.len();
                }
            }
        } else if !ids.is_empty() {
            // Delete the rows in one transaction, then remove the blobs from disk.
            let blobs = self.delete_versions(&ids)?;
            report.versions_deleted = blobs.len() as u64;
            for blob in &blobs {
                let path = self.blob_path(blob.ws_id, blob.version_id, blob.storage);
                // file size before removal; a vanished file contributes nothing
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                match std::fs::remove_file(&path) {
                    Ok(()) => report.bytes_freed += len,
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }

        report.orphan_blobs = self.sweep_orphans(dry_run, &mut report.bytes_freed)?;
        Ok(report)
    }

    /// Convenience: gc with the current time and dry_run = false.
    pub fn gc_auto(&self, ws: Option<WorkspaceId>, policy: &RetentionPolicy) -> Result<GcReport> {
        self.gc(ws, policy, now_micros(), false)
    }

    /// Whether `id` exists in the versions table — a FRESH read, not the sweep's
    /// `known` snapshot: another connection (the daemon) may have committed the
    /// version between the snapshot and this file's turn to be examined.
    fn version_exists(&self, id: VersionId) -> bool {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT 1 FROM versions WHERE id = ?1", [id], |_| Ok(()))
            .is_ok()
    }

    /// Walks `workspaces_dir()` → each workspace dir → `blobs/`: a `v<id>-*.zst` file whose
    /// id is no longer in the db, or a stray `.tmp` file (record crashed midway), is an
    /// orphan. `dry_run` only counts; otherwise the file is removed (NotFound ignored) and
    /// its bytes added to `bytes_freed`.
    ///
    /// TODO(concurrency): a `.tmp` another record is writing at that very moment is also
    /// counted as an orphan — acceptable because the daemon runs GC at startup/every 6h,
    /// and a failed record rolls back and is retried.
    fn sweep_orphans(&self, dry_run: bool, bytes_freed: &mut u64) -> Result<u64> {
        let known = self.known_version_ids()?;
        let mut orphans = 0u64;
        let Ok(ws_entries) = std::fs::read_dir(self.workspaces_dir()) else {
            return Ok(0); // no workspaces dir yet — nothing to clean
        };
        for ws_dir in ws_entries.flatten() {
            let Ok(blob_entries) = std::fs::read_dir(ws_dir.path().join("blobs")) else {
                continue; // workspace has never written a blob
            };
            for entry in blob_entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let orphan = if name.ends_with(".tmp") {
                    true
                } else if let Some(id) = parse_blob_file_name(name) {
                    // Re-check under the connection lock: the version may have been
                    // committed AFTER the `known` snapshot (GC runs on its own
                    // connection while the daemon records) — deleting such a blob
                    // would lose a committed version forever.
                    !known.contains(&id) && !self.version_exists(id)
                } else {
                    false
                };
                if !orphan {
                    continue;
                }
                orphans += 1;
                if dry_run {
                    continue;
                }
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                match std::fs::remove_file(&path) {
                    Ok(()) => *bytes_freed += len,
                    Err(e) if e.kind() == ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        Ok(orphans)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EventKind;
    use time::Month;

    const HOUR: i64 = 3_600_000_000;

    /// unix micros UTC of a specific date-time — used as a hard assert anchor.
    fn micros_at(y: i32, m: Month, d: u8, h: u8, mi: u8) -> i64 {
        time::Date::from_calendar_date(y, m, d)
            .unwrap()
            .with_time(time::Time::from_hms(h, mi, 0).unwrap())
            .assume_utc()
            .unix_timestamp_nanos() as i64 / 1000
    }

    fn ver(id: i64, ts_micros: i64, storage: Storage) -> crate::model::Version {
        crate::model::Version {
            id,
            file_id: 1,
            seq: id, // shorthand: seq equals id
            timestamp_micros: ts_micros,
            kind: EventKind::External,
            label: None,
            storage,
            base_seq: (storage == Storage::Delta).then_some(id - 1),
            size: 10,
            content_hash: format!("h{id}"),
        }
    }

    fn gc_file(rows: &[(i64, i64, Storage)]) -> GcFile {
        GcFile {
            ws_id: 1,
            file_id: 1,
            versions: rows.iter().map(|&(id, ts, st)| ver(id, ts, st)).collect(),
        }
    }

    // ----- working_day_cutoff -----

    #[test]
    fn cutoff_friday_steps_back_working_days() {
        // anchored to the real calendar: 2026-11-06 is a Friday
        let fri = time::Date::from_calendar_date(2026, Month::November, 6).unwrap();
        assert_eq!(fri.weekday(), Weekday::Friday);
        let now = micros_at(2026, Month::November, 6, 15, 0);
        // back 1 working day → Thursday 00:00 UTC
        assert_eq!(working_day_cutoff(now, 1), micros_at(2026, Month::November, 5, 0, 0));
        // back 3 → Tuesday 00:00 UTC (does not cross the weekend)
        let tue = time::Date::from_calendar_date(2026, Month::November, 3).unwrap();
        assert_eq!(tue.weekday(), Weekday::Tuesday);
        assert_eq!(working_day_cutoff(now, 3), micros_at(2026, Month::November, 3, 0, 0));
    }

    #[test]
    fn cutoff_skips_weekend() {
        // Monday 2026-11-09 10:00 UTC, back 1 working day → Friday 2026-11-06 00:00
        // (skips Sun 11-08 and Sat 11-07)
        let monday = micros_at(2026, Month::November, 9, 10, 0);
        assert_eq!(working_day_cutoff(monday, 1), micros_at(2026, Month::November, 6, 0, 0));
        // Friday back 5 working days → the previous Friday 00:00 (2026-10-30)
        let friday = micros_at(2026, Month::November, 6, 9, 30);
        assert_eq!(working_day_cutoff(friday, 5), micros_at(2026, Month::October, 30, 0, 0));
    }

    #[test]
    fn cutoff_is_midnight_regardless_of_time_of_day() {
        let morning = micros_at(2026, Month::November, 6, 0, 1);
        let evening = micros_at(2026, Month::November, 6, 23, 59);
        assert_eq!(working_day_cutoff(morning, 1), working_day_cutoff(evening, 1));
        assert_eq!(working_day_cutoff(morning, 1), micros_at(2026, Month::November, 5, 0, 0));
    }

    #[test]
    fn cutoff_zero_days_is_today_midnight() {
        let saturday = micros_at(2026, Month::November, 7, 23, 59);
        assert_eq!(working_day_cutoff(saturday, 0), micros_at(2026, Month::November, 7, 0, 0));
    }

    // ----- deletion_plan -----

    #[test]
    fn plan_picks_old_prefix_outside_min_versions() {
        // 60 versions 2h apart; index < 40 is older than the cutoff; min_versions = 10
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..60i64)
            .map(|i| {
                let ts = cutoff - 80 * HOUR + i * 2 * HOUR;
                let st = if i % 5 == 0 { Storage::Full } else { Storage::Delta };
                (i + 1, ts, st)
            })
            .collect();
        let gf = gc_file(&rows);
        // exactly the first 40 (v40 hits the cutoff exactly → not a candidate); the cascade
        // stops immediately because v40 is full
        assert_eq!(deletion_plan(&gf, cutoff, 10), (1..=40).collect::<Vec<_>>());
    }

    #[test]
    fn plan_cascades_through_deltas_until_full() {
        // the first survivor (index 40) is a delta → the cascade extends the run over
        // 40..44 and stops at the full at index 45
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..60i64)
            .map(|i| {
                let ts = cutoff - 80 * HOUR + i * 2 * HOUR;
                let st = if i == 0 || i == 45 { Storage::Full } else { Storage::Delta };
                (i + 1, ts, st)
            })
            .collect();
        let gf = gc_file(&rows);
        assert_eq!(deletion_plan(&gf, cutoff, 10), (1..=45).collect::<Vec<_>>());
    }

    #[test]
    fn plan_respects_min_versions_floor() {
        // only 30 versions < min_versions = 50 → nothing is deleted even though every
        // version is old
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..30i64)
            .map(|i| (i + 1, cutoff - 100 * HOUR + i * HOUR, Storage::Full))
            .collect();
        let gf = gc_file(&rows);
        assert!(deletion_plan(&gf, cutoff, 50).is_empty());
    }

    #[test]
    fn plan_never_crosses_protected_zone() {
        // 60 versions all older than the cutoff → exactly 50 deleted (the newest 10 kept)
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..60i64)
            .map(|i| {
                let st = if i % 5 == 0 { Storage::Full } else { Storage::Delta };
                (i + 1, cutoff - 100 * HOUR + i * HOUR, st)
            })
            .collect();
        let gf = gc_file(&rows);
        assert_eq!(deletion_plan(&gf, cutoff, 10).len(), 50);
    }

    // ----- blob name parsing -----

    #[test]
    fn parse_blob_names() {
        assert_eq!(parse_blob_file_name("v12-full.zst"), Some(12));
        assert_eq!(parse_blob_file_name("v345-delta.zst"), Some(345));
        assert_eq!(parse_blob_file_name("v99999-full.zst"), Some(99999));
        assert_eq!(parse_blob_file_name("v12-full.zst.tmp"), None); // handled by the .tmp branch of the sweep
        assert_eq!(parse_blob_file_name("x.tmp"), None);
        assert_eq!(parse_blob_file_name("v1-2-full.zst"), None);
        assert_eq!(parse_blob_file_name("vabc-full.zst"), None);
        assert_eq!(parse_blob_file_name("unrelated.txt"), None);
    }
}
