//! Retention / GC — dọn version cũ theo chính sách "ngày làm việc" (kiểu JetBrains) và
//! sweep blob mồ côi (blob trên disk không còn row nào trỏ tới — do crash giữa record).
//!
//! Luồng: `gc_scan()` group version theo file → `deletion_plan()` tính run xóa cho từng file
//! (prefix cũ hơn cutoff, ngoài `min_versions` mới nhất, cascade qua delta khi xóa full) →
//! `delete_versions()` xóa row trong transaction → xóa blob file tương ứng → sweep mồ côi.

use std::collections::HashSet;
use std::io::ErrorKind;

use time::{OffsetDateTime, Time, Weekday};

use crate::model::{GcFile, GcReport, Result, RetentionPolicy, Storage, VersionId, WorkspaceId};
use crate::store::{now_micros, LocalHistory};

/// Tính cutoff (unix micros UTC): lùi `working_days` ngày LÀM VIỆC (Mon–Fri) từ ngày của
/// `now`, bỏ qua Thứ 7/Chủ nhật; cutoff = 00:00 UTC của ngày làm việc thứ N lùi lại.
///
/// `working_days == 0` → 00:00 UTC của chính ngày `now` (kể cả khi `now` rơi vào cuối tuần).
/// TODO(semantics): `0` ngày + `now` là cuối tuần chưa có định nghĩa chặt trong spec —
/// chọn cách đơn giản nhất như trên.
pub(crate) fn working_day_cutoff(now_micros: i64, working_days: u32) -> i64 {
    let now = OffsetDateTime::from_unix_timestamp_nanos(now_micros as i128 * 1000)
        .expect("unix micros nằm ngoài khoảng time crate biểu diễn được");
    let mut day = now.date(); // ngày UTC của now
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

/// THUẦN TÚNG (không đụng db/disk) — kế hoạch xóa cho 1 file: trả về các VersionId cần xóa,
/// theo thứ tự seq tăng dần.
///
/// - Candidate = prefix của `versions` (seq tăng dần) có `timestamp_micros < cutoff` VÀ nằm
///   ngoài `min_versions` version mới nhất (index < versions.len() - min_versions).
/// - CASCADE: sau khi chọn run, kéo dài run qua các version kế tiếp nếu nó là Delta (mất base
///   full thì chain sau không tái tạo được); dừng khi chạm vùng được bảo vệ
///   (`run.len() < versions.len() - min_versions`) hoặc gặp Full.
///
/// Biên đã chấp nhận theo đặc tả: một Delta nằm TRONG vùng bảo vệ có thể mất base (bị xóa)
/// khi `min_versions` nhỏ — row còn nhưng reconstruct sẽ lỗi chain; GC không tự sửa.
pub(crate) fn deletion_plan(
    gc_file: &GcFile,
    cutoff_micros: i64,
    min_versions: u32,
) -> Vec<VersionId> {
    let versions = &gc_file.versions;
    // Vùng bảo vệ: min_versions version mới nhất (index >= boundary).
    // len < min_versions → boundary = 0 → không xóa gì.
    let boundary = versions.len().saturating_sub(min_versions as usize);

    let mut run: Vec<VersionId> = Vec::new();
    // 1) prefix ứng viên: cũ hơn cutoff và ngoài vùng bảo vệ — dừng ở version đầu không thỏa.
    for v in versions.iter().take(boundary) {
        if v.timestamp_micros >= cutoff_micros {
            break;
        }
        run.push(v.id);
    }
    // 2) cascade: kéo dài qua Delta kế tiếp; Full hoặc vùng bảo vệ chặn run.
    while run.len() < boundary && versions[run.len()].storage != Storage::Full {
        run.push(versions[run.len()].id);
    }
    run
}

/// Parse tên file blob `v<id>-full.zst` / `v<id>-delta.zst` → version id.
/// Tên khác (tên lạ, thư mục...) → None — GC không đụng vào những gì nó không hiểu.
fn parse_blob_file_name(name: &str) -> Option<VersionId> {
    let rest = name.strip_prefix('v')?;
    let (id, tail) = rest.split_once('-')?;
    if tail != "full.zst" && tail != "delta.zst" {
        return None;
    }
    id.parse().ok()
}

impl LocalHistory {
    /// GC đầy đủ. `ws` = None → quét mọi workspace. `dry_run` = chỉ đếm, không xóa db/blob.
    /// Cuối cùng luôn chạy orphan sweep (không lọc theo `ws` — blob mồ côi không thuộc row
    /// nào nên phải quét hết mọi thư mục workspace).
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
        // (version_id, ws, storage) — cho dry_run tính dung lượng blob sẽ giải phóng
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
            // Không đụng db/disk: bytes_freed = dung lượng blob sẽ giải phóng (metadata nếu có).
            for (vid, w, storage) in &planned {
                let path = self.blob_path(*w, *vid, *storage);
                if let Ok(meta) = std::fs::metadata(&path) {
                    report.bytes_freed += meta.len();
                }
            }
        } else if !ids.is_empty() {
            // Xóa row trong một transaction, rồi mới dọn blob trên disk.
            let blobs = self.delete_versions(&ids)?;
            report.versions_deleted = blobs.len() as u64;
            for blob in &blobs {
                let path = self.blob_path(blob.ws_id, blob.version_id, blob.storage);
                // dung lượng file TRƯỚC khi xóa; file đã biến mất → bỏ qua
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

    /// Tiện ích: gc với now = hiện tại, dry_run = false.
    pub fn gc_auto(&self, ws: Option<WorkspaceId>, policy: &RetentionPolicy) -> Result<GcReport> {
        self.gc(ws, policy, now_micros(), false)
    }

    /// Duyệt `workspaces_dir()` → mỗi thư mục ws con → `blobs/`: file `v<id>-*.zst` mà id
    /// không còn trong db, hoặc file `.tmp` rác (record bị crash giữa chừng) → mồ côi.
    /// `dry_run` chỉ đếm; ngược lại xóa file (bỏ qua NotFound) và cộng bytes vào `bytes_freed`.
    ///
    /// TODO(concurrency): `.tmp` đang được record process khác ghi trong tích tắc cũng bị
    /// tính là mồ côi — chấp nhận được vì daemon GC chạy lúc start/mỗi 6h, record thất bại
    /// sẽ rollback và ghi lại.
    fn sweep_orphans(&self, dry_run: bool, bytes_freed: &mut u64) -> Result<u64> {
        let known = self.known_version_ids()?;
        let mut orphans = 0u64;
        let Ok(ws_entries) = std::fs::read_dir(self.workspaces_dir()) else {
            return Ok(0); // chưa có thư mục workspaces — không có gì để dọn
        };
        for ws_dir in ws_entries.flatten() {
            let Ok(blob_entries) = std::fs::read_dir(ws_dir.path().join("blobs")) else {
                continue; // ws chưa ghi blob lần nào
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
                    !known.contains(&id)
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

    /// unix micros UTC của ngày-giờ cụ thể — dùng làm mốc assert cứng.
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
            seq: id, // cho gọn: seq trùng id
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
        // khóa mốc theo lịch thật: 2026-11-06 là Thứ 6
        let fri = time::Date::from_calendar_date(2026, Month::November, 6).unwrap();
        assert_eq!(fri.weekday(), Weekday::Friday);
        let now = micros_at(2026, Month::November, 6, 15, 0);
        // lùi 1 ngày làm việc → Thứ 5 00:00 UTC
        assert_eq!(working_day_cutoff(now, 1), micros_at(2026, Month::November, 5, 0, 0));
        // lùi 3 → Thứ 3 00:00 UTC (không băng qua cuối tuần)
        let tue = time::Date::from_calendar_date(2026, Month::November, 3).unwrap();
        assert_eq!(tue.weekday(), Weekday::Tuesday);
        assert_eq!(working_day_cutoff(now, 3), micros_at(2026, Month::November, 3, 0, 0));
    }

    #[test]
    fn cutoff_skips_weekend() {
        // Thứ 2 2026-11-09 10:00 UTC lùi 1 ngày làm việc → Thứ 6 2026-11-06 00:00
        // (bỏ qua CN 11-08 và T7 11-07)
        let monday = micros_at(2026, Month::November, 9, 10, 0);
        assert_eq!(working_day_cutoff(monday, 1), micros_at(2026, Month::November, 6, 0, 0));
        // Thứ 6 lùi 5 ngày làm việc → Thứ 6 tuần trước 00:00 (2026-10-30)
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
        // 60 version cách nhau 2h; version index < 40 cũ hơn cutoff; min_versions = 10
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..60i64)
            .map(|i| {
                let ts = cutoff - 80 * HOUR + i * 2 * HOUR;
                let st = if i % 5 == 0 { Storage::Full } else { Storage::Delta };
                (i + 1, ts, st)
            })
            .collect();
        let gf = gc_file(&rows);
        // prefix đúng 40 version đầu (v40 trúng đúng cutoff → không tính), cascade dừng ngay
        // vì v40 là Full
        assert_eq!(deletion_plan(&gf, cutoff, 10), (1..=40).collect::<Vec<_>>());
    }

    #[test]
    fn plan_cascades_through_deltas_until_full() {
        // survivor đầu (index 40) là Delta → cascade kéo run qua 40..44, dừng ở Full index 45
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
        // chỉ 30 version < min_versions=50 → không xóa gì dù mọi version đều cũ
        let cutoff = 1_000_000_000_000_000;
        let rows: Vec<_> = (0..30i64)
            .map(|i| (i + 1, cutoff - 100 * HOUR + i * HOUR, Storage::Full))
            .collect();
        let gf = gc_file(&rows);
        assert!(deletion_plan(&gf, cutoff, 50).is_empty());
    }

    #[test]
    fn plan_never_crosses_protected_zone() {
        // 60 version đều cũ hơn cutoff → xóa đúng 50 (giữ nguyên 10 mới nhất)
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

    // ----- parse tên blob -----

    #[test]
    fn parse_blob_names() {
        assert_eq!(parse_blob_file_name("v12-full.zst"), Some(12));
        assert_eq!(parse_blob_file_name("v345-delta.zst"), Some(345));
        assert_eq!(parse_blob_file_name("v99999-full.zst"), Some(99999));
        assert_eq!(parse_blob_file_name("v12-full.zst.tmp"), None); // rơi vào nhánh .tmp
        assert_eq!(parse_blob_file_name("x.tmp"), None);
        assert_eq!(parse_blob_file_name("v1-2-full.zst"), None);
        assert_eq!(parse_blob_file_name("vabc-full.zst"), None);
        assert_eq!(parse_blob_file_name("unrelated.txt"), None);
    }
}
