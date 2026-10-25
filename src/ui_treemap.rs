//! Treemap pane (bottom).

use eframe::egui::{self, Align2, Color32, FontId, Mesh, Pos2, Sense, Shape, Vec2};
use crate::app::AppState;
use crate::model::{NodeId, Tree};
use crate::treemap;
use crate::ui_legend::color_for;
use crate::actions::shell_path;
use crate::ui_tree::{apply_menu_action, ensure_visible, format_size, item_menu, placeholder};
use std::sync::Arc;

/// Above this many selected blocks the outline is one bounding box, so a huge selection costs no more than a small one.
const HIGHLIGHT_LIMIT: usize = 2000;
const OUTLINE_W: f32 = 2.0;
/// Height of the name strip at the top of a large folder.
const HEADER_H: f32 = 14.0;
const DIR_FILL: Color32 = Color32::from_gray(52);

/// Layout of the pane at one size. Both meshes are kept in absolute screen coordinates, placed at `origin`.
/// `highlight` outlines the selection and is refreshed only when `selected` changes.
pub struct TreemapCache {
    pub size: (f32, f32),
    pub blocks: Vec<(NodeId, treemap::Rect)>,
    /// Folders drawn as a frame with their name in a strip at the top, each before its subfolders.
    pub dirs: Vec<(NodeId, treemap::Rect)>,
    pub mesh: Arc<Mesh>,
    pub origin: Pos2,
    pub highlight_for: Option<NodeId>,
    pub highlight: Arc<Mesh>,
}

impl TreemapCache {
    fn new(tree: &Tree, size: (f32, f32)) -> Self {
        let bounds = treemap::Rect { x: 0.0, y: 0.0, w: size.0, h: size.1 };
        let (blocks, dirs) = treemap::layout_with_headers(tree, tree.root, bounds, HEADER_H);
        let mut mesh = Mesh::default();
        for (_, d) in &dirs {
            mesh.add_colored_rect(to_egui(d, Pos2::ZERO), DIR_FILL);
        }
        for (id, b) in &blocks {
            add_shaded_rect(&mut mesh, to_egui(b, Pos2::ZERO), color_for(tree.nodes[id.0 as usize].ext));
        }
        Self {
            size,
            blocks,
            dirs,
            mesh: Arc::new(mesh),
            origin: Pos2::ZERO,
            highlight_for: None,
            highlight: Arc::new(Mesh::default()),
        }
    }

    /// Moves both meshes to `origin`. Costs a pass over the vertices only when the pane has moved.
    fn place(&mut self, origin: Pos2) {
        if origin != self.origin {
            let delta = origin - self.origin;
            Arc::make_mut(&mut self.mesh).translate(delta);
            Arc::make_mut(&mut self.highlight).translate(delta);
            self.origin = origin;
        }
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    ui.vertical_centered(|ui| ui.strong(shell_path(tree.path(tree.root))));
    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, Sense::click());
    let origin = response.rect.min;

    let size = (size.x, size.y);
    if needs_relayout(state.treemap_cache.as_ref().map(|c| c.size), size) {
        state.treemap_cache = None;
    }
    let cache = state.treemap_cache.get_or_insert_with(|| TreemapCache::new(tree, size));
    cache.place(origin);
    if cache.highlight_for != state.selected {
        // A framed folder is outlined as one box, anything else block by block.
        let rects = state.selected.map_or_else(Vec::new, |id| match cache.dirs.iter().find(|&&(d, _)| d == id) {
            Some(&(_, r)) => vec![r],
            None => blocks_under(tree, &cache.blocks, id),
        });
        cache.highlight = Arc::new(outline_mesh(&highlight_outline_rects(&rects), origin));
        cache.highlight_for = state.selected;
    }

    painter.add(Shape::Mesh(Arc::clone(&cache.mesh)));
    for (id, d) in &cache.dirs {
        let node = &tree.nodes[id.0 as usize];
        let strip = egui::Rect::from_min_size(origin + Vec2::new(d.x + 3.0, d.y), Vec2::new(d.w - 4.0, HEADER_H));
        painter.with_clip_rect(strip.intersect(response.rect)).text(
            strip.left_center(),
            Align2::LEFT_CENTER,
            format!("{} ({})", node.name, format_size(node.size)),
            FontId::proportional(11.0),
            Color32::from_gray(225),
        );
    }
    painter.add(Shape::Mesh(Arc::clone(&cache.highlight)));

