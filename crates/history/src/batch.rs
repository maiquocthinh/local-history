//! Batch grouping (plan M6 §3.3/§3.4): listing queries over the `batches` table, the
//! per-file version→batch map, and `revert_batch` — restoring every file of a batch to its
//! pre-batch state.
//!
//! Batches are created lazily by `record` (see `record::assign_batch`); this module only
//! reads them and reverts them.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::model::*;
use crate::store::{now_micros, row_to_version, LocalHistory};

/// Primary key of a `batches` row.
pub type BatchId = i64;

/// Auto-batch policy. `window_ms` is the sliding window that keeps a batch open
/// (`now - ended_at <= window_ms`); `timeout_ms` is reserved for the manual open/close
/// lifecycle (a later phase — the lazy lifecycle derives everything from the window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchPolicy {
    pub window_ms: u64,
    pub timeout_ms: u64,
}

impl Default for BatchPolicy {
    fn default() -> Self {
        Self { window_ms: 3000, timeout_ms: 5000 }
    }
}

/// One batch in a workspace listing (plan M6 §3.3).
#[derive(Debug, Clone)]
pub struct BatchSummary {
    pub id: BatchId,
    pub ws_id: WorkspaceId,
    /// Unix micros — timestamp of the batch's first version.
    pub started_at: i64,
    /// Unix micros — timestamp of the batch's newest version.
    pub ended_at: i64,
    pub kind: String,
    pub label: Option<String>,
    pub version_count: u64,
    pub file_count: u64,
}

/// One version of one file inside a batch (all versions, not just representatives — a file
/// changed twice in the same wave appears twice; `revert_batch` picks the representative).
#[derive(Debug, Clone)]
pub struct BatchEntry {
    pub rel_path: String,
    pub version: Version,
}

/// Result of a whole-batch revert (plan M6 §3.4). `errors` holds `(rel_path, message)` —
/// one failing file never blocks the others.
#[derive(Debug, Clone, Default)]
pub struct RevertReport {
    pub reverted: u32,
    pub restored: u32,
    pub deleted: u32,
    pub skipped: u32,
    pub errors: Vec<(String, String)>,
}

