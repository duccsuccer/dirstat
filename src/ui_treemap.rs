//! Treemap pane (bottom).

use eframe::egui::{self, Color32, Pos2, Sense, Stroke, StrokeKind, Vec2};
use crate::app::AppState;
use crate::model::{NodeId, Tree};
use crate::treemap;
use crate::ui_legend::color_for;
use crate::ui_tree::{ensure_visible, format_size, placeholder};

/// Layout of the pane at one size. `highlight` outlines the selection and is refreshed only when `selected` changes.
pub struct TreemapCache {
    pub size: (f32, f32),
    pub blocks: Vec<(NodeId, treemap::Rect)>,
    pub highlight_for: Option<NodeId>,
    pub highlight: Vec<treemap::Rect>,
}

impl TreemapCache {
    fn new(tree: &Tree, size: (f32, f32)) -> Self {
        let bounds = treemap::Rect { x: 0.0, y: 0.0, w: size.0, h: size.1 };
        Self {
            size,
            blocks: treemap::layout(tree, tree.root, bounds),
            highlight_for: None,
            highlight: Vec::new(),
        }
    }
}

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(tree) = state.tree.as_ref() else {
        placeholder(ui, state);
        return;
    };
    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, Sense::click());
    let origin = response.rect.min;

    let size = (size.x, size.y);
    if state.treemap_cache.as_ref().is_some_and(|c| c.size != size) {
        state.treemap_cache = None;
    }
    let cache = state.treemap_cache.get_or_insert_with(|| TreemapCache::new(tree, size));
    if cache.highlight_for != state.selected {
        cache.highlight = state.selected.map_or_else(Vec::new, |id| blocks_under(tree, &cache.blocks, id));
        cache.highlight_for = state.selected;
    }

    for (id, block) in &cache.blocks {
        painter.rect_filled(block_rect(origin, block), 0.0, color_for(tree.nodes[id.0 as usize].ext));
    }
    for block in &cache.highlight {
        painter.rect_stroke(block_rect(origin, block), 0.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
    }

    let hit = |pos: Pos2| hit_test(&cache.blocks, pos.x - origin.x, pos.y - origin.y);
    if response.clicked() {
        if let Some(id) = response.interact_pointer_pos().and_then(hit) {
            state.selected = Some(id);
            ensure_visible(tree, &mut state.expanded, id);
            state.scroll_to_selected = true;
        }
    }
    if let Some(id) = response.hover_pos().and_then(hit) {
        let node = &tree.nodes[id.0 as usize];
        response.on_hover_ui_at_pointer(|ui| {
            ui.label(tree.path(id));
            ui.label(format_size(node.size));
            ui.label(format!("Logical size: {}", format_size(node.logical)));
        });
    }
}

fn block_rect(origin: Pos2, block: &treemap::Rect) -> egui::Rect {
    egui::Rect::from_min_size(origin + Vec2::new(block.x, block.y), Vec2::new(block.w, block.h))
}

/// The block containing (`x`, `y`), in the same local coordinates as `blocks`; `None` over a gap.
pub fn hit_test(blocks: &[(NodeId, treemap::Rect)], x: f32, y: f32) -> Option<NodeId> {
    blocks
        .iter()
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
}
