//! GUI application: three-pane layout (workspace + a tabbed file/activity
//! list on the left, timeline on the right, diff in the center). Data is
//! polled from the engine once per second; revert/label writes and whole-batch
//! reverts go through a dedicated per-operation write connection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use history::{
    BatchEntry, BatchId, BatchSummary, EventKind, FileSummary, Hunk, LocalHistory, RevertReport,
    Version, VersionId, WorkspaceId,
};
use rust_i18n::t;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::diff_view::{self, DiffModel, Line, TextSelection};
use crate::icons;
use crate::state::{self, GuiState, LangPref};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Max batches listed in the activity tab (plan M6 §3.6).
const BATCH_LIST_LIMIT: u32 = 50;

/// Which tab the left panel shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftTab {
    /// Tracked files (search box + file list).
    Files,
    /// Batch activity ("Hoạt động") — batch list with a whole-batch revert.
    Activity,
}

/// Which pair of contents the diff compares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Selected version against the version immediately before it.
    Previous,
    /// Selected version against the current on-disk content.
    CurrentDisk,
    /// Two arbitrary versions picked by the user.
    AnyTwo,
}

impl DiffMode {
    /// Display label for the mode, from the i18n `diff.mode_*` keys.
    fn label(self) -> String {
        match self {
            DiffMode::Previous => t!("diff.mode_prev").to_string(),
            DiffMode::CurrentDisk => t!("diff.mode_disk").to_string(),
            DiffMode::AnyTwo => t!("diff.mode_any").to_string(),
        }
    }
}

/// Resolves the (left, right) version pair to diff. `versions` is newest-first;
/// `custom_left`/`custom_right` are the user's picks in [`DiffMode::AnyTwo`];
/// [`DiffMode::CurrentDisk`] returns `None` on the right, meaning "against disk".
pub fn pick_pair(
    mode: DiffMode,
    versions: &[Version],
    selected: Option<VersionId>,
    custom_left: Option<VersionId>,
    custom_right: Option<VersionId>,
) -> (Option<VersionId>, Option<VersionId>) {
    let Some(sel) = selected else { return (None, None) };
    let idx = versions.iter().position(|v| v.id == sel);
    match mode {
        DiffMode::Previous => {
            let prev = idx.and_then(|i| versions.get(i + 1)).map(|v| v.id);
            (Some(sel), prev)
        }
        DiffMode::CurrentDisk => (Some(sel), None),
        DiffMode::AnyTwo => {
            let left = custom_left.or(Some(sel));
            // right defaults to the version just before the left one (older)
            let right = custom_right.or_else(|| {
                let li = left.and_then(|id| versions.iter().position(|v| v.id == id));
                li.and_then(|i| versions.get(i + 1)).map(|v| v.id)
            });
            (left, right)
        }
    }
}

/// Finds the workspace containing `file` via component-wise ASCII
/// case-insensitive prefix match and returns `(workspace_id, rel_path)` with
/// `/` separators.
// TODO: Unicode-aware case folding for non-ASCII path components.
pub fn resolve_ws_and_rel(
    workspaces: &[(WorkspaceId, PathBuf)],
    file: &Path,
) -> Option<(WorkspaceId, String)> {
    let fcomps: Vec<String> = file
        .components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect();
    for (id, root) in workspaces {
        let rcomps: Vec<String> = root
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        if fcomps.len() > rcomps.len()
            && fcomps[..rcomps.len()]
                .iter()
                .zip(rcomps.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Some((*id, fcomps[rcomps.len()..].join("/")));
        }
    }
    None
}

/// Message shown in the status bar; kept until a newer one replaces it.
#[derive(Debug, Clone)]
pub struct Status {
    /// Message body.
    pub text: String,
    /// Success (true) or error (false) — selects the toast glyph and color.
    pub ok: bool,
}

/// A revert awaiting user confirmation in the modal.
#[derive(Debug, Clone)]
pub struct ConfirmRevert {
    /// Version to revert to.
    pub id: VersionId,
    /// Sequence number of that version, for the confirmation message.
    pub seq: i64,
    /// Workspace-relative path of the file.
    pub rel: String,
}

/// Identity of the displayed diff; a key change rebuilds the model (and
/// reloads content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffKey {
    /// Comparison mode the pair was picked for.
    pub mode: DiffMode,
    /// Left (old) side of the pair.
    pub left: Option<VersionId>,
    /// `None` in [`DiffMode::CurrentDisk`] means "against disk"; `None` in
    /// [`DiffMode::AnyTwo`] means the pair is incomplete.
    pub right: Option<VersionId>,
    /// Data token (total version count); a change means the store data may
    /// have moved under the pair.
    pub token: u64,
}

/// Cached diff model; rebuilt whenever the incoming [`DiffKey`] differs.
#[derive(Debug)]
pub struct CachedDiff {
    /// Key the model was built for.
    pub key: DiffKey,
    /// Renderable diff model.
    pub model: DiffModel,
    /// Raw on-disk bytes the model was built against (CurrentDisk mode only).
    /// Copying the unified diff uses THESE bytes, not a fresh read, so the
    /// copied diff always matches what the panes are showing.
    pub disk_bytes: Option<Vec<u8>>,
}

/// One file block of the batch view: header info + the renderable side-by-side
/// model (left = before the batch, right = the batched version).
#[derive(Debug, Clone)]
pub struct BatchBlock {
    /// Workspace-relative path of the file (shown in the block header).
    pub rel_path: String,
    /// Kind of the batched version (shown in the block header).
    pub kind: EventKind,
    /// Size of the batched content, in bytes (shown in the block header).
    pub size: u64,
    /// Side-by-side model; `left_title`/`right_title` carry the i18n pane
    /// titles ("Before batch" / "In batch (seq N)").
    pub model: DiffModel,
}

/// Cached batch view (one block per batch entry, engine order = rel_path,
/// seq); rebuilt whenever the selected batch, the data token, or the entry
/// list changes — the same pattern as [`CachedDiff`], whose key includes the
/// token.
#[derive(Debug)]
pub struct CachedBatchView {
    /// Batch the blocks were built for.
    pub batch_id: BatchId,
    /// Data token (`last_token`) the blocks were built against; a change means
    /// the store data moved (e.g. a whole-batch revert) and a rebuild is due.
    pub token: u64,
    /// Fingerprint of the `batch_entries` list the blocks were built from
    /// ([`batch_entries_fingerprint`]). Separates the just-cleared list (a
    /// batch click clears the entries; the refill lands on the next poll)
    /// from the loaded one — the token alone cannot (a click changes no
    /// store data), so without it an empty view would stay cached forever.
    pub entries_fp: u64,
    /// One block per batch entry, in `batch_entries` order.
    pub blocks: Vec<BatchBlock>,
}

/// Application state: polled engine data, current selection, cached diff, and
/// pending modal actions.
pub struct App {
    pub store_root: PathBuf,
    pub state: GuiState,
    pub state_path: PathBuf,
    /// Read-only store connection; `None` until it opens (the store picker
    /// screen shows in the meantime).
    pub ro: Option<LocalHistory>,
    pub workspaces: Vec<(WorkspaceId, PathBuf)>,
    pub current_ws: Option<WorkspaceId>,
    pub files: Vec<FileSummary>,
    /// rel path (`/`-separated) of the selected file.
    pub selected_file: Option<String>,
    pub versions: Vec<Version>,
    pub selected_version: Option<VersionId>,
    // left panel tabs + batches ("Hoạt động" tab, plan M6 §3.6)
    pub left_tab: LeftTab,
    /// Latest batches of the current workspace, newest first (capped at 50).
    pub batches: Vec<BatchSummary>,
    /// Batch selected in the activity tab.
    pub selected_batch: Option<BatchId>,
    /// Versions of the selected batch, reloaded on each poll.
    pub batch_entries: Vec<BatchEntry>,
    /// Entry clicked in the activity tab: its index in `batch_entries` (which
    /// maps 1:1 onto the batch view blocks — one block per entry, same order)
    /// plus the clicked rel_path. The index picks the right block when a file
    /// changed twice in one batch (two entries/blocks share one rel_path);
    /// the path re-check drops a stale index (entries reloaded in between).
    /// One-shot: the center view scrolls the block into view once, then clears.
    pub batch_focus_entry: Option<(usize, String)>,
    /// Cached batch view blocks; rebuilt when the selected batch or the data
    /// token changes (see [`CachedBatchView`]).
    pub batch_diff_cache: Option<CachedBatchView>,
    /// `version_id -> batch_id` of the selected file — the timeline ⊕ badge
    /// (`batch_map_for_file`; `struct Version` stays untouched).
    pub batch_versions: HashMap<VersionId, BatchId>,
    // timeline filters
    pub kind_filter: Option<EventKind>,
    pub label_filter: String,
    pub file_search: String,
    // diff state
    pub diff_mode: DiffMode,
    pub diff_left_id: Option<VersionId>,
    pub diff_right_id: Option<VersionId>,
    pub diff_cache: Option<CachedDiff>,
    /// Character-level diff selection (drag / shift+click highlight + Ctrl+C
    /// raw copy). RESET whenever the diff pair changes — see `diff_panel`.
    pub diff_selection: TextSelection,
    // pending modal actions
    pub confirm_revert: Option<ConfirmRevert>,
    /// A whole-batch revert awaiting confirmation in the modal.
    pub confirm_revert_batch: Option<BatchSummary>,
    pub label_edit_open: bool,
    pub label_draft: String,
    pub status: Status,
    pub last_poll: Option<OffsetDateTime>,
    /// When the next poll runs (`None` = next frame); polls never run every frame.
    pub next_poll: Option<Instant>,
    pub last_token: u64,
    pub force_refresh: bool,
    // --file startup target
    pub pending_file_path: Option<PathBuf>,
    pub pending_file_rel: Option<String>,
    pub pending_announced: bool,
}

