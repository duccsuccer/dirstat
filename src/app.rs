//! Application state, the scan lifecycle and the top bar.

use eframe::egui;
use crate::actions::{self, AppItem, Chosen, Entry};
use crate::model::{NodeId, Tree};
use crate::scan::{self, ScanEvent, ScanHandle, ScanMode, ScanTarget};
use crate::theme::{self, palette};
use crate::{ui_legend, ui_tree, ui_treemap::{self, is_within, TreemapCache}};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::time::Instant;
use windows::Win32::Foundation::HWND;

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
    /// The folder the treemap shows, below the scan root after a Zoom in; the scan root once a tree is shown.
    pub treemap_root: NodeId,
    /// Set by a treemap click; the tree pane scrolls to `selected` once, then clears it.
    pub scroll_to_selected: bool,
    /// Item right-clicked in the last frame, and whether Shift was held; its shell menu opens in the next frame,
    /// once the selection has been drawn.
    pub pending_menu: Option<(NodeId, bool)>,
    /// The main window, as the shell menu's owner; 0 when unknown.
    pub hwnd: isize,
    /// (total, free) bytes of the volume holding the scan target, read when the scan starts.
    pub disk: Option<(u64, u64)>,
    pub scan_started: Option<Instant>,
    /// How long the last finished scan took.
    pub scan_secs: Option<f32>,
    /// File type names the shell gave for each extension, so it is asked once per extension.
    pub type_names: HashMap<String, String>,
    /// The system theme and window focus at the last frame; the Windows app mode is read again when either changes.
    pub theme_key: Option<(Option<egui::Theme>, bool)>,
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
            treemap_root: NodeId(0),
            scroll_to_selected: false,
            pending_menu: None,
            hwnd: 0,
            disk: None,
            scan_started: None,
            scan_secs: None,
            type_names: HashMap::new(),
            theme_key: None,
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

    /// Shows the context menu for item `id`: dirstat's items, then Explorer's own unless the item has no shell menu.
    /// Once a shell command has run, drops the item from the tree if the command removed it (Delete, Shift+Delete).
    /// Errors go to `error`.
    pub fn context_menu(&mut self, id: NodeId, extended: bool) {
        let Some(tree) = &self.tree else { return };
        if !tree.is_attached(id) || !has_menu(tree, id) {
            return;
        }
        let path = actions::shell_path(tree.path(id));
        let entries = menu_entries(tree, id, self.treemap_root);
        match actions::show_context_menu(HWND(self.hwnd as *mut _), Path::new(&path), extended, &entries, has_shell_menu(tree, id)) {
            Ok(Chosen::Shell) => self.forget_if_gone(id, exists),
            Ok(Chosen::App(item)) => self.app_item(id, item),
            Ok(Chosen::Nothing) => {}
            Err(e) => self.error = Some(format!("{path}: {}", e.message())),
        }
    }

    /// Runs one of dirstat's own menu items on item `id`. Errors go to `error`.
    fn app_item(&mut self, id: NodeId, item: AppItem) {
        let Some(tree) = &self.tree else { return };
        let path = actions::shell_path(tree.path(id));
        let dir = actions::shell_path(tree.path(folder_of(tree, id)));
        let result = match item {
            AppItem::Explore => actions::explore(Path::new(&path), !tree.nodes[id.0 as usize].is_dir).map_err(|e| format!("{path}: {e}")),
            AppItem::CommandPrompt => actions::command_prompt(Path::new(&dir)).map_err(|e| format!("{dir}: {e}")),
            AppItem::CopyPath => actions::copy_text(&path).map_err(|e| format!("{path}: {e}")),
            AppItem::Export => {
                // Cancelling the dialog is not an error.
                let Some(file) = rfd::FileDialog::new()
                    .set_file_name("dirstat-export.csv")
                    .add_filter("CSV", &["csv"])
                    .save_file()
                else {
                    return;
                };
                std::fs::write(&file, csv_export(tree, id)).map_err(|e| format!("{}: {e}", file.display()))
            }
            AppItem::CopyInfo => actions::copy_text(&info_line(tree, id)).map_err(|e| format!("{path}: {e}")),
            AppItem::ZoomIn => {
                self.treemap_root = folder_of(tree, id);
                self.invalidate_treemap();
                Ok(())
            }
            AppItem::ZoomOut => {
                self.treemap_root = zoom_out_root(tree, self.treemap_root);
                self.invalidate_treemap();
                Ok(())
            }
        };
        if let Err(e) = result {
            self.error = Some(e);
        }
    }

    /// Removes item `id` from the tree, taking its sizes and counts off every total, when `exists` says its path is
    /// gone. Never the root, the unaccounted item or an item already removed.
    pub(crate) fn forget_if_gone(&mut self, id: NodeId, exists: impl FnOnce(&Path) -> bool) {
        let Some(tree) = self.tree.as_mut() else { return };
        // A detached node's path would be relative (just its name), so it must never be checked.
        if !tree.is_attached(id) || !has_shell_menu(tree, id) || exists(Path::new(&tree.path(id))) {
            return;
        }
        let parent = tree.nodes[id.0 as usize].parent;
        self.selected = parent;
        tree.remove(id);
        // A treemap root that was removed (or lay inside the removed folder) shows its parent instead.
        if !tree.is_attached(self.treemap_root) {
            self.treemap_root = parent.unwrap_or(tree.root);
        }
        self.invalidate_treemap();
    }

    /// Copies the selected item's path to the clipboard (Ctrl+C). The unaccounted item has none.
    fn copy_selected_path(&mut self) {
        let (Some(tree), Some(id)) = (&self.tree, self.selected) else { return };
        if tree.is_unaccounted(id) {
            return;
        }
        let path = actions::shell_path(tree.path(id));
        if let Err(e) = actions::copy_text(&path) {
            self.error = Some(format!("{path}: {e}"));
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
                    self.treemap_root = tree.root;
                    // Ids from the old tree may name a different item in the new one.
                    self.pending_menu = None;
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
        let key = (ui.ctx().system_theme(), ui.input(|i| i.focused));
        if self.theme_key != Some(key) {
            self.theme_key = Some(key);
            theme::apply(ui.ctx());
        }
        if let Some((id, extended)) = self.pending_menu.take() {
            self.context_menu(id, extended);
        }
        if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
            self.copy_selected_path();
        }
        // 1px lines between the panes, drawn on the edges where two panes meet.
        let line = egui::Stroke::new(1.0, palette(ui.visuals().dark_mode).separator);
        let top = egui::Panel::top("top_bar").frame(panel_frame(ui, egui::Margin::symmetric(32, 24))).show(ui, |ui| top_bar(ui, self)).response.rect;
        ui.painter().hline(top.x_range(), top.bottom() - 0.5, line);
        let status = egui::Panel::bottom("status")
            .frame(panel_frame(ui, egui::Margin::symmetric(16, 3)))
            .show(ui, |ui| {
                if let Some(tree) = &self.tree {
                    let text = selection_status(tree, status_node(tree, self.selected, self.treemap_root));
                    ui.colored_label(palette(ui.visuals().dark_mode).muted, text);
                }
            })
            .response
            .rect;
        ui.painter().hline(status.x_range(), status.top() + 0.5, line);
        // The treemap takes a share of the height left between the top bar and the status line.
        let treemap_height = ui.available_rect_before_wrap().height() * 0.38;
        let treemap = egui::Panel::bottom("treemap")
            .frame(panel_frame(ui, egui::Margin::symmetric(8, 0)))
            .resizable(true)
            .default_size(treemap_height)
            .show(ui, |ui| ui_treemap::show(ui, self))
            .response
            .rect;
        ui.painter().hline(treemap.x_range(), treemap.top() + 0.5, line);
        let legend = egui::Panel::right("legend")
            .frame(panel_frame(ui, egui::Margin::symmetric(8, 0)))
            .resizable(true)
            .default_size(ui_legend::WIDTH)
            .min_size(ui_legend::MIN_WIDTH)
            .show(ui, |ui| ui_legend::show(ui, self))
            .response
            .rect;
        ui.painter().vline(legend.left() + 0.5, legend.y_range(), line);
        egui::CentralPanel::default().frame(panel_frame(ui, egui::Margin::symmetric(8, 0))).show(ui, |ui| {
            if self.target.is_none() {
                ui.centered_and_justified(|ui| ui.heading("Choose a drive or folder to scan"));
            } else {
                ui_tree::show(ui, self);
            }
        });
    }
}

