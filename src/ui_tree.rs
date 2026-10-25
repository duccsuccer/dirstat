//! Directory tree pane (top-left).

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, TextStyle, Vec2};
use crate::actions;
use crate::app::AppState;
use crate::model::{NodeId, Tree};
use std::collections::HashSet;
use std::path::Path;

const INDENT: f32 = 16.0;
const ARROW_W: f32 = 16.0;
const CELL_PAD: f32 = 4.0;
const SIZE_W: f32 = 110.0;
const SHARE_W: f32 = 130.0;
const BAR_W: f32 = 60.0;
const LOGICAL_W: f32 = 110.0;

enum Click {
    Select(NodeId),
    Toggle(NodeId),
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    let rows = visible_rows(tree, &state.expanded);
    let selected = state.selected;
    let expanded = &state.expanded;
    let row_h = ui.text_style_height(&TextStyle::Body) + 4.0;
    let mut click = None;
    let mut menu = None;

    header(ui, row_h);
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if state.scroll_to_selected {
        state.scroll_to_selected = false;
        if let Some(row) = selected.and_then(|id| rows.iter().position(|&(r, _)| r == id)) {
            scroll = scroll.vertical_scroll_offset(row as f32 * (row_h + ui.spacing().item_spacing.y));
        }
    }
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            for &(id, depth) in &rows[range] {
                let node = &tree.nodes[id.0 as usize];
                let expandable = node.is_dir && !node.children.is_empty();
                let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
                let cols = columns(rect);
                let arrow_rect = Rect::from_min_size(
                    Pos2::new(cols.name.left() + depth as f32 * INDENT, rect.top()),
                    Vec2::new(ARROW_W, row_h),
                );
                let color = ui.visuals().text_color();
                let painter = ui.painter();

                if selected == Some(id) {
                    painter.rect_filled(rect, 0.0, ui.visuals().selection.bg_fill);
                }
                if expandable {
                    arrow(painter, arrow_rect.center(), expanded.contains(&id), color);
                }
                let mid = rect.center().y;
                label(painter, cols.name, Pos2::new(arrow_rect.right(), mid), &node.name, color);
                label(painter, cols.size, Pos2::new(cols.size.left() + CELL_PAD, mid), &format_size(node.size), color);
                share_bar(ui, painter, cols.share, tree.percent_of_parent(id), mid);
                label(painter, cols.logical, Pos2::new(cols.logical.left() + CELL_PAD, mid), &format_size(node.logical), color);

                if response.clicked() {
                    let on_arrow = expandable && response.interact_pointer_pos().is_some_and(|p| arrow_rect.contains(p));
                    click = Some(if on_arrow { Click::Toggle(id) } else { Click::Select(id) });
                }
                if response.secondary_clicked() {
                    click = Some(Click::Select(id));
                }
                if let Some(action) = item_menu(&response, id != tree.root) {
                    menu = Some((id, action));
                }
            }
        });

    match click {
        Some(Click::Select(id)) => state.selected = Some(id),
        Some(Click::Toggle(id)) => {
            if !state.expanded.remove(&id) {
                state.expanded.insert(id);
            }
        }
        None => {}
    }
    if let Some((id, action)) = menu {
        apply_menu_action(state, id, action);
    }
}

pub(crate) enum MenuAction {
    Open,
    CopyPath,
    Delete,
}

/// Right-click menu for one item; "Delete to Recycle Bin" is offered only when `deletable`.
pub(crate) fn item_menu(response: &egui::Response, deletable: bool) -> Option<MenuAction> {
    let mut action = None;
    response.context_menu(|ui| {
        if ui.button("Open in Explorer").clicked() {
            action = Some(MenuAction::Open);
        }
        if ui.button("Copy path").clicked() {
            action = Some(MenuAction::CopyPath);
        }
        if deletable && ui.button("Delete to Recycle Bin").clicked() {
            action = Some(MenuAction::Delete);
        }
    });
    action
}

