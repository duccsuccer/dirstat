//! Application state, the scan lifecycle and the top bar.

use eframe::egui;
use crate::actions::{self, DeleteOutcome};
use crate::model::{NodeId, Tree};
use crate::scan::{self, ScanEvent, ScanHandle, ScanMode, ScanTarget};
use crate::{ui_legend, ui_tree, ui_treemap::{self, TreemapCache}};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Instant;

pub struct AppState {
    pub tree: Option<Tree>,
    pub selected: Option<NodeId>,
    pub expanded: HashSet<NodeId>,
    pub mode: Option<ScanMode>,
    pub progress: (u64, u64),
    pub scan: Option<ScanHandle>,
    pub rx: Receiver<ScanEvent>,
    pub target: Option<ScanTarget>,
    pub error: Option<String>,
    pub skipped: u64,
    pub treemap_cache: Option<TreemapCache>,
    /// Set by a treemap click; the tree pane scrolls to `selected` once, then clears it.
    pub scroll_to_selected: bool,
    /// Item whose delete confirmation dialog is open.
    pub pending_delete: Option<NodeId>,
    /// Item whose permanent-delete confirmation dialog is open.
    pub pending_permanent: Option<NodeId>,
    /// Why the last delete did not happen; shown in a dialog until dismissed.
    pub delete_notice: Option<String>,
    /// (total, free) bytes of the volume holding the scan target, read when the scan starts.
    pub disk: Option<(u64, u64)>,
    pub scan_started: Option<Instant>,
    /// How long the last finished scan took.
    pub scan_secs: Option<f32>,
    /// File type names the shell gave for each extension, so it is asked once per extension.
    pub type_names: HashMap<String, String>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let (_, rx) = channel();
        AppState {
            tree: None,
            selected: None,
            expanded: HashSet::new(),
            mode: None,
            progress: (0, 0),
            scan: None,
            rx,
            target: None,
            error: None,
            skipped: 0,
            treemap_cache: None,
            scroll_to_selected: false,
            pending_delete: None,
            pending_permanent: None,
            delete_notice: None,
            disk: None,
            scan_started: None,
            scan_secs: None,
            type_names: HashMap::new(),
        }
    }

    /// Cancels the running scan and gives the next scan a fresh channel, so events from older scans cannot arrive.
    pub(crate) fn install_channel(&mut self) -> Sender<ScanEvent> {
        self.scan = None;
        let (tx, rx) = channel();
        self.rx = rx;
        self.progress = (0, 0);
        self.error = None;
        tx
    }

    pub fn begin_scan(&mut self, target: ScanTarget, ctx: &egui::Context) {
        let tx = self.install_channel();
        self.disk = disk_space(&match &target {
            ScanTarget::Volume(letter) => PathBuf::from(format!(r"{letter}:\")),
            ScanTarget::Folder(path) => path.clone(),
        });
        (self.scan_started, self.scan_secs) = (Some(Instant::now()), None);
        let ctx = ctx.clone();
        self.scan = Some(scan::start(target.clone(), tx, Box::new(move || ctx.request_repaint())));
        self.target = Some(target);
    }

    /// Drops the cached treemap layout so the next frame recomputes it (call after the tree changes).
    pub fn invalidate_treemap(&mut self) {
        self.treemap_cache = None;
    }

    /// Sends the selected item to the Recycle Bin once the user has confirmed. Never deletes the scan root.
    pub fn delete_selected(&mut self, confirmed: bool) {
        self.delete_selected_with(confirmed, actions::recycle);
    }

    /// Deletes the selected item for good, without the Recycle Bin, once the user has confirmed. Never the scan root.
    pub fn delete_selected_permanently(&mut self, confirmed: bool) {
        self.delete_selected_with(confirmed, |path, _| actions::delete_permanently(path));
    }

    /// Runs `recycle` (or a permanent delete) on the confirmed selection and updates the tree when it is gone.
    pub(crate) fn delete_selected_with(&mut self, confirmed: bool, recycle: impl FnOnce(&Path, u64) -> DeleteOutcome) {
        let (Some(tree), Some(id)) = (self.tree.as_mut(), self.selected) else { return };
        // A detached node's path would be relative (just its name), so it must never reach `recycle`.
        if !confirmed || id == tree.root || !tree.is_attached(id) || tree.is_unaccounted(id) {
            return;
        }
        // The larger of the two sizes, so the bin-limit check can only err towards refusing.
        let node = &tree.nodes[id.0 as usize];
        let size = node.logical.max(node.size);
        match recycle(Path::new(&tree.path(id)), size) {
            DeleteOutcome::Recycled | DeleteOutcome::Deleted => {
                self.selected = tree.nodes[id.0 as usize].parent;
                tree.remove(id);
                self.delete_notice = None;
                self.invalidate_treemap();
            }
            DeleteOutcome::Refused(message) | DeleteOutcome::Failed(message) => self.delete_notice = Some(message),
        }
    }

    /// Applies the scan events that arrived since the last call.
    pub fn pump(&mut self) {
        loop {
            let event = match self.rx.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    // The scan thread ended without Done or Failed (it panicked).
                    if self.scan.take().is_some() {
                        self.error = Some("scan stopped unexpectedly".into());
                    }
                    return;
                }
            };
            match event {
                ScanEvent::Progress { files, bytes } => self.progress = (files, bytes),
                ScanEvent::Done { tree, mode, skipped } => {
                    self.expanded = HashSet::from([tree.root]);
                    self.selected = None;
                    // Ids from the old tree may name a different item in the new one.
                    self.pending_delete = None;
                    self.pending_permanent = None;
                    self.treemap_cache = None;
                    self.tree = Some(tree);
                    self.mode = Some(mode);
                    self.skipped = skipped;
                    self.scan = None;
                    self.scan_secs = self.scan_started.map(|t| t.elapsed().as_secs_f32());
                }
                ScanEvent::Failed(message) => {
                    self.error = Some(message);
                    self.scan = None;
                }
            }
        }
    }
}