/// True when item `id` is a real file or folder below the scan root. The root (often a whole drive) and the
/// unaccounted item get no shell menu.
pub(crate) fn has_shell_menu(tree: &Tree, id: NodeId) -> bool {
    id != tree.root && !tree.is_unaccounted(id)
}

/// True when item `id` gets a context menu at all: every item but the unaccounted one, which is no file or folder.
pub(crate) fn has_menu(tree: &Tree, id: NodeId) -> bool {
    !tree.is_unaccounted(id)
}

/// The folder item `id` belongs to: itself when it is a folder, else its parent.
pub(crate) fn folder_of(tree: &Tree, id: NodeId) -> NodeId {
    let node = &tree.nodes[id.0 as usize];
    if node.is_dir {
        id
    } else {
        node.parent.unwrap_or(id)
    }
}

/// False only when nothing is at `path` any more; a link counts as itself, whatever it points to.
fn exists(path: &Path) -> bool {
    !matches!(std::fs::symlink_metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

/// Background of a pane, with the given padding inside it.
fn panel_frame(ui: &egui::Ui, margin: egui::Margin) -> egui::Frame {
    egui::Frame::new().fill(palette(ui.visuals().dark_mode).bg).inner_margin(margin)
}

/// A text button without a frame: muted, and in the text colour while hovered.
fn link(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.fg_stroke.color = palette(ui.visuals().dark_mode).muted;
        ui.add_enabled(enabled, egui::Button::new(text))
    })
    .inner
}

