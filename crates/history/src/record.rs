//! record — ghi version mới vào chain: dedup blake3, phát hiện binary, chính sách full/delta,
//! ghi blob atomic (`.tmp` + rename) và INSERT trong một transaction.
//!
//! Chính sách (docs/architecture.md §3.4):
//! - `Delete` → luôn full với nội dung RỖNG (marker; blob vẫn ghi để giữ bất biến row⇒blob).
//! - Binary (có NUL trong 8KB đầu hoặc không phải UTF-8) → chỉ full.
//! - Text → full nếu version đầu của file, hoặc chain delta đã ≥ 20, hoặc patch JSON
//!   ≥ 1/2 nội dung; còn không → delta với `base_seq` = seq của version mới nhất.

use std::path::Path;

use crate::model::*;
use crate::patch;
use crate::store::{now_micros, row_to_version, LocalHistory};

/// Ghi blob atomic: tạo thư mục, ghi file `.tmp` rồi rename lên đường cuối.
fn write_blob_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "blob path không có thư mục cha")
    })?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

impl LocalHistory {
    /// Ghi một version mới cho `file` với `content`. Trả `Ok(None)` nếu bị bỏ qua
    /// (file quá lớn hoặc trùng version mới nhất — dedup).
    pub fn record(
        &self,
        ws: WorkspaceId,
        file: &Path,
        content: &[u8],
        kind: EventKind,
        label: Option<&str>,
    ) -> Result<Option<VersionId>> {
        // 1. rel_path chuẩn hóa (phân tách '/')
        let rel = self.resolve_rel_path(ws, file)?;

        // 2. Quá giới hạn kích thước → bỏ qua, không record.
        if content.len() as u64 > MAX_FILE_SIZE {
            log::warn!("bỏ qua record ({}): lớn hơn MAX_FILE_SIZE", file.display());
            return Ok(None);
        }

        // Trạng thái hiện tại của file: files row, version mới nhất, chuỗi delta liên tiếp
        // ngay trước (một lock ngắn, không giữ lock qua các đoạn nặng phía sau).
        let (file_id, latest, chain): (Option<FileId>, Option<Version>, i64) = {
            let conn = self.conn.lock().unwrap();
            let file_id: Option<FileId> = match conn.query_row(
                "SELECT id FROM files WHERE workspace_id = ?1 AND rel_path = ?2",
                rusqlite::params![ws, rel],
                |r| r.get(0),
            ) {
                Ok(id) => Some(id),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e.into()),
            };
            let latest: Option<Version> = match file_id {
                None => None,
                Some(fid) => match conn.query_row(
                    "SELECT id, file_id, seq, timestamp, kind, label, storage, base_seq, size, content_hash
                     FROM versions WHERE file_id = ?1 ORDER BY seq DESC LIMIT 1",
                    [fid],
                    row_to_version,
                ) {
                    Ok(v) => Some(v),
                    Err(rusqlite::Error::QueryReturnedNoRows) => None,
                    Err(e) => return Err(e.into()),
                },
            };
            // Đi từ latest lên tới Full (hoặc hết) — đếm các version storage=Delta liên tiếp.
            let chain = match file_id {
                None => 0,
                Some(fid) => {
                    // 20 delta là đủ quyết định (chain >= 20 → full), 21 để chắc chắn.
                    let mut stmt = conn.prepare(
                        "SELECT storage FROM versions WHERE file_id = ?1 ORDER BY seq DESC LIMIT 21",
                    )?;
                    let rows = stmt.query_map([fid], |r| r.get::<_, String>(0))?;
                    let mut n: i64 = 0;
                    for r in rows {
                        match Storage::parse(&r?) {
                            Ok(Storage::Delta) => n += 1,
                            _ => break,
                        }
                    }
                    n
                }
            };
            (file_id, latest, chain)
        };

        // 3. Hash nội dung (blake3 hex lowercase).
        let hash = blake3::hash(content).to_hex().to_string();

        // 4. Dedup: trùng hash + kind với version mới nhất → không tạo version.
        //    Riêng Delete: latest cũng là Delete thì bỏ qua dù nội dung gì.
        let dup = match &latest {
            Some(l) if kind == EventKind::Delete => l.kind == EventKind::Delete,
            Some(l) => l.kind == kind && l.content_hash == hash,
            None => false,
        };
        if dup {
            return Ok(None);
        }

        // 5. Binary detect: không có byte 0 trong 8KB đầu VÀ toàn bộ nội dung là UTF-8 hợp lệ.
        let is_text = !content[..content.len().min(8192)].contains(&0u8)
            && std::str::from_utf8(content).is_ok();

        // 6-7. Chọn full/delta + nội dung blob (full = nội dung; delta = JSON hunks).
        let (storage, base_seq, blob_data) = if kind == EventKind::Delete {
            // Delete: luôn full, nội dung rỗng làm marker.
            (Storage::Full, None, Vec::new())
        } else if !is_text {
            // Binary: chỉ full.
            (Storage::Full, None, content.to_vec())
        } else if let Some(fid) = file_id {
            // File đã có lịch sử — thử delta lên nội dung mới nhất.
            match self.latest_content(fid)? {
                Some((lv, old)) => {
                    let hunks = patch::make_patch(&old, content)?;
                    let json = patch::encode_hunks(&hunks)?;
                    if chain >= 20 || json.len() >= content.len() / 2 {
                        (Storage::Full, None, content.to_vec())
                    } else {
                        (Storage::Delta, Some(lv.seq), json)
                    }
                }
                // Row file có nhưng chưa có version nào (hiếm) → full đầu tiên.
                None => (Storage::Full, None, content.to_vec()),
            }
        } else {
            // Version đầu của file → full.
            (Storage::Full, None, content.to_vec())
        };

