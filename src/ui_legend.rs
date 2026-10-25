//! File-type pane (top-right): one row per extension, with the colour it has in the treemap.

use eframe::egui::{self, Align2, Rect, Sense, Vec2};
use crate::app::AppState;
use crate::model::UNACCOUNTED;
use crate::theme::swatch;
use crate::ui_tree::{cells, format_count, format_size, header, placeholder, row_background, row_height, text};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES};

/// Default and narrowest width of the pane in points, margins included. The default fits every column.
pub const WIDTH: f32 = 420.0;
pub const MIN_WIDTH: f32 = 240.0;

/// Title and width of each column; the file type takes the width the others leave, but never less than `FLEX_MIN`.
const COLUMNS: [(&str, f32); 6] = [("", 12.0), ("Extension", 72.0), ("File Type", 0.0), ("Size on disk", 96.0), ("Percent", 56.0), ("Files", 80.0)];
const FLEX: usize = 2;
const FLEX_MIN: f32 = 88.0;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    let type_names = &mut state.type_names;
    let row_h = row_height(ui);
    let widths = COLUMNS.map(|(_, w)| w);
    let min_width = widths.iter().sum::<f32>() + FLEX_MIN;
    let total = tree.nodes[tree.root.0 as usize].size;
    let dark = ui.visuals().dark_mode;

    // A narrow pane scrolls sideways instead of clipping its columns.
    egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
        ui.set_width(ui.available_width().max(min_width));
        header(ui, row_h, &COLUMNS, FLEX);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, row_h, tree.exts.len(), |ui, range| {
                for rank in range {
                    let ext = &tree.exts[rank];
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
                    let cells = cells(rect, &widths, FLEX);
                    let color = ui.visuals().text_color();
                    let painter = ui.painter();

                    row_background(ui, rect, false);
                    let swatch_rect = Rect::from_center_size(cells[0].center(), Vec2::splat(9.0));
                    painter.with_clip_rect(cells[0]).rect_filled(swatch_rect, 0.0, swatch(dark, rank as u16));
                    let (name, kind) = ext_columns(&ext.ext);
                    let kind = kind.unwrap_or_else(|| type_names.entry(ext.ext.clone()).or_insert_with(|| shell_type_name(&ext.ext)).clone());
                    text(painter, cells[1], &name, Align2::LEFT_CENTER, color);
                    text(painter, cells[2], &kind, Align2::LEFT_CENTER, color);
                    text(painter, cells[3], &format_size(ext.size), Align2::RIGHT_CENTER, color);
                    text(painter, cells[4], &format!("{:.1} %", share_of(ext.size, total)), Align2::RIGHT_CENTER, color);
                    text(painter, cells[5], &format_count(ext.count), Align2::RIGHT_CENTER, color);
                }
            });
    });
}

/// `part` as a percentage of `whole`; 0 when `whole` is 0.
fn share_of(part: u64, whole: u64) -> f32 {
    if whole == 0 { 0.0 } else { part as f32 / whole as f32 * 100.0 }
}

/// Extension and file-type column texts for one `Tree::exts` entry. A `None` type means: ask the shell.
pub(crate) fn ext_columns(ext: &str) -> (String, Option<String>) {
    match ext {
        "" => (String::new(), Some("(no extension)".into())),
        "(other)" | UNACCOUNTED => (String::new(), Some(ext.into())),
        _ => (format!(".{ext}"), None),
    }
}

/// What Explorer calls files with extension `ext` (e.g. "Application extension"); empty when the shell has no answer.
fn shell_type_name(ext: &str) -> String {
    let mut info = SHFILEINFOW::default();
    // USEFILEATTRIBUTES: the name is looked up from the extension alone, no file is touched.
    let found = unsafe {
        SHGetFileInfoW(
            &HSTRING::from(format!(".{ext}")),
            FILE_ATTRIBUTE_NORMAL,
            Some(&mut info),
            size_of::<SHFILEINFOW>() as u32,
            SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
        )
    };
    if found == 0 {
        return String::new();
    }
    let len = info.szTypeName.iter().position(|&c| c == 0).unwrap_or(info.szTypeName.len());
    String::from_utf16_lossy(&info.szTypeName[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_columns_name_real_extensions_and_label_the_rest() {
        assert_eq!(ext_columns("dll"), (".dll".to_string(), None));
        assert_eq!(ext_columns(""), (String::new(), Some("(no extension)".into())));
        assert_eq!(ext_columns("(other)"), (String::new(), Some("(other)".into())));
        assert_eq!(ext_columns(UNACCOUNTED), (String::new(), Some(UNACCOUNTED.into())));
    }

    #[test]
    fn default_width_fits_every_column() {
        let fixed: f32 = COLUMNS.iter().map(|&(_, w)| w).sum();
        // The pane's margin is 8 points on each side (app.rs).
        assert!(fixed + FLEX_MIN <= WIDTH - 16.0);
    }

    #[test]
    fn share_of_is_a_percentage_of_the_whole() {
        assert_eq!(share_of(1, 4), 25.0);
        assert_eq!(share_of(5, 0), 0.0);
    }

    #[test]
    fn shell_names_a_known_extension() {
        // Reads the registry only; every Windows install knows .exe.
        assert!(!shell_type_name("exe").is_empty());
    }
}