    // A file block, or else the innermost framed folder (its name strip or frame).
    let hit = |pos: Pos2| {
        let (x, y) = (pos.x - origin.x, pos.y - origin.y);
        hit_test(&cache.blocks, x, y).or_else(|| hit_test(cache.dirs.iter().rev(), x, y))
    };
    if response.clicked() || response.secondary_clicked() {
        if let Some(id) = response.interact_pointer_pos().and_then(hit) {
            state.selected = Some(id);
            ensure_visible(tree, &mut state.expanded, id);
            state.scroll_to_selected = true;
            ui.ctx().request_repaint();
        }
    }
    // The menu acts on the selection, which a right-click on a block has just set.
    // The unaccounted item is no file: nothing to open, copy or delete.
    let menu = state
        .selected
        .filter(|&id| !tree.is_unaccounted(id))
        .and_then(|id| item_menu(&response, id != tree.root).map(|action| (id, action)));
    if let Some(id) = response.hover_pos().and_then(hit) {
        let node = &tree.nodes[id.0 as usize];
        response.on_hover_ui_at_pointer(|ui| {
            ui.label(tree.path(id));
            ui.label(format_size(node.size));
            ui.label(format!("Logical size: {}", format_size(node.logical)));
        });
    }
    if let Some((id, action)) = menu {
        apply_menu_action(state, id, action);
    }
}

/// True when the pane must be laid out again. Sizes equal after rounding to whole pixels count as unchanged.
pub fn needs_relayout(old: Option<(f32, f32)>, new: (f32, f32)) -> bool {
    old.is_none_or(|o| o.0.round() != new.0.round() || o.1.round() != new.1.round())
}

/// The outline rects to draw: the rects themselves up to `HIGHLIGHT_LIMIT`, past it one box around all of them.
pub fn highlight_outline_rects(rects: &[treemap::Rect]) -> Vec<treemap::Rect> {
    if rects.len() <= HIGHLIGHT_LIMIT {
        return rects.to_vec();
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY);
    for r in rects {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.x + r.w);
        y1 = y1.max(r.y + r.h);
    }
    vec![treemap::Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }]
}

fn to_egui(r: &treemap::Rect, origin: Pos2) -> egui::Rect {
    egui::Rect::from_min_size(origin + Vec2::new(r.x, r.y), Vec2::new(r.w, r.h))
}

/// A block shaded like a cushion: lighter at the top left, darker at the bottom right.
fn add_shaded_rect(mesh: &mut Mesh, r: egui::Rect, color: Color32) {
    let scale = |f: f32| {
        let c = |v: u8| (v as f32 * f).min(255.0) as u8;
        Color32::from_rgb(c(color.r()), c(color.g()), c(color.b()))
    };
    let i = mesh.vertices.len() as u32;
    mesh.colored_vertex(r.left_top(), scale(1.3));
    mesh.colored_vertex(r.right_top(), color);
    mesh.colored_vertex(r.left_bottom(), color);
    mesh.colored_vertex(r.right_bottom(), scale(0.55));
    mesh.add_triangle(i, i + 1, i + 2);
    mesh.add_triangle(i + 1, i + 3, i + 2);
}

/// Four thin white edges per rect, in absolute coordinates for a pane at `origin`.
fn outline_mesh(rects: &[treemap::Rect], origin: Pos2) -> Mesh {
    let mut mesh = Mesh::default();
    for r in rects {
        let min = origin + Vec2::new(r.x, r.y);
        let max = min + Vec2::new(r.w, r.h);
        let t = OUTLINE_W;
        for edge in [
            egui::Rect::from_min_max(min, Pos2::new(max.x, min.y + t)),
            egui::Rect::from_min_max(Pos2::new(min.x, max.y - t), max),
            egui::Rect::from_min_max(min, Pos2::new(min.x + t, max.y)),
            egui::Rect::from_min_max(Pos2::new(max.x - t, min.y), max),
        ] {
            mesh.add_colored_rect(edge, Color32::WHITE);
        }
    }
    mesh
}