impl LocalHistory {
    /// Whether the store already carries the v2 batch schema (plan M6 §3.1). Read-only
    /// connections (the CLI read commands, the GUI poll) cannot run the migration — only
    /// the rw `LocalHistory::open` does — so every batch READ must tolerate a v1 database
    /// (plan M6 acceptance #6): no `batches` table ⇔ the store predates batching ⇔ there
    /// are no batches yet. MIGRATION_V2 creates the table and the `versions.batch_id`
    /// column in one `execute_batch`, so table presence implies column presence.
    pub(crate) fn has_batch_schema(&self) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'batches'",
            [],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Batches of a workspace, newest first. A v1 store (not yet migrated — read-only
    /// connections never migrate) yields an empty list instead of a SQLite error.
    pub fn batches(&self, ws: WorkspaceId, limit: u32) -> Result<Vec<BatchSummary>> {
        if !self.has_batch_schema()? {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT b.id, b.workspace_id, b.started_at, b.ended_at, b.kind, b.label,
                    (SELECT COUNT(*) FROM versions v WHERE v.batch_id = b.id),
                    (SELECT COUNT(DISTINCT v.file_id) FROM versions v WHERE v.batch_id = b.id)
             FROM batches b
             WHERE b.workspace_id = ?1
             ORDER BY b.started_at DESC, b.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![ws, limit], |r| {
            Ok(BatchSummary {
                id: r.get(0)?,
                ws_id: r.get(1)?,
                started_at: r.get(2)?,
                ended_at: r.get(3)?,
                kind: r.get(4)?,
                label: r.get(5)?,
                version_count: r.get::<_, i64>(6)? as u64,
                file_count: r.get::<_, i64>(7)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Every version of every file in one batch, ordered by rel_path then seq.
    /// A batch id that does not exist is `Error::NotFound` (a v1 store has no batches at
    /// all, so every id is "not found" there too); an existing empty batch yields an
    /// empty vec.
    pub fn batch_entries(&self, batch_id: BatchId) -> Result<Vec<BatchEntry>> {
        if !self.has_batch_schema()? {
            return Err(Error::NotFound(format!("batch {batch_id}")));
        }
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT 1 FROM batches WHERE id = ?1",
            [batch_id],
            |r| r.get::<_, i64>(0),
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Error::NotFound(format!("batch {batch_id}")),
            other => other.into(),
        })?;
        let mut stmt = conn.prepare(
            "SELECT v.id, v.file_id, v.seq, v.timestamp, v.kind, v.label, v.storage, v.base_seq, v.size, v.content_hash,
                    f.rel_path
             FROM versions v JOIN files f ON f.id = v.file_id
             WHERE v.batch_id = ?1
             ORDER BY f.rel_path ASC, v.seq ASC",
        )?;
        let rows = stmt.query_map([batch_id], |r| {
            Ok(BatchEntry {
                rel_path: r.get(10)?,
                version: row_to_version(r)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// `version_id -> batch_id` for every batched version of one file — the GUI timeline
    /// badge. Versions without a batch (batch_id NULL) are not in the map; a v1 store
    /// (not yet migrated) yields an empty map instead of a SQLite error.
    pub fn batch_map_for_file(
        &self,
        ws: WorkspaceId,
        file: &Path,
    ) -> Result<HashMap<VersionId, BatchId>> {
        if !self.has_batch_schema()? {
            return Ok(HashMap::new());
        }
        let rel = self.resolve_rel_path(ws, file)?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT v.id, v.batch_id
             FROM versions v JOIN files f ON f.id = v.file_id
             WHERE f.workspace_id = ?1 AND f.rel_path = ?2 AND v.batch_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![ws, rel], |r| {
            Ok((r.get::<_, VersionId>(0)?, r.get::<_, BatchId>(1)?))
        })?;
        let mut map = HashMap::new();
        for r in rows {
            let (k, v) = r?;
            map.insert(k, v);
        }
        Ok(map)
    }

    /// Reverts every file of a batch to its state BEFORE the batch (plan M6 §3.4).
    ///
    /// Per file (representative = the in-batch version with the highest seq; pre-batch = the
    /// chain version with the highest seq below the batch's lowest seq for that file):
    /// - file exists on disk, existed before the batch: disk content unchanged since the
    ///   batch → `revert()` to the pre-batch version (inherits pre_revert + restore marker);
    ///   edited afterwards → `skipped` unless `force` (force reverts anyway);
    /// - file exists on disk but was ABSENT before the batch (the pre-batch version is a
    ///   delete marker — it stores no content): disk unchanged → removed (absence IS the
    ///   pre-batch state); edited afterwards → `skipped` unless `force` (force removes);
    /// - file created within the batch: disk still identical to the batch content → deleted
    ///   from disk (that IS the pre-batch state); edited afterwards → `skipped` unless
    ///   `force` (force deletes anyway);
    /// - file missing on disk, deleted within the batch → restored from the pre-batch
    ///   content (missing parent directories are created);
    /// - file missing on disk AND already absent before the batch (pre-batch version is a
    ///   delete marker) → `skipped` always: there is nothing to restore and a delete marker
    ///   holds empty content — "restoring" it would fabricate a 0-byte file, even with
    ///   `force`;
    /// - file missing on disk, removed after the batch (not by it) → `skipped` unless
    ///   `force` (force restores).
    ///
    /// A failing file only appends to `RevertReport::errors`; the other files are still
    /// processed. The batch is closed first so the revert's own pre_revert/restore records
    /// form a NEW batch ("current state after revert") instead of rejoining the reverted one.
    pub fn revert_batch(&self, batch_id: BatchId, force: bool) -> Result<RevertReport> {
        let ws: WorkspaceId = {
            let conn = self.conn.lock().unwrap();
            conn.query_row(
                "SELECT workspace_id FROM batches WHERE id = ?1",
                [batch_id],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => {
                    Error::NotFound(format!("batch {batch_id}"))
                }
                other => other.into(),
            })?
        };
        // Close the batch: its own revert records must not slide `ended_at` / rejoin it
        // (plan M6 §3.4 step 3 — the revert wave forms its own post-revert batch).
        {
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "UPDATE batches SET closed_at = COALESCE(closed_at, ?1) WHERE id = ?2",
                params![now_micros(), batch_id],
            )?;
        }
        let entries = self.batch_entries(batch_id)?;
        let mut report = RevertReport::default();
        if entries.is_empty() {
            return Ok(report);
        }
        let ws_root = self.workspace_root(ws)?;
        // Group per file; versions ascending by seq (entries arrive ordered by rel_path, seq).
        let mut groups: BTreeMap<FileId, (String, Vec<Version>)> = BTreeMap::new();
        for e in entries {
            let group = groups
                .entry(e.version.file_id)
                .or_insert_with(|| (e.rel_path.clone(), Vec::new()));
            group.1.push(e.version);
        }
        for (_file_id, (rel_path, versions)) in groups {
            if let Err(e) =
                self.revert_batch_one(ws, &ws_root, &rel_path, &versions, force, &mut report)
            {
                report.errors.push((rel_path, e.to_string()));
            }
        }
        Ok(report)
    }

    /// Reverts one file of a batch (one branch of the plan M6 §3.4 algorithm). `versions`
    /// are the file's in-batch versions, ascending by seq; the group is never empty.
    fn revert_batch_one(
        &self,
        ws: WorkspaceId,
        ws_root: &Path,
        rel_path: &str,
        versions: &[Version],
        force: bool,
        report: &mut RevertReport,
    ) -> Result<()> {
        let representative = versions
            .last()
            .ok_or_else(|| Error::CorruptChain(format!("batch entry rỗng cho {rel_path}")))?;
        let min_seq = versions
            .first()
            .ok_or_else(|| Error::CorruptChain(format!("batch entry rỗng cho {rel_path}")))?
            .seq;
        let file = rel_to_path(ws_root, rel_path);
        // Step 1: content state right before the batch — the newest chain version with a seq
        // below the batch's lowest seq for this file (= min_seq - 1 on an intact chain, but
        // robust to gaps left by GC). None → the file was created within the batch.
        let hist = self.history(ws, &file)?;
        let pre_batch = hist.iter().find(|v| v.seq < min_seq);
        let disk_exists = file.exists();
        let disk_hash = if disk_exists {
            Some(blake3::hash(&std::fs::read(&file)?).to_hex().to_string())
        } else {
            None
        };
        let unchanged = disk_hash.as_deref() == Some(representative.content_hash.as_str());

        if disk_exists {
            match pre_batch {
                Some(pre) if pre.kind == EventKind::Delete => {
                    // The file was absent before the batch (pre-batch version is a delete
                    // marker): absence IS the pre-batch state, so the disk file is removed —
                    // never overwritten with the marker's empty content.
                    if unchanged || force {
                        std::fs::remove_file(&file)?;
                        report.deleted += 1;
                    } else {
                        report.skipped += 1; // edited after the batch → keep the user's work
                    }
                }
                Some(pre) => {
                    if unchanged || force {
                        self.revert(ws, &file, pre.id)?;
                        report.reverted += 1;
                    } else {
                        report.skipped += 1; // edited after the batch → keep the user's work
                    }
                }
                None => {
                    // Created within the batch: deleting it returns the pre-batch state.
                    if unchanged || force {
                        std::fs::remove_file(&file)?;
                        report.deleted += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
            }
        } else {
            match pre_batch {
                Some(pre) if pre.kind == EventKind::Delete => {
                    // The file was ALREADY absent before the batch: a no-op — there is
                    // nothing to restore and a delete marker stores empty content, so
                    // "restoring" it would fabricate a 0-byte file (even with `force`).
                    report.skipped += 1;
                }
                Some(pre) => {
                    if representative.kind == EventKind::Delete || force {
                        self.restore_from_blob(&file, pre.id)?;
                        report.restored += 1;
                    } else {
                        report.skipped += 1; // the batch did not remove the file
                    }
                }
                None => {
                    // Created in the batch and gone from disk since.
                    if representative.kind == EventKind::Delete {
                        // Deleted within the batch → restore the in-batch content that
                        // preceded the delete (plan M6 §3.4 fallback when there is no
                        // pre-batch version).
                        match versions.iter().rev().find(|v| v.kind != EventKind::Delete) {
                            Some(src) => {
                                self.restore_from_blob(&file, src.id)?;
                                report.restored += 1;
                            }
                            None => report.skipped += 1, // nothing to restore
                        }
                    } else if force {
                        self.restore_from_blob(&file, representative.id)?;
                        report.restored += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes a version's content to `file`, creating missing parent directories.
    fn restore_from_blob(&self, file: &Path, version_id: VersionId) -> Result<()> {
        let data = self.content(version_id)?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(file, &data)?;
        Ok(())
    }
}

/// Absolutizes a stored rel_path (`/`-separated) against the workspace root.
fn rel_to_path(ws_root: &Path, rel_path: &str) -> PathBuf {
    let mut path = ws_root.to_path_buf();
    for comp in rel_path.split('/') {
        path.push(comp);
    }
    path
}