impl App {
    /// Builds the app with the engine's default store root and, when launched
    /// with `--file`, the file whose timeline to open once its data resolves.
    pub fn new(default_root: PathBuf, initial_file: Option<PathBuf>) -> Self {
        let mut state = state::load_state(&state::state_path(&default_root));
        let store_root = state
            .last_store_root
            .as_ref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| default_root.clone());
        if store_root != default_root {
            // the chosen store's own state may carry a more specific last_ws_root
            let st2 = state::load_state(&state::state_path(&store_root));
            if state.last_ws_root.is_none() {
                state.last_ws_root = st2.last_ws_root;
            }
        }
        let state_path = state::state_path(&store_root);
        state.last_store_root = Some(store_root.to_string_lossy().to_string());
        // Set the locale BEFORE the first string is built ("status.ready") — the
        // first frame must honor the persisted state; later switches go through
        // set_lang (egui repaints anyway).
        rust_i18n::set_locale(state::resolve_lang(
            LangPref::parse(state.lang.as_deref()),
            sys_locale::get_locale().as_deref(),
        ));
        App {
            store_root,
            state,
            state_path,
            ro: None,
            workspaces: Vec::new(),
            current_ws: None,
            files: Vec::new(),
            selected_file: None,
            versions: Vec::new(),
            selected_version: None,
            left_tab: LeftTab::Files,
            batches: Vec::new(),
            selected_batch: None,
            batch_entries: Vec::new(),
            batch_focus_entry: None,
            batch_diff_cache: None,
            batch_versions: HashMap::new(),
            kind_filter: None,
            label_filter: String::new(),
            file_search: String::new(),
            diff_mode: DiffMode::Previous,
            diff_left_id: None,
            diff_right_id: None,
            diff_cache: None,
            diff_selection: TextSelection::default(),
            confirm_revert: None,
            confirm_revert_batch: None,
            label_edit_open: false,
            label_draft: String::new(),
            status: Status { text: t!("status.ready").to_string(), ok: true },
            last_poll: None,
            next_poll: None,
            last_token: 0,
            force_refresh: true,
            pending_file_path: initial_file,
            pending_file_rel: None,
            pending_announced: false,
        }
    }

    fn set_status(&mut self, text: impl Into<String>, ok: bool) {
        self.status = Status { text: text.into(), ok };
    }

    /// Switches the UI language at runtime: persists the preference and applies
    /// the global locale; subsequent repaints render the new language.
    fn set_lang(&mut self, pref: LangPref) {
        if self.state.lang.as_deref() == Some(pref.as_str()) {
            return;
        }
        self.state.lang = Some(pref.as_str().to_string());
        self.save_state();
        rust_i18n::set_locale(state::resolve_lang(
            pref,
            sys_locale::get_locale().as_deref(),
        ));
    }

    fn save_state(&mut self) {
        if let Err(e) = state::save_state(&self.state_path, &self.state) {
            self.set_status(t!("status.state_save", e = e).to_string(), false);
        }
    }

    /// Absolute file path built from the workspace root and a `/`-separated rel path.
    fn ws_file_path(&self, ws: WorkspaceId, rel: &str) -> PathBuf {
        let root = self
            .workspaces
            .iter()
            .find(|(id, _)| *id == ws)
            .map(|(_, p)| p.clone())
            .unwrap_or_default();
        rel.split('/').fold(root, |acc, part| acc.join(part))
    }

    // -----------------------------------------------------------------------
    // Polling (read-only connection)
    // -----------------------------------------------------------------------

    fn poll(&mut self) {
        self.last_poll = Some(state::local_now());
        if self.ro.is_none() {
            match LocalHistory::open_read_only(&self.store_root) {
                Ok(s) => {
                    self.ro = Some(s);
                    self.force_refresh = true;
                }
                Err(_) => return, // no store yet — the store picker screen shows instead
            }
        }

        // workspaces (cheap query, runs every poll)
        if let Some(store) = self.ro.as_ref() {
            match store.workspaces() {
                Ok(wss) => {
                    if wss != self.workspaces {
                        self.workspaces = wss;
                        self.force_refresh = true;
                    }
                }
                Err(e) => {
                    self.set_status(t!("err.workspaces", e = e).to_string(), false);
                    return;
                }
            }
        } else {
            return;
        }

        // --file: resolve workspace + rel path (once)
        if let Some(file) = self.pending_file_path.take() {
            match resolve_ws_and_rel(&self.workspaces, &file) {
                Some((id, rel)) => {
                    self.current_ws = Some(id);
                    self.pending_file_rel = Some(rel);
                    self.force_refresh = true;
                }
                None => {
                    self.set_status(
                        t!("msg.file_outside_ws", file = file.display()).to_string(),
                        false,
                    );
                }
            }
        }

        // first-run auto-pick: last_ws_root, else the first workspace
        if self.current_ws.is_none() {
            let pick = self
                .state
                .last_ws_root
                .as_ref()
                .and_then(|r| {
                    self.workspaces
                        .iter()
                        .find(|(_, p)| p.to_string_lossy().eq_ignore_ascii_case(r))
                })
                .map(|(id, _)| *id)
                .or_else(|| self.workspaces.first().map(|(id, _)| *id));
            if let Some(id) = pick {
                self.switch_ws(id);
            }
        }
        let Some(ws) = self.current_ws else { return };

        // file list + a cheap change token: file count + total version count
        let files = match self.ro.as_ref().map(|s| s.list_files(ws, None)) {
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                self.set_status(t!("err.list_files", e = e).to_string(), false);
                return;
            }
            None => return,
        };
        let token = files
            .iter()
            .map(|f| f.version_count as u64)
            .sum::<u64>()
            .wrapping_add(files.len() as u64);
        self.files = files;
        if token != self.last_token || self.force_refresh {
            self.last_token = token;
            self.force_refresh = false;
            self.on_data_changed();
        }

        // batches ("Hoạt động" tab) + the selected batch's entries — cheap
        // capped queries, refreshed on the same 1s poll as the file list
        let batches = if let Some(store) = self.ro.as_ref() {
            store.batches(ws, BATCH_LIST_LIMIT)
        } else {
            return;
        };
        match batches {
            Ok(b) => self.batches = b,
            Err(e) => self.set_status(t!("batch.err_list", e = e).to_string(), false),
        }
        if let Some(id) = self.selected_batch {
            let entries = if let Some(store) = self.ro.as_ref() {
                store.batch_entries(id)
            } else {
                return;
            };
            match entries {
                Ok(list) => self.batch_entries = list,
                Err(history::Error::NotFound(_)) => {
                    // the batch vanished (GC'd / store switched) — drop the
                    // selection and everything built on top of it
                    self.selected_batch = None;
                    self.batch_entries.clear();
                    self.batch_diff_cache = None;
                    self.batch_focus_entry = None;
                }
                Err(e) => self.set_status(t!("batch.err_list", e = e).to_string(), false),
            }
        }
    }

    /// Handles freshly changed data (a new token seen by a poll, or a forced refresh).
    fn on_data_changed(&mut self) {
        // --file: select the file once it shows up in the list
        if let Some(rel) = self.pending_file_rel.clone() {
            if self.selected_file.as_deref() != Some(rel.as_str())
                && self.files.iter().any(|f| f.rel_path == rel)
            {
                self.selected_file = Some(rel);
                self.selected_version = None;
            }
            if self.selected_file.is_none() && !self.files.is_empty() && !self.pending_announced {
                self.pending_announced = true;
                self.set_status(
                    t!(
                        "msg.file_no_versions_yet",
                        file = self.pending_file_rel.clone().unwrap_or_default()
                    )
                    .to_string(),
                    true,
                );
            }
        }
        self.refresh_versions();
    }

    /// Reloads the versions of the selected file (content is NOT loaded here —
    /// only when a diff pair is built) plus the file's version→batch map for
    /// the timeline badge.
    fn refresh_versions(&mut self) {
        let Some(ws) = self.current_ws else { return };
        let Some(rel) = self.selected_file.clone() else {
            self.versions.clear();
            self.selected_version = None;
            self.batch_versions.clear();
            return;
        };
        let Some(store) = self.ro.as_ref() else { return };
        let file = self.ws_file_path(ws, &rel);
        // both queries run before any status write — `store` borrows self.ro
        let hist = store.history(ws, &file);
        let badge_map = store.batch_map_for_file(ws, &file);
        match hist {
            Ok(vs) => {
                self.versions = vs;
                if let Some(sel) = self.selected_version {
                    if !self.versions.iter().any(|v| v.id == sel) {
                        self.selected_version = self.versions.first().map(|v| v.id);
                    }
                } else {
                    self.selected_version = self.versions.first().map(|v| v.id);
                }
            }
            Err(e) => self.set_status(t!("err.history", e = e).to_string(), false),
        }
        match badge_map {
            Ok(m) => self.batch_versions = m,
            Err(e) => {
                // badge data only — report it, but never block the timeline
                self.batch_versions.clear();
                self.set_status(t!("batch.err_map", e = e).to_string(), false);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Selection / store switches
    // -----------------------------------------------------------------------

    fn switch_ws(&mut self, id: WorkspaceId) {
        if self.current_ws == Some(id) {
            return;
        }
        self.current_ws = Some(id);
        self.files.clear();
        self.versions.clear();
        self.selected_file = None;
        self.selected_version = None;
        self.batches.clear();
        self.selected_batch = None;
        self.batch_entries.clear();
        self.batch_diff_cache = None;
        self.batch_focus_entry = None;
        self.batch_versions.clear();
        self.diff_cache = None;
        self.last_token = 0;
        if let Some((_, root)) = self.workspaces.iter().find(|(wid, _)| *wid == id) {
            self.state.last_ws_root = Some(root.to_string_lossy().to_string());
        }
        self.save_state();
        self.force_refresh = true;
        self.next_poll = None; // poll on the next frame
    }

    fn switch_store(&mut self, dir: PathBuf) {
        self.store_root = dir;
        self.state_path = state::state_path(&self.store_root);
        self.state = state::load_state(&self.state_path);
        self.state.last_store_root = Some(self.store_root.to_string_lossy().to_string());
        self.ro = None;
        self.workspaces.clear();
        self.current_ws = None;
        self.files.clear();
        self.versions.clear();
        self.selected_file = None;
        self.selected_version = None;
        self.batches.clear();
        self.selected_batch = None;
        self.batch_entries.clear();
        self.batch_diff_cache = None;
        self.batch_focus_entry = None;
        self.batch_versions.clear();
        self.diff_cache = None;
        self.last_token = 0;
        self.force_refresh = true;
        self.next_poll = None;
        self.save_state();
    }

    // -----------------------------------------------------------------------
    // Write operations (dedicated connection, opened per operation)
    // -----------------------------------------------------------------------

    fn add_workspace(&mut self) {
        let Some(dir) = rfd::FileDialog::new()
            .set_title(t!("dlg.pick_ws"))
            .pick_folder()
        else {
            return;
        };
        let res = LocalHistory::open(&self.store_root)
            .map_err(|e| e.to_string())
            .and_then(|store| store.register_workspace(&dir).map_err(|e| e.to_string()));
        match res {
            Ok(id) => {
                self.set_status(t!("status.ws_added", path = dir.display()).to_string(), true);
                self.switch_ws(id);
            }
            Err(e) => self.set_status(t!("err.ws_add", e = e).to_string(), false),
        }
    }

    fn do_revert(&mut self, target: ConfirmRevert) {
        self.confirm_revert = None;
        let Some(ws) = self.current_ws else { return };
        let file = self.ws_file_path(ws, &target.rel);
        let res = LocalHistory::open(&self.store_root)
            .map_err(|e| e.to_string())
            .and_then(|store| store.revert(ws, &file, target.id).map_err(|e| e.to_string()));
        match res {
            Ok(()) => {
                self.force_refresh = true;
                self.poll(); // reload versions now to find the pre_revert entry
                self.next_poll = Some(Instant::now() + POLL_INTERVAL);
                let pre = self
                    .versions
                    .iter()
                    .find(|v| v.kind == EventKind::PreRevert)
                    .map(|v| v.seq);
                match pre {
                    Some(seq) => self.set_status(
                        t!(
                            "msg.reverted_pre",
                            file = target.rel,
                            seq = target.seq,
                            pre = seq
                        )
                        .to_string(),
                        true,
                    ),
                    None => self.set_status(
                        t!("msg.reverted_noop", seq = target.seq).to_string(),
                        true,
                    ),
                }
            }
            Err(e) => self.set_status(t!("err.revert", e = e).to_string(), false),
        }
    }

    /// Whole-batch revert from the activity tab (plan M6 §3.6): dedicated write
    /// connection per operation, `force = false` — files edited after the batch
    /// are skipped (and reported), never clobbered.
    fn do_revert_batch(&mut self, batch: BatchSummary) {
        self.confirm_revert_batch = None;
        let res = LocalHistory::open(&self.store_root)
            .map_err(|e| e.to_string())
            .and_then(|store| store.revert_batch(batch.id, false).map_err(|e| e.to_string()));
        match res {
            Ok(report) => {
                self.set_status(format_revert_report(&report), report.errors.is_empty());
                self.force_refresh = true;
                self.next_poll = None; // reload versions + batches on the next frame
            }
            Err(e) => self.set_status(t!("batch.err_revert", e = e).to_string(), false),
        }
    }

    fn submit_label(&mut self) {
        self.label_edit_open = false;
        let Some(id) = self.selected_version else { return };
        let text = self.label_draft.trim().to_string();
        let label = if text.is_empty() { None } else { Some(text.as_str()) };
        let res = LocalHistory::open(&self.store_root)
            .map_err(|e| e.to_string())
            .and_then(|store| store.set_label(id, label).map_err(|e| e.to_string()));
        match res {
            Ok(()) => {
                self.set_status(
                    if label.is_some() {
                        t!("msg.label_set", text = text).to_string()
                    } else {
                        t!("msg.label_cleared").to_string()
                    },
                    true,
                );
                self.force_refresh = true;
                self.next_poll = None;
            }
            Err(e) => self.set_status(t!("err.label", e = e).to_string(), false),
        }
    }

    // -----------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------

    fn update_ui(&mut self, ui: &mut egui::Ui) {
        // Order matters: bottom → left → right → CentralPanel LAST (egui convention).
        egui::Panel::bottom("lh_status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::left("lh_files")
            .default_size(280.0)
            .min_size(200.0)
            .resizable(true)
            .show(ui, |ui| self.left_panel(ui));
        egui::Panel::right("lh_timeline")
            .default_size(340.0)
            .min_size(280.0)
            .resizable(true)
            .show(ui, |ui| self.timeline_panel(ui));
        egui::CentralPanel::default().show(ui, |ui| self.diff_panel(ui));
        self.show_modals(ui.ctx());
    }

    fn store_picker_screen(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(90.0);
                ui.heading(egui::RichText::new(format!(
                    "{} {}",
                    icons::WARNING,
                    t!("store.missing_title")
                )));
                ui.add_space(8.0);
                ui.label(t!("store.missing_text", path = self.store_root.display()));
                ui.label(t!("store.missing_hint"));
                ui.add_space(16.0);
                if ui
                    .button(egui::RichText::new(format!(
                        "{} {}",
                        icons::FOLDER_OPEN,
                        t!("btn.pick_store")
                    )))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title(t!("dlg.pick_store"))
                        .pick_folder()
                {
                    if dir.join("index.db").exists() {
                        self.switch_store(dir);
                    } else {
                        self.set_status(t!("err.no_index").to_string(), false);
                    }
                }
            });
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.small(t!("status.store", root = self.store_root.display()));
            ui.separator();
            let ws = self
                .current_ws
                .and_then(|id| self.workspaces.iter().find(|(w, _)| *w == id))
                .map(|(_, p)| p.display().to_string())
                .unwrap_or_else(|| "—".to_string());
            ui.small(t!("status.ws", root = ws));
            ui.separator();
            let poll = self.last_poll.map(state::hhmm).unwrap_or_else(|| "—".into());
            ui.small(t!("status.poll", time = poll));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // language dropdown — runtime switch (persists state + set_locale)
                let pref = LangPref::parse(self.state.lang.as_deref());
                egui::ComboBox::from_id_salt("lang_combo")
                    .selected_text(match pref {
                        LangPref::Auto => t!("lang.auto"),
                        LangPref::En => t!("lang.en"),
                        LangPref::Vi => t!("lang.vi"),
                    })
                    .width(96.0)
                    .show_ui(ui, |ui| {
                        for (p, label) in [
                            (LangPref::Auto, t!("lang.auto")),
                            (LangPref::En, t!("lang.en")),
                            (LangPref::Vi, t!("lang.vi")),
                        ] {
                            if ui.selectable_label(pref == p, label).clicked() {
                                self.set_lang(p);
                                // repaint immediately so every panel switches language
                                ui.ctx().request_repaint();
                            }
                        }
                    });
                ui.separator();
                // toast: check/error glyph + message text
                let icon = if self.status.ok { icons::CHECK } else { icons::WARNING };
                let text = egui::RichText::new(format!("{icon} {}", self.status.text)).small();
                let text = if self.status.ok {
                    text.color(egui::Color32::from_rgb(110, 200, 130))
                } else {
                    text.color(egui::Color32::from_rgb(235, 110, 100))
                };
                ui.label(text);
            });
        });
    }

    fn left_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(t!("panel.workspace"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(egui::RichText::new(icons::ADD).size(16.0))
                    .on_hover_text(t!("panel.ws_add_tip"))
                    .clicked()
                {
                    self.add_workspace();
                }
            });
        });
        let selected_ws = self.current_ws;
        let wss = self.workspaces.clone();
        let sel_label = wss
            .iter()
            .find(|(id, _)| Some(*id) == selected_ws)
            .map(|(_, p)| p.display().to_string())
            .unwrap_or_else(|| t!("panel.ws_none_selected").to_string());
        egui::ComboBox::from_id_salt("ws_combo")
            .selected_text(sel_label)
            .truncate()
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                for (id, root) in &wss {
                    let selected = Some(*id) == selected_ws;
                    if ui
                        .selectable_label(selected, root.display().to_string())
                        .clicked()
                    {
                        self.switch_ws(*id);
                    }
                }
                if wss.is_empty() {
                    ui.weak(t!("panel.ws_empty_hint"));
                }
            });
        ui.separator();

        // tab bar: tracked files | batch activity (plan M6 §3.6)
        ui.horizontal(|ui| {
            if ui
                .selectable_label(self.left_tab == LeftTab::Files, t!("batch.tab_files"))
                .clicked()
            {
                self.left_tab = LeftTab::Files;
            }
            if ui
                .selectable_label(self.left_tab == LeftTab::Activity, t!("batch.tab_activity"))
                .clicked()
            {
                self.left_tab = LeftTab::Activity;
            }
        });
        ui.separator();
        match self.left_tab {
            LeftTab::Files => self.files_tab(ui),
            LeftTab::Activity => self.activity_tab(ui),
        }
    }

    /// "Files" tab: search box + the tracked-file list.
    fn files_tab(&mut self, ui: &mut egui::Ui) {
        ui.add(
            egui::TextEdit::singleline(&mut self.file_search)
                .hint_text(format!("{} {}", icons::SEARCH, t!("panel.search_hint")))
                .desired_width(f32::INFINITY),
        );
        ui.add_space(4.0);

        let query = self.file_search.to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("file_list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut shown = 0;
                for i in 0..self.files.len() {
                    let f = self.files[i].clone();
                    if !query.is_empty() && !f.rel_path.to_lowercase().contains(&query) {
                        continue;
                    }
                    shown += 1;
                    let selected = self.selected_file.as_deref() == Some(f.rel_path.as_str());
                    if self.file_row(ui, &f, selected).clicked() {
                        self.selected_file = Some(f.rel_path.clone());
                        self.selected_version = None;
                        self.diff_cache = None;
                        self.refresh_versions();
                    }
                }
                if shown == 0 {
                    ui.weak(if self.files.is_empty() {
                        t!("panel.files_empty")
                    } else {
                        t!("panel.files_no_match")
                    });
                }
            });
    }

    /// One file row: rel path, version count and last-modified time.
    fn file_row(&self, ui: &mut egui::Ui, f: &FileSummary, selected: bool) -> egui::Response {
        let h = 34.0;
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
        if selected {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let p = ui.painter_at(rect);
        let dim = egui::Color32::from_rgb(150, 150, 150);
        p.text(
            egui::pos2(rect.left() + 6.0, rect.top() + 10.0),
            egui::Align2::LEFT_CENTER,
            f.rel_path.as_str(),
            egui::FontId::proportional(13.0),
            egui::Color32::from_rgb(225, 225, 225),
        );
        let offset = state::local_utc_offset();
        let now = (state::local_now().unix_timestamp_nanos() / 1000) as i64;
        let ago = state::format_relative_time(f.last_timestamp_micros, now, offset);
        let info = if f.version_count == 1 {
            t!("panel.files_summary_one", count = f.version_count, ago = ago)
        } else {
            t!("panel.files_summary", count = f.version_count, ago = ago)
        };
        p.text(
            egui::pos2(rect.left() + 6.0, rect.bottom() - 10.0),
            egui::Align2::LEFT_CENTER,
            info,
            egui::FontId::proportional(11.0),
            dim,
        );
        // kind-colored dot for the latest event, top-right
        p.circle_filled(egui::pos2(rect.right() - 10.0, rect.top() + 10.0), 3.5, state::kind_color(f.last_kind));
        resp.on_hover_text(f.rel_path.as_str())
    }

    /// "Activity" tab: batch list; selecting a batch shows its entries plus a
    /// whole-batch revert button (plan M6 §3.6).
    fn activity_tab(&mut self, ui: &mut egui::Ui) {
        let now = (state::local_now().unix_timestamp_nanos() / 1000) as i64;
        let offset = state::local_utc_offset();
        // The list must NOT take the whole panel height — the selected batch's
        // entries + revert button live below it (a full-height ScrollArea would
        // squeeze them to zero pixels, which is exactly the bug seen live).
        let list_max = (ui.available_height() * 0.45).max(120.0);
        egui::ScrollArea::vertical()
            .id_salt("batch_list")
            .auto_shrink([false, true])
            .max_height(list_max)
            .show(ui, |ui| {
                if self.batches.is_empty() {
                    ui.weak(t!("batch.empty"));
                }
                for i in 0..self.batches.len() {
                    let b = self.batches[i].clone();
                    let selected = self.selected_batch == Some(b.id);
                    if self.batch_row(ui, &b, selected, now, offset).clicked() {
                        self.selected_batch = Some(b.id);
                        self.batch_entries.clear();
                        // the focus belonged to the previous batch's view
                        self.batch_focus_entry = None;
                        self.next_poll = None; // load the entries on the next frame
                    }
                }
            });

        // details of the selected batch: header (entries count + revert) + entry list
        if let Some(id) = self.selected_batch {
            ui.separator();
            ui.horizontal(|ui| {
                let count = self
                    .batches
                    .iter()
                    .find(|b| b.id == id)
                    .map(|b| b.file_count)
                    .unwrap_or(0);
                ui.strong(t!("batch.entries_title", id = id, count = file_count_text(count)));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(egui::RichText::new(format!(
                            "{} {}",
                            icons::UNDO,
                            t!("batch.btn_revert_all")
                        )))
                        .clicked()
                    {
                        self.confirm_revert_batch =
                            self.batches.iter().find(|b| b.id == id).cloned();
                    }
                });
            });
            egui::ScrollArea::vertical()
                .id_salt("batch_entries")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for i in 0..self.batch_entries.len() {
                        let e = self.batch_entries[i].clone();
                        // focused = this entry's block is (about to be) scrolled
                        // to in the center batch view — compared by INDEX plus
                        // rel_path (the same condition `batch_focus_block`
                        // applies) so only the clicked row lights up when a file
                        // has two entries, and a stale flag never highlights
                        let focused = self
                            .batch_focus_entry
                            .as_ref()
                            .is_some_and(|(fi, rel)| *fi == i && *rel == e.rel_path);
                        if self.batch_entry_row(ui, &e, focused).clicked() {
                            self.batch_focus_entry = Some((i, e.rel_path.clone()));
                        }
                    }
                    if self.batch_entries.is_empty() {
                        ui.weak(t!("batch.no_entries"));
                    }
                });
        }
    }

    /// One clickable batch-entry row: rel path + kind · size. Clicking focuses
    /// the entry: the center batch view scrolls to that entry's block (see
    /// `batch_view_panel`). Same look as `batch_row` — highlight on focus/hover.
    fn batch_entry_row(&self, ui: &mut egui::Ui, e: &BatchEntry, focused: bool) -> egui::Response {
        let h = 22.0;
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
        if focused {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let p = ui.painter_at(rect);
        let (rel, meta) = batch_entry_row(e);
        p.text(
            egui::pos2(rect.left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            rel,
            egui::FontId::proportional(12.5),
            egui::Color32::from_rgb(225, 225, 225),
        );
        p.text(
            egui::pos2(rect.right() - 6.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            meta,
            egui::FontId::proportional(11.0),
            egui::Color32::from_rgb(150, 150, 150),
        );
        resp
    }

    /// One batch row: relative start time · file count · label-or-kind.
    fn batch_row(
        &self,
        ui: &mut egui::Ui,
        b: &BatchSummary,
        selected: bool,
        now_micros: i64,
        offset: UtcOffset,
    ) -> egui::Response {
        let h = 30.0;
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
        if selected {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let p = ui.painter_at(rect);
        let text = batch_row_text(b, now_micros, offset);
        p.text(
            egui::pos2(rect.left() + 6.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            text,
            egui::FontId::proportional(12.5),
            egui::Color32::from_rgb(225, 225, 225),
        );
        resp.on_hover_text(
            t!("batch.entries_title", id = b.id, count = file_count_text(b.file_count)).to_string(),
        )
    }

    fn timeline_panel(&mut self, ui: &mut egui::Ui) {
        match self.selected_file.clone() {
            Some(rel) => {
                let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                ui.heading(egui::RichText::new(format!("{} {}", icons::HISTORY, name)));
                ui.small(rel);
            }
            None => {
                ui.weak(t!("panel.timeline_no_file"));
            }
        }
        ui.add_space(2.0);

        // kind + label filters
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(format!(
                "{} {}",
                icons::FILTER_LIST,
                t!("filter.kind")
            )));
            let current = self
                .kind_filter
                .map(|k| k.as_str().to_string())
                .unwrap_or_else(|| t!("filter.all").to_string());
            egui::ComboBox::from_id_salt("kind_filter")
                .selected_text(current)
                .width(110.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(self.kind_filter.is_none(), t!("filter.all"))
                        .clicked()
                    {
                        self.kind_filter = None;
                    }
                    for k in state::ALL_KINDS {
                        if ui
                            .selectable_label(self.kind_filter == Some(k), k.as_str())
                            .clicked()
                        {
                            self.kind_filter = Some(k);
                        }
                    }
                });
            ui.label(t!("filter.label"));
            ui.add(
                egui::TextEdit::singleline(&mut self.label_filter)
                    .hint_text(t!("filter.label_hint"))
                    .desired_width(120.0),
            );
        });

        // actions on the selected version
        ui.horizontal(|ui| {
            let has_sel = self.selected_version.is_some() && self.selected_file.is_some();
            if ui
                .add_enabled(
                    has_sel,
                    egui::Button::new(format!("{} {}", icons::UNDO, t!("btn.revert"))),
                )
                .clicked()
            {
                self.confirm_revert = self
                    .selected_version
                    .and_then(|id| self.versions.iter().find(|v| v.id == id))
                    .map(|v| ConfirmRevert {
                        id: v.id,
                        seq: v.seq,
                        rel: self.selected_file.clone().unwrap_or_default(),
                    });
            }
            if ui
                .add_enabled(
                    has_sel,
                    egui::Button::new(format!("{} {}", icons::LABEL, t!("btn.label"))),
                )
                .clicked()
            {
                self.label_draft = self
                    .selected_version
                    .and_then(|id| self.versions.iter().find(|v| v.id == id))
                    .and_then(|v| v.label.clone())
                    .unwrap_or_default();
                self.label_edit_open = true;
            }
        });
        ui.separator();

        let query = self.label_filter.to_lowercase();
        let kind_filter = self.kind_filter;
        let selected_version = self.selected_version;
        let offset: UtcOffset = state::local_utc_offset();
        let today: Date = state::local_now().date();

        egui::ScrollArea::vertical()
            .id_salt("timeline")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let items: Vec<(Date, Version)> = self
                    .versions
                    .iter()
                    .filter(|v| {
                        let label_ok = query.is_empty()
                            || v.label
                                .as_deref()
                                .map(|l| l.to_lowercase().contains(&query))
                                .unwrap_or(false);
                        kind_filter.is_none_or(|k| v.kind == k) && label_ok
                    })
                    .map(|v| {
                        (
                            state::unix_micros_to_local(v.timestamp_micros, offset).date(),
                            v.clone(),
                        )
                    })
                    .collect();
                let groups = state::group_by_day(items, today);
                if self.versions.is_empty() {
                    ui.weak(t!("timeline.empty"));
                } else if groups.is_empty() {
                    ui.weak(t!("timeline.no_match"));
                }
                for g in groups {
                    ui.add_space(4.0);
                    ui.strong(g.header);
                    for v in g.items {
                        let selected = selected_version == Some(v.id);
                        if self.version_row(ui, &v, selected).clicked() {
                            self.selected_version = Some(v.id);
                            if self.diff_mode == DiffMode::AnyTwo {
                                self.diff_left_id = Some(v.id);
                            }
                        }
                    }
                }
            });
    }

    /// One version row: time · kind badge · label · storage/size.
    fn version_row(&self, ui: &mut egui::Ui, v: &Version, selected: bool) -> egui::Response {
        let h = 26.0;
        let (rect, resp) =
            ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click());
        if selected {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.6));
        } else if resp.hovered() {
            ui.painter()
                .rect_filled(rect, 4.0, ui.visuals().widgets.hovered.bg_fill);
        }
        let p = ui.painter_at(rect);
        let y = rect.center().y;
        let dim = egui::Color32::from_rgb(150, 150, 150);
        let dt = state::unix_micros_to_local(v.timestamp_micros, state::local_utc_offset());
        p.text(
            egui::pos2(rect.left() + 6.0, y),
            egui::Align2::LEFT_CENTER,
            state::hhmm(dt),
            egui::FontId::monospace(12.0),
            dim,
        );
        // kind badge (tinted background, kind-colored text)
        let kind_col = state::kind_color(v.kind);
        let galley = p.layout_no_wrap(v.kind.as_str().to_string(), egui::FontId::proportional(11.0), kind_col);
        let pad = 6.0;
        let bw = galley.size().x + 2.0 * pad;
        let bx = rect.left() + 46.0;
        p.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(bx, y - galley.size().y / 2.0),
                egui::vec2(bw, galley.size().y),
            ),
            3.0,
            kind_col.gamma_multiply(0.2),
        );
        p.galley(egui::pos2(bx + pad, y - galley.size().y / 2.0), galley, kind_col);
        // label (italic, light purple)
        let mut after_label = bx + bw + 8.0;
        if let Some(label) = &v.label {
            let label_col = egui::Color32::from_rgb(200, 180, 240);
            let g = p.layout_no_wrap(label.clone(), egui::FontId::proportional(12.5), label_col);
            let advance = g.size().x + 8.0;
            p.galley(egui::pos2(after_label, y - g.size().y / 2.0), g, label_col);
            after_label += advance;
        }
        // ⊕N badge: the version belongs to a batch (plan M6 §3.6) — N = files
        // in that batch; data from batch_map_for_file, count from the batch list
        if let Some(&bid) = self.batch_versions.get(&v.id) {
            let count = self.batches.iter().find(|b| b.id == bid).map(|b| b.file_count);
            let badge = match count {
                Some(c) => format!("{}{c}", icons::CONTROL_POINT),
                None => icons::CONTROL_POINT.to_string(),
            };
            let badge_col = egui::Color32::from_rgb(120, 190, 240);
            let g = p.layout_no_wrap(badge, egui::FontId::proportional(12.0), badge_col);
            p.galley(egui::pos2(after_label, y - g.size().y / 2.0), g, badge_col);
        }
        // storage + size, right-aligned
        let right = format!("{} · {}", v.storage.as_str(), state::human_size(v.size));
        p.text(
            egui::pos2(rect.right() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            right,
            egui::FontId::proportional(11.0),
            dim,
        );
        let mut tip = t!(
            "tip.version",
            seq = v.seq,
            kind = v.kind.as_str(),
            hash = &v.content_hash[..v.content_hash.len().min(8)]
        )
        .to_string();
        if let Some(&bid) = self.batch_versions.get(&v.id) {
            let count = self
                .batches
                .iter()
                .find(|b| b.id == bid)
                .map(|b| b.file_count)
                .unwrap_or(0);
            tip.push_str(&format!(
                "\n{}",
                t!("batch.badge_tip", id = bid, count = file_count_text(count))
            ));
        }
        resp.on_hover_text(tip)
    }

    fn diff_panel(&mut self, ui: &mut egui::Ui) {
        // While the Activity tab has a batch selected, the center shows the
        // WHOLE batch (PR-page style) instead of the single-file diff; the
        // Files tab (or a cleared batch) brings the old panel back unchanged.
        if self.left_tab == LeftTab::Activity && self.selected_batch.is_some() {
            self.batch_view_panel(ui);
            return;
        }
        ui.horizontal(|ui| {
            ui.label(t!("diff.compare"));
            egui::ComboBox::from_id_salt("diff_mode")
                .selected_text(self.diff_mode.label())
                .width(200.0)
                .show_ui(ui, |ui| {
                    for m in [DiffMode::Previous, DiffMode::CurrentDisk, DiffMode::AnyTwo] {
                        ui.selectable_value(&mut self.diff_mode, m, m.label());
                    }
                });
            if self.diff_mode == DiffMode::AnyTwo {
                self.any_two_pickers(ui);
            }
            // Copy diff: the ENGINE's unified diff of the current pair;
            // disabled with no renderable model (nothing picked yet) or binary.
            let can_copy_diff = self
                .diff_cache
                .as_ref()
                .is_some_and(|c| !c.model.binary && c.model.note.is_none());
            if ui
                .add_enabled(
                    can_copy_diff,
                    egui::Button::new(format!("{} {}", icons::COPY, t!("diff.copy"))),
                )
                .on_hover_text(t!("diff.copy_tip"))
                .clicked()
            {
                self.copy_unified_diff(ui.ctx());
            }
        });
        ui.separator();

        // cache: rebuild the model only when the key changes (content loads only here)
        let key = self.current_diff_key();
        let stale = match (&self.diff_cache, &key) {
            (None, Some(_)) => true,
            (Some(c), Some(k)) => c.key != *k,
            _ => false,
        };
        if stale {
            // the diff pair (or its data) changed ⇒ the caret selection refers
            // to a different model — RESET it instead of carrying carets across
            self.diff_selection.reset();
            self.diff_cache = key.map(|k| {
                // read the disk content ONCE here, for both the model and the
                // "Copy diff" action (they must describe the same bytes)
                let disk_bytes = if k.mode == DiffMode::CurrentDisk {
                    self.selected_file
                        .as_ref()
                        .and_then(|rel| std::fs::read(self.ws_file_path(self.current_ws?, rel)).ok())
                } else {
                    None
                };
                CachedDiff { key: k, model: self.build_diff_model(disk_bytes.as_deref()), disk_bytes }
            });
        }
        // split borrows: the cache is shared while the selection is mutated;
        // `Some(n)` = the raw text of n non-empty selected cells was copied
        // (Ctrl+C)
        let copied = if let Some(c) = &self.diff_cache {
            let mut sel = std::mem::take(&mut self.diff_selection);
            let copied = diff_view::show_diff(ui, &c.model, &mut sel);
            self.diff_selection = sel;
            copied
        } else {
            None
        };
        if let Some(n) = copied {
            self.set_status(t!("diff.copied", n = n).to_string(), true);
        }
    }

    /// Center panel in batch mode: every file of the selected batch as one
    /// side-by-side block (before→in-batch), stacked continuously like a PR
    /// page. The cache rebuilds when the selected batch, the data token, or
    /// the entry list changes (`CachedBatchView`); clicking an entry in the
    /// activity tab scrolls its block into view (`batch_focus_entry` →
    /// `Response::scroll_to_me`).
    fn batch_view_panel(&mut self, ui: &mut egui::Ui) {
        // Panel title, drawn BEFORE the ScrollArea — a full-height scroll would
        // squeeze anything drawn after it to zero pixels (activity_tab lesson).
        if let Some(id) = self.selected_batch {
            let count = self
                .batches
                .iter()
                .find(|b| b.id == id)
                .map(|b| b.file_count)
                .unwrap_or(0);
            ui.strong(t!("batch.entries_title", id = id, count = file_count_text(count)));
            ui.separator();
        }

        // cache: rebuild when the selected batch, the data token, or the entry
        // list changed (a revert bumps the token, so force-refresh flows end up
        // here too; a batch click clears the entries and the refill lands on
        // the next poll, which changes the fingerprint — the click frame builds
        // from the just-cleared list, so the fingerprint must gate the cache or
        // that empty build would stick forever)
        let wanted = self.selected_batch.map(|id| {
            (id, self.last_token, batch_entries_fingerprint(&self.batch_entries))
        });
        let stale = match (&self.batch_diff_cache, wanted) {
            (Some(c), Some((id, tok, fp))) => {
                c.batch_id != id || c.token != tok || c.entries_fp != fp
            }
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        };
        if stale {
            self.batch_diff_cache = wanted.map(|(id, tok, fp)| self.build_batch_view(id, tok, fp));
        }
        let Some(cache) = &self.batch_diff_cache else { return };

        // resolve the focus BEFORE drawing (the entry click may target a block
        // drawn later — `scroll_to_me` registers the target for this frame and
        // the ScrollArea consumes it after the whole pass)
        let focus_block = batch_focus_block(
            &cache.blocks,
            self.batch_focus_entry.as_ref().map(|(i, rel)| (*i, rel.as_str())),
        );

        egui::ScrollArea::vertical()
            .id_salt("batch_view")
            // hug short content, scroll only when it overflows — never reserve
            // the full panel height (activity_tab layout lesson)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (i, block) in cache.blocks.iter().enumerate() {
                    Self::draw_batch_block(ui, block, Some(i) == focus_block);
                    ui.add_space(10.0);
                }
                if cache.blocks.is_empty() {
                    ui.weak(t!("batch.no_entries"));
                }
            });
        // the scroll target is registered (egui takes it after the content
        // pass) — the one-shot flag can go now
        if focus_block.is_some() {
            self.batch_focus_entry = None;
        }
    }

    /// One batch view block: header (rel path + kind · size, highlighted while
    /// focused) + pane titles + side-by-side rows reusing `diff_view`'s grid
    /// drawing. Returns the header response so the caller can scroll to it.
    fn draw_batch_block(ui: &mut egui::Ui, block: &BatchBlock, focused: bool) -> egui::Response {
        let header = batch_block_header(ui, block, focused);
        if focused {
            header.scroll_to_me(Some(egui::Align::Min));
        }
        diff_view::draw_pane_headings(ui, &block.model.left_title, &block.model.right_title);
        if let Some(note) = diff_view::note_text(&block.model) {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(note).weak());
            });
        }
        if block.model.binary {
            return header;
        }
        let total = block.model.left.len().max(block.model.right.len());
        if total == 0 {
            return header;
        }
        let layout = diff_view::measure_layout(ui, block.model.max_chars);
        for r in 0..total {
            ui.horizontal(|ui| {
                diff_view::draw_row_pair(
                    ui,
                    &layout,
                    block.model.left.get(r),
                    block.model.right.get(r),
                    None,
                    None,
                );
            });
        }
        // ponytail: char-level drag-select/copy inside the batch view — a later
        // phase; the single-file diff (show_diff) keeps the full editor-style
        // selection today.
        header
    }

    /// Builds the batch view blocks for one batch: one block per entry (engine
    /// order = rel_path, seq). Left = the newest version with a smaller seq
    /// (none ⇒ the file was created in this batch ⇒ empty left side); right =
    /// the batched version. Content loads only here, one batch at a time;
    /// every store failure becomes a note block — never a panic.
    fn build_batch_view(&self, batch_id: BatchId, token: u64, entries_fp: u64) -> CachedBatchView {
        let mut blocks = Vec::new();
        let (Some(ws), Some(store)) = (self.current_ws, self.ro.as_ref()) else {
            return CachedBatchView { batch_id, token, entries_fp, blocks };
        };
        for e in &self.batch_entries {
            let file = self.ws_file_path(ws, &e.rel_path);
            let hist = match store.history(ws, &file) {
                Ok(h) => h,
                Err(err) => {
                    blocks.push(batch_note_block(e, t!("err.history", e = err).to_string()));
                    continue;
                }
            };
            let pre_id = pick_pre_version(&hist, e.version.seq);
            let hunks = match pre_id {
                Some(pid) => match store.diff_lines(pid, e.version.id) {
                    Ok(h) => Some(h),
                    // one side is binary — the engine refused to diff; the
                    // builder renders the binary note instead
                    Err(history::Error::BinaryContent(_)) => None,
                    Err(err) => {
                        blocks.push(batch_note_block(e, t!("err.diff", e = err).to_string()));
                        continue;
                    }
                },
                None => Some(Vec::new()), // created in this batch — no hunks needed
            };
            let pre_bytes = match pre_id {
                Some(pid) => match store.content(pid) {
                    Ok(b) => b,
                    Err(err) => {
                        blocks.push(batch_note_block(e, t!("err.content", e = err).to_string()));
                        continue;
                    }
                },
                None => Vec::new(), // no pre-batch version ⇒ empty left side
            };
            let batch_bytes = match e.version.kind {
                // deleted in this batch: the engine stores an empty marker
                // anyway; force empty so the right side can never show stale bytes
                EventKind::Delete => Vec::new(),
                _ => match store.content(e.version.id) {
                    Ok(b) => b,
                    Err(err) => {
                        blocks.push(batch_note_block(e, t!("err.content", e = err).to_string()));
                        continue;
                    }
                },
            };
            blocks.push(BatchBlock {
                rel_path: e.rel_path.clone(),
                kind: e.version.kind,
                size: e.version.size,
                model: build_batch_block(&e.version, &pre_bytes, &batch_bytes, hunks.as_deref()),
            });
        }
        CachedBatchView { batch_id, token, entries_fp, blocks }
    }

    /// Copies the unified diff of the current pair straight from the ENGINE
    /// (`diff_text` between two versions, `diff_text_content` against disk) —
    /// never a locally regenerated diff. Errors surface as an error toast.
    fn copy_unified_diff(&mut self, ctx: &egui::Context) {
        let Some(cache) = &self.diff_cache else { return };
        if cache.model.binary || cache.model.note.is_some() {
            return; // the button is disabled in these cases; defensive
        }
        let (left_id, right_id) = pick_pair(
            self.diff_mode,
            &self.versions,
            self.selected_version,
            self.diff_left_id,
            self.diff_right_id,
        );
        let (Some(store), Some(lid)) = (self.ro.as_ref(), left_id) else {
            return;
        };
        let res = match right_id {
            Some(rid) => store.diff_text(lid, rid).map_err(|e| e.to_string()),
            None => {
                // the bytes the panes were built from (empty = file deleted) —
                // copying from the cache keeps the diff identical to what is shown
                store
                    .diff_text_content(&cache.disk_bytes.clone().unwrap_or_default(), lid)
                    .map_err(|e| e.to_string())
            }
        };
        match res {
            Ok(text) => {
                ctx.copy_text(text);
                self.set_status(t!("diff.copied_diff").to_string(), true);
            }
            Err(e) => self.set_status(t!("err.diff", e = e).to_string(), false),
        }
    }

    fn any_two_pickers(&mut self, ui: &mut egui::Ui) {
        let versions = self.versions.clone();
        let label_of = |id: Option<VersionId>| -> String {
            id.and_then(|id| versions.iter().find(|v| v.id == id))
                .map(version_title)
                .unwrap_or_else(|| t!("diff.pick").to_string())
        };
        egui::ComboBox::from_id_salt("diff_left")
            .selected_text(label_of(self.diff_left_id))
            .width(170.0)
            .show_ui(ui, |ui| {
                for v in &versions {
                    if ui
                        .selectable_label(self.diff_left_id == Some(v.id), version_title(v))
                        .clicked()
                    {
                        self.diff_left_id = Some(v.id);
                    }
                }
            });
        ui.label(egui::RichText::new(icons::SWAP_HORIZ).weak());
        egui::ComboBox::from_id_salt("diff_right")
            .selected_text(label_of(self.diff_right_id))
            .width(170.0)
            .show_ui(ui, |ui| {
                for v in &versions {
                    if ui
                        .selectable_label(self.diff_right_id == Some(v.id), version_title(v))
                        .clicked()
                    {
                        self.diff_right_id = Some(v.id);
                    }
                }
            });
    }

    fn current_diff_key(&self) -> Option<DiffKey> {
        let (left, right) = pick_pair(
            self.diff_mode,
            &self.versions,
            self.selected_version,
            self.diff_left_id,
            self.diff_right_id,
        );
        Some(DiffKey { mode: self.diff_mode, left, right, token: self.last_token })
    }

    /// Builds the diff model for the current pair: left/right content + hunks →
    /// two line arrays. Content is loaded ONLY here — one pair at a time.
    /// `disk_bytes` (CurrentDisk mode) is the already-read disk content: `None`
    /// means the file is missing or the mode doesn't need it.
    fn build_diff_model(&self, disk_bytes: Option<&[u8]>) -> DiffModel {
        let (left_id, right_id) = pick_pair(
            self.diff_mode,
            &self.versions,
            self.selected_version,
            self.diff_left_id,
            self.diff_right_id,
        );
        if self.current_ws.is_none() {
            return DiffModel::empty_note(t!("diff.no_ws"));
        };
        let Some(lid) = left_id else {
            return if self.selected_file.is_some() && self.versions.is_empty() {
                DiffModel::empty_note(t!("diff.no_versions"))
            } else {
                DiffModel::empty_note(t!("diff.no_version"))
            };
        };
        let Some(store) = self.ro.as_ref() else {
            return DiffModel::empty_note(t!("diff.no_store"));
        };
        let Some(left_v) = self.versions.iter().find(|v| v.id == lid).cloned() else {
            return DiffModel::empty_note(t!("diff.version_gone"));
        };

        // left content
        let left_bytes = match store.content(lid) {
            Ok(b) => b,
            Err(e) => return DiffModel::empty_note(t!("err.content", e = e)),
        };
        let left_text = match String::from_utf8(left_bytes) {
            Ok(t) => t,
            Err(_) => return DiffModel::binary_note(),
        };
        let ver_title = version_title(&left_v);

        match right_id {
            Some(rid) => {
                let Some(right_v) = self.versions.iter().find(|v| v.id == rid).cloned() else {
                    return DiffModel::empty_note(t!("diff.version_right_gone"));
                };
                let right_bytes = match store.content(rid) {
                    Ok(b) => b,
                    Err(e) => return DiffModel::empty_note(t!("err.content", e = e)),
                };
                let right_text = match String::from_utf8(right_bytes) {
                    Ok(t) => t,
                    Err(_) => return DiffModel::binary_note(),
                };
                match store.diff_lines(lid, rid) {
                    Ok(hunks) => {
                        // old = left (the selected version), new = right
                        let base = state::split_lines(&left_text);
                        let target = state::split_lines(&right_text);
                        let (l, r) = diff_view::apply_hunks_to_sides(&base, &target, &hunks);
                                DiffModel {
                                    left_title: ver_title,
                                    right_title: version_title(&right_v),
                            left: l,
                            right: r,
                            binary: false,
                            note: None,
                            identical: hunks.is_empty(),
                            max_chars: 0,
                        }
                        .finished()
                    }
                    Err(e) => DiffModel::empty_note(t!("err.diff", e = e)),
                }
            }
            None => {
                // against current disk: left = disk (the old side of the engine's
                // `diff_*_content(disk, version)` diff), right = version (the new
                // side) — same old→new pane order as the two-versions mode, so a
                // copied diff's `-` lines are the left pane and `+` lines the right
                if self.selected_file.is_none() {
                    return DiffModel::empty_note(t!("diff.no_file"));
                }
                let Some(disk_bytes) = disk_bytes else {
                    // read failed or no file — same as the deleted-file case: the
                    // disk side is empty, the version's content still shows right
                    let ver_lines: Vec<Line> = state::split_lines(&left_text)
                        .into_iter()
                        .enumerate()
                        .map(|(i, t)| Line { text: t.to_string(), no: Some(i + 1), del: false, add: false })
                        .collect();
                    return DiffModel {
                        left_title: t!("diff.disk_title").to_string(),
                        right_title: ver_title,
                        left: Vec::new(),
                        right: ver_lines,
                        binary: false,
                        note: Some(t!("diff.file_missing").to_string()),
                        identical: false,
                        max_chars: 0,
                    }
                    .finished();
                };
                let disk_text = match String::from_utf8(disk_bytes.to_vec()) {
                    Ok(t) => t,
                    Err(_) => return DiffModel::binary_note(),
                };
                match store.diff_lines_content(disk_text.as_bytes(), lid) {
                    Ok(hunks) => {
                        // old = disk (left pane), new = version (right pane):
                        // panes follow the engine diff's old→new order, like the
                        // two-versions mode (`-` lines = left, `+` = right)
                        let disk_lines = state::split_lines(&disk_text);
                        let ver_lines = state::split_lines(&left_text);
                        let (old_side, new_side) =
                            diff_view::apply_hunks_to_sides(&disk_lines, &ver_lines, &hunks);
                        DiffModel {
                            left_title: t!("diff.disk_title").to_string(),
                            right_title: ver_title,
                            left: old_side,
                            right: new_side,
                            binary: false,
                            note: None,
                            identical: hunks.is_empty(),
                            max_chars: 0,
                        }
                        .finished()
                    }
                    Err(e) => DiffModel::empty_note(t!("err.diff", e = e)),
                }
            }
        }
    }

    fn show_modals(&mut self, ctx: &egui::Context) {
        if let Some(c) = self.confirm_revert.clone() {
            let modal = egui::Modal::new(egui::Id::unique("confirm_revert"));
            let resp = modal.show(ctx, |ui| {
                ui.set_width(380.0);
                ui.strong(t!("confirm.revert_text", rel = c.rel, seq = c.seq));
                ui.label(t!("confirm.revert_note"));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new(t!("btn.do_revert")).strong())
                        .clicked()
                    {
                        self.do_revert(c.clone());
                    }
                    if ui
                        .button(egui::RichText::new(format!(
                            "{} {}",
                            icons::CLOSE,
                            t!("btn.cancel")
                        )))
                        .clicked()
                    {
                        self.confirm_revert = None;
                    }
                });
            });
            if resp.backdrop_response.clicked() {
                self.confirm_revert = None;
            }
        }

        if let Some(b) = self.confirm_revert_batch.clone() {
            let modal = egui::Modal::new(egui::Id::unique("confirm_revert_batch"));
            let resp = modal.show(ctx, |ui| {
                ui.set_width(380.0);
                ui.strong(t!(
                    "batch.confirm_text",
                    id = b.id,
                    count = file_count_text(b.file_count)
                ));
                ui.label(t!("batch.confirm_note"));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(egui::RichText::new(t!("batch.btn_revert_all")).strong())
                        .clicked()
                    {
                        self.do_revert_batch(b.clone());
                    }
                    if ui
                        .button(egui::RichText::new(format!(
                            "{} {}",
                            icons::CLOSE,
                            t!("btn.cancel")
                        )))
                        .clicked()
                    {
                        self.confirm_revert_batch = None;
                    }
                });
            });
            if resp.backdrop_response.clicked() {
                self.confirm_revert_batch = None;
            }
        }

        if self.label_edit_open {
            let modal = egui::Modal::new(egui::Id::unique("label_edit"));
            let resp = modal.show(ctx, |ui| {
                ui.set_width(380.0);
                ui.strong(t!("confirm.label_title"));
                ui.add_space(4.0);
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.label_draft)
                        .hint_text(t!("confirm.label_hint"))
                        .desired_width(f32::INFINITY),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(egui::RichText::new(t!("btn.ok")).strong()).clicked() || enter {
                        self.submit_label();
                    }
                    if ui
                        .button(egui::RichText::new(format!(
                            "{} {}",
                            icons::CLOSE,
                            t!("btn.cancel")
                        )))
                        .clicked()
                    {
                        self.label_edit_open = false;
                    }
                });
            });
            if resp.backdrop_response.clicked() {
                self.label_edit_open = false;
            }
        }
    }
}

