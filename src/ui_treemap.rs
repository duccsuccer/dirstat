//! Treemap pane (bottom).

use eframe::egui::{self, Color32, FontId, Mesh, Pos2, Sense, Shape, Vec2};
use eframe::egui::text::{LayoutJob, TextWrapping};
use crate::app::{has_menu, AppState};
use crate::model::{NodeId, Tree};
use crate::treemap;
use crate::theme::{palette, swatch};
use crate::actions::shell_path;
use crate::ui_tree::{ensure_visible, format_size, placeholder};
use std::sync::Arc;

/// Above this many selected blocks the outline is one bounding box, so a huge selection costs no more than a small one.
const HIGHLIGHT_LIMIT: usize = 2000;
const OUTLINE_W: f32 = 2.0;
/// Pixels between neighbouring blocks, left unpainted so the background shows through.
const GAP: f32 = 2.0;
/// Space left and right of a folder label, and the narrowest label worth drawing, in points.
const LABEL_PAD: f32 = 3.0;
const LABEL_MIN_W: f32 = 10.0;
/// Height of the name strip at the top of a large folder, in points.
const HEADER_H: f32 = 14.0;
/// Height of the strip above the treemap that names the shown folder.
const TITLE_H: f32 = 22.0;

/// Layout of the pane at one size. The layout is in physical pixels, so every edge and border line lands on a
/// whole pixel; both meshes are in absolute screen points, placed at `origin`. `highlight` outlines the selection
/// and is refreshed only when `selected` changes.
pub struct TreemapCache {
    /// Pane size in physical pixels.
    pub size: (f32, f32),
    pub blocks: Vec<(NodeId, treemap::Rect)>,
    /// Every folder laid out, each before its subfolders; large ones have a name strip at the top.
    pub dirs: Vec<(NodeId, treemap::Rect)>,
    pub mesh: Arc<Mesh>,
    pub origin: Pos2,
    pub highlight_for: Option<NodeId>,
    pub highlight: Arc<Mesh>,
    /// The look the mesh was coloured for; a change of app mode lays the pane out again.
    pub dark: bool,
}