/// Runs a context-menu choice; delete only opens the confirmation dialog. Open and copy failures go to `error`.
pub(crate) fn apply_menu_action(state: &mut AppState, id: NodeId, action: MenuAction) {
    let Some(tree) = &state.tree else { return };
    let path = tree.path(id);
    let result = match action {
        MenuAction::Open => actions::open_in_explorer(Path::new(&path)).map_err(|e| e.to_string()),
        MenuAction::CopyPath => actions::copy_path(&path),
        MenuAction::Delete => {
            state.pending_delete = Some(id);
            Ok(())
        }
    };
    if let Err(e) = result {
        state.error = Some(format!("{path}: {e}"));
    }
}

/// Shown instead of the tree while there is none: scan progress, then any error.
pub(crate) fn placeholder(ui: &mut egui::Ui, state: &AppState) {
    if state.scan.is_some() {
        ui.label("Scanning…");
    }
    if let Some(error) = &state.error {
        ui.colored_label(Color32::RED, error);
    }
}

/// Node and depth for each visible row, in display order.
pub fn visible_rows(tree: &Tree, expanded: &HashSet<NodeId>) -> Vec<(NodeId, usize)> {
    let mut rows = Vec::new();
    let mut stack = vec![(tree.root, 0)];
    while let Some((id, depth)) = stack.pop() {
        rows.push((id, depth));
        if expanded.contains(&id) {
            // Pushed in reverse so the first (largest) child is popped next.
            for &child in tree.nodes[id.0 as usize].children.iter().rev() {
                stack.push((child, depth + 1));
            }
        }
    }
    rows
}

/// Expands every ancestor of `id`; `id` itself is left as it was.
pub fn ensure_visible(tree: &Tree, expanded: &mut HashSet<NodeId>, id: NodeId) {
    let mut cur = tree.nodes[id.0 as usize].parent;
    while let Some(parent) = cur {
        expanded.insert(parent);
        cur = tree.nodes[parent.0 as usize].parent;
    }
}

/// Human-readable size with 1024-based units, e.g. `1.5 GB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

struct Columns {
    name: Rect,
    size: Rect,
    share: Rect,
    logical: Rect,
}

/// Splits a row into its columns; the name column takes the space the fixed-width columns leave.
fn columns(row: Rect) -> Columns {
    let logical = Rect::from_min_max(Pos2::new(row.right() - LOGICAL_W, row.top()), row.right_bottom());
    let share = Rect::from_min_max(Pos2::new(logical.left() - SHARE_W, row.top()), Pos2::new(logical.left(), row.bottom()));
    let size = Rect::from_min_max(Pos2::new(share.left() - SIZE_W, row.top()), Pos2::new(share.left(), row.bottom()));
    let name = Rect::from_min_max(row.left_top(), Pos2::new(size.left(), row.bottom()));
    Columns { name, size, share, logical }
}

fn header(ui: &mut egui::Ui, row_h: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
    let cols = columns(rect);
    let color = ui.visuals().strong_text_color();
    let mid = rect.center().y;
    let painter = ui.painter();
    label(painter, cols.name, Pos2::new(cols.name.left() + ARROW_W, mid), "Name", color);
    label(painter, cols.size, Pos2::new(cols.size.left() + CELL_PAD, mid), "Size on disk", color);
    label(painter, cols.share, Pos2::new(cols.share.left() + CELL_PAD, mid), "% of parent", color);
    label(painter, cols.logical, Pos2::new(cols.logical.left() + CELL_PAD, mid), "Logical size", color);
}

fn label(painter: &egui::Painter, clip: Rect, pos: Pos2, text: &str, color: Color32) {
    painter.with_clip_rect(clip).text(pos, Align2::LEFT_CENTER, text, FontId::default(), color);
}

