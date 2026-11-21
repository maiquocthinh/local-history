//! Writes new versions onto the chain: blake3 dedup, binary detection, the full/delta
//! policy, atomic blob writes (`.tmp` + rename) and the INSERT inside one transaction.
//!
//! Policy (docs/architecture.md §3.4):
//! - `Delete` → always full with EMPTY content (marker; the blob is still written to keep
//!   the row⇔blob invariant).
//! - Binary (a NUL byte in the first 8 KiB, or invalid UTF-8) → full only.
//! - Text → full if it is the file's first version, if the delta chain already has >= 20
//!   entries, or if the patch JSON is >= 1/2 of the content; otherwise a delta with
//!   `base_seq` = the latest version's seq.

use std::path::Path;

use crate::batch::BatchId;
use crate::model::*;
use crate::patch;
use crate::store::{now_micros, row_to_version, LocalHistory};

/// Writes a blob atomically: create the directory, write a `.tmp` file, then rename over
/// the final path.
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
    /// Records a new version of `file` with `content`.
    /// Returns `Ok(None)` when the write is skipped (file too large, or identical to the
    /// latest version — dedup).
    pub fn record(
        &self,
        ws: WorkspaceId,
        file: &Path,
        content: &[u8],
        kind: EventKind,
        label: Option<&str>,
    ) -> Result<Option<VersionId>> {
        let rel = self.resolve_rel_path(ws, file)?;

        if content.len() as u64 > MAX_FILE_SIZE {
            log::warn!("bỏ qua record ({}): lớn hơn MAX_FILE_SIZE", file.display());
            return Ok(None);
        }

        // Current state of the file: files row, latest version, and the run of consecutive
        // deltas right before it (one short lock — not held across the heavy work below).
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
            // Walk from latest back to the last full (or the start), counting consecutive
            // delta versions.
            let chain = match file_id {
                None => 0,
                Some(fid) => {
                    // 20 consecutive deltas force a full; peek at 21 to be certain.
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

        let hash = blake3::hash(content).to_hex().to_string();

        // Dedup: same hash + kind as the latest version → no new version.
        // Delete is special: if the latest is already a Delete, skip regardless of content.
        // Rename never dedups — it is an EVENT (path change), not a content change.
        let dup = match &latest {
            Some(l) if kind == EventKind::Delete => l.kind == EventKind::Delete,
            Some(l) => kind != EventKind::Rename && l.kind == kind && l.content_hash == hash,
            None => false,
        };
        if dup {
            return Ok(None);
        }

        // Text detection: no NUL byte in the first 8 KiB AND the whole content is valid UTF-8.
        let is_text = !content[..content.len().min(8192)].contains(&0u8)
            && std::str::from_utf8(content).is_ok();

        // Pick full/delta + the blob payload (full = content; delta = hunk JSON).
        let (storage, base_seq, blob_data) = if kind == EventKind::Delete {
            // Delete: always full, empty content as the marker.
            (Storage::Full, None, Vec::new())
        } else if !is_text {
            // Binary: full only.
            (Storage::Full, None, content.to_vec())
        } else if let Some(fid) = file_id {
            // File already has history — try a delta against its latest content.
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
                // File row exists but has no version yet (rare) → first full.
                None => (Storage::Full, None, content.to_vec()),
            }
        } else {
            // First version of the file → full.
            (Storage::Full, None, content.to_vec())
        };

        // `size` = reconstructed content size: a Delete marker reconstructs to empty.
        let size: u64 = if kind == EventKind::Delete { 0 } else { content.len() as u64 };

        // Transaction: INSERT files (if missing) → INSERT versions → write blob →
        // UPDATE current_seq → COMMIT. A blob failure rolls back; a crash only leaves an
        // orphan blob for GC.
        let ts = now_micros();
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
                ts,
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
        // Release the connection lock before assigning the batch — `assign_batch` re-locks
        // the same non-reentrant mutex, and the version is already committed at this point.
        drop(conn);
        // Auto-batch (plan M6 §3.2, lazy lifecycle): best-effort, AFTER the version is
        // committed — a batch failure must never fail the record; the version simply stays
        // with `batch_id NULL`.
        self.assign_batch(ws, version_id, ts);
        Ok(Some(version_id))
    }

    /// Records a rename: the files row — with its WHOLE version chain — moves to the new
    /// relative path, so history follows the file across the rename (JetBrains-style), and
    /// one `Rename` version (label = the old rel path) is appended to the same chain.
    ///
    /// Returns `Ok(None)` when nothing was recorded (same path, or content over the cap).
    /// Errors when `old_path` has no tracked row or `new_path` collides with a tracked row.
    /// The content is read from `new_path`; blake3 dedup makes an unchanged-content rename
    /// nearly free (the blob is reused).
    pub fn rename(&self, ws: WorkspaceId, old_path: &Path, new_path: &Path) -> Result<Option<VersionId>> {
        let old_rel = self.resolve_rel_path(ws, old_path)?;
        let new_rel = self.resolve_rel_path(ws, new_path)?;
        if old_rel == new_rel {
            return Ok(None);
        }
        let content = std::fs::read(new_path)?;
        if content.len() as u64 > MAX_FILE_SIZE {
            log::warn!("bỏ qua rename ({}): lớn hơn MAX_FILE_SIZE", new_path.display());
            return Ok(None);
        }
        {
            let conn = self.conn.lock().unwrap();
            let row_at = |rel: &str| -> Option<FileId> {
                conn.query_row(
                    "SELECT id FROM files WHERE workspace_id = ?1 AND rel_path = ?2",
                    rusqlite::params![ws, rel],
                    |r| r.get(0),
                )
                .ok()
            };
            let old_row = row_at(&old_rel);
            let dest_row = row_at(&new_rel);
            match (old_row, dest_row) {
                // A tracked file already occupies the destination (e.g. an overwrite rename
                // on disk): the two chains stay separate — the daemon's fallback records the
                // delete marker on the old path and the new content on the destination.
                (_, Some(_)) => {
                    return Err(Error::NotFound(format!(
                        "đích '{new_rel}' đã có history — không ghi đè"
                    )));
                }
                (None, None) => {
                    return Err(Error::NotFound(format!(
                        "không có file '{old_rel}' để đổi tên"
                    )));
                }
                (Some(_), None) => {}
            }
            let moved: usize = conn.execute(
                "UPDATE files SET rel_path = ?1 WHERE workspace_id = ?2 AND rel_path = ?3",
                rusqlite::params![new_rel, ws, old_rel],
            )?;
            if moved == 0 {
                return Err(Error::NotFound(format!(
                    "không có file '{old_rel}' để đổi tên"
                )));
            }
        }
        // The row now lives under the new rel — record() finds it and appends the Rename
        // version to the SAME chain (dedup is disabled for Rename).
        self.record(ws, new_path, &content, EventKind::Rename, Some(&old_rel))
    }

    /// Attaches the committed version to the open batch of `ws`, creating a batch when
    /// there is none.
    ///
    /// Lazy lifecycle (plan M6 §3.2 — deliberate deviation from architecture §10.2/10.3):
    /// there is no timer thread. A batch is "open" while the newest batch of the workspace
    /// has `closed_at NULL` and `ts - ended_at <= window_ms`; a record past the window (or
    /// from a different workspace) lazily closes the previous batch (`closed_at`) and starts
    /// a fresh one. `ended_at` always equals the timestamp of the batch's newest version.
    pub(crate) fn assign_batch(&self, ws: WorkspaceId, version_id: VersionId, ts: i64) {
        if let Err(e) = self.try_assign_batch(ws, version_id, ts) {
            log::warn!("gắn batch thất bại cho version {version_id}: {e}");
        }
    }

    fn try_assign_batch(&self, ws: WorkspaceId, version_id: VersionId, ts: i64) -> Result<()> {
        let window_micros = self.batch_policy.lock().unwrap().window_ms as i64 * 1000;
        let conn = self.conn.lock().unwrap();
        let latest: Option<(BatchId, i64, Option<i64>)> = match conn.query_row(
            "SELECT id, ended_at, closed_at FROM batches WHERE workspace_id = ?1
             ORDER BY started_at DESC, id DESC LIMIT 1",
            [ws],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ) {
            Ok(row) => Some(row),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        // Newest batch of the workspace still inside the window and not closed → join it and
        // slide its end forward.
        if let Some((batch_id, ended_at, None)) = &latest
            && ts - ended_at <= window_micros
        {
            conn.execute(
                "UPDATE versions SET batch_id = ?1 WHERE id = ?2",
                rusqlite::params![batch_id, version_id],
            )?;
            conn.execute(
                "UPDATE batches SET ended_at = ?1 WHERE id = ?2",
                rusqlite::params![ts, batch_id],
            )?;
            return Ok(());
        }
        // Not joinable: lazily close the previous batch if nobody did yet, then open a new
        // one for this record.
        if let Some((batch_id, _, None)) = &latest {
            conn.execute(
                "UPDATE batches SET closed_at = ?1 WHERE id = ?2 AND closed_at IS NULL",
                rusqlite::params![ts, batch_id],
            )?;
        }
        conn.execute(
            "INSERT INTO batches (workspace_id, started_at, ended_at, kind) VALUES (?1, ?2, ?3, 'auto')",
            rusqlite::params![ws, ts, ts],
        )?;
        let batch_id: BatchId = conn.last_insert_rowid();
        conn.execute(
            "UPDATE versions SET batch_id = ?1 WHERE id = ?2",
            rusqlite::params![batch_id, version_id],
        )?;
        Ok(())
    }

    /// Reverts `file` to the content of `version_id`.
    ///
    /// Always safe: if disk differs from the target, the current state is snapshotted with
    /// kind `pre_revert` BEFORE the overwrite; the target is then written with kind
    /// `restore`. The version must belong to the given (workspace, file) pair — otherwise
    /// `Error::NotFound`.
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

        let cur = if file.exists() { Some(std::fs::read(file)?) } else { None };
        if cur.as_deref() == Some(target.as_slice()) {
            return Ok(());
        }
        if let Some(cur) = &cur {
            // Safety snapshot before overwriting.
            self.record(ws, file, cur, EventKind::PreRevert, None)?;
        }
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, &target)?;
        self.record(ws, file, &target, EventKind::Restore, None)?;
        Ok(())
    }

    /// Attaches a semantic label: creates a version of kind `Label` holding the current
    /// disk content. The file must exist on disk.
    pub fn add_label(&self, ws: WorkspaceId, file: &Path, label: &str) -> Result<VersionId> {
        if !file.exists() {
            return Err(Error::NotFound(format!("file không tồn tại: {}", file.display())));
        }
        let content = std::fs::read(file)?;
        match self.record(ws, file, &content, EventKind::Label, Some(label))? {
            Some(id) => Ok(id),
            None => {
                // Blocked by dedup (content unchanged and the latest version is already a
                // Label) — return the id of the existing latest version.
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

    /// Changes or clears the label of an existing version, without creating a new one.
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