        // `size` = kích thước nội dung sau khi tái tạo: marker Delete tái tạo thành rỗng.
        let size: u64 = if kind == EventKind::Delete { 0 } else { content.len() as u64 };

        // 8. Transaction: INSERT files (nếu thiếu) → INSERT versions → ghi blob → UPDATE
        //    current_seq → COMMIT. Blob lỗi → rollback (crash chỉ để lại blob mồ côi, GC dọn).
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "INSERT OR IGNORE INTO files (workspace_id, rel_path) VALUES (?1, ?2)",
            rusqlite::params![ws, rel],
        )?;
        let file_id: FileId = tx.query_row(
            "SELECT id FROM files WHERE workspace_id = ?1 AND rel_path = ?2",
            rusqlite::params![ws, rel],
            |r| r.get(0),
        )?;
        let current_seq: i64 =
            tx.query_row("SELECT current_seq FROM files WHERE id = ?1", [file_id], |r| r.get(0))?;
        let seq = current_seq + 1;
        tx.execute(
            "INSERT INTO versions (file_id, seq, timestamp, kind, label, storage, base_seq, size, content_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                file_id,
                seq,
                now_micros(),
                kind.as_str(),
                label,
                storage.as_str(),
                base_seq,
                size as i64,
                hash,
            ],
        )?;
        let version_id: VersionId = tx.last_insert_rowid();

        let blob_path = self.blob_path(ws, version_id, storage);
        if let Err(e) = zstd::bulk::compress(&blob_data, 3)
            .and_then(|c| write_blob_atomic(&blob_path, &c))
        {
            let _ = tx.rollback();
            return Err(e.into());
        }
        tx.execute(
            "UPDATE files SET current_seq = ?1 WHERE id = ?2",
            rusqlite::params![seq, file_id],
        )?;
        tx.commit()?;
        Ok(Some(version_id))
    }

    /// Revert `file` về nội dung của `version_id`.
    ///
    /// Luôn an toàn: nếu disk khác target, snapshot trạng thái hiện tại với kind=`pre_revert`
    /// TRƯỚC khi ghi đè; sau đó ghi target với kind=`restore`. Version phải thuộc đúng
    /// (ws, file) — ngược lại `Error::NotFound`.
    pub fn revert(&self, ws: WorkspaceId, file: &Path, version_id: VersionId) -> Result<()> {
        let rel = self.resolve_rel_path(ws, file)?;
        let target_v = self.version_by_id(version_id)?;
        let file_id: Option<FileId> = {
            let conn = self.conn.lock().unwrap();
            match conn.query_row(
                "SELECT id FROM files WHERE workspace_id = ?1 AND rel_path = ?2",
                rusqlite::params![ws, rel],
                |r| r.get(0),
            ) {
                Ok(id) => Some(id),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e.into()),
            }
        };
        if file_id != Some(target_v.file_id) {
            return Err(Error::NotFound(format!(
                "version {version_id} không thuộc file {} trong workspace {ws}",
                file.display()
            )));
        }
        let target = self.content(version_id)?;

        // Disk đã giống target → không ghi gì cả.
        let cur = if file.exists() { Some(std::fs::read(file)?) } else { None };
        if cur.as_deref() == Some(target.as_slice()) {
            return Ok(());
        }
        if let Some(cur) = &cur {
            // Snapshot an toàn TRƯỚC khi ghi đè.
            self.record(ws, file, cur, EventKind::PreRevert, None)?;
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, &target)?;
        self.record(ws, file, &target, EventKind::Restore, None)?;
        Ok(())
    }

    /// Gắn label ngữ nghĩa: tạo version kind=`Label` với nội dung hiện tại trên disk.
    /// File phải tồn tại trên disk.
    pub fn add_label(&self, ws: WorkspaceId, file: &Path, label: &str) -> Result<VersionId> {
        if !file.exists() {
            return Err(Error::NotFound(format!("file không tồn tại: {}", file.display())));
        }
        let content = std::fs::read(file)?;
        match self.record(ws, file, &content, EventKind::Label, Some(label))? {
            Some(id) => Ok(id),
            None => {
                // Bị dedup chặn (nội dung không đổi và latest đã là Label) — trả về id của
                // version mới nhất hiện có.
                let rel = self.resolve_rel_path(ws, file)?;
                let conn = self.conn.lock().unwrap();
                match conn.query_row(
                    "SELECT v.id, v.file_id, v.seq, v.timestamp, v.kind, v.label, v.storage, v.base_seq, v.size, v.content_hash
                     FROM versions v JOIN files f ON f.id = v.file_id
                     WHERE f.workspace_id = ?1 AND f.rel_path = ?2
                     ORDER BY v.seq DESC LIMIT 1",
                    rusqlite::params![ws, rel],
                    row_to_version,
                ) {
                    Ok(v) => Ok(v.id),
                    Err(rusqlite::Error::QueryReturnedNoRows) => Err(Error::NotFound(format!(
                        "không có version nào cho {}",
                        file.display()
                    ))),
                    Err(e) => Err(e.into()),
                }
            }
        }
    }

    /// Đổi/xóa label của một version có sẵn (không tạo version mới).
    pub fn set_label(&self, version_id: VersionId, label: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE versions SET label = ?1 WHERE id = ?2",
            rusqlite::params![label, version_id],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("version {version_id}")));
        }
        Ok(())
    }
}
