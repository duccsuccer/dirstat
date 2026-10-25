//! Directory tree pane (top-left), and the table drawing it shares with the file-type pane.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Shape, Stroke, TextStyle, Vec2};
use crate::app::{has_menu, AppState};
use crate::model::{NodeId, Tree};
use crate::ui_legend::color_for;
use std::collections::HashSet;

const INDENT: f32 = 16.0;
const ARROW_W: f32 = 16.0;
const ICON_W: f32 = 16.0;
const CELL_PAD: f32 = 4.0;
/// Title and width of each column; the first takes the width the others leave.
const COLUMNS: [(&str, f32); 7] =
    [("Folder", 0.0), ("% of Parent", 100.0), ("Size", 80.0), ("Allocated", 80.0), ("Items", 80.0), ("Files", 80.0), ("Folders", 70.0)];
/// The tree is ordered by size on disk, the Allocated column.
const SORTED: usize = 3;
const DIR_ICON: Color32 = Color32::from_rgb(230, 190, 60);

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
    let row_h = row_height(ui);
    let widths = COLUMNS.map(|(_, w)| w);
    let mut click = None;
    let mut menu = None;

    header(ui, row_h, &COLUMNS, 0, SORTED);
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if state.scroll_to_selected {
        state.scroll_to_selected = false;
        if let Some(row) = selected.and_then(|id| rows.iter().position(|&(r, _)| r == id)) {
            scroll = scroll.vertical_scroll_offset(row as f32 * row_h);
        }
    }
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            for (index, &(id, depth)) in rows.iter().enumerate().skip(range.start).take(range.len()) {
                let node = &tree.nodes[id.0 as usize];
                let expandable = node.is_dir && !node.children.is_empty();
                let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::click());
                let cells = cells(rect, &widths, 0);
                let arrow_rect = Rect::from_min_size(
                    Pos2::new(cells[0].left() + depth as f32 * INDENT, rect.top()),
                    Vec2::new(ARROW_W, row_h),
                );
                let color = ui.visuals().text_color();
                let painter = ui.painter();

                row_background(ui, rect, &cells, index, selected == Some(id));
                if expandable {
                    arrow(painter, arrow_rect.center(), expanded.contains(&id), color);
                }
                let mid = rect.center().y;
                let icon = Rect::from_center_size(Pos2::new(arrow_rect.right() + ICON_W / 2.0, mid), Vec2::new(12.0, 10.0));
                painter.with_clip_rect(cells[0]).rect_filled(icon, 1.0, if node.is_dir { DIR_ICON } else { color_for(node.ext) });
                let name_pos = Pos2::new(arrow_rect.right() + ICON_W + CELL_PAD, mid);
                painter.with_clip_rect(cells[0]).text(name_pos, Align2::LEFT_CENTER, &node.name, FontId::default(), color);
                share_bar(ui, painter, cells[1], tree.percent_of_parent(id));
                text(painter, cells[2], &format_size(node.logical), Align2::RIGHT_CENTER, color);
                text(painter, cells[3], &format_size(node.size), Align2::RIGHT_CENTER, color);
                if node.is_dir {
                    let (files, folders) = (u64::from(node.files), u64::from(node.folders));
                    text(painter, cells[4], &format_count(files + folders), Align2::RIGHT_CENTER, color);
                    text(painter, cells[5], &format_count(files), Align2::RIGHT_CENTER, color);
                    text(painter, cells[6], &format_count(folders), Align2::RIGHT_CENTER, color);
                }

                if response.clicked() {
                    let on_arrow = expandable && response.interact_pointer_pos().is_some_and(|p| arrow_rect.contains(p));
                    click = Some(if on_arrow { Click::Toggle(id) } else { Click::Select(id) });
                }
                if response.secondary_clicked() {
                    click = Some(Click::Select(id));
                    if has_menu(tree, id) {
                        menu = Some(id);
                    }
                }
            }
        });

    #[allow(clippy::collapsible_match)] // the toggle's `if` cannot become a guard without losing exhaustiveness
    match click {
        Some(Click::Select(id)) => state.selected = Some(id),
        Some(Click::Toggle(id)) => {
            if !state.expanded.remove(&id) {
                state.expanded.insert(id);
            }
        }
        None => {}
    }
    if let Some(id) = menu {
        state.pending_menu = Some((id, ui.input(|i| i.modifiers.shift)));
        ui.ctx().request_repaint();
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

/// `n` with thousands separators, e.g. `2,541,851`.
pub fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

/// Height of one compact table row.
pub(crate) fn row_height(ui: &mut egui::Ui) -> f32 {
    // Rows sit edge to edge so the striping has no gaps.
    ui.spacing_mut().item_spacing.y = 0.0;
    ui.text_style_height(&TextStyle::Body) + 4.0
}

/// Splits `row` into one cell per width; column `flex` takes whatever the other widths leave.
pub(crate) fn cells(row: Rect, widths: &[f32], flex: usize) -> Vec<Rect> {
    let fixed: f32 = widths.iter().enumerate().filter(|&(i, _)| i != flex).map(|(_, w)| w).sum();
    let mut x = row.left();
    widths
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let w = if i == flex { (row.width() - fixed).max(0.0) } else { w };
            let cell = Rect::from_min_max(Pos2::new(x, row.top()), Pos2::new(x + w, row.bottom()));
            x += w;
            cell
        })
        .collect()
}

