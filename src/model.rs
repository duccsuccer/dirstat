//! In-memory tree of files and directories produced by a scan.

use std::collections::HashMap;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// `ext` indexes `Tree::exts`; `u16::MAX` for directories.
#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub parent: Option<NodeId>,
    pub is_dir: bool,
    /// Size on disk; for directories, the total of all descendants.
    pub size: u64,
    pub logical: u64,
    pub ext: u16,
    pub children: Vec<NodeId>,
}

#[derive(Clone, Debug)]
pub struct ExtStat {
    pub ext: String,
    pub size: u64,
    pub count: u64,
}

#[derive(Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    pub root: NodeId,
    pub exts: Vec<ExtStat>,
}

#[derive(Debug)]
pub struct TreeBuilder {
    nodes: Vec<Node>,
    ext_ids: HashMap<String, u16>,
    other_id: Option<u16>,
    exts: Vec<ExtStat>,
}

const NO_EXT: u16 = u16::MAX;
/// Last usable extension id; ids below it are individual extensions, this one is the "(other)" bucket.
const OTHER_EXT_ID: u16 = u16::MAX - 1;

impl TreeBuilder {
    pub fn new(root_name: &str) -> Self {
        let root = Node {
            name: root_name.to_string(),
            parent: None,
            is_dir: true,
            size: 0,
            logical: 0,
            ext: NO_EXT,
            children: Vec::new(),
        };
        TreeBuilder { nodes: vec![root], ext_ids: HashMap::new(), other_id: None, exts: Vec::new() }
    }

    pub fn add(&mut self, parent: NodeId, name: &str, is_dir: bool, size: u64, logical: u64) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        let (size, logical, ext) = if is_dir {
            (0, 0, NO_EXT)
        } else {
            let ext = self.intern_ext(name);
            let stat = &mut self.exts[ext as usize];
            stat.size += size;
            stat.count += 1;
            (size, logical, ext)
        };
        self.nodes.push(Node {
            name: name.to_string(),
            parent: Some(parent),
            is_dir,
            size,
            logical,
            ext,
            children: Vec::new(),
        });
        self.nodes[parent.0 as usize].children.push(id);
        id
    }

    fn intern_ext(&mut self, name: &str) -> u16 {
        let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_lowercase();
        if let Some(&id) = self.ext_ids.get(&ext) {
            return id;
        }
        if self.exts.len() < OTHER_EXT_ID as usize {
            let id = self.exts.len() as u16;
            self.exts.push(ExtStat { ext: ext.clone(), size: 0, count: 0 });
            self.ext_ids.insert(ext, id);
            return id;
        }
        // Table full: every further new extension shares one "(other)" bucket.
        match self.other_id {
            Some(id) => id,
            None => {
                let id = self.exts.len() as u16;
                self.exts.push(ExtStat { ext: "(other)".to_string(), size: 0, count: 0 });
                self.other_id = Some(id);
                id
            }
        }
    }

    pub fn finish(mut self) -> Tree {
        // Children are always added after their parent, so a reverse pass sees each directory's children before the directory itself.
        for i in (1..self.nodes.len()).rev() {
            if let Some(p) = self.nodes[i].parent {
                let (size, logical) = (self.nodes[i].size, self.nodes[i].logical);
                let parent = &mut self.nodes[p.0 as usize];
                parent.size += size;
                parent.logical += logical;
            }
        }

        let sizes: Vec<u64> = self.nodes.iter().map(|n| n.size).collect();
        for node in &mut self.nodes {
            node.children.sort_by_key(|c| std::cmp::Reverse(sizes[c.0 as usize]));
        }

        let old_exts = std::mem::take(&mut self.exts);
        let mut order: Vec<usize> = (0..old_exts.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(old_exts[i].size));
        let mut new_id = vec![NO_EXT; old_exts.len()];
        for (new, &old) in order.iter().enumerate() {
            new_id[old] = new as u16;
        }
        for node in self.nodes.iter_mut().filter(|n| !n.is_dir) {
            node.ext = new_id[node.ext as usize];
        }
        let exts = order.iter().map(|&i| old_exts[i].clone()).collect();

        Tree { nodes: self.nodes, root: NodeId(0), exts }
    }
}

impl Tree {
    pub fn path(&self, id: NodeId) -> String {
        let mut names = Vec::new();
        let mut cur = Some(id);
        while let Some(c) = cur {
            let node = &self.nodes[c.0 as usize];
            names.push(node.name.as_str());
            cur = node.parent;
        }
        names.reverse();
        names.join("\\")
    }

    /// Detaches `id` from its parent and subtracts its sizes from every ancestor and from `exts`.
    /// The arena slot is kept so other `NodeId`s stay valid. The root cannot be removed.
    pub fn remove(&mut self, id: NodeId) {
        let node = &self.nodes[id.0 as usize];
        let Some(parent) = node.parent else { return };
        let (size, logical) = (node.size, node.logical);

        self.nodes[parent.0 as usize].children.retain(|&c| c != id);
        self.nodes[id.0 as usize].parent = None;

        let mut cur = Some(parent);
        while let Some(c) = cur {
            let ancestor = &mut self.nodes[c.0 as usize];
            ancestor.size -= size;
            ancestor.logical -= logical;
            cur = ancestor.parent;
        }

        let mut stack = vec![id];
        while let Some(c) = stack.pop() {
            let node = &self.nodes[c.0 as usize];
            if node.is_dir {
                stack.extend(node.children.iter().copied());
            } else {
                let stat = &mut self.exts[node.ext as usize];
                stat.size -= node.size;
                stat.count -= 1;
            }
        }
    }

