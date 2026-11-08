//! App GUI — bố cục 3 vùng (workspace+file trái · timeline phải · diff giữa),
//! poll dữ liệu qua engine mỗi 1s, revert/label qua connection write riêng.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eframe::egui;
use history::{EventKind, FileSummary, LocalHistory, Version, VersionId, WorkspaceId};
use rust_i18n::t;
use time::{Date, OffsetDateTime, UtcOffset};

use crate::diff_view::{self, DiffModel, Line};
use crate::icons;
use crate::state::{self, GuiState, LangPref};

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Chế độ so sánh của diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    /// Version đang chọn ↔ version ngay trước nó.
    Previous,
    /// Version đang chọn ↔ nội dung hiện tại trên disk.
    CurrentDisk,
    /// Hai version bất kỳ do user chọn.
    AnyTwo,
}

impl DiffMode {
    /// Nhãn hiển thị của chế độ so sánh (qua i18n `diff.mode_*`).
    fn label(self) -> String {
        match self {
            DiffMode::Previous => t!("diff.mode_prev").to_string(),
            DiffMode::CurrentDisk => t!("diff.mode_disk").to_string(),
            DiffMode::AnyTwo => t!("diff.mode_any").to_string(),
        }
    }
}

/// Chọn cặp version (trái, phải) cho diff. `versions` sắp mới nhất trước.
/// `custom_left/custom_right` là lựa chọn của user trong chế độ AnyTwo;
/// chế độ CurrentDisk trả phải = None (đánh dấu "so với disk").
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
            // phải mặc định là version ngay trước trái (cũ hơn)
            let right = custom_right.or_else(|| {
                let li = left.and_then(|id| versions.iter().position(|v| v.id == id));
                li.and_then(|i| versions.get(i + 1)).map(|v| v.id)
            });
            (left, right)
        }
    }
}

/// Tìm workspace chứa `file` (so sánh từng thành phần path, không phân biệt hoa thường
/// ASCII) → (ws_id, rel_path phân tách '/'). TODO: case-fold Unicode cho path tiếng Việt.
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

/// Thông báo hiện ở status bar (giữ lại đến khi có thông báo mới).
#[derive(Debug, Clone)]
pub struct Status {
    pub text: String,
    pub ok: bool,
}

/// Modal xác nhận revert.
#[derive(Debug, Clone)]
pub struct ConfirmRevert {
    pub id: VersionId,
    pub seq: i64,
    pub rel: String,
}

/// Khóa của diff đang hiển thị — đổi khóa ⇒ build lại model (load content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffKey {
    pub mode: DiffMode,
    pub left: Option<VersionId>,
    /// None trong chế độ CurrentDisk = so với disk; None ở AnyTwo = chưa chọn đủ.
    pub right: Option<VersionId>,
    /// Token dữ liệu (tổng version) — thay đổi ⇒ disk/cặp có thể đã đổi.
    pub token: u64,
}

#[derive(Debug)]
pub struct CachedDiff {
    pub key: DiffKey,
    pub model: DiffModel,
}

pub struct App {
    pub store_root: PathBuf,
    pub state: GuiState,
    pub state_path: PathBuf,
    /// Connection read-only (None = chưa mở được — màn hình chọn store).
    pub ro: Option<LocalHistory>,
    pub workspaces: Vec<(WorkspaceId, PathBuf)>,
    pub current_ws: Option<WorkspaceId>,
    pub files: Vec<FileSummary>,
    /// rel_path ('/') của file đang chọn.
    pub selected_file: Option<String>,
    pub versions: Vec<Version>,
    pub selected_version: Option<VersionId>,
    // filter timeline
    pub kind_filter: Option<EventKind>,
    pub label_filter: String,
    pub file_search: String,
    // diff
    pub diff_mode: DiffMode,
    pub diff_left_id: Option<VersionId>,
    pub diff_right_id: Option<VersionId>,
    pub diff_cache: Option<CachedDiff>,
    // thao tác
    pub confirm_revert: Option<ConfirmRevert>,
    pub label_edit_open: bool,
    pub label_draft: String,
    pub status: Status,
    pub last_poll: Option<OffsetDateTime>,
    /// Lần poll kế tiếp (None = poll ngay frame tới) — poll không chạy mỗi frame.
    pub next_poll: Option<Instant>,
    pub last_token: u64,
    pub force_refresh: bool,
    // --file
    pub pending_file_path: Option<PathBuf>,
    pub pending_file_rel: Option<String>,
    pub pending_announced: bool,
}

