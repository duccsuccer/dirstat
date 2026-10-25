//! Squarified treemap layout (Bruls, Huizing, van Wijk 2000) over the scan tree.

use crate::model::{NodeId, Tree};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Nodes with the rect each is drawn in.
pub type Rects = Vec<(NodeId, Rect)>;

/// Narrowest folder rect that gets a header strip.
const MIN_HEADER_W: f32 = 40.0;
/// Rects narrower or lower than this many pixels are not laid out: their area shows as their folder's background.
pub const MIN_DRAWN: f32 = 2.0;

/// Returns one rect per drawn file block. Directories are subdivided by size and never drawn.
pub fn layout(tree: &Tree, root: NodeId, bounds: Rect) -> Vec<(NodeId, Rect)> {
    layout_with_headers(tree, root, bounds, 0.0).0
}

/// Like [`layout`], but a folder rect at least `MIN_HEADER_W` wide and three headers high keeps a `header`-high strip
/// at its top for its name and its children fill the rest ([`content_rect`]). Returns the file blocks and every
/// folder laid out (the root first, each folder before its subfolders). A `header` of 0 gives no strips.
///
/// With `bounds` on whole pixels every rect is on whole pixels, lies inside its folder's content rect, and the
/// children of a folder tile that rect with shared edges, no gaps and no overlaps. Rects under `MIN_DRAWN` pixels
/// in width or height are left out (with their subtree), so their area shows as their folder's background.
pub fn layout_with_headers(tree: &Tree, root: NodeId, bounds: Rect, header: f32) -> (Rects, Rects) {
    let mut out = Vec::new();
    let mut dirs = Vec::new();
    // Explicit stack instead of recursion, so deep directory chains cannot overflow the call stack.
    let mut stack = vec![(root, bounds)];
    while let Some((dir, rect)) = stack.pop() {
        if !is_drawn(rect) {
            continue;
        }
        dirs.push((dir, rect));
        let rect = content_rect(rect, header);
        let children: Vec<NodeId> = tree.nodes[dir.0 as usize]
            .children
            .iter()
            .copied()
            .filter(|c| tree.nodes[c.0 as usize].size > 0)
            .collect();
        let total: u64 = children.iter().map(|c| tree.nodes[c.0 as usize].size).sum();
        if total == 0 {
            continue;
        }
        let scale = area(rect) / total as f32;
        let areas: Vec<f32> = children
            .iter()
            .map(|c| tree.nodes[c.0 as usize].size as f32 * scale)
            .collect();
        for (child, r) in children.into_iter().zip(squarify(&areas, rect)) {
            if !is_drawn(r) {
                continue;
            }
            if tree.nodes[child.0 as usize].is_dir {
                stack.push((child, r));
            } else {
                out.push((child, r));
            }
        }
    }
    (out, dirs)
}

/// True when a folder laid out in `r` gets a `header`-high name strip at its top.
pub fn has_header(r: Rect, header: f32) -> bool {
    header > 0.0 && r.w >= MIN_HEADER_W && r.h >= 3.0 * header
}

/// The part of a folder rect `r` its children are laid out in: below its name strip when it has one.
pub fn content_rect(r: Rect, header: f32) -> Rect {
    let top = if has_header(r, header) { header } else { 0.0 };
    Rect { x: r.x, y: r.y + top, w: r.w, h: r.h - top }
}

fn is_drawn(r: Rect) -> bool {
    r.w >= MIN_DRAWN && r.h >= MIN_DRAWN
}

fn area(r: Rect) -> f32 {
    r.w * r.h
}