impl TreemapCache {
    /// Lays out the folder `root` and below it; `size` in physical pixels, `ppp` pixels per point.
    fn new(tree: &Tree, root: NodeId, size: (f32, f32), ppp: f32, dark: bool) -> Self {
        let p = palette(dark);
        let bounds = treemap::Rect { x: 0.0, y: 0.0, w: size.0.floor(), h: size.1.floor() };
        let (blocks, dirs) = treemap::layout_with_headers(tree, root, bounds, header_px(ppp));
        // Paint order: the background colour under everything, then folders (parents first), then files. Each rect
        // leaves its right and bottom GAP pixels unpainted, so the background shows as a gap between neighbours.
        // Labels and the selection outline are drawn over this mesh.
        let mut mesh = Mesh::default();
        mesh.add_colored_rect(to_egui(&bounds, Pos2::ZERO, ppp), p.bg);
        for (_, d) in &dirs {
            mesh.add_colored_rect(to_egui(&inner(d), Pos2::ZERO, ppp), p.neutral);
        }
        for (id, b) in &blocks {
            mesh.add_colored_rect(to_egui(&inner(b), Pos2::ZERO, ppp), swatch(dark, tree.nodes[id.0 as usize].ext));
        }
        Self {
            size,
            blocks,
            dirs,
            mesh: Arc::new(mesh),
            origin: Pos2::ZERO,
            highlight_for: None,
            highlight: Arc::new(Mesh::default()),
            dark,
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
    let root = state.treemap_root;
    let dark = ui.visuals().dark_mode;
    // The path of the shown folder, centred in a thin strip above the blocks.
    let strip = ui
        .allocate_ui_with_layout(Vec2::new(ui.available_width(), TITLE_H), egui::Layout::top_down(egui::Align::Center), |ui| {
            ui.strong(shell_path(tree.path(root)))
        })
        .response
        .rect;
    ui.painter().hline(strip.x_range(), strip.bottom() - 0.5, egui::Stroke::new(1.0, palette(dark).separator));
    let size = ui.available_size();
    let (response, painter) = ui.allocate_painter(size, Sense::click());
    let origin = response.rect.min;
    let ppp = ui.ctx().pixels_per_point();

    // In pixels, so a change of display scale also lays the pane out again.
    let size = (size.x * ppp, size.y * ppp);
    if needs_relayout(state.treemap_cache.as_ref().map(|c| c.size), size) || state.treemap_cache.as_ref().is_some_and(|c| c.dark != dark) {
        state.treemap_cache = None;
    }
    let cache = state.treemap_cache.get_or_insert_with(|| TreemapCache::new(tree, root, size, ppp, dark));
    cache.place(origin);
    if cache.highlight_for != state.selected {
        // A framed folder is outlined as one box, anything else block by block.
        let rects = state.selected.map_or_else(Vec::new, |id| match cache.dirs.iter().find(|&&(d, _)| d == id) {
            Some(&(_, r)) => vec![r],
            None => blocks_under(tree, &cache.blocks, id),
        });
        cache.highlight = Arc::new(outline_mesh(&highlight_outline_rects(&rects), origin, ppp, palette(dark).text));
        cache.highlight_for = state.selected;
    }

    painter.add(Shape::Mesh(Arc::clone(&cache.mesh)));
    // Labels go over the blocks, each clipped to its folder's strip (where no child is laid out) and cut short
    // with an ellipsis when it does not fit.
    let header = header_px(ppp);
    for (id, d) in cache.dirs.iter().filter(|(_, d)| treemap::has_header(*d, header)) {
        let node = &tree.nodes[id.0 as usize];
        let strip = to_egui(&treemap::Rect { h: header, ..inner(d) }, origin, ppp);
        let text_w = strip.width() - LABEL_PAD * 2.0;
        if text_w < LABEL_MIN_W {
            continue;
        }
        let color = palette(dark).text;
        let text = format!("{} ({})", node.name, format_size(node.size));
        let mut job = LayoutJob::simple_singleline(text, FontId::proportional(11.0), color);
        job.wrap = TextWrapping::truncate_at_width(text_w);
        let galley = painter.layout_job(job);
        let pos = Pos2::new(strip.left() + LABEL_PAD, strip.center().y - galley.size().y / 2.0);
        painter.with_clip_rect(strip.intersect(response.rect)).galley(pos, galley, color);
    }
    painter.add(Shape::Mesh(Arc::clone(&cache.highlight)));

    // A file block, or else the innermost folder (its name strip or the background around small items).
    let hit = |pos: Pos2| {
        let (x, y) = to_pixels(pos, origin, ppp);
        hit_test(&cache.blocks, x, y).or_else(|| hit_test(cache.dirs.iter().rev(), x, y))
    };
    if response.clicked() || response.secondary_clicked() {
        if let Some(id) = response.interact_pointer_pos().and_then(hit) {
            state.selected = Some(id);
            ensure_visible(tree, &mut state.expanded, id);
            state.scroll_to_selected = true;
            if response.secondary_clicked() && has_menu(tree, id) {
                state.pending_menu = Some((id, ui.input(|i| i.modifiers.shift)));
            }
            ui.ctx().request_repaint();
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

/// Header strip height in whole pixels.
fn header_px(ppp: f32) -> f32 {
    (HEADER_H * ppp).round()
}

/// `r` less its right and bottom `GAP` pixels, which show the background as the gap between neighbours.
fn inner(r: &treemap::Rect) -> treemap::Rect {
    treemap::Rect { w: r.w - GAP, h: r.h - GAP, ..*r }
}

/// A rect in pixels as screen points for a pane at `origin`.
fn to_egui(r: &treemap::Rect, origin: Pos2, ppp: f32) -> egui::Rect {
    egui::Rect::from_min_size(origin + Vec2::new(r.x, r.y) / ppp, Vec2::new(r.w, r.h) / ppp)
}

/// The inverse of [`to_egui`] for the mouse: screen position `pos` in the pane's pixels. Windows reports the mouse
/// on whole pixels; the half pixel moves to that pixel's centre, which is what decides the rect it is painted in.
fn to_pixels(pos: Pos2, origin: Pos2, ppp: f32) -> (f32, f32) {
    ((pos.x - origin.x) * ppp + 0.5, (pos.y - origin.y) * ppp + 0.5)
}

/// Four thin edges per rect (in pixels) in `color`, in absolute screen points for a pane at `origin`.
fn outline_mesh(rects: &[treemap::Rect], origin: Pos2, ppp: f32, color: Color32) -> Mesh {
    let mut mesh = Mesh::default();
    for r in rects {
        let egui::Rect { min, max } = to_egui(r, origin, ppp);
        let t = OUTLINE_W;
        for edge in [
            egui::Rect::from_min_max(min, Pos2::new(max.x, min.y + t)),
            egui::Rect::from_min_max(Pos2::new(min.x, max.y - t), max),
            egui::Rect::from_min_max(min, Pos2::new(min.x + t, max.y)),
            egui::Rect::from_min_max(Pos2::new(max.x - t, min.y), max),
        ] {
            mesh.add_colored_rect(edge, color);
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
pub(crate) fn is_within(tree: &Tree, mut node: NodeId, ancestor: NodeId) -> bool {
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
    fn a_click_inside_a_drawn_block_hits_that_block_at_any_scale() {
        let blocks = [(NodeId(1), block(0.0, 0.0, 5.0, 4.0)), (NodeId(2), block(5.0, 0.0, 3.0, 4.0))];
        let origin = Pos2::new(10.3, 20.7);
        for ppp in [1.0, 1.25, 1.5, 2.0] {
            for &(id, r) in &blocks {
                let drawn = to_egui(&inner(&r), origin, ppp);
                for pos in [drawn.min, drawn.center(), drawn.max - Vec2::splat(0.01)] {
                    let (x, y) = to_pixels(pos, origin, ppp);
                    assert_eq!(hit_test(&blocks, x, y), Some(id), "ppp {ppp}, {pos:?}");
                }
            }
        }
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