/// Pane title for a version: `seq N · kind · HH:MM`.
fn version_title(v: &Version) -> String {
    let dt = state::unix_micros_to_local(v.timestamp_micros, state::local_utc_offset());
    format!("seq {} · {} · {}", v.seq, v.kind.as_str(), state::hhmm(dt))
}

/// Pre-formatted file count for batch rows ("1 file" / "6 files", i18n
/// `batch.file_count*`).
fn file_count_text(count: u64) -> String {
    if count == 1 {
        t!("batch.file_count_one", count = count).to_string()
    } else {
        t!("batch.file_count", count = count).to_string()
    }
}

/// One batch-list row: relative start time · file count · label-or-kind
/// (i18n `batch.row`; the batch's label wins over its technical kind).
fn batch_row_text(b: &BatchSummary, now_micros: i64, offset: UtcOffset) -> String {
    let ago = state::format_relative_time(b.started_at, now_micros, offset);
    let kind = b.label.clone().unwrap_or_else(|| b.kind.clone());
    t!("batch.row", ago = ago, count = file_count_text(b.file_count), kind = kind).to_string()
}

/// One batch-entry row: `(rel_path, "kind · size")`.
fn batch_entry_row(e: &BatchEntry) -> (String, String) {
    (
        e.rel_path.clone(),
        format!("{} · {}", e.version.kind.as_str(), state::human_size(e.version.size)),
    )
}

