//! File-type pane (top-right): one row per extension, with the colour it has in the treemap.

use eframe::egui::{self, Align2, Color32, Rect, Sense, Vec2};
use crate::app::AppState;
use crate::model::UNACCOUNTED;
use crate::ui_tree::{cells, format_count, format_size, header, placeholder, row_background, row_height, share_bar, text};
use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_NORMAL;
use windows::Win32::UI::Shell::{SHGetFileInfoW, SHFILEINFOW, SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES};

const PALETTE: [Color32; 12] = [
    Color32::from_rgb(66, 133, 244),
    Color32::from_rgb(219, 68, 55),
    Color32::from_rgb(244, 180, 0),
    Color32::from_rgb(15, 157, 88),
    Color32::from_rgb(171, 71, 188),
    Color32::from_rgb(0, 172, 193),
    Color32::from_rgb(255, 112, 67),
    Color32::from_rgb(124, 179, 66),
    Color32::from_rgb(121, 85, 72),
    Color32::from_rgb(63, 81, 181),
    Color32::from_rgb(233, 30, 99),
    Color32::from_rgb(158, 157, 36),
];
const GREY: Color32 = Color32::GRAY;
/// Title and width of each column; the file type takes the width the others leave.
const COLUMNS: [(&str, f32); 7] =
    [("", 20.0), ("Extension", 76.0), ("File Type", 0.0), ("Percent", 76.0), ("Size", 76.0), ("Allocated", 76.0), ("Files", 70.0)];
const FLEX: usize = 2;
/// The extensions are ordered by size on disk, the Allocated column.
const SORTED: usize = 5;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    let type_names = &mut state.type_names;
    let total = tree.nodes[tree.root.0 as usize].size;
    let row_h = row_height(ui);
    let widths = COLUMNS.map(|(_, w)| w);

    header(ui, row_h, &COLUMNS, FLEX, SORTED);
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_h, tree.exts.len(), |ui, range| {
            for rank in range {
                let ext = &tree.exts[rank];
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
                let cells = cells(rect, &widths, FLEX);
                let color = ui.visuals().text_color();
                let painter = ui.painter();

                row_background(ui, rect, &cells, rank, false);
                let swatch = Rect::from_center_size(cells[0].center(), Vec2::splat(12.0));
                painter.with_clip_rect(cells[0]).rect_filled(swatch, 2.0, color_for(rank as u16));
                let (name, kind) = ext_columns(&ext.ext);
                let kind = kind.unwrap_or_else(|| type_names.entry(ext.ext.clone()).or_insert_with(|| shell_type_name(&ext.ext)).clone());
                text(painter, cells[1], &name, Align2::LEFT_CENTER, color);
                text(painter, cells[2], &kind, Align2::LEFT_CENTER, color);
                let share = if total == 0 { 0.0 } else { ext.size as f64 * 100.0 / total as f64 };
                share_bar(ui, painter, cells[3], share as f32);
                text(painter, cells[4], &format_size(ext.logical), Align2::RIGHT_CENTER, color);
                text(painter, cells[5], &format_size(ext.size), Align2::RIGHT_CENTER, color);
                text(painter, cells[6], &format_count(ext.count), Align2::RIGHT_CENTER, color);
            }
        });
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

/// Color for the extension at `ext_index` (its rank in `Tree::exts`); ranks past the palette and the directory sentinel share one grey.
pub fn color_for(ext_index: u16) -> Color32 {
    PALETTE.get(ext_index as usize).copied().unwrap_or(GREY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn color_for_is_stable_and_distinct_for_top_ten() {
        let top: HashSet<egui::Color32> = (0..10u16).map(color_for).collect();
        assert_eq!(top.len(), 10);
        assert_eq!(color_for(3), color_for(3));

        let grey = color_for(u16::MAX);
        assert!(!top.contains(&grey));
        assert_eq!(color_for(PALETTE.len() as u16), grey);
        assert_eq!(color_for(u16::MAX - 2), grey);
    }

    #[test]
    fn ext_columns_name_real_extensions_and_label_the_rest() {
        assert_eq!(ext_columns("dll"), (".dll".to_string(), None));
        assert_eq!(ext_columns(""), (String::new(), Some("(no extension)".into())));
        assert_eq!(ext_columns("(other)"), (String::new(), Some("(other)".into())));
        assert_eq!(ext_columns(UNACCOUNTED), (String::new(), Some(UNACCOUNTED.into())));
    }

    #[test]
    fn shell_names_a_known_extension() {
        // Reads the registry only; every Windows install knows .exe.
        assert!(!shell_type_name("exe").is_empty());
    }
}