impl eframe::App for AppState {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("top_bar").show(ui, |ui| top_bar(ui, self));
        egui::Panel::top("tabs").show(ui, |ui| {
            // Only one view exists; the tab is there for the familiar layout.
            let _ = ui.selectable_label(true, "Tree View");
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            if let Some(tree) = &self.tree {
                ui.label(selection_status(tree, self.selected.unwrap_or(tree.root)));
            }
        });
        let treemap_height = ui.ctx().content_rect().height() * 0.4;
        egui::Panel::bottom("treemap")
            .resizable(true)
            .default_size(treemap_height)
            .show(ui, |ui| ui_treemap::show(ui, self));
        egui::Panel::right("legend")
            .resizable(true)
            .default_size(500.0)
            .show(ui, |ui| ui_legend::show(ui, self));
        egui::CentralPanel::default().show(ui, |ui| {
            if self.target.is_none() {
                ui.centered_and_justified(|ui| ui.heading("Choose a drive or folder to scan"));
            } else {
                ui_tree::show(ui, self);
            }
        });
        delete_dialogs(ui.ctx(), self);
    }
}

/// The delete confirmations for `pending_delete` and `pending_permanent`, then the notice explaining a refused or
/// failed delete.
fn delete_dialogs(ctx: &egui::Context, state: &mut AppState) {
    if let (Some(id), Some(tree)) = (state.pending_delete, &state.tree) {
        let text = format!("Move {} ({}) to the Recycle Bin?", tree.path(id), ui_tree::format_size(tree.nodes[id.0 as usize].size));
        let mut choice = None;
        dialog("Delete to Recycle Bin").show(ctx, |ui| {
            ui.label(text);
            ui.horizontal(|ui| {
                if ui.button("Delete").clicked() {
                    choice = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(false);
                }
            });
        });
        if let Some(delete) = choice {
            state.pending_delete = None;
            if delete {
                state.selected = Some(id);
                state.delete_selected(true);
            }
        }
    }
    if let (Some(id), Some(tree)) = (state.pending_permanent, &state.tree) {
        let text = format!(
            "Permanently delete {} ({})?\n\nIt will not go to the Recycle Bin. This cannot be undone.",
            tree.path(id),
            ui_tree::format_size(tree.nodes[id.0 as usize].size)
        );
        let mut choice = None;
        dialog("Delete permanently").show(ctx, |ui| {
            ui.label(text);
            ui.horizontal(|ui| {
                let cancel = ui.button("Cancel");
                // Enter or Space acts on the focused button, so focus starts on Cancel.
                if ui.memory(|m| m.focused().is_none()) {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    choice = Some(false);
                }
                if ui.button("Delete permanently").clicked() {
                    choice = Some(true);
                }
            });
        });
        if let Some(delete) = choice {
            state.pending_permanent = None;
            if delete {
                state.selected = Some(id);
                state.delete_selected_permanently(true);
            }
        }
    }
    if let Some(notice) = &state.delete_notice {
        let mut dismissed = false;
        dialog("Not deleted").show(ctx, |ui| {
            ui.label(notice);
            dismissed = ui.button("OK").clicked();
        });
        if dismissed {
            state.delete_notice = None;
        }
    }
}