/// The one filled button: accent pink, a deeper pink while hovered.
fn accent_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    let pal = palette(ui.visuals().dark_mode);
    ui.scope(|ui| {
        let w = &mut ui.visuals_mut().widgets;
        for (state, fill) in [(&mut w.inactive, pal.accent), (&mut w.hovered, pal.accent_hover), (&mut w.active, pal.accent_hover)] {
            state.weak_bg_fill = fill;
            state.fg_stroke.color = pal.on_accent;
        }
        ui.add_enabled(enabled, egui::Button::new(text))
    })
    .inner
}

fn top_bar(ui: &mut egui::Ui, state: &mut AppState) {
    let pal = palette(ui.visuals().dark_mode);
    let drive_id = egui::Id::unique("drive_choice");
    let mut choice: Option<char> = ui.data(|d| d.get_temp(drive_id)).flatten();
    ui.horizontal(|ui| {
        let scan = choice.map_or("Scan".to_string(), |letter| format!("Scan {letter}:"));
        if accent_button(ui, &scan, choice.is_some()).clicked() {
            if let Some(letter) = choice {
                state.begin_scan(ScanTarget::Volume(letter), ui.ctx());
            }
        }
        ui.scope(|ui| {
            ui.visuals_mut().widgets.inactive.fg_stroke.color = pal.muted;
            ui.menu_button("Change drive", |ui| {
                for letter in logical_drives() {
                    if ui.button(format!("{letter}:")).clicked() {
                        choice = Some(letter);
                        ui.close();
                    }
                }
            });
        });
        if link(ui, "Browse folder", true).clicked() {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                state.begin_scan(ScanTarget::Folder(path), ui.ctx());
            }
        }
        if link(ui, "Rescan", state.target.is_some()).clicked() {
            if let Some(target) = state.target.clone() {
                state.begin_scan(target, ui.ctx());
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.colored_label(pal.muted, disk_summary(state.disk));
        });
    });
    ui.data_mut(|d| d.insert_temp(drive_id, choice));
    let selection = state.tree.as_ref().map(|t| actions::shell_path(t.path(state.selected.unwrap_or(t.root))));
    ui.colored_label(pal.muted, selection.unwrap_or_default());
    ui.colored_label(pal.muted, status_text(state));
    let fraction = progress_fraction(state);
    ui.add(egui::ProgressBar::new(fraction.unwrap_or(0.0)).desired_height(3.0).animate(fraction.is_none()));
    if let Some(error) = &state.error {
        ui.colored_label(pal.error, error);
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

/// The volume's used, free and total space on one line, e.g. `833.3 GB used · 97.1 GB free · 930.4 GB`.
fn disk_summary(disk: Option<(u64, u64)>) -> String {
    let Some((total, free)) = disk else { return String::new() };
    format!(
        "{} used · {} free · {}",
        ui_tree::format_size(total.saturating_sub(free)),
        ui_tree::format_size(free),
        ui_tree::format_size(total)
    )
}

/// Item `id`'s items, files and folders as the tree counts them (a file is one item and one file).
fn counts(tree: &Tree, id: NodeId) -> (u64, u64, u64) {
    let node = &tree.nodes[id.0 as usize];
    if node.is_dir {
        let (files, folders) = (u64::from(node.files), u64::from(node.folders));
        (files + folders, files, folders)
    } else {
        (1, 1, 0)
    }
}

/// The line Copy file and size info puts on the clipboard for item `id`: tab-separated path, logical size, size on
/// disk, items, files and folders.
pub fn info_line(tree: &Tree, id: NodeId) -> String {
    let node = &tree.nodes[id.0 as usize];
    let (items, files, folders) = counts(tree, id);
    [
        actions::shell_path(tree.path(id)),
        node.logical.to_string(),
        node.size.to_string(),
        items.to_string(),
        files.to_string(),
        folders.to_string(),
    ]
    .join("\t")
}

/// dirstat's part of the context menu for item `id`, while the treemap shows `treemap_root`. Zoom in is grayed when
/// the treemap already shows the item's folder, Zoom out at the scan root.
pub(crate) fn menu_entries(tree: &Tree, id: NodeId, treemap_root: NodeId) -> [Entry; 8] {
    [
        Entry::Item(AppItem::Explore, true),
        Entry::Item(AppItem::CommandPrompt, true),
        Entry::Item(AppItem::CopyPath, true),
        Entry::Item(AppItem::Export, true),
        Entry::Item(AppItem::CopyInfo, true),
        Entry::Separator,
        Entry::Item(AppItem::ZoomIn, folder_of(tree, id) != treemap_root),
        Entry::Item(AppItem::ZoomOut, treemap_root != tree.root),
    ]
}

/// The treemap root after Zoom out: the parent of `root`, or `root` itself at the scan root.
pub(crate) fn zoom_out_root(tree: &Tree, root: NodeId) -> NodeId {
    tree.nodes[root.0 as usize].parent.unwrap_or(root)
}

/// The item the status line describes: the selection while it lies inside the treemap's root, else the root.
pub(crate) fn status_node(tree: &Tree, selected: Option<NodeId>, treemap_root: NodeId) -> NodeId {
    match selected {
        Some(id) if is_within(tree, id, treemap_root) => id,
        _ => treemap_root,
    }
}

/// The CSV file Export writes for item `id` and everything below it: a header, then one row per folder and file,
/// `id` first and then in tree order. UTF-8 with a byte order mark, CRLF line ends.
pub fn csv_export(tree: &Tree, id: NodeId) -> String {
    let mut out = String::from("\u{feff}Path,Size,Allocated,Items,Files,Folders\r\n");
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        let node = &tree.nodes[current.0 as usize];
        let (items, files, folders) = counts(tree, current);
        let fields = [
            csv_field(&actions::shell_path(tree.path(current))),
            node.logical.to_string(),
            node.size.to_string(),
            items.to_string(),
            files.to_string(),
            folders.to_string(),
        ];
        out += &fields.join(",");
        out += "\r\n";
        // Pushed in reverse so the children come out in tree order.
        stack.extend(node.children.iter().rev().copied());
    }
    out
}