/// Squarified subdivision of `rect` into one rect per entry of `areas`, which must sum to the area of `rect`. Edges
/// are rounded to whole pixels and neighbours share them exactly, so for a `rect` on whole pixels the rects tile it
/// with no gap or overlap. A rect rounded away has zero width or height.
fn squarify(areas: &[f32], rect: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(areas.len());
    // Edges of the part still free. They stay unrounded until a rect is emitted, so an edge two rects share is the
    // same float for both and rounds the same way; the last item of a row and the last row end on the far edge.
    let (mut x0, mut y0, x1, y1) = (rect.x, rect.y, rect.x + rect.w, rect.y + rect.h);
    let mut i = 0;
    while i < areas.len() {
        let (w, h) = (x1 - x0, y1 - y0);
        if w <= 0.0 || h <= 0.0 {
            // Rounding left no room: the remaining children get empty rects and are dropped by the caller.
            out.resize(areas.len(), snapped(x0, y0, x0, y0));
            break;
        }
        let side = w.min(h);
        let mut sum = areas[i];
        let (mut min, mut max) = (areas[i], areas[i]);
        let mut worst = worst_ratio(sum, min, max, side);
        let mut end = i + 1;
        while end < areas.len() {
            let a = areas[end];
            let (s, mn, mx) = (sum + a, min.min(a), max.max(a));
            let next = worst_ratio(s, mn, mx, side);
            if next > worst {
                break;
            }
            (sum, min, max, worst) = (s, mn, mx, next);
            end += 1;
        }
        let row = &areas[i..end];
        let last_row = end == areas.len();
        if w >= h {
            // Lay the row as a column on the left edge.
            let cx = if last_row { x1 } else { (x0 + sum / h).min(x1) };
            let mut y = y0;
            for (k, &a) in row.iter().enumerate() {
                let ny = if k + 1 == row.len() { y1 } else { (y + h * a / sum).min(y1) };
                out.push(snapped(x0, y, cx, ny));
                y = ny;
            }
            x0 = cx;
        } else {
            // Lay the row as a strip along the top edge.
            let cy = if last_row { y1 } else { (y0 + sum / w).min(y1) };
            let mut x = x0;
            for (k, &a) in row.iter().enumerate() {
                let nx = if k + 1 == row.len() { x1 } else { (x + w * a / sum).min(x1) };
                out.push(snapped(x, y0, nx, cy));
                x = nx;
            }
            y0 = cy;
        }
        i = end;
    }
    out
}

/// The rect between the given edges, each rounded to the nearest whole pixel.
fn snapped(x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
    let (x0, y0, x1, y1) = (x0.round(), y0.round(), x1.round(), y1.round());
    Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
}

