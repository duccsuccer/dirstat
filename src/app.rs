//! Application state, the scan lifecycle and the top bar.

use eframe::egui;
use crate::model::{NodeId, Tree};
use crate::scan::{self, ScanEvent, ScanHandle, ScanMode, ScanTarget};
use crate::{ui_legend, ui_tree, ui_treemap};
use std::collections::HashSet;
use std::sync::mpsc::{channel, Receiver, Sender};

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

    /// Applies the scan events that arrived since the last call.
    pub fn pump(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                ScanEvent::Progress { files, bytes } => self.progress = (files, bytes),
                ScanEvent::Done { tree, mode, skipped } => {
                    self.expanded = HashSet::from([tree.root]);
                    self.selected = None;
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
    }
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
        if state.scan.is_some() {
            let (files, bytes) = state.progress;
            ui.label(format!("Scanning: {files} files, {bytes} bytes"));
        }
    });
    ui.data_mut(|d| d.insert_temp(drive_id, choice));
    if let Some(error) = &state.error {
        ui.colored_label(egui::Color32::RED, error);
    }
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

    #[test]
    fn failed_event_sets_error() {
        let mut state = AppState::new();
        let tx = state.install_channel();
        tx.send(ScanEvent::Failed("cannot read C:\\".into())).unwrap();
        state.pump();
        assert_eq!(state.error.as_deref(), Some("cannot read C:\\"));
        assert!(state.scan.is_none());
    }
}