/// The version the file had just BEFORE the batch: the newest entry of the
/// file's history with a seq SMALLER than the batched version's seq (seq is
/// per-file, so this is the previous chain entry regardless of batches).
/// `None` = no smaller seq ⇒ the file was created in this batch (empty left
/// side). Takes the MAXIMUM smaller seq, so it is correct for any input order
/// (the engine returns newest-first).
pub fn pick_pre_version(history: &[Version], seq: i64) -> Option<VersionId> {
    history.iter().filter(|v| v.seq < seq).max_by_key(|v| v.seq).map(|v| v.id)
}

/// A block that only reports a store read failure: the header stays (rel path,
/// kind, size), the panes carry the message instead of content.
fn batch_note_block(e: &BatchEntry, note: String) -> BatchBlock {
    BatchBlock {
        rel_path: e.rel_path.clone(),
        kind: e.version.kind,
        size: e.version.size,
        model: DiffModel::empty_note(note),
    }
}

/// Builds one batch block's side-by-side model from raw sides. `hunks == None`
/// means the engine refused to diff (`BinaryContent`) — the block shows the
/// binary note. Either side being invalid UTF-8 renders the same note. A
/// Delete version forces an EMPTY right side (the engine stores Delete as an
/// empty marker; forced here so stale bytes can never leak into the pane).
/// Zero hunks ⇒ the "identical" note (rendered via [`diff_view::note_text`]).
pub fn build_batch_block(
    v: &Version,
    pre_bytes: &[u8],
    batch_bytes: &[u8],
    hunks: Option<&[Hunk]>,
) -> DiffModel {
    let left_title = t!("batch.view_left_title").to_string();
    let right_title = t!("batch.view_right_title", seq = v.seq).to_string();
    let binary = |binary: bool| DiffModel {
        left_title: left_title.clone(),
        right_title: right_title.clone(),
        left: Vec::new(),
        right: Vec::new(),
        binary,
        note: Some(t!("diff.binary").to_string()),
        identical: false,
        max_chars: 0,
    };
    let Some(hunks) = hunks else { return binary(true) };
    let Ok(pre_text) = std::str::from_utf8(pre_bytes) else { return binary(true) };
    let batch_bytes: &[u8] = if v.kind == EventKind::Delete { &[] } else { batch_bytes };
    let Ok(batch_text) = std::str::from_utf8(batch_bytes) else { return binary(true) };
    let base = state::split_lines(pre_text);
    let target = state::split_lines(batch_text);
    let (left, right) = diff_view::apply_hunks_to_sides(&base, &target, hunks);
    DiffModel {
        left_title,
        right_title,
        left,
        right,
        binary: false,
        note: None,
        identical: hunks.is_empty(),
        max_chars: 0,
    }
    .finished()
}