/// `s` as one RFC 4180 field: quoted when it holds a comma, a quote or a line break, with its quotes doubled.
fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
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
    fn item_still_there_after_the_menu_stays() {
        let (mut state, _, f) = state_with_file();
        state.forget_if_gone(f, |path| {
            assert_eq!(path, Path::new(r"C:\a\f.bin"));
            true
        });
        assert_eq!(root_size(&state), 17);
        assert_eq!(state.selected, Some(f));
    }

    #[test]
    fn item_gone_after_the_menu_leaves_the_tree() {
        let (mut state, a, f) = state_with_file();
        state.treemap_cache = Some(TreemapCache {
            size: (1.0, 1.0),
            blocks: Vec::new(),
            mesh: Default::default(),
            origin: egui::Pos2::ZERO,
            dirs: Vec::new(),
            highlight_for: None,
            highlight: Default::default(),
            dark: false,
        });
        state.forget_if_gone(f, |_| false);
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(tree.nodes[a.0 as usize].size, 0);
        assert_eq!(tree.nodes[a.0 as usize].files, 0);
        assert_eq!(root_size(&state), 7);
        assert!(!tree.nodes[a.0 as usize].children.contains(&f));
        assert_eq!(state.selected, Some(a));
        assert!(state.treemap_cache.is_none());
    }

    #[test]
    fn root_is_never_forgotten() {
        let (mut state, _, _) = state_with_file();
        let root = state.tree.as_ref().unwrap().root;
        state.forget_if_gone(root, |_| panic!("the root must not be checked"));
        assert_eq!(root_size(&state), 17);
    }

    #[test]
    fn detached_item_is_never_checked() {
        let (mut state, a, f) = state_with_file();
        state.tree.as_mut().unwrap().remove(a);
        state.forget_if_gone(f, |_| panic!("a detached item must not be checked"));
        assert_eq!(root_size(&state), 7);
    }

    #[test]
    fn unaccounted_item_has_no_menu_and_is_never_forgotten() {
        let (mut state, _, f) = state_with_file();
        let tree = state.tree.as_mut().unwrap();
        tree.add_unaccounted(100);
        let item = tree.nodes[tree.root.0 as usize].children[0];
        assert!(tree.is_unaccounted(item));
        assert!(!has_shell_menu(tree, item));
        assert!(!has_shell_menu(tree, tree.root));
        assert!(has_shell_menu(tree, f));
        assert!(!has_menu(tree, item));
        assert!(has_menu(tree, tree.root));
        state.forget_if_gone(item, |_| panic!("the unaccounted item must not be checked"));
        assert_eq!(root_size(&state), 100);
    }

    #[test]
    fn rescan_clears_pending_menu() {
        let (mut state, _, f) = state_with_file();
        state.pending_menu = Some((f, false));
        let tx = state.install_channel();
        tx.send(ScanEvent::Done { tree: tree(), mode: ScanMode::Walk, skipped: 0 }).unwrap();
        state.pump();
        assert!(state.pending_menu.is_none());
    }

    #[test]
    fn info_line_is_tab_separated_for_files_and_folders() {
        let (state, a, f) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(info_line(tree, f), "C:\\a\\f.bin\t10\t10\t1\t1\t0");
        assert_eq!(info_line(tree, a), "C:\\a\t10\t10\t1\t1\t0");
        assert_eq!(info_line(tree, tree.root), "C:\\\t17\t17\t3\t2\t1");
    }

    #[test]
    fn csv_export_has_bom_header_and_one_row_per_item() {
        let mut b = TreeBuilder::new("C:");
        let dir = b.add(NodeId(0), "a, \"b\"", true, 0, 0);
        b.add(dir, "line\nbreak.txt", false, 10, 8);
        b.add(NodeId(0), "plain.bin", false, 7, 7);
        let tree = b.finish();
        let expected = "\u{feff}Path,Size,Allocated,Items,Files,Folders\r\n\
            C:\\,15,17,3,2,1\r\n\
            \"C:\\a, \"\"b\"\"\",8,10,1,1,0\r\n\
            \"C:\\a, \"\"b\"\"\\line\nbreak.txt\",8,10,1,1,0\r\n\
            C:\\plain.bin,7,7,1,1,0\r\n";
        assert_eq!(csv_export(&tree, tree.root), expected);
        assert_eq!(&csv_export(&tree, tree.root).as_bytes()[..3], [0xEF, 0xBB, 0xBF]);
    }

    #[test]
    fn csv_field_quotes_only_what_needs_it() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("two\r\nlines"), "\"two\r\nlines\"");
    }

    #[test]
    fn zoom_out_goes_to_the_parent_and_stops_at_the_root() {
        let (state, a, _) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(zoom_out_root(tree, a), tree.root);
        assert_eq!(zoom_out_root(tree, tree.root), tree.root);
    }

    #[test]
    fn zoom_menu_items_are_grayed_when_they_would_do_nothing() {
        let (state, a, f) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        let root = tree.root;
        let zoom = |entries: [Entry; 8]| (entries[6], entries[7]);
        // Scan root shown: the root's own folder is already shown, and there is nothing above it.
        assert_eq!(zoom(menu_entries(tree, root, root)), (Entry::Item(AppItem::ZoomIn, false), Entry::Item(AppItem::ZoomOut, false)));
        // Folder a shown: a file in it is shown already, and the root is one step up.
        assert_eq!(zoom(menu_entries(tree, f, a)), (Entry::Item(AppItem::ZoomIn, false), Entry::Item(AppItem::ZoomOut, true)));
        // Folder a shown, the root's own item (g.bin) zooms in to the root, which is not shown yet.
        assert_eq!(zoom(menu_entries(tree, NodeId(3), a)), (Entry::Item(AppItem::ZoomIn, true), Entry::Item(AppItem::ZoomOut, true)));
    }

    #[test]
    fn zoom_actions_move_the_treemap_root_and_drop_its_layout() {
        let (mut state, a, f) = state_with_file();
        let root = state.tree.as_ref().unwrap().root;
        state.treemap_cache = Some(TreemapCache {
            size: (1.0, 1.0),
            blocks: Vec::new(),
            mesh: Default::default(),
            origin: egui::Pos2::ZERO,
            dirs: Vec::new(),
            highlight_for: None,
            highlight: Default::default(),
            dark: false,
        });
        state.app_item(f, AppItem::ZoomIn);
        assert_eq!(state.treemap_root, a);
        assert!(state.treemap_cache.is_none());
        assert_eq!(state.selected, Some(f), "the tree selection stays");
        state.app_item(f, AppItem::ZoomOut);
        assert_eq!(state.treemap_root, root);
    }

    #[test]
    fn removing_the_treemap_root_moves_it_to_its_parent() {
        let (mut state, a, _) = state_with_file();
        let root = state.tree.as_ref().unwrap().root;
        state.treemap_root = a;
        state.forget_if_gone(a, |_| false);
        assert_eq!(state.treemap_root, root);
    }

    #[test]
    fn removing_another_item_keeps_the_treemap_root() {
        let (mut state, a, _) = state_with_file();
        state.treemap_root = a;
        state.forget_if_gone(NodeId(3), |_| false); // g.bin
        assert_eq!(state.treemap_root, a);
    }

    #[test]
    fn status_follows_the_selection_inside_the_treemap_root() {
        let (state, a, f) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(status_node(tree, Some(f), a), f);
        assert_eq!(status_node(tree, Some(NodeId(3)), a), a, "g.bin is outside folder a");
        assert_eq!(status_node(tree, None, a), a);
    }

    #[test]
    fn folder_of_is_the_item_or_its_parent() {
        let (state, a, f) = state_with_file();
        let tree = state.tree.as_ref().unwrap();
        assert_eq!(folder_of(tree, a), a);
        assert_eq!(folder_of(tree, f), a);
        assert_eq!(folder_of(tree, tree.root), tree.root);
    }

    #[test]
    fn exists_sees_files_and_misses_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("here.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(exists(&file));
        assert!(exists(dir.path()));
        assert!(!exists(&dir.path().join("gone.txt")));
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
    fn disk_summary_shows_used_free_and_total() {
        let gb = 1024 * 1024 * 1024;
        assert_eq!(disk_summary(Some((1000 * gb, 250 * gb))), "750.0 GB used · 250.0 GB free · 1000.0 GB");
        assert_eq!(disk_summary(None), "");
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