/// The block containing (`x`, `y`), in the same local coordinates as `blocks`; `None` over a gap.
pub fn hit_test<'a>(blocks: impl IntoIterator<Item = &'a (NodeId, treemap::Rect)>, x: f32, y: f32) -> Option<NodeId> {
    blocks
        .into_iter()
        .find(|(_, r)| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
        .map(|(id, _)| *id)
}

/// The rects of `id` itself when it is a file, or of every drawn descendant file when it is a directory.
pub fn blocks_under(tree: &Tree, blocks: &[(NodeId, treemap::Rect)], id: NodeId) -> Vec<treemap::Rect> {
    blocks
        .iter()
        .filter(|(block, _)| is_within(tree, *block, id))
        .map(|(_, r)| *r)
        .collect()
}

/// True when `node` is `ancestor` or lies below it.
fn is_within(tree: &Tree, mut node: NodeId, ancestor: NodeId) -> bool {
    loop {
        if node == ancestor {
            return true;
        }
        match tree.nodes[node.0 as usize].parent {
            Some(parent) => node = parent,
            None => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;

    fn block(x: f32, y: f32, w: f32, h: f32) -> treemap::Rect {
        treemap::Rect { x, y, w, h }
    }

    #[test]
    fn hit_test_finds_block_and_misses_gap() {
        let blocks = [
            (NodeId(1), block(0.0, 0.0, 50.0, 100.0)),
            (NodeId(2), block(50.0, 0.0, 50.0, 50.0)),
        ];
        assert_eq!(hit_test(&blocks, 10.0, 10.0), Some(NodeId(1)));
        assert_eq!(hit_test(&blocks, 50.0, 10.0), Some(NodeId(2)));
        assert_eq!(hit_test(&blocks, 60.0, 80.0), None);
        assert_eq!(hit_test(&blocks, 120.0, 10.0), None);
    }

    #[test]
    fn blocks_under_directory_returns_descendant_blocks() {
        let mut b = TreeBuilder::new("root");
        let dir = b.add(NodeId(0), "dir", true, 0, 0);
        let f1 = b.add(dir, "f1.bin", false, 10, 10);
        let f2 = b.add(dir, "f2.bin", false, 10, 10);
        let g = b.add(NodeId(0), "g.bin", false, 10, 10);
        let tree = b.finish();
        let (r1, r2, r3) = (block(0.0, 0.0, 1.0, 1.0), block(1.0, 0.0, 1.0, 1.0), block(2.0, 0.0, 1.0, 1.0));
        let blocks = [(f1, r1), (f2, r2), (g, r3)];
        assert_eq!(blocks_under(&tree, &blocks, dir), vec![r1, r2]);
        assert_eq!(blocks_under(&tree, &blocks, tree.root), vec![r1, r2, r3]);
    }

    #[test]
    fn blocks_under_file_returns_its_block() {
        let mut b = TreeBuilder::new("root");
        let g = b.add(NodeId(0), "g.bin", false, 10, 10);
        let other = b.add(NodeId(0), "o.bin", false, 10, 10);
        let tree = b.finish();
        let blocks = [(g, block(0.0, 0.0, 1.0, 1.0)), (other, block(1.0, 0.0, 1.0, 1.0))];
        assert_eq!(blocks_under(&tree, &blocks, g), vec![block(0.0, 0.0, 1.0, 1.0)]);
    }

    #[test]
    fn needs_relayout_ignores_subpixel_jitter() {
        assert!(!needs_relayout(Some((100.0, 50.0)), (100.4, 49.6)));
    }

    #[test]
    fn needs_relayout_true_when_none_or_size_changes() {
        assert!(needs_relayout(None, (100.0, 50.0)));
        assert!(needs_relayout(Some((100.0, 50.0)), (101.0, 50.0)));
        assert!(needs_relayout(Some((100.0, 50.0)), (100.0, 51.0)));
    }

    #[test]
    fn few_highlights_stay_individual() {
        let rects = [block(0.0, 0.0, 1.0, 1.0), block(5.0, 5.0, 2.0, 2.0)];
        assert_eq!(highlight_outline_rects(&rects), rects.to_vec());
    }

    #[test]
    fn many_highlights_collapse_to_bounding_box() {
        let rects: Vec<treemap::Rect> = (0..2001).map(|i| block(i as f32, 0.0, 1.0, 1.0)).collect();
        assert_eq!(highlight_outline_rects(&rects), vec![block(0.0, 0.0, 2001.0, 1.0)]);
    }
}
