//! Diagnostic: `cargo run --release --example size_diff -- <folder on an NTFS drive>` (MFT part needs an elevated shell).
//! Scans the folder with the walk and with the MFT and prints every file whose (size on disk, logical size)
//! differs, then both totals. Without elevation it prints the walk's sizes for every file.

use dirstat::model::{NodeId, Tree};
use dirstat::scan::{mft, walk};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn files(t: &Tree) -> BTreeMap<String, (u64, u64)> {
    (0..t.nodes.len() as u32)
        .map(NodeId)
        .filter(|&id| !t.nodes[id.0 as usize].is_dir)
        .map(|id| (t.path(id).to_lowercase(), (t.nodes[id.0 as usize].size, t.nodes[id.0 as usize].logical)))
        .collect()
}

fn main() {
    let folder = PathBuf::from(std::env::args().nth(1).expect("usage: size_diff <folder>"));
    let letter = folder.to_string_lossy().chars().next().expect("a folder on a drive letter");
    let (walked, skipped) = walk::walk(&folder, &AtomicBool::new(false), &mut |_, _| {});
    let w = files(&walked);
    println!("walk: {} files, {skipped} skipped", w.len());
    let m = match mft::scan_volume(letter, Some(&folder), &AtomicBool::new(false), &mut |_, _| {}) {
        Ok(t) => files(&t),
        Err(e) => {
            println!("MFT scan failed ({e}); walk sizes (size on disk, logical):");
            for (path, sizes) in &w {
                println!("  {sizes:?}  {path}");
            }
            return;
        }
    };
    println!("(size on disk, logical) per differing file: MFT vs walk");
    for path in w.keys().chain(m.keys().filter(|p| !w.contains_key(*p))) {
        let (a, b) = (m.get(path), w.get(path));
        if a != b {
            println!("  MFT {a:?}  walk {b:?}  {path}");
        }
    }
    let sum = |f: &BTreeMap<String, (u64, u64)>| f.values().fold((0, 0), |(s, l), (a, b)| (s + a, l + b));
    println!("totals: MFT {:?}  walk {:?}", sum(&m), sum(&w));
}
