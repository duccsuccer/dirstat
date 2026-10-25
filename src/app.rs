//! Application state, the scan lifecycle and the top bar.

use eframe::egui;
use crate::actions::{self, DeleteOutcome};
use crate::model::{NodeId, Tree};
use crate::scan::{self, ScanEvent, ScanHandle, ScanMode, ScanTarget};
use crate::{ui_legend, ui_tree, ui_treemap::{self, TreemapCache}};
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};

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
    /// Why the last delete did not happen; shown in a dialog until dismissed.
    pub delete_notice: Option<String>,
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
            delete_notice: None,
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
            DeleteOutcome::Recycled => {
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
                    self.treemap_cache = None;
                    self.tree = Some(tree);
                    self.mode = Some(mode);
                    self.skipped = skipped;
                    self.scan = None;
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
        let treemap_height = ui.ctx().content_rect().height() * 0.45;
        egui::Panel::bottom("treemap")
            .resizable(true)
            .default_size(treemap_height)
            .show(ui, |ui| ui_treemap::show(ui, self));
        egui::Panel::right("legend")
            .resizable(true)
            .default_size(240.0)
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

/// The delete confirmation for `pending_delete`, then the notice explaining a refused or failed delete.
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

fn top_bar(ui: &mut egui::Ui, state: &mut AppState) {
    let drive_id = egui::Id::unique("drive_choice");
    let mut choice: Option<char> = ui.data(|d| d.get_temp(drive_id)).flatten();
    ui.horizontal(|ui| {
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
        if let Some(mode) = state.mode {
            ui.label(match mode {
                ScanMode::Mft => "Fast (MFT)",
                ScanMode::Walk => "Fallback (walk)",
            });
        }
        if let Some(text) = skipped_label(state) {
            ui.label(text);
        }
        if state.scan.is_some() {
            ui.label(progress_label(state.progress));
        }
    });
    ui.data_mut(|d| d.insert_temp(drive_id, choice));
    if let Some(error) = &state.error {
        ui.colored_label(egui::Color32::RED, error);
    }
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