fn dialog(title: &str) -> egui::Window<'_> {
    egui::Window::new(title).collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
}

/// Dark, compact look: near-black panels, faint row stripes, blue selection.
pub fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        let visuals = &mut style.visuals;
        visuals.panel_fill = egui::Color32::from_gray(24);
        visuals.window_fill = egui::Color32::from_gray(30);
        visuals.extreme_bg_color = egui::Color32::from_gray(14);
        visuals.faint_bg_color = egui::Color32::from_gray(32);
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 90, 158);
        visuals.widgets.noninteractive.bg_stroke.color = egui::Color32::from_gray(48);
    });
}

/// Width of the top bar's left block: drive picker, buttons, status and progress bar.
const CONTROLS_W: f32 = 420.0;

fn top_bar(ui: &mut egui::Ui, state: &mut AppState) {
    let drive_id = egui::Id::unique("drive_choice");
    let mut choice: Option<char> = ui.data(|d| d.get_temp(drive_id)).flatten();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width(CONTROLS_W);
            ui.horizontal(|ui| {
                ui.label("Select:");
                egui::ComboBox::from_id_salt("drive")
                    .selected_text(choice.map_or("Drive".to_string(), |letter| format!("{letter}:")))
                    .show_ui(ui, |ui| {
                        for letter in logical_drives() {
                            ui.selectable_value(&mut choice, Some(letter), format!("{letter}:"));
                        }
                    });
                if ui.add_enabled(choice.is_some(), egui::Button::new("Scan")).clicked() {
                    if let Some(letter) = choice {
                        state.begin_scan(ScanTarget::Volume(letter), ui.ctx());
                    }
                }
                if ui.button("Browse folder").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        state.begin_scan(ScanTarget::Folder(path), ui.ctx());
                    }
                }
                if ui.add_enabled(state.target.is_some(), egui::Button::new("Rescan")).clicked() {
                    if let Some(target) = state.target.clone() {
                        state.begin_scan(target, ui.ctx());
                    }
                }
            });
            ui.label(status_text(state));
            let fraction = progress_fraction(state);
            ui.add(egui::ProgressBar::new(fraction.unwrap_or(0.0)).desired_height(12.0).animate(fraction.is_none()));
        });
        ui.separator();
        egui::Grid::new("disk_info").num_columns(2).spacing([24.0, 2.0]).show(ui, |ui| {
            let selection = state.tree.as_ref().map(|t| actions::shell_path(t.path(state.selected.unwrap_or(t.root))));
            ui.label("Selection:");
            ui.strong(selection.unwrap_or_default());
            ui.end_row();
            for (name, value) in disk_lines(state.disk) {
                ui.label(name);
                ui.strong(value);
                ui.end_row();
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(drive_id, choice));
    if let Some(error) = &state.error {
        ui.colored_label(egui::Color32::RED, error);
    }
}

/// The line over the progress bar: live counters while scanning, then how the last scan went.
fn status_text(state: &AppState) -> String {
    if state.scan.is_some() {
        return progress_label(state.progress);
    }
    let Some(mode) = state.mode.filter(|_| state.tree.is_some()) else { return String::new() };
    let mut text = match state.scan_secs {
        Some(secs) => format!("Scan complete in {secs:.2} seconds"),
        None => "Scan complete".to_string(),
    };
    text += match mode {
        ScanMode::Mft => " (fast, MFT)",
        ScanMode::Walk => " (fallback walk)",
    };
    if let Some(skipped) = skipped_label(state) {
        text += &format!("; {skipped}");
    }
    text
}

/// Progress bar fill: while a whole volume scans, bytes found against the volume's used space (`None`, an animated
/// bar, when that is unknown or the target is a folder); full once a tree is shown.
fn progress_fraction(state: &AppState) -> Option<f32> {
    if state.scan.is_none() {
        return Some(if state.tree.is_some() { 1.0 } else { 0.0 });
    }
    let (total, free) = state.disk.filter(|_| matches!(state.target, Some(ScanTarget::Volume(_))))?;
    let used = total.saturating_sub(free);
    // The files never quite reach used space (metadata), so the bar stops short until the scan is done.
    (used > 0).then(|| (state.progress.1 as f32 / used as f32).min(0.99))
}

/// The "Total space", "Space used" and "Space free" rows of the info block, the last two with their share of the total.
fn disk_lines(disk: Option<(u64, u64)>) -> [(&'static str, String); 3] {
    let Some((total, free)) = disk else {
        return [("Total space:", "-".into()), ("Space used:", "-".into()), ("Space free:", "-".into())];
    };
    let used = total.saturating_sub(free);
    let with_share = |n: u64| {
        let share = if total == 0 { 0.0 } else { n as f64 * 100.0 / total as f64 };
        format!("{}  ({share:.1}%)", ui_tree::format_size(n))
    };
    [("Total space:", ui_tree::format_size(total)), ("Space used:", with_share(used)), ("Space free:", with_share(free))]
}

/// The status line under the treemap for item `id`: how many files it holds (1 for a file), its logical size and
/// its size on disk.
pub fn selection_status(tree: &Tree, id: NodeId) -> String {
    let node = &tree.nodes[id.0 as usize];
    let files = if node.is_dir { u64::from(node.files) } else { 1 };
    format!(
        "Selected Files: {}    Total Size: {}    Allocated: {}",
        ui_tree::format_count(files),
        ui_tree::format_size(node.logical),
        ui_tree::format_size(node.size)
    )
}

/// (total, free) bytes of the volume holding `path`, as Explorer shows them; `None` when they cannot be read.
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let (mut total, mut free) = (0u64, 0u64);
    unsafe { GetDiskFreeSpaceExW(&windows::core::HSTRING::from(path), None, Some(&mut total), Some(&mut free)) }.ok()?;
    Some((total, free))
}

/// Tells the user the totals miss what the walk could not read; only once a tree is shown.
fn skipped_label(state: &AppState) -> Option<String> {
    (state.tree.is_some() && state.skipped > 0).then(|| format!("{} items could not be read", state.skipped))
}

fn progress_label((files, bytes): (u64, u64)) -> String {
    format!("Scanning: {files} files, {}", ui_tree::format_size(bytes))
}

/// Drive letters the system reports as present (GetLogicalDriveStringsW lists only existing drives).
fn logical_drives() -> Vec<char> {
    use windows::Win32::Storage::FileSystem::GetLogicalDriveStringsW;
    let mut buf = [0u16; 512];
    let len = unsafe { GetLogicalDriveStringsW(Some(&mut buf[..])) } as usize;
    buf[..len.min(buf.len())]
        .split(|&unit| unit == 0)
        .filter_map(|root| root.first().and_then(|&unit| char::from_u32(unit as u32)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;

    fn tree() -> Tree {
        TreeBuilder::new("test").finish()
    }

    #[test]
    fn stale_events_after_rescan_are_ignored() {
        let mut state = AppState::new();
        let first = state.install_channel();
        let _second = state.install_channel();
        assert!(first.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Walk, skipped: 0 }).is_err());
        state.pump();
        assert!(state.tree.is_none());
        assert!(state.mode.is_none());
        assert_eq!(state.progress, (0, 0));
    }

    #[test]
    fn progress_event_updates_counters() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        tx.send(ScanEvent::Progress { files: 3, bytes: 4096 }).unwrap();
        state.pump();
        assert_eq!(state.progress, (3, 4096));
    }

    #[test]
    fn done_event_sets_tree_and_mode() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        let tree = tree();
        let root = tree.root;
        tx.send(ScanEvent::Done { tree, mode: ScanMode::Mft, skipped: 2 }).unwrap();
        state.pump();
        assert!(state.tree.is_some());
        assert_eq!(state.mode, Some(ScanMode::Mft));
        assert_eq!(state.skipped, 2);
        assert!(state.scan.is_none());
        assert_eq!(state.expanded, HashSet::from([root]));
    }

    /// Root "C:" with "a" (dir) holding "f.bin" (10 bytes), and "g.bin" (7 bytes).
    fn state_with_file() -> (AppState, NodeId, NodeId) {
        let mut b = TreeBuilder::new("C:");
        let a = b.add(NodeId(0), "a", true, 0, 0);
        let f = b.add(a, "f.bin", false, 10, 10);
        b.add(NodeId(0), "g.bin", false, 7, 7);
        let mut state = AppState::new();
        state.tree = Some(b.finish());
        state.selected = Some(f);
        (state, a, f)
    }

    fn root_size(state: &AppState) -> u64 {
        let tree = state.tree.as_ref().unwrap();
        tree.nodes[tree.root.0 as usize].size
    }

    #[test]
    fn delete_selected_requires_confirmation() {
        let (mut state, _, f) = state_with_file();
        state.delete_selected_with(false, |_, _| panic!("recycle must not be called"));
        assert_eq!(root_size(&state), 17);
        assert_eq!(state.selected, Some(f));
    }

    #[test]
    fn delete_root_is_never_performed() {
        let (mut state, _, _) = state_with_file();
        state.selected = state.tree.as_ref().map(|t| t.root);
        state.delete_selected_with(true, |_, _| panic!("recycle must not be called"));
        assert_eq!(root_size(&state), 17);
    }

    #[test]
    fn delete_without_selection_does_nothing() {
        let (mut state, _, _) = state_with_file();
        state.selected = None;
        state.delete_selected_with(true, |_, _| panic!("recycle must not be called"));
        assert_eq!(root_size(&state), 17);
    }

    #[test]
    fn refused_delete_leaves_tree_unchanged() {
        let (mut state, _, f) = state_with_file();
        state.delete_selected_with(true, |path, size| {
            assert_eq!(path, Path::new(r"C:\a\f.bin"));
            assert_eq!(size, 10);
            DeleteOutcome::Refused("no bin".into())
        });
        assert_eq!(root_size(&state), 17);
        assert_eq!(state.selected, Some(f));
        assert_eq!(state.delete_notice.as_deref(), Some("no bin"));
    }

    #[test]
    fn failed_delete_sets_notice() {
        let (mut state, _, _) = state_with_file();
        state.delete_selected_with(true, |_, _| DeleteOutcome::Failed("locked".into()));
        assert_eq!(root_size(&state), 17);
        assert_eq!(state.delete_notice.as_deref(), Some("locked"));
    }

    #[test]
    fn recycled_delete_updates_tree_in_place() {
        let (mut state, a, f) = state_with_file();
        state.delete_notice = Some("old".into());
        state.treemap_cache = Some(TreemapCache {
            size: (1.0, 1.0),
            blocks: Vec::new(),
            mesh: Default::default(),
            origin: egui::Pos2::ZERO,
            dirs: Vec::new(),
            highlight_for: None,
            highlight: Default::default(),
        });
        state.delete_selected_with(true, |_, _| DeleteOutcome::Recycled);
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree.nodes[a.0 as usize].size, 0);
        assert_eq!(root_size(&state), 7);
        assert!(!state.tree.as_ref().unwrap().nodes[a.0 as usize].children.contains(&f));
        assert_eq!(state.selected, Some(a));
        assert!(state.delete_notice.is_none());
        assert!(state.treemap_cache.is_none());
    }

    #[test]
    fn detached_selection_is_never_deleted() {
        let (mut state, a, f) = state_with_file();
        state.tree.as_mut().unwrap().remove(a);
        state.selected = Some(f);
        state.delete_selected_with(true, |_, _| panic!("recycle must not be called"));
        assert_eq!(root_size(&state), 7);
    }

    #[test]
    fn permanent_delete_updates_tree_in_place() {
        let (mut state, a, _) = state_with_file();
        state.delete_selected_with(true, |path, _| {
            assert_eq!(path, Path::new(r"C:\a\f.bin"));
            DeleteOutcome::Deleted
        });
        assert_eq!(root_size(&state), 7);
        assert_eq!(state.selected, Some(a));
    }

    #[test]
    fn rescan_clears_pending_permanent_delete() {
        let (mut state, _, f) = state_with_file();
        state.pending_permanent = Some(f);
        let tx = state.install_channel();
        tx.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Walk, skipped: 0 }).unwrap();
        state.pump();
        assert!(state.pending_permanent.is_none());
    }

    #[test]
    fn unaccounted_item_is_never_deleted() {
        let (mut state, _, _) = state_with_file();
        let tree = state.tree.as_mut().unwrap();
        tree.add_unaccounted(100);
        let item = tree.nodes[tree.root.0 as usize].children[0];
        assert!(tree.is_unaccounted(item));
        state.selected = Some(item);
        state.delete_selected_with(true, |_, _| panic!("recycle must not be called"));
        assert_eq!(root_size(&state), 100);
    }

    #[test]
    fn rescan_clears_pending_delete() {
        let (mut state, _, f) = state_with_file();
        state.pending_delete = Some(f);
        let tx = state.install_channel();
        tx.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Walk, skipped: 0 }).unwrap();
        state.pump();
        assert!(state.pending_delete.is_none());
    }

    #[test]
    fn failed_event_sets_error() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        tx.send(ScanEvent::Failed("cannot read C:\\".into())).unwrap();
        state.pump();
        assert_eq!(state.error.as_deref(), Some("cannot read C:\\"));
        assert!(state.scan.is_none());
    }

    #[test]
    fn skipped_items_are_shown_with_the_tree() {
        let mut state = AppState::new();
        state.skipped = 3;
        assert_eq!(skipped_label(&state), None);
        state.tree = Some(tree());
        assert_eq!(skipped_label(&state).as_deref(), Some("3 items could not be read"));
        state.skipped = 0;
        assert_eq!(skipped_label(&state), None);
    }

    #[test]
    fn done_event_records_scan_time() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        state.scan_started = Some(Instant::now());
        tx.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Mft, skipped: 0 }).unwrap();
        state.pump();
        assert!(state.scan_secs.is_some_and(|s| s >= 0.0));
        assert!(status_text(&state).starts_with("Scan complete in "), "{}", status_text(&state));
    }

    #[test]
    fn status_text_follows_the_scan() {
        let mut state = AppState::new();
        assert_eq!(status_text(&state), "");
        state.scan = Some(ScanHandle::detached());
        state.progress = (12, 1536);
        assert_eq!(status_text(&state), "Scanning: 12 files, 1.5 KB");
        state.scan = None;
        state.tree = Some(tree());
        state.mode = Some(ScanMode::Walk);
        state.skipped = 3;
        assert_eq!(status_text(&state), "Scan complete (fallback walk); 3 items could not be read");
    }

    #[test]
    fn progress_fraction_compares_bytes_with_used_space() {
        let mut state = AppState::new();
        assert_eq!(progress_fraction(&state), Some(0.0));
        state.scan = Some(ScanHandle::detached());
        state.target = Some(ScanTarget::Volume('C'));
        assert_eq!(progress_fraction(&state), None);
        state.disk = Some((1000, 600));
        state.progress = (5, 100);
        assert_eq!(progress_fraction(&state), Some(0.25));
        state.progress = (5, 900);
        assert_eq!(progress_fraction(&state), Some(0.99));
        state.target = Some(ScanTarget::Folder(PathBuf::from(r"C:\x")));
        assert_eq!(progress_fraction(&state), None);
        state.scan = None;
        state.tree = Some(tree());
        assert_eq!(progress_fraction(&state), Some(1.0));
    }

    #[test]
    fn disk_lines_show_used_and_free_shares() {
        let gb = 1024 * 1024 * 1024;
        let lines = disk_lines(Some((1000 * gb, 250 * gb)));
        assert_eq!(lines[0], ("Total space:", "1000.0 GB".to_string()));
        assert_eq!(lines[1], ("Space used:", "750.0 GB  (75.0%)".to_string()));
        assert_eq!(lines[2], ("Space free:", "250.0 GB  (25.0%)".to_string()));
        assert_eq!(disk_lines(None)[1].1, "-");
        assert_eq!(disk_lines(Some((0, 0)))[1].1, "0 B  (0.0%)");
    }

    #[test]
    fn selection_status_counts_files_and_both_sizes() {
        let (state, a, f) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(selection_status(tree, tree.root), "Selected Files: 2    Total Size: 17 B    Allocated: 17 B");
        assert_eq!(selection_status(tree, a), "Selected Files: 1    Total Size: 10 B    Allocated: 10 B");
        assert_eq!(selection_status(tree, f), "Selected Files: 1    Total Size: 10 B    Allocated: 10 B");
    }

    #[test]
    fn disk_space_reads_the_temp_volume() {
        let (total, free) = disk_space(&std::env::temp_dir()).unwrap();
        assert!(total > 0 && free <= total);
    }

    #[test]
    fn progress_label_formats_bytes() {
        assert_eq!(progress_label((12, 1536)), "Scanning: 12 files, 1.5 KB");
    }

    #[test]
    fn scan_thread_that_dies_without_a_result_is_an_error() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        state.scan = Some(ScanHandle::detached());
        tx.send(ScanEvent::Progress { files: 1, bytes: 1 }).unwrap();
        drop(tx);
        state.pump();
        assert_eq!(state.error.as_deref(), Some("scan stopped unexpectedly"));
        assert!(state.scan.is_none());
        assert_eq!(state.progress, (1, 1));
    }

    #[test]
    fn finished_scan_closing_its_channel_is_not_an_error() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        state.scan = Some(ScanHandle::detached());
        tx.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Walk, skipped: 0 }).unwrap();
        drop(tx);
        state.pump();
        assert!(state.error.is_none());
        assert!(state.tree.is_some());
    }
}