/// Worst aspect ratio of a row with the given area sum and min/max item area, laid along a side of length `side`.
fn worst_ratio(sum: f32, min: f32, max: f32, side: f32) -> f32 {
    let s2 = side * side;
    (s2 * max / (sum * sum)).max(sum * sum / (s2 * min))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;

    const BOUNDS: Rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
    const EPS: f32 = 1e-3;

    fn area(r: Rect) -> f32 {
        r.w * r.h
    }

    fn file_tree(sizes: &[u64]) -> Tree {
        let mut b = TreeBuilder::new("root");
        for (i, &s) in sizes.iter().enumerate() {
            b.add(NodeId(0), &format!("f{i}.bin"), false, s, s);
        }
        b.finish()
    }

    fn rect_of(tree: &Tree, blocks: &[(NodeId, Rect)], name: &str) -> Rect {
        let (_, r) = blocks
            .iter()
            .find(|(id, _)| tree.nodes[id.0 as usize].name == name)
            .unwrap_or_else(|| panic!("no block named {name}"));
        *r
    }

    fn overlap_area(a: Rect, b: Rect) -> f32 {
        let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
        let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
        if w > 0.0 && h > 0.0 { w * h } else { 0.0 }
    }

    #[test]
    fn areas_proportional_to_size() {
        let tree = file_tree(&[50, 30, 20]);
        let blocks = layout(&tree, tree.root, BOUNDS);
        assert_eq!(blocks.len(), 3);
        for (id, expected) in [(NodeId(1), 5000.0), (NodeId(2), 3000.0), (NodeId(3), 2000.0)] {
            let (_, r) = blocks.iter().find(|(b, _)| *b == id).expect("block present");
            assert!((area(*r) - expected).abs() <= expected * 0.01, "{id:?}: {r:?}");
        }
    }

    #[test]
    fn blocks_stay_inside_bounds_and_do_not_overlap() {
        let tree = file_tree(&[400, 250, 150, 100, 50, 30]);
        let blocks = layout(&tree, tree.root, BOUNDS);
        assert_eq!(blocks.len(), 6);
        for (i, (_, r)) in blocks.iter().enumerate() {
            assert!(r.x >= -EPS && r.y >= -EPS, "{r:?} starts outside");
            assert!(r.x + r.w <= 100.0 + EPS && r.y + r.h <= 100.0 + EPS, "{r:?} ends outside");
            for (_, other) in &blocks[i + 1..] {
                assert!(overlap_area(*r, *other) <= EPS, "{r:?} overlaps {other:?}");
            }
        }
    }

    #[test]
    fn subpixel_blocks_are_dropped() {
        let mut sizes = vec![1_000_000u64];
        sizes.extend(std::iter::repeat_n(1, 1000));
        let tree = file_tree(&sizes);
        let blocks = layout(&tree, tree.root, BOUNDS);
        assert!(blocks.len() < 1001);
        assert!(blocks.iter().all(|(_, r)| r.w >= MIN_DRAWN && r.h >= MIN_DRAWN));
        assert!(blocks.iter().any(|(id, _)| tree.nodes[id.0 as usize].size == 1_000_000));
    }

    #[test]
    fn zero_total_returns_empty() {
        let tree = file_tree(&[0, 0]);
        assert!(layout(&tree, tree.root, BOUNDS).is_empty());
    }

    #[test]
    fn nested_directories_recurse() {
        let mut b = TreeBuilder::new("root");
        let dir = b.add(NodeId(0), "dir", true, 0, 0);
        b.add(dir, "a.bin", false, 50, 50);
        b.add(dir, "b.bin", false, 50, 50);
        b.add(NodeId(0), "c.bin", false, 100, 100);
        let tree = b.finish();
        let blocks = layout(&tree, tree.root, BOUNDS);
        assert_eq!(blocks.len(), 3);
        assert!((area(rect_of(&tree, &blocks, "c.bin")) - 5000.0).abs() <= 50.0);
        assert!((area(rect_of(&tree, &blocks, "a.bin")) - 2500.0).abs() <= 25.0);
        assert!((area(rect_of(&tree, &blocks, "b.bin")) - 2500.0).abs() <= 25.0);
    }

    #[test]
    fn large_folders_get_a_header_strip_above_their_children() {
        let mut b = TreeBuilder::new("root");
        let big = b.add(NodeId(0), "big", true, 0, 0);
        b.add(big, "a.bin", false, 90, 90);
        let tiny = b.add(NodeId(0), "tiny", true, 0, 0);
        b.add(tiny, "t.bin", false, 1, 1);
        let tree = b.finish();

        let (blocks, dirs) = layout_with_headers(&tree, tree.root, BOUNDS, 10.0);

        // Root and "big" are large enough for a strip; "tiny" (about 1 percent of the area) is not.
        let headed: Vec<NodeId> = dirs.iter().filter(|&&(_, r)| has_header(r, 10.0)).map(|&(id, _)| id).collect();
        assert_eq!(headed, [tree.root, big]);
        let big_rect = rect_of(&tree, &dirs, "big");
        assert!(big_rect.y >= 10.0, "the root's strip stays free: {big_rect:?}");
        let a = rect_of(&tree, &blocks, "a.bin");
        assert_eq!(a, Rect { y: big_rect.y + 10.0, h: big_rect.h - 10.0, ..big_rect }, "below the strip of {big_rect:?}");
    }

    /// xorshift64: repeatable pseudo-random numbers for the property tests below.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A size spread over many orders of magnitude, like real files.
        fn size(&mut self) -> u64 {
            let bits = self.below(40);
            1 + self.below(1 << bits)
        }
    }

    fn contains(outer: Rect, inner: Rect) -> bool {
        inner.x >= outer.x && inner.y >= outer.y && inner.x + inner.w <= outer.x + outer.w && inner.y + inner.h <= outer.y + outer.h
    }

    fn on_whole_pixels(r: Rect) -> bool {
        [r.x, r.y, r.w, r.h].iter().all(|v| v.fract() == 0.0)
    }

    #[test]
    fn squarify_tiles_its_rect_exactly() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..500 {
            let rect = Rect {
                x: rng.below(50) as f32,
                y: rng.below(50) as f32,
                w: 1.0 + rng.below(1500) as f32,
                h: 1.0 + rng.below(800) as f32,
            };
            let sizes: Vec<u64> = (0..1 + rng.below(80)).map(|_| rng.size()).collect();
            let total: u64 = sizes.iter().sum();
            let areas: Vec<f32> = sizes.iter().map(|&s| s as f32 * area(rect) / total as f32).collect();

            let rects = squarify(&areas, rect);
            assert_eq!(rects.len(), areas.len());
            for (i, r) in rects.iter().enumerate() {
                assert!(on_whole_pixels(*r) && r.w >= 0.0 && r.h >= 0.0, "{r:?}");
                assert!(contains(rect, *r), "{r:?} outside {rect:?}");
                for other in &rects[i + 1..] {
                    assert_eq!(overlap_area(*r, *other), 0.0, "{r:?} overlaps {other:?}");
                }
            }
            // No overlaps, all inside, and the areas add up: the rects tile `rect` with no gap.
            assert_eq!(rects.iter().map(|&r| area(r)).sum::<f32>(), area(rect), "{rect:?}");
        }
    }

    /// A random tree under the root: up to `depth` folder levels, each with a few files and subfolders.
    fn random_tree(rng: &mut Rng, depth: u32) -> Tree {
        let mut b = TreeBuilder::new("root");
        let mut open = vec![(NodeId(0), 0)];
        while let Some((dir, level)) = open.pop() {
            for i in 0..rng.below(12) {
                b.add(dir, &format!("f{i}.bin"), false, rng.size(), 0);
            }
            if level < depth {
                for i in 0..rng.below(5) {
                    open.push((b.add(dir, &format!("d{i}"), true, 0, 0), level + 1));
                }
            }
        }
        b.finish()
    }

    #[test]
    fn nested_layout_keeps_children_inside_their_folder_and_off_its_label() {
        const HEADER: f32 = 14.0;
        let bounds = Rect { x: 0.0, y: 0.0, w: 1200.0, h: 500.0 };
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        for _ in 0..40 {
            let tree = random_tree(&mut rng, 4);
            let (blocks, dirs) = layout_with_headers(&tree, tree.root, bounds, HEADER);
            assert_eq!(dirs[0], (tree.root, bounds));
            let dir_rect: std::collections::HashMap<NodeId, Rect> = dirs.iter().copied().collect();
            let items: Vec<(NodeId, Rect)> = blocks.iter().chain(&dirs[1..]).copied().collect();

            let mut siblings: std::collections::HashMap<NodeId, Vec<Rect>> = Default::default();
            for &(id, r) in &items {
                let node = &tree.nodes[id.0 as usize];
                let parent = node.parent.unwrap();
                let content = content_rect(dir_rect[&parent], HEADER);
                assert!(on_whole_pixels(r) && r.w >= MIN_DRAWN && r.h >= MIN_DRAWN, "{r:?}");
                assert!(contains(content, r), "{r:?} outside its folder's content {content:?}");
                // Area proportional to size, up to the half pixel each edge may move when rounded.
                let parent_size = tree.nodes[parent.0 as usize].size as f32;
                let expected = node.size as f32 / parent_size * area(content);
                assert!((area(r) - expected).abs() <= r.w + r.h + 2.0, "{r:?}: area {} vs {expected}", area(r));
                siblings.entry(parent).or_default().push(r);
            }
            for rects in siblings.values() {
                for (i, r) in rects.iter().enumerate() {
                    for other in &rects[i + 1..] {
                        assert_eq!(overlap_area(*r, *other), 0.0, "siblings {r:?} and {other:?} overlap");
                    }
                }
            }
            // A folder's name strip is covered by nothing but the folder itself and the folders around it.
            for &(dir, d) in dirs.iter().filter(|&&(_, d)| has_header(d, HEADER)) {
                let strip = Rect { h: HEADER, ..d };
                for &(id, r) in &items {
                    let mut around = Some(dir);
                    while around.is_some_and(|a| a != id) {
                        around = tree.nodes[around.unwrap().0 as usize].parent;
                    }
                    if around.is_none() {
                        assert_eq!(overlap_area(strip, r), 0.0, "{r:?} covers the label strip {strip:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn deep_chain_does_not_overflow_stack() {
        let mut b = TreeBuilder::new("root");
        let mut parent = NodeId(0);
        for i in 0..1000 {
            parent = b.add(parent, &format!("d{i}"), true, 0, 0);
        }
        b.add(parent, "leaf.bin", false, 10, 10);
        let tree = b.finish();
        let blocks = layout(&tree, tree.root, BOUNDS);
        assert_eq!(blocks.len(), 1);
        assert!((area(blocks[0].1) - 10_000.0).abs() <= 1.0);
    }
}