fn share_bar(ui: &egui::Ui, painter: &egui::Painter, cell: Rect, percent: f32, mid: f32) {
    let bar = Rect::from_min_size(Pos2::new(cell.left() + CELL_PAD, mid - 4.0), Vec2::new(BAR_W, 8.0));
    painter.rect_filled(bar, 2.0, ui.visuals().extreme_bg_color);
    let filled = (percent / 100.0).clamp(0.0, 1.0);
    painter.rect_filled(
        Rect::from_min_size(bar.min, Vec2::new(BAR_W * filled, 8.0)),
        2.0,
        ui.visuals().hyperlink_color,
    );
    label(painter, cell, Pos2::new(bar.right() + CELL_PAD, mid), &format!("{percent:.1}%"), ui.visuals().text_color());
}

/// Expander triangle: pointing right when collapsed, down when open.
fn arrow(painter: &egui::Painter, center: Pos2, open: bool, color: Color32) {
    let points = if open {
        vec![center + Vec2::new(-4.0, -2.5), center + Vec2::new(4.0, -2.5), center + Vec2::new(0.0, 2.5)]
    } else {
        vec![center + Vec2::new(-2.5, -4.0), center + Vec2::new(-2.5, 4.0), center + Vec2::new(2.5, 0.0)]
    };
    painter.add(Shape::convex_polygon(points, color, Stroke::NONE));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;

    // Arena ids: root 0, a 1, sub 2, f.bin 3, g.bin 4.
    // Display order: root, a (size 100) before g (size 7).
    fn sample() -> Tree {
        let mut b = TreeBuilder::new("root");
        let root = NodeId(0);
        let a = b.add(root, "a", true, 0, 0);
        let sub = b.add(a, "sub", true, 0, 0);
        b.add(sub, "f.bin", false, 100, 100);
        b.add(root, "g.bin", false, 7, 7);
        b.finish()
    }

    fn set(ids: &[u32]) -> HashSet<NodeId> {
        ids.iter().map(|&i| NodeId(i)).collect()
    }

    #[test]
    fn visible_rows_respects_expansion_and_depth() {
        let t = sample();

        assert_eq!(visible_rows(&t, &set(&[])), vec![(NodeId(0), 0)]);
        assert_eq!(
            visible_rows(&t, &set(&[0])),
            vec![(NodeId(0), 0), (NodeId(1), 1), (NodeId(4), 1)]
        );
        assert_eq!(
            visible_rows(&t, &set(&[0, 1])),
            vec![(NodeId(0), 0), (NodeId(1), 1), (NodeId(2), 2), (NodeId(4), 1)]
        );
        assert_eq!(
            visible_rows(&t, &set(&[0, 1, 2])),
            vec![(NodeId(0), 0), (NodeId(1), 1), (NodeId(2), 2), (NodeId(3), 3), (NodeId(4), 1)]
        );
    }

    #[test]
    fn ensure_visible_expands_all_ancestors() {
        let t = sample();
        let mut expanded = HashSet::new();

        ensure_visible(&t, &mut expanded, NodeId(3));

        assert_eq!(expanded, set(&[0, 1, 2]));
    }

    #[test]
    fn deep_chain_visible_rows_does_not_overflow() {
        let mut b = TreeBuilder::new("root");
        let mut parent = NodeId(0);
        for i in 0..1000 {
            parent = b.add(parent, &format!("d{i}"), true, 0, 0);
        }
        b.add(parent, "leaf.txt", false, 1, 1);
        let t = b.finish();
        let all: HashSet<NodeId> = (0..t.nodes.len() as u32).map(NodeId).collect();

        let rows = visible_rows(&t, &all);

        assert_eq!(rows.len(), 1002);
        assert_eq!(rows.last().unwrap().1, 1001);
    }

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1536 * 1024 * 1024), "1.5 GB");
        assert_eq!(format_size(1024u64.pow(4)), "1.0 TB");
        assert_eq!(format_size(1024u64.pow(5)), "1024.0 TB");
    }
}
