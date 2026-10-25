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

/// Returns one rect per drawn file block. Directories are subdivided by size and never drawn.
pub fn layout(tree: &Tree, root: NodeId, bounds: Rect) -> Vec<(NodeId, Rect)> {
    layout_with_headers(tree, root, bounds, 0.0).0
}

/// Like [`layout`], but a folder rect at least `MIN_HEADER_W` wide and three headers high keeps a `header`-high strip
/// at its top for its name; its children fill the rest, one pixel in from the left, right and bottom edges, so the
/// folder shows as a frame. Returns the file blocks and the folders that got a strip, each folder before its
/// subfolders. A `header` of 0 gives no strips.
pub fn layout_with_headers(tree: &Tree, root: NodeId, bounds: Rect, header: f32) -> (Rects, Rects) {
    let mut out = Vec::new();
    let mut dirs = Vec::new();
    // Explicit stack instead of recursion, so deep directory chains cannot overflow the call stack.
    let mut stack = vec![(root, bounds)];
    while let Some((dir, mut rect)) = stack.pop() {
        if header > 0.0 && rect.w >= MIN_HEADER_W && rect.h >= 3.0 * header {
            dirs.push((dir, rect));
            rect = Rect { x: rect.x + 1.0, y: rect.y + header, w: rect.w - 2.0, h: rect.h - header - 1.0 };
        }
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
            if r.w < 1.0 || r.h < 1.0 {
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

fn area(r: Rect) -> f32 {
    r.w * r.h
}

/// Squarified subdivision of `rect` into one rect per entry of `areas`, which must sum to the area of `rect`.
fn squarify(areas: &[f32], mut rect: Rect) -> Vec<Rect> {
    let mut out = Vec::with_capacity(areas.len());
    let mut i = 0;
    while i < areas.len() {
        if rect.w <= 0.0 || rect.h <= 0.0 {
            // Rounding left no room: the remaining children get empty rects and are dropped by the caller.
            out.resize(areas.len(), Rect { x: rect.x, y: rect.y, w: 0.0, h: 0.0 });
            break;
        }
        let side = rect.w.min(rect.h);
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
        if rect.w >= rect.h {
            // Lay the row as a column on the left edge.
            let col_w = sum / rect.h;
            let mut y = rect.y;
            for &a in &areas[i..end] {
                let h = a / col_w;
                out.push(Rect { x: rect.x, y, w: col_w, h });
                y += h;
            }
            rect = Rect { x: rect.x + col_w, y: rect.y, w: rect.w - col_w, h: rect.h };
        } else {
            // Lay the row as a strip along the top edge.
            let row_h = sum / rect.w;
            let mut x = rect.x;
            for &a in &areas[i..end] {
                let w = a / row_h;
                out.push(Rect { x, y: rect.y, w, h: row_h });
                x += w;
            }
            rect = Rect { x: rect.x, y: rect.y + row_h, w: rect.w, h: rect.h - row_h };
        }
        i = end;
    }
    out
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
        assert!(blocks.iter().all(|(_, r)| r.w >= 1.0 && r.h >= 1.0));
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

        // Root and "big" are large enough; "tiny" (about 1 percent of the area) is not.
        let ids: Vec<NodeId> = dirs.iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, [tree.root, big]);
        let big_rect = dirs[1].1;
        assert!(big_rect.y >= 10.0, "the root's strip stays free: {big_rect:?}");
        let a = rect_of(&tree, &blocks, "a.bin");
        assert!((a.y - (big_rect.y + 10.0)).abs() < EPS, "{a:?} starts below the strip of {big_rect:?}");
        assert!((a.x - (big_rect.x + 1.0)).abs() < EPS && (a.w - (big_rect.w - 2.0)).abs() < EPS);
        assert!(layout_with_headers(&tree, tree.root, BOUNDS, 0.0).1.is_empty());
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