    /// Percent (0 to 100) of the parent's size; the root counts as 100 percent of itself.
    pub fn percent_of_parent(&self, id: NodeId) -> f32 {
        let node = &self.nodes[id.0 as usize];
        let Some(parent) = node.parent else { return 100.0 };
        let total = self.nodes[parent.0 as usize].size;
        if total == 0 {
            0.0
        } else {
            node.size as f32 * 100.0 / total as f32
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_sums_directories_and_sorts_children() {
        let mut b = TreeBuilder::new("root");
        let root = NodeId(0);
        let a = b.add(root, "a", true, 0, 0);
        b.add(a, "x", false, 10, 8);
        b.add(a, "y", false, 30, 20);
        let z = b.add(root, "z", false, 5, 5);
        let t = b.finish();

        assert_eq!(t.nodes[a.0 as usize].size, 40);
        assert_eq!(t.nodes[a.0 as usize].logical, 28);
        assert_eq!(t.nodes[root.0 as usize].size, 45);
        assert_eq!(t.nodes[root.0 as usize].children, vec![a, z]);
        let a_children: Vec<&str> = t.nodes[a.0 as usize]
            .children
            .iter()
            .map(|c| t.nodes[c.0 as usize].name.as_str())
            .collect();
        assert_eq!(a_children, vec!["y", "x"]);
    }

    #[test]
    fn ext_stats_sorted_by_size() {
        let mut b = TreeBuilder::new("root");
        let root = NodeId(0);
        b.add(root, "a.TXT", false, 10, 10);
        b.add(root, "b.txt", false, 5, 5);
        let rs = b.add(root, "c.rs", false, 100, 100);
        let none = b.add(root, "d", false, 1, 1);
        let t = b.finish();

        let names: Vec<(&str, u64, u64)> = t.exts.iter().map(|e| (e.ext.as_str(), e.size, e.count)).collect();
        assert_eq!(names, vec![("rs", 100, 1), ("txt", 15, 2), ("", 1, 1)]);
        let rs_node = &t.nodes[rs.0 as usize];
        assert_eq!(t.exts[rs_node.ext as usize].ext, "rs");
        let none_node = &t.nodes[none.0 as usize];
        assert_eq!(t.exts[none_node.ext as usize].ext, "");
    }

    #[test]
    fn remove_subtracts_from_all_ancestors() {
        let mut b = TreeBuilder::new("root");
        let root = NodeId(0);
        let a = b.add(root, "a", true, 0, 0);
        let sub = b.add(a, "sub", true, 0, 0);
        let f = b.add(sub, "f.bin", false, 100, 50);
        let g = b.add(root, "g.bin", false, 7, 7);
        let mut t = b.finish();

        t.remove(f);

        assert!(!t.nodes[sub.0 as usize].children.contains(&f));
        assert_eq!(t.nodes[sub.0 as usize].size, 0);
        assert_eq!(t.nodes[sub.0 as usize].logical, 0);
        assert_eq!(t.nodes[a.0 as usize].size, 0);
        assert_eq!(t.nodes[root.0 as usize].size, 7);
        assert_eq!(t.nodes[root.0 as usize].logical, 7);
        assert_eq!(t.nodes[root.0 as usize].children, vec![a, g]);
        let bin = t.exts.iter().find(|e| e.ext == "bin").unwrap();
        assert_eq!((bin.size, bin.count), (7, 1));
    }

    #[test]
    fn empty_tree_percent_is_zero_not_nan() {
        let mut b = TreeBuilder::new("root");
        let empty = b.add(NodeId(0), "empty", false, 0, 0);
        let t = b.finish();

        let pct = t.percent_of_parent(empty);
        assert_eq!(pct, 0.0);
        assert!(!pct.is_nan());
    }

    #[test]
    fn path_joins_names_from_root() {
        let mut b = TreeBuilder::new("C:");
        let a = b.add(NodeId(0), "a", true, 0, 0);
        let f = b.add(a, "b.txt", false, 1, 1);
        let t = b.finish();

        assert_eq!(t.path(f), "C:\\a\\b.txt");
        assert_eq!(t.path(t.root), "C:");
    }

    #[test]
    fn ext_table_overflow_maps_to_shared_other_bucket() {
        let mut b = TreeBuilder::new("root");
        let root = NodeId(0);
        // Real extensions fill ids 0..=65533; the next new one opens the "(other)" bucket.
        let total = 70_000u64;
        for i in 0..total {
            b.add(root, &format!("f.e{i}"), false, 1, 1);
        }
        let t = b.finish();

        let other = t.exts.iter().find(|e| e.ext == "(other)").unwrap();
        assert_eq!((other.size, other.count), (total - 65_534, total - 65_534));
        assert_eq!(t.exts.len(), 65_535);
        assert_eq!(t.exts.iter().map(|e| e.count).sum::<u64>(), total);
        // Largest bucket sorts first.
        assert_eq!(t.exts[0].ext, "(other)");
    }
}
