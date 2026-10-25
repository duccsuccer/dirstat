//! Directory tree pane (top-left), and the table drawing it shares with the file-type pane.

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, TextStyle, Vec2};
use crate::app::{has_menu, AppState};
use crate::model::{NodeId, Tree};
use crate::theme::{palette, swatch};
use std::collections::HashSet;

const INDENT: f32 = 16.0;
const ARROW_W: f32 = 16.0;
const ICON_W: f32 = 16.0;
const CELL_PAD: f32 = 4.0;
/// Title and width of each column; the first takes the width the others leave.
const COLUMNS: [(&str, f32); 5] = [("Folder", 0.0), ("Share", 100.0), ("Size", 80.0), ("Allocated", 80.0), ("Files", 80.0)];

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
    let dark = ui.visuals().dark_mode;
    let mut click = None;
    let mut menu = None;

    header(ui, row_h, &COLUMNS, 0);
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if state.scroll_to_selected {
        state.scroll_to_selected = false;
        if let Some(row) = selected.and_then(|id| rows.iter().position(|&(r, _)| r == id)) {
            scroll = scroll.vertical_scroll_offset(row as f32 * row_h);
        }
    }
    scroll.show_rows(ui, row_h, rows.len(), |ui, range| {
            for &(id, depth) in rows.iter().skip(range.start).take(range.len()) {
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

                row_background(ui, rect, selected == Some(id));
                if expandable {
                    let tint = if response.hovered() { palette(dark).text } else { palette(dark).muted };
                    chevron(painter, arrow_rect, expanded.contains(&id), tint);
                }
                let mid = rect.center().y;
                let icon = Rect::from_center_size(Pos2::new(arrow_rect.right() + ICON_W / 2.0, mid), Vec2::new(12.0, 10.0));
                painter.with_clip_rect(cells[0]).rect_filled(icon, 0.0, if node.is_dir { palette(dark).muted } else { swatch(dark, node.ext) });
                let name_pos = Pos2::new(arrow_rect.right() + ICON_W + CELL_PAD, mid);
                painter.with_clip_rect(cells[0]).text(name_pos, Align2::LEFT_CENTER, &node.name, FontId::default(), color);
                share_bar(ui, painter, cells[1], tree.percent_of_parent(id));
                text(painter, cells[2], &format_size(node.logical), Align2::RIGHT_CENTER, color);
                text(painter, cells[3], &format_size(node.size), Align2::RIGHT_CENTER, color);
                if node.is_dir {
                    text(painter, cells[4], &format_count(u64::from(node.files)), Align2::RIGHT_CENTER, color);
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
        ui.colored_label(palette(ui.visuals().dark_mode).error, error);
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
    ui.text_style_height(&TextStyle::Body) + 10.0
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

/// Small muted column titles over a hairline. Columns after `flex` are right-aligned, as their numbers are.
pub(crate) fn header(ui: &mut egui::Ui, row_h: f32, columns: &[(&str, f32)], flex: usize) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), row_h), Sense::hover());
    let widths: Vec<f32> = columns.iter().map(|&(_, w)| w).collect();
    let pal = palette(ui.visuals().dark_mode);
    let painter = ui.painter();
    for (i, (cell, (title, _))) in cells(rect, &widths, flex).iter().zip(columns).enumerate() {
        let (x, align) = if i > flex { (cell.right() - CELL_PAD, Align2::RIGHT_CENTER) } else { (cell.left() + CELL_PAD, Align2::LEFT_CENTER) };
        painter.with_clip_rect(*cell).text(Pos2::new(x, cell.center().y), align, title, FontId::proportional(11.0), pal.header);
    }
    painter.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, pal.rule));
}

/// The selected row's fill, then the hairline under every row.
pub(crate) fn row_background(ui: &egui::Ui, row: Rect, selected: bool) {
    let pal = palette(ui.visuals().dark_mode);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(row, 0.0, pal.selected);
    }
    painter.hline(row.x_range(), row.bottom() - 0.5, Stroke::new(1.0, pal.rule));
}

/// `s` in `cell`, clipped to it, against its left or right edge.
pub(crate) fn text(painter: &egui::Painter, cell: Rect, s: &str, align: Align2, color: Color32) {
    let x = if align == Align2::RIGHT_CENTER { cell.right() - CELL_PAD } else { cell.left() + CELL_PAD };
    painter.with_clip_rect(cell).text(Pos2::new(x, cell.center().y), align, s, FontId::default(), color);
}

/// A 3px line across the cell, filled to `percent` in the accent colour.
pub(crate) fn share_bar(ui: &egui::Ui, painter: &egui::Painter, cell: Rect, percent: f32) {
    let pal = palette(ui.visuals().dark_mode);
    let track = Rect::from_min_size(
        Pos2::new(cell.left() + CELL_PAD, cell.center().y - 1.5),
        Vec2::new((cell.width() - 2.0 * CELL_PAD).max(0.0), 3.0),
    );
    painter.rect_filled(track, 0.0, pal.rule);
    let filled = (percent / 100.0).clamp(0.0, 1.0);
    painter.rect_filled(Rect::from_min_size(track.min, Vec2::new(track.width() * filled, 3.0)), 0.0, pal.accent);
}

/// The two arms of the expander chevron centred in `r`: pointing down when `open`, right when collapsed.
pub(crate) fn chevron_arms(r: Rect, open: bool) -> [[Pos2; 2]; 2] {
    const H: f32 = 3.5;
    let c = r.center();
    if open {
        [
            [c + Vec2::new(-H, -H / 2.0), c + Vec2::new(0.0, H / 2.0)],
            [c + Vec2::new(0.0, H / 2.0), c + Vec2::new(H, -H / 2.0)],
        ]
    } else {
        [
            [c + Vec2::new(-H / 2.0, -H), c + Vec2::new(H / 2.0, 0.0)],
            [c + Vec2::new(H / 2.0, 0.0), c + Vec2::new(-H / 2.0, H)],
        ]
    }
}

/// Expander chevron as two thin lines, drawn rather than set in a font.
fn chevron(painter: &egui::Painter, r: Rect, open: bool, color: Color32) {
    for [a, b] in chevron_arms(r, open) {
        painter.line_segment([a, b], Stroke::new(1.4, color));
    }
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
    fn chevron_points_down_when_open_and_right_when_closed() {
        let r = Rect::from_min_size(Pos2::new(10.0, 20.0), Vec2::new(16.0, 22.0));
        let c = r.center();
        let [down_a, down_b] = chevron_arms(r, true);
        // Both arms meet at one tip below the centre, and start above it.
        assert_eq!(down_a[1], down_b[0]);
        assert!(down_b[0].y > c.y);
        assert!(down_a[0].y < c.y && down_a[0].x < c.x && down_b[1].x > c.x);
        let [right_a, right_b] = chevron_arms(r, false);
        // The tip is right of the centre; the arms start at the left.
        assert_eq!(right_a[1], right_b[0]);
        assert!(right_b[0].x > c.x);
        assert!(right_a[0].x < c.x && right_b[1].x < c.x);
    }

    #[test]
    fn chevron_stays_inside_its_hit_target() {
        let r = Rect::from_min_size(Pos2::ZERO, Vec2::new(16.0, 16.0));
        for open in [false, true] {
            for [a, b] in chevron_arms(r, open) {
                for p in [a, b] {
                    assert!(r.contains(p), "{p:?} outside {r:?}");
                }
            }
        }
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
