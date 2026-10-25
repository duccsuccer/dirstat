//! Extension legend pane (top-right).

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, TextStyle, Vec2};
use crate::app::AppState;
use crate::ui_tree::placeholder;

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
const SHARE_W: f32 = 70.0;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    let total = tree.nodes[tree.root.0 as usize].size;
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, row_h, tree.exts.len(), |ui, range| {
            for rank in range {
                let ext = &tree.exts[rank];
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
                let text_color = ui.visuals().text_color();
                let painter = ui.painter();
                let mid = rect.center().y;

                let swatch = Rect::from_min_size(Pos2::new(rect.left() + 4.0, mid - 6.0), Vec2::splat(12.0));
                painter.rect_filled(swatch, 2.0, color_for(rank as u16));

                let name = if ext.ext.is_empty() { "(none)" } else { ext.ext.as_str() };
                let name_clip = Rect::from_min_max(rect.left_top(), Pos2::new(rect.right() - SHARE_W, rect.bottom()));
                painter.with_clip_rect(name_clip).text(
                    Pos2::new(swatch.right() + 8.0, mid),
                    Align2::LEFT_CENTER,
                    name,
                    FontId::default(),
                    text_color,
                );

                let share = if total == 0 { 0.0 } else { ext.size as f64 * 100.0 / total as f64 };
                painter.text(
                    Pos2::new(rect.right() - 4.0, mid),
                    Align2::RIGHT_CENTER,
                    format!("{share:.1}%"),
                    FontId::default(),
                    text_color,
                );
            }
        });
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
}