/// Column titles with grid lines; column `sorted` gets a down arrow, as the rows are ordered by it.
pub(crate) fn header(ui: &mut egui::Ui, row_h: f32, columns: &[(&str, f32)], flex: usize, sorted: usize) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
    let widths: Vec<f32> = columns.iter().map(|&(_, w)| w).collect();
    let cells = cells(rect, &widths, flex);
    let color = ui.visuals().strong_text_color();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, ui.visuals().faint_bg_color);
    for (i, (cell, (title, _))) in cells.iter().zip(columns).enumerate() {
        text(painter, *cell, title, Align2::LEFT_CENTER, color);
        if i == sorted {
            arrow(painter, Pos2::new(cell.right() - 10.0, cell.center().y), true, color);
        }
        grid_line(ui, painter, *cell);
    }
}

/// Selection highlight or the stripe of every other row, then the column grid lines.
pub(crate) fn row_background(ui: &egui::Ui, row: Rect, cells: &[Rect], index: usize, selected: bool) {
    let painter = ui.painter();
    if selected {
        painter.rect_filled(row, 0.0, ui.visuals().selection.bg_fill);
    } else if index % 2 == 1 {
        painter.rect_filled(row, 0.0, ui.visuals().faint_bg_color);
    }
    for cell in cells {
        grid_line(ui, painter, *cell);
    }
}

fn grid_line(ui: &egui::Ui, painter: &egui::Painter, cell: Rect) {
    let stroke = Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color);
    painter.vline(cell.right() - 0.5, cell.y_range(), stroke);
}

/// `s` in `cell`, clipped to it, against its left or right edge.
pub(crate) fn text(painter: &egui::Painter, cell: Rect, s: &str, align: Align2, color: Color32) {
    let x = if align == Align2::RIGHT_CENTER { cell.right() - CELL_PAD } else { cell.left() + CELL_PAD };
    painter.with_clip_rect(cell).text(Pos2::new(x, cell.center().y), align, s, FontId::default(), color);
}

/// A bar filled to `percent` of the cell, with the number over it.
pub(crate) fn share_bar(ui: &egui::Ui, painter: &egui::Painter, cell: Rect, percent: f32) {
    let bar = cell.shrink2(Vec2::new(CELL_PAD, 2.0));
    painter.rect_filled(bar, 0.0, ui.visuals().extreme_bg_color);
    let filled = (percent / 100.0).clamp(0.0, 1.0);
    painter.rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * filled, bar.height())), 0.0, Color32::from_gray(95));
    text(painter, cell, &format!("{percent:.1} %"), Align2::RIGHT_CENTER, ui.visuals().text_color());
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
    fn format_count_groups_thousands() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1000), "1,000");
        assert_eq!(format_count(2_541_851), "2,541,851");
        assert_eq!(format_count(u64::from(u32::MAX)), "4,294,967,295");
    }

    #[test]
    fn cells_give_the_flex_column_the_rest() {
        let row = Rect::from_min_size(Pos2::new(10.0, 0.0), Vec2::new(300.0, 20.0));
        let c = cells(row, &[0.0, 100.0, 50.0], 0);
        assert_eq!((c[0].left(), c[0].width()), (10.0, 150.0));
        assert_eq!((c[1].left(), c[1].width()), (160.0, 100.0));
        assert_eq!((c[2].left(), c[2].right()), (260.0, 310.0));
        let c = cells(row, &[20.0, 0.0, 50.0], 1);
        assert_eq!((c[1].left(), c[1].width()), (30.0, 230.0));
        // Too narrow: the flex column shrinks to nothing, never below.
        let c = cells(row, &[0.0, 400.0], 0);
        assert_eq!(c[0].width(), 0.0);
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