/// Index of the block a focus target maps to. The focus carries the clicked
/// entry's INDEX (blocks map 1:1 onto `batch_entries`, same order), so a file
/// changed twice in one batch — two entries, two blocks, one rel_path — still
/// scrolls to the block the user clicked, not the first with that path. The
/// rel_path rides along as a stale-index guard: entries reloaded between the
/// click and the draw (GC, batch switch) must not scroll to a mismatched
/// block, so a path mismatch ⇒ `None`. `None` focus ⇒ `None`.
pub fn batch_focus_block(blocks: &[BatchBlock], focus: Option<(usize, &str)>) -> Option<usize> {
    let (idx, rel) = focus?;
    blocks.get(idx).is_some_and(|b| b.rel_path == rel).then_some(idx)
}

/// Cheap fingerprint of the `batch_entries` list the batch view builds from:
/// entry count + sum of the version ids (u64-wrapping). Stable across polls
/// (the query orders by rel_path, seq and history is immutable), so it only
/// changes when the list actually changes — which is what the cache key needs.
pub fn batch_entries_fingerprint(entries: &[BatchEntry]) -> u64 {
    let mut fp = entries.len() as u64;
    for e in entries {
        fp = fp.wrapping_add(e.version.id as u64);
    }
    fp
}

/// One batch-view block header: rel path (left) + kind · size (right), tinted
/// while this block is the focus target. The response is returned so the
/// caller can scroll it into view (`Response::scroll_to_me`).
fn batch_block_header(ui: &mut egui::Ui, block: &BatchBlock, focused: bool) -> egui::Response {
    let h = 26.0;
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::hover());
    if focused {
        ui.painter()
            .rect_filled(rect, 4.0, ui.visuals().selection.bg_fill.gamma_multiply(0.35));
    }
    let p = ui.painter_at(rect);
    p.text(
        egui::pos2(rect.left() + 4.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        block.rel_path.as_str(),
        egui::FontId::proportional(14.0),
        egui::Color32::from_rgb(240, 240, 240),
    );
    p.text(
        egui::pos2(rect.right() - 6.0, rect.center().y),
        egui::Align2::RIGHT_CENTER,
        format!("{} · {}", block.kind.as_str(), state::human_size(block.size)),
        egui::FontId::proportional(11.0),
        egui::Color32::from_rgb(150, 150, 150),
    );
    resp
}