impl App {
    pub fn new(default_root: PathBuf, initial_file: Option<PathBuf>) -> Self {
        let mut state = state::load_state(&state::state_path(&default_root));
        let store_root = state
            .last_store_root
            .as_ref()
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| default_root.clone());
        if store_root != default_root {
            // state của store đã chọn có thể có last_ws_root cụ thể hơn
            let st2 = state::load_state(&state::state_path(&store_root));
            if state.last_ws_root.is_none() {
                state.last_ws_root = st2.last_ws_root;
            }
        }
        let state_path = state::state_path(&store_root);
        state.last_store_root = Some(store_root.to_string_lossy().to_string());
        // Đặt locale TRƯỚC khi tạo chuỗi đầu tiên ("status.ready") — ngôn ngữ của frame
        // đầu phải theo state; đổi sau này qua dropdown (set_lang) cũng đủ vì egui vẽ lại.
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
            kind_filter: None,
            label_filter: String::new(),
            file_search: String::new(),
            diff_mode: DiffMode::Previous,
            diff_left_id: None,
            diff_right_id: None,
            diff_cache: None,
            confirm_revert: None,
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

    /// Đổi ngôn ngữ runtime: ghi state + set_locale toàn cục (frame kế hiển thị ngôn ngữ
    /// mới — egui vẽ lại liên tục theo poll).
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

    /// Đường dẫn file tuyệt đối từ ws + rel_path.
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
    // Poll dữ liệu (connection read-only)
    // -----------------------------------------------------------------------

    fn poll(&mut self) {
        self.last_poll = Some(state::local_now());
        if self.ro.is_none() {
            match LocalHistory::open_read_only(&self.store_root) {
                Ok(s) => {
                    self.ro = Some(s);
                    self.force_refresh = true;
                }
                Err(_) => return, // chưa có store — UI hiển thị màn hình chọn store
            }
        }

        // workspaces (query rẻ, gọi mỗi poll)
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

        // --file: resolve workspace + rel_path (làm một lần)
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

        // tự chọn workspace lần đầu: last_ws_root → workspace đầu tiên
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

        // danh sách file + token "rẻ" nhận biết dữ liệu đổi: số file + tổng version
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
    }