/// Toast text for a whole-batch revert: the [`RevertReport`] counters plus the
/// per-file error count (`batch.report*`).
fn format_revert_report(r: &RevertReport) -> String {
    let base = t!(
        "batch.report",
        reverted = r.reverted,
        restored = r.restored,
        deleted = r.deleted,
        skipped = r.skipped
    )
    .to_string();
    if r.errors.is_empty() {
        base
    } else {
        format!("{base} · {}", t!("batch.report_errors", n = r.errors.len()))
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // poll every POLL_INTERVAL (not every frame); forced ops set next_poll = None
        match self.next_poll {
            Some(t) if Instant::now() < t => {}
            _ => {
                self.poll();
                self.next_poll = Some(Instant::now() + POLL_INTERVAL);
            }
        }
        ui.ctx().request_repaint_after(POLL_INTERVAL);
        if self.ro.is_none() {
            self.store_picker_screen(ui);
            return;
        }
        self.update_ui(ui);
    }
}

// ---------------------------------------------------------------------------
// Tests — pair selection + workspace resolution (the pure part of app)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use history::Storage;

    fn ver(id: i64, seq: i64) -> Version {
        Version {
            id,
            file_id: 1,
            seq,
            timestamp_micros: 0,
            kind: EventKind::External,
            label: None,
            storage: Storage::Full,
            base_seq: None,
            size: 0,
            content_hash: String::new(),
        }
    }

    #[test]
    fn pick_pair_previous_mode() {
        let versions = vec![ver(30, 3), ver(20, 2), ver(10, 1)];
        // mid-chain: "previous" = the older version right after it
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(20), None, None),
            (Some(20), Some(10))
        );
        // newest: previous = the second version
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(30), None, None),
            (Some(30), Some(20))
        );
        // first version: nothing before it
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(10), None, None),
            (Some(10), None)
        );
        // nothing selected
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, None, None, None),
            (None, None)
        );
    }

    #[test]
    fn pick_pair_disk_and_anytwo() {
        let versions = vec![ver(30, 3), ver(20, 2), ver(10, 1)];
        // CurrentDisk: right = None (marks "against disk")
        assert_eq!(
            pick_pair(DiffMode::CurrentDisk, &versions, Some(20), None, None),
            (Some(20), None)
        );
        // AnyTwo honors the user's picks
        assert_eq!(
            pick_pair(DiffMode::AnyTwo, &versions, Some(20), Some(30), Some(10)),
            (Some(30), Some(10))
        );
        // AnyTwo: left defaults to the selected version, right to the version before it
        assert_eq!(
            pick_pair(DiffMode::AnyTwo, &versions, Some(30), None, None),
            (Some(30), Some(20))
        );
    }

    #[test]
    fn resolve_ws_prefix() {
        let ws = vec![(1i64, PathBuf::from(r"D:\proj"))];
        let f = Path::new(r"D:\proj\src\main.rs");
        assert_eq!(
            resolve_ws_and_rel(&ws, f),
            Some((1, "src/main.rs".to_string()))
        );
        // ASCII case-insensitive
        let f2 = Path::new(r"d:\PROJ\A.txt");
        assert_eq!(resolve_ws_and_rel(&ws, f2), Some((1, "A.txt".to_string())));
        // file outside the workspace → None
        assert_eq!(resolve_ws_and_rel(&ws, Path::new(r"D:\other\a.rs")), None);
        // the workspace root itself → None (must be strictly inside)
        assert_eq!(resolve_ws_and_rel(&ws, Path::new(r"D:\proj")), None);
        // same-name prefix but a different directory (D:\project ≠ D:\proj)
        assert_eq!(
            resolve_ws_and_rel(&ws, Path::new(r"D:\project\a.rs")),
            None
        );
    }

    // ----- batch tab formatting (plan M6 §3.6 — pure part of the UI) -----

    /// Builds a BatchSummary fixture: started = ended = `started`, one version per file.
    fn batch(
        id: i64,
        started: i64,
        files: u64,
        kind: &str,
        label: Option<&str>,
    ) -> BatchSummary {
        BatchSummary {
            id,
            ws_id: 1,
            started_at: started,
            ended_at: started,
            kind: kind.to_string(),
            label: label.map(String::from),
            version_count: files,
            file_count: files,
        }
    }

    #[test]
    fn batch_row_and_file_count_formatting() {
        let _guard = crate::state::LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let now = 1_800_000_000_000_000i64;
        let utc = UtcOffset::UTC;
        // a label wins over the technical kind
        let b = batch(7, now - 5 * 60_000_000, 6, "auto", Some("deploy"));
        assert_eq!(batch_row_text(&b, now, utc), "5 minutes ago · 6 files · deploy");
        // no label → kind; singular file count
        let b2 = batch(8, now - 30_000_000, 1, "auto", None);
        assert_eq!(batch_row_text(&b2, now, utc), "just now · 1 file · auto");
        assert_eq!(file_count_text(1), "1 file");
        assert_eq!(file_count_text(6), "6 files");
        // relative time buckets are reused from format_relative_time
        let b3 = batch(9, now - 3 * 3_600_000_000, 2, "manual", None);
        assert_eq!(batch_row_text(&b3, now, utc), "3 hours ago · 2 files · manual");
        // vi locale
        rust_i18n::set_locale("vi");
        assert_eq!(batch_row_text(&b, now, utc), "5 phút trước · 6 file · deploy");
        assert_eq!(batch_row_text(&b2, now, utc), "vừa xong · 1 file · auto");
        rust_i18n::set_locale("en"); // restore the default for other tests
    }

    #[test]
    fn batch_entry_row_is_rel_path_plus_kind_and_size() {
        let mut v = ver(10, 1);
        v.kind = EventKind::External;
        v.size = 1536;
        let e = BatchEntry { rel_path: "src/main.rs".to_string(), version: v };
        assert_eq!(batch_entry_row(&e), ("src/main.rs".to_string(), "external · 1.5 KB".to_string()));
    }

    #[test]
    fn revert_report_toast_text() {
        let _guard = crate::state::LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let mut r = RevertReport {
            reverted: 3,
            restored: 1,
            deleted: 2,
            skipped: 4,
            errors: Vec::new(),
        };
        assert_eq!(
            format_revert_report(&r),
            "batch reverted — 3 reverted · 1 restored · 2 deleted · 4 skipped"
        );
        // a failing file appends the error count to the same toast
        r.errors.push(("a.txt".to_string(), "boom".to_string()));
        assert_eq!(
            format_revert_report(&r),
            "batch reverted — 3 reverted · 1 restored · 2 deleted · 4 skipped · 1 file(s) failed"
        );
        rust_i18n::set_locale("vi");
        assert_eq!(
            format_revert_report(&r),
            "đã revert cả đợt — 3 revert · 1 khôi phục · 2 xóa · 4 bỏ qua · 1 file lỗi"
        );
        rust_i18n::set_locale("en"); // restore the default for other tests
    }

    #[test]
    fn revert_report_error_count_matches_entries() {
        let _guard = crate::state::LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let r = RevertReport {
            errors: vec![
                ("a.txt".to_string(), "x".to_string()),
                ("b.txt".to_string(), "y".to_string()),
            ],
            ..Default::default()
        };
        assert!(format_revert_report(&r).ends_with("2 file(s) failed"));
        rust_i18n::set_locale("en"); // restore the default for other tests
    }

    // ----- batch view (pure part: pre-batch pick, block building, focus map) -----

    fn hunk(old_start: u64, old_lines: u64, new_start: u64, new_lines: u64, new_text: &str) -> Hunk {
        Hunk {
            old_start,
            old_lines,
            new_start,
            new_lines,
            new_text: new_text.to_string(),
        }
    }

    fn texts(model: &DiffModel, side: fn(&DiffModel) -> &Vec<Line>) -> Vec<String> {
        side(model).iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn pick_pre_version_picks_largest_smaller_seq() {
        // engine order: newest first
        let hist = vec![ver(30, 5), ver(20, 3), ver(10, 1)];
        // the newest seq below the batched one — even with gaps in the chain
        assert_eq!(pick_pre_version(&hist, 6), Some(30));
        assert_eq!(pick_pre_version(&hist, 4), Some(20));
        assert_eq!(pick_pre_version(&hist, 3), Some(10), "only seq 1 is below 3 here");
        // nothing smaller ⇒ created in this batch
        assert_eq!(pick_pre_version(&hist, 1), None);
        // unsorted input still works (maximum smaller seq, not first match)
        let unsorted = vec![ver(10, 1), ver(30, 5), ver(20, 3)];
        assert_eq!(pick_pre_version(&unsorted, 4), Some(20));
        assert_eq!(pick_pre_version(&unsorted, 1), None);
    }

    #[test]
    fn batch_block_created_in_batch_has_empty_left() {
        let _guard = crate::state::LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let v = ver(40, 4);
        // created in this batch: no pre-batch version — empty left, hunks from empty
        let hunks = [hunk(1, 0, 1, 2, "hello\nworld\n")];
        let m = build_batch_block(&v, &[], b"hello\nworld\n", Some(&hunks));
        assert_eq!(m.left_title, "Before batch");
        assert_eq!(m.right_title, "In batch (seq 4)");
        assert!(!m.binary);
        assert!(!m.identical);
        assert!(m.left.iter().all(|l| l.no.is_none()), "left rows are fillers (no pre-batch lines)");
        assert_eq!(texts(&m, |m| &m.right), vec!["hello\n", "world\n"]);
        assert_eq!(m.right.iter().filter(|l| l.add).count(), 2);
    }

    #[test]
    fn batch_block_deleted_in_batch_has_empty_right() {
        let mut v = ver(30, 3);
        v.kind = EventKind::Delete;
        let hunks = [hunk(1, 3, 1, 0, "")];
        let m = build_batch_block(&v, b"a\nb\nc\n", &[], Some(&hunks));
        // every left row is a deletion, the right side carries no content
        assert!(m.left.iter().all(|l| l.del));
        assert!(m.right.iter().all(|l| l.text.is_empty()), "deleted in batch ⇒ empty right");
        assert!(!m.identical);
        // even stale bytes passed by a caller are forced away from the right pane
        let m = build_batch_block(&v, b"a\n", b"STALE", Some(&hunks));
        assert!(m.right.iter().all(|l| l.text.is_empty()));
    }

    #[test]
    fn batch_block_binary_shows_note_only() {
        let v = ver(50, 5);
        // non-UTF8 on the batch side — no rows, only the binary note
        let m = build_batch_block(&v, b"text\n", &[0x80, 0x81], Some(&[]));
        assert!(m.binary);
        assert!(m.note.is_some());
        assert!(m.left.is_empty() && m.right.is_empty());
        // non-UTF8 on the pre side renders the same note
        let m = build_batch_block(&v, &[0xff], b"text\n", Some(&[]));
        assert!(m.binary);
        // the engine refusing to diff (BinaryContent) → no hunks ⇒ same note
        let m = build_batch_block(&v, b"\xff", b"\x80", None);
        assert!(m.binary);
        assert!(m.note.is_some());
    }

    #[test]
    fn batch_block_identical_shares_note_text_with_show_diff() {
        let _guard = crate::state::LOCALE_TEST_LOCK.lock().unwrap();
        rust_i18n::set_locale("en");
        let v = ver(60, 6);
        let m = build_batch_block(&v, b"a\nb\n", b"a\nb\n", Some(&[]));
        assert!(m.identical);
        assert!(!m.binary);
        assert_eq!(m.left.len(), 2);
        assert_eq!(m.right.len(), 2);
        assert!(m.left.iter().chain(m.right.iter()).all(|l| !l.del && !l.add));
        // zero hunks ⇒ the same "identical" note the single-file diff shows
        assert_eq!(
            diff_view::note_text(&m),
            Some("Both sides are identical.".to_string())
        );
        rust_i18n::set_locale("en");
    }

    /// Block fixture: rel path + a default model.
    fn block(rel: &str) -> BatchBlock {
        BatchBlock {
            rel_path: rel.to_string(),
            kind: EventKind::External,
            size: 0,
            model: DiffModel::default(),
        }
    }

    #[test]
    fn batch_focus_block_maps_entry_index_to_its_block() {
        let blocks = vec![block("a.txt"), block("b.txt"), block("b.txt"), block("c.txt")];
        // duplicate rel_path: the index picks the block the user clicked, not
        // the first one carrying that path
        assert_eq!(batch_focus_block(&blocks, Some((0, "a.txt"))), Some(0));
        assert_eq!(batch_focus_block(&blocks, Some((1, "b.txt"))), Some(1));
        assert_eq!(
            batch_focus_block(&blocks, Some((2, "b.txt"))),
            Some(2),
            "second entry of a twice-changed file ⇒ second block"
        );
        assert_eq!(batch_focus_block(&blocks, Some((3, "c.txt"))), Some(3));
        assert_eq!(batch_focus_block(&blocks, None), None, "no focus ⇒ no block");
    }

    #[test]
    fn batch_focus_block_drops_a_stale_index() {
        let blocks = vec![block("a.txt"), block("b.txt"), block("b.txt")];
        // entries reloaded between click and draw (GC / batch switch): a
        // mismatched path or an out-of-bounds index scrolls nowhere
        assert_eq!(
            batch_focus_block(&blocks, Some((2, "c.txt"))),
            None,
            "path mismatch ⇒ None"
        );
        assert_eq!(
            batch_focus_block(&blocks, Some((3, "b.txt"))),
            None,
            "index past the end ⇒ None"
        );
        assert_eq!(batch_focus_block(&blocks, Some((0, "gone.txt"))), None);
    }

    #[test]
    fn batch_entries_fingerprint_separates_cleared_from_loaded() {
        assert_eq!(batch_entries_fingerprint(&[]), 0, "cleared/pending list");
        let mk = |rel: &str, id: i64| BatchEntry { rel_path: rel.into(), version: ver(id, 1) };
        let one = vec![mk("a.txt", 7)];
        let two = vec![mk("a.txt", 7), mk("b.txt", 11)];
        assert_eq!(
            batch_entries_fingerprint(&two),
            batch_entries_fingerprint(&two.clone()),
            "stable across polls (same list ⇒ same fingerprint)"
        );
        assert_ne!(
            batch_entries_fingerprint(&one),
            batch_entries_fingerprint(&two),
            "the click-frame refill (cleared → loaded) must change the fingerprint"
        );
        assert_ne!(
            batch_entries_fingerprint(&two),
            batch_entries_fingerprint(&[mk("a.txt", 7), mk("b.txt", 12)]),
            "different version ids ⇒ different fingerprint"
        );
    }

    #[test]
    fn batch_view_i18n_keys_en_vi() {
        // explicit-locale lookups — process-global state untouched
        assert_eq!(t!("batch.view_left_title", locale = "en"), "Before batch");
        assert_eq!(t!("batch.view_left_title", locale = "vi"), "Trước đợt");
        assert_eq!(t!("batch.view_right_title", locale = "en", seq = 7), "In batch (seq 7)");
        assert_eq!(t!("batch.view_right_title", locale = "vi", seq = 7), "Trong đợt (seq 7)");
    }
}