    /// Dữ liệu vừa đổi (poll thấy token mới / force refresh).
    fn on_data_changed(&mut self) {
        // --file: chọn file khi nó xuất hiện trong list
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

    /// Nạp lại versions của file đang chọn (KHÔNG load content — content chỉ load khi
    /// build cặp diff).
    fn refresh_versions(&mut self) {
        let Some(ws) = self.current_ws else { return };
        let Some(rel) = self.selected_file.clone() else {
            self.versions.clear();
            self.selected_version = None;
            return;
        };
        let Some(store) = self.ro.as_ref() else { return };
        let file = self.ws_file_path(ws, &rel);
        match store.history(ws, &file) {
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
    }

    // -----------------------------------------------------------------------
    // Chuyển đổi selection / store
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
        self.diff_cache = None;
        self.last_token = 0;
        if let Some((_, root)) = self.workspaces.iter().find(|(wid, _)| *wid == id) {
            self.state.last_ws_root = Some(root.to_string_lossy().to_string());
        }
        self.save_state();
        self.force_refresh = true;
        self.next_poll = None; // poll ngay frame tới
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
        self.diff_cache = None;
        self.last_token = 0;
        self.force_refresh = true;
        self.next_poll = None;
        self.save_state();
    }

    // -----------------------------------------------------------------------
    // Thao tác ghi (connection write riêng, mở từng lần)
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
                self.poll(); // nạp lại versions ngay để tìm pre_revert
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
        // Thứ tự: bottom → trái → phải → CentralPanel CUỐI (quy ước egui).
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
                // dropdown ngôn ngữ — góc phải cùng, đổi runtime (ghi state + set_locale)
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
                                // vẽ lại ngay để mọi panel chuyển sang ngôn ngữ mới
                                ui.ctx().request_repaint();
                            }
                        }
                    });
                ui.separator();
                // toast: icon kiểm chéo/lỗi (icon font) + text thông báo
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

    /// Row file: rel_path + badge số version + thời gian sửa cuối.
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
        // chấm màu kind mới nhất ở góc phải
        p.circle_filled(egui::pos2(rect.right() - 10.0, rect.top() + 10.0), 3.5, state::kind_color(f.last_kind));
        resp.on_hover_text(f.rel_path.as_str())
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

        // filter kind + label
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

        // thao tác trên version đang chọn
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

    /// Row version: giờ · badge kind · label · storage/size.
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
        // giờ
        p.text(
            egui::pos2(rect.left() + 6.0, y),
            egui::Align2::LEFT_CENTER,
            state::hhmm(dt),
            egui::FontId::monospace(12.0),
            dim,
        );
        // badge kind (nền nhạt + chữ màu kind)
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
        // label (italic, màu tím nhạt)
        if let Some(label) = &v.label {
            let label_col = egui::Color32::from_rgb(200, 180, 240);
            let g = p.layout_no_wrap(label.clone(), egui::FontId::proportional(12.5), label_col);
            p.galley(egui::pos2(bx + bw + 8.0, y - g.size().y / 2.0), g, label_col);
        }
        // storage + size bên phải
        let right = format!("{} · {}", v.storage.as_str(), state::human_size(v.size));
        p.text(
            egui::pos2(rect.right() - 6.0, y),
            egui::Align2::RIGHT_CENTER,
            right,
            egui::FontId::proportional(11.0),
            dim,
        );
        let tip = t!(
            "tip.version",
            seq = v.seq,
            kind = v.kind.as_str(),
            hash = &v.content_hash[..v.content_hash.len().min(8)]
        );
        resp.on_hover_text(tip.to_string())
    }

    fn diff_panel(&mut self, ui: &mut egui::Ui) {
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
        });
        ui.separator();

        // cache: build lại model khi khóa đổi (content chỉ load ở đây)
        let key = self.current_diff_key();
        let stale = match (&self.diff_cache, &key) {
            (None, Some(_)) => true,
            (Some(c), Some(k)) => c.key != *k,
            _ => false,
        };
        if stale {
            self.diff_cache =
                key.map(|k| CachedDiff { key: k, model: self.build_diff_model() });
        }
        if let Some(c) = &self.diff_cache {
            diff_view::show_diff(ui, &c.model);
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

    /// Build model diff cho cặp hiện tại: content trái/phải + hunks → 2 mảng dòng.
    /// Content CHỈ load ở đây — một cặp tại một thời điểm.
    fn build_diff_model(&self) -> DiffModel {
        let (left_id, right_id) = pick_pair(
            self.diff_mode,
            &self.versions,
            self.selected_version,
            self.diff_left_id,
            self.diff_right_id,
        );
        let Some(ws) = self.current_ws else {
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

        // content trái
        let left_bytes = match store.content(lid) {
            Ok(b) => b,
            Err(e) => return DiffModel::empty_note(t!("err.content", e = e)),
        };
        let left_text = match String::from_utf8(left_bytes) {
            Ok(t) => t,
            Err(_) => return DiffModel::binary_note(),
        };
        let left_title = version_title(&left_v);

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
                        // old = trái (version đang chọn), new = phải
                        let base = state::split_lines(&left_text);
                        let target = state::split_lines(&right_text);
                        let (l, r) = diff_view::apply_hunks_to_sides(&base, &target, &hunks);
                        DiffModel {
                            left_title,
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
                // so với disk hiện tại: trái = version, phải = disk
                let Some(rel) = self.selected_file.clone() else {
                    return DiffModel::empty_note(t!("diff.no_file"));
                };
                let path = self.ws_file_path(ws, &rel);
                match std::fs::read(&path) {
                    Err(_) => {
                        // file đã bị xóa — vẫn hiển thị nội dung version bên trái
                        let left_lines: Vec<Line> = state::split_lines(&left_text)
                            .into_iter()
                            .enumerate()
                            .map(|(i, t)| Line { text: t.to_string(), no: Some(i + 1), del: false, add: false })
                            .collect();
                        DiffModel {
                            left_title,
                            right_title: t!("diff.disk_title").to_string(),
                            left: left_lines,
                            right: Vec::new(),
                            binary: false,
                            note: Some(t!("diff.file_missing").to_string()),
                            identical: false,
                            max_chars: 0,
                        }
                        .finished()
                    }
                    Ok(disk_bytes) => {
                        let disk_text = match String::from_utf8(disk_bytes) {
                            Ok(t) => t,
                            Err(_) => return DiffModel::binary_note(),
                        };
                        match store.diff_lines_content(disk_text.as_bytes(), lid) {
                            Ok(hunks) => {
                                // old = disk, new = version ⇒ trái = new side, phải = old side
                                let disk_lines = state::split_lines(&disk_text);
                                let ver_lines = state::split_lines(&left_text);
                                let (old_side, new_side) =
                                    diff_view::apply_hunks_to_sides(&disk_lines, &ver_lines, &hunks);
                                DiffModel {
                                    left_title,
                                    right_title: t!("diff.disk_title").to_string(),
                                    left: new_side,
                                    right: old_side,
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
        }
    }

    fn show_modals(&mut self, ctx: &egui::Context) {
        // Modal xác nhận revert
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

        // Modal gắn nhãn
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

/// Tiêu đề pane: "seq N · kind · HH:MM".
fn version_title(v: &Version) -> String {
    let dt = state::unix_micros_to_local(v.timestamp_micros, state::local_utc_offset());
    format!("seq {} · {} · {}", v.seq, v.kind.as_str(), state::hhmm(dt))
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // poll mỗi POLL_INTERVAL (không poll mỗi frame) — force op đặt next_poll = None
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
// Tests — logic chọn cặp + resolve workspace (phần thuần của app)
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
        // giữa chain: "version trước" = version cũ hơn ngay sau nó
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(20), None, None),
            (Some(20), Some(10))
        );
        // mới nhất: trước = version thứ 2
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(30), None, None),
            (Some(30), Some(20))
        );
        // version đầu tiên: không có version trước
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, Some(10), None, None),
            (Some(10), None)
        );
        // chưa chọn gì
        assert_eq!(
            pick_pair(DiffMode::Previous, &versions, None, None, None),
            (None, None)
        );
    }

    #[test]
    fn pick_pair_disk_and_anytwo() {
        let versions = vec![ver(30, 3), ver(20, 2), ver(10, 1)];
        // disk: phải = None (đánh dấu so với disk)
        assert_eq!(
            pick_pair(DiffMode::CurrentDisk, &versions, Some(20), None, None),
            (Some(20), None)
        );
        // AnyTwo dùng lựa chọn của user
        assert_eq!(
            pick_pair(DiffMode::AnyTwo, &versions, Some(20), Some(30), Some(10)),
            (Some(30), Some(10))
        );
        // AnyTwo: trái mặc định = version đang chọn, phải mặc định = version trước nó
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
        // không phân biệt hoa thường (ASCII)
        let f2 = Path::new(r"d:\PROJ\A.txt");
        assert_eq!(resolve_ws_and_rel(&ws, f2), Some((1, "A.txt".to_string())));
        // file ngoài ws → None
        assert_eq!(resolve_ws_and_rel(&ws, Path::new(r"D:\other\a.rs")), None);
        // chính ws root → None (phải nằm BÊN TRONG)
        assert_eq!(resolve_ws_and_rel(&ws, Path::new(r"D:\proj")), None);
        // prefix trùng tên nhưng là thư mục khác (D:\project ≠ D:\proj)
        assert_eq!(
            resolve_ws_and_rel(&ws, Path::new(r"D:\project\a.rs")),
            None
        );
    }
}
