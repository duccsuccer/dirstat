//! Fast scanner: reads the NTFS Master File Table of a volume and rebuilds the folder tree from it.

use crate::model::{NodeId, Tree, TreeBuilder};
use crate::scan::mft_parse::{parse_any, Extension, MftRecord, Parsed};
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, Prefix};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;
use windows::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_GET_RETRIEVAL_POINTERS, NTFS_VOLUME_DATA_BUFFER, STARTING_VCN_INPUT_BUFFER,
};
use windows::Win32::System::IO::DeviceIoControl;

/// Record number of the volume's root directory.
const ROOT_RECORD: u64 = 5;
/// Records below this are NTFS metafiles (`$MFT`, `$LogFile`, `$Bitmap`, ...), except the root directory.
const FIRST_USER_RECORD: u64 = 24;
/// Bytes per MFT read; a power of two, so a whole number of records.
const CHUNK_BYTES: u64 = 4 << 20;
/// The volume is opened with `FILE_FLAG_NO_BUFFERING`, so read buffers are aligned to at least this (and to the
/// sector size); offsets and lengths are sector multiples by construction.
const MIN_BUFFER_ALIGN: u64 = 4096;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);
const SHARE_ALL: u32 = FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0;

/// One run of the `$MFT` stream: `clusters` clusters starting at virtual cluster `vcn`,
/// stored at logical cluster `lcn` (`None` for a sparse run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    vcn: u64,
    lcn: Option<u64>,
    clusters: u64,
}

/// `len` bytes read from volume byte `offset` (`None`: sparse, left zeroed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    offset: Option<u64>,
    len: u64,
}

/// A run of whole records; its pieces, read back to back, hold records `first_record..`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Chunk {
    first_record: u64,
    pieces: Vec<Piece>,
}

/// Scans NTFS volume `letter` by reading its MFT (needs administrator rights). With `subtree`, keeps only
/// that folder and its descendants; the root is named as `walk` names it (see `root_name`). Calls `on_progress` about every 100 ms with running file and allocated-byte
/// totals. Returns `Err` when the volume cannot be opened, is not NTFS, a read fails, or `cancel` is set.
pub fn scan_volume(
    letter: char,
    subtree: Option<&Path>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<Tree, String> {
    let folder = match subtree {
        Some(path) => folder_components(letter, path)?,
        None => Vec::new(),
    };
    let volume = OpenOptions::new()
        .read(true)
        .share_mode(SHARE_ALL)
        // Uncached: cached volume pages can be stale, since NTFS writes its metadata by another path.
        .custom_flags(FILE_FLAG_NO_BUFFERING.0 | FILE_FLAG_SEQUENTIAL_SCAN.0)
        .open(format!(r"\\.\{letter}:"))
        .map_err(|e| format!("cannot open volume {letter}: ({e})"))?;
    let data = volume_data(&volume).map_err(|e| format!("{letter}: is not a readable NTFS volume ({e})"))?;
    let (sector, cluster, record) =
        (data.BytesPerSector as u64, data.BytesPerCluster as u64, data.BytesPerFileRecordSegment as u64);
    let geometry_ok = sector.is_power_of_two() && cluster % sector == 0 && record % sector == 0;
    if !geometry_ok || !record.is_power_of_two() || record > CHUNK_BYTES {
        return Err(format!("{letter}: unexpected NTFS geometry: sector {sector}, cluster {cluster}, record {record}"));
    }
    let extents = mft_extents(letter)?;
    let chunks = plan_chunks(&extents, cluster, record, data.MftValidDataLength as u64, CHUNK_BYTES);
    let align = sector.max(MIN_BUFFER_ALIGN) as usize;
    let records = read_records(&volume, &chunks, record as usize, align, cancel, on_progress)?;
    if cancel.load(Ordering::Relaxed) {
        return Err("scan cancelled".into());
    }
    build_tree(merge_extensions(records), &root_name(letter, subtree), &folder)
}

/// Path components of `path` below the root of volume `letter`, with links and short names resolved.
/// Used only to find the folder in the MFT, never to name it.
fn folder_components(letter: char, path: &Path) -> Result<Vec<String>, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    let mut parts = real.components();
    let on_volume = matches!(parts.next(), Some(Component::Prefix(p))
        if matches!(p.kind(), Prefix::VerbatimDisk(d) | Prefix::Disk(d) if (d as char).eq_ignore_ascii_case(&letter)));
    if !on_volume {
        return Err(format!("{} is not on volume {letter}:", real.display()));
    }
    Ok(parts
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect())
}

fn volume_data(volume: &File) -> windows::core::Result<NTFS_VOLUME_DATA_BUFFER> {
    let mut data = NTFS_VOLUME_DATA_BUFFER::default();
    let mut returned = 0u32;
    // SAFETY: the output pointer and size describe `data`, which outlives the synchronous call.
    unsafe {
        DeviceIoControl(
            HANDLE(volume.as_raw_handle()),
            FSCTL_GET_NTFS_VOLUME_DATA,
            None,
            0,
            Some((&mut data as *mut NTFS_VOLUME_DATA_BUFFER).cast()),
            size_of::<NTFS_VOLUME_DATA_BUFFER>() as u32,
            Some(&mut returned),
            None,
        )?;
    }
    Ok(data)
}

/// Runs of the `$MFT` stream, from `FSCTL_GET_RETRIEVAL_POINTERS` (which also covers an `$MFT` whose
/// runs spill into attribute-list extension records).
fn mft_extents(letter: char) -> Result<Vec<Extent>, String> {
    let mft = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
        .open(format!(r"{letter}:\$MFT"))
        .map_err(|e| format!("cannot open {letter}:\\$MFT ({e})"))?;
    // u64 storage keeps the output 8-byte aligned for the LARGE_INTEGER fields.
    let mut storage = vec![0u64; 8192];
    let mut extents: Vec<Extent> = Vec::new();
    loop {
        let input = STARTING_VCN_INPUT_BUFFER { StartingVcn: extents.last().map_or(0, |e| e.vcn + e.clusters) as i64 };
        let mut returned = 0u32;
        // SAFETY: the input and output pointers and sizes describe `input` and `storage`, which outlive the
        // synchronous call.
        let result = unsafe {
            DeviceIoControl(
                HANDLE(mft.as_raw_handle()),
                FSCTL_GET_RETRIEVAL_POINTERS,
                Some((&input as *const STARTING_VCN_INPUT_BUFFER).cast()),
                size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                Some(storage.as_mut_ptr().cast()),
                (storage.len() * size_of::<u64>()) as u32,
                Some(&mut returned),
                None,
            )
        };
        let more = match result {
            Ok(()) => false,
            Err(e) if e.code() == ERROR_MORE_DATA.to_hresult() => true,
            Err(e) => return Err(format!("cannot list {letter}:\\$MFT extents ({e})")),
        };
        // SAFETY: any initialized u64 memory is valid as bytes, and u8 needs no alignment.
        let bytes = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>(), storage.len() * size_of::<u64>()) };
        let got = bytes
            .get(..returned as usize)
            .and_then(parse_retrieval_pointers)
            .filter(|got| !got.is_empty())
            .ok_or_else(|| format!("malformed {letter}:\\$MFT extent list"))?;
        extents.extend(got);
        if !more {
            return Ok(extents);
        }
    }
}

/// Reads and parses every chunk on rayon workers while this thread reports progress.
fn read_records(
    volume: &File,
    chunks: &[Chunk],
    record: usize,
    align: usize,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, u64),
) -> Result<Vec<Parsed>, String> {
    let (files, bytes) = (AtomicU64::new(0), AtomicU64::new(0));
    let (files, bytes) = (&files, &bytes);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let per_chunk = std::thread::scope(|s| {
        let worker = s.spawn(move || {
            // Dropped when the worker ends, which ends the progress loop below.
            let _done = done_tx;
            chunks
                .par_iter()
                .map(|chunk| {
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(Vec::new());
                    }
                    let recs = read_chunk(volume, chunk, record, align)?;
                    let (mut f, mut b) = (0, 0);
                    for r in &recs {
                        match r {
                            Parsed::Base(r) => (f, b) = (f + u64::from(!r.is_dir), b + r.allocated),
                            Parsed::Extension(e) => b += e.data.map_or(0, |(allocated, _)| allocated),
                        }
                    }
                    files.fetch_add(f, Ordering::Relaxed);
                    bytes.fetch_add(b, Ordering::Relaxed);
                    Ok(recs)
                })
                .collect::<Result<Vec<Vec<Parsed>>, String>>()
        });
        while let Err(RecvTimeoutError::Timeout) = done_rx.recv_timeout(PROGRESS_EVERY) {
            on_progress(files.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
        }
        on_progress(files.load(Ordering::Relaxed), bytes.load(Ordering::Relaxed));
        worker.join().unwrap()
    })?;
    Ok(per_chunk.into_iter().flatten().collect())
}

/// Reads one chunk into an `align`-aligned buffer and parses its records, numbered by MFT position.
fn read_chunk(volume: &File, chunk: &Chunk, record: usize, align: usize) -> Result<Vec<Parsed>, String> {
    let len: usize = chunk.pieces.iter().map(|p| p.len as usize).sum();
    let mut storage = vec![0u8; len + align];
    let start = storage.as_ptr().align_offset(align);
    let buf = &mut storage[start..start + len];
    let mut at = 0;
    for piece in &chunk.pieces {
        let dest = &mut buf[at..at + piece.len as usize];
        if let Some(offset) = piece.offset {
            match volume.seek_read(dest, offset) {
                Ok(n) if n == dest.len() => {}
                Ok(n) => return Err(format!("short MFT read at volume byte {offset}: {n} of {} bytes", dest.len())),
                Err(e) => return Err(format!("MFT read at volume byte {offset} failed ({e})")),
            }
        }
        at += piece.len as usize;
    }
    Ok(buf
        .chunks_exact_mut(record)
        .zip(chunk.first_record..)
        .filter_map(|(rec, index)| parse_any(rec, index))
        .collect())
}

/// The target as `walk` names it: `subtree` as given, made absolute, without `\\?\` or a trailing `\`
/// (never canonicalized, so both scan modes name a target alike), or `X:` for the whole volume.
fn root_name(letter: char, subtree: Option<&Path>) -> String {
    let Some(path) = subtree else { return format!("{}:", letter.to_ascii_uppercase()) };
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let name = absolute.to_string_lossy();
    name.strip_prefix(r"\\?\").unwrap_or(&name).trim_end_matches('\\').to_string()
}

/// Folds extension records into their base records: a file with no unnamed data of its own (logical size 0) adds
/// the sizes of the first `$DATA` extent held in an extension, and a base record with no `$FILE_NAME` of its own
/// takes one held in an extension. Base records still nameless are dropped, as are extensions whose base record
/// is not in use.
fn merge_extensions(parsed: Vec<Parsed>) -> Vec<MftRecord> {
    let mut records = Vec::with_capacity(parsed.len());
    let mut extensions: HashMap<u64, Extension> = HashMap::new();
    for p in parsed {
        match p {
            Parsed::Base(r) => records.push(r),
            Parsed::Extension(e) => {
                let merged = extensions.entry(e.base).or_default();
                merged.data = merged.data.or(e.data);
                merged.name = merged.name.take().or(e.name);
            }
        }
    }
    records.retain_mut(|r| {
        if let Some(e) = extensions.get_mut(&r.index) {
            // The base may already hold named streams' allocation, so the unnamed stream's is added to it.
            if let (false, 0, Some((allocated, logical))) = (r.is_dir, r.logical, e.data) {
                (r.allocated, r.logical) = (r.allocated + allocated, logical);
            }
            if let (true, Some((parent, name))) = (r.name.is_empty(), e.name.take()) {
                (r.parent, r.name) = (parent, name);
            }
        }
        !r.name.is_empty()
    });
    records
}

/// Rebuilds the folder tree from parsed records, with its root named `root_name`. `folder` holds the path
/// components, below the volume root, of the folder to keep: the tree is rooted there. Empty keeps the whole
/// volume. Components match names exactly, as stored on disk. Metafiles, symlinks and junctions, and records
/// whose parent chain never reaches the root are left out. Returns `Err` when `folder` is not a reachable
/// directory.
pub fn build_tree(records: Vec<MftRecord>, root_name: &str, folder: &[String]) -> Result<Tree, String> {
    // Links have nothing below them in the MFT, so dropping them also stops any descent through them.
    let mut kept: Vec<MftRecord> =
        records.into_iter().filter(|r| r.index >= FIRST_USER_RECORD && !r.is_reparse).collect();
    kept.sort_unstable_by_key(|r| r.parent);
    let children = |parent: u64| {
        let lo = kept.partition_point(|r| r.parent < parent);
        let hi = kept.partition_point(|r| r.parent <= parent);
        &kept[lo..hi]
    };

    let mut start = ROOT_RECORD;
    for name in folder {
        start = children(start)
            .iter()
            .find(|r| r.is_dir && r.name == *name)
            .ok_or_else(|| format!("folder {} not found in the MFT", folder.join("\\")))?
            .index;
    }
    let mut builder = TreeBuilder::new(root_name);
    // Iterative, so deep chains cannot overflow the stack. Each record sits under one parent, so none is
    // added twice, and orphans and parent cycles are never reached from `start`.
    let mut stack = vec![(start, NodeId(0))];
    // Folders in the kept subtree, for hard links whose first name lies outside it (subtree scans only).
    let mut dirs: HashMap<u64, NodeId> = HashMap::new();
    while let Some((index, node)) = stack.pop() {
        if !folder.is_empty() {
            dirs.insert(index, node);
        }
        for r in children(index) {
            let id = builder.add(node, &r.name, r.is_dir, r.allocated, r.logical);
            if r.is_dir {
                stack.push((r.index, id));
            }
        }
    }
    // A file is added once: under its first name when that is in the subtree, else under its first link that is.
    for r in kept.iter().filter(|r| !r.extra_links.is_empty() && !folder.is_empty() && !dirs.contains_key(&r.parent)) {
        if let Some((node, name)) = r.extra_links.iter().find_map(|(p, name)| Some((*dirs.get(p)?, name))) {
            builder.add(node, name, r.is_dir, r.allocated, r.logical);
        }
    }
    Ok(builder.finish())
}

/// Decodes a `RETRIEVAL_POINTERS_BUFFER`: extent count (u32) @0, starting VCN (i64) @8, then
/// (next VCN, LCN) i64 pairs from @16. An LCN of -1 marks a sparse run.
fn parse_retrieval_pointers(buf: &[u8]) -> Option<Vec<Extent>> {
    let i64_at = |off: usize| Some(i64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?));
    let count = u32::from_le_bytes(buf.get(0..4)?.try_into().ok()?) as usize;
    let mut vcn = i64_at(8)?;
    let mut extents = Vec::with_capacity(count.min(buf.len() / 16));
    for i in 0..count {
        let (next_vcn, lcn) = (i64_at(16 + 16 * i)?, i64_at(24 + 16 * i)?);
        extents.push(Extent { vcn: vcn as u64, lcn: (lcn >= 0).then_some(lcn as u64), clusters: (next_vcn - vcn) as u64 });
        vcn = next_vcn;
    }
    Some(extents)
}

/// Splits the `$MFT` stream, cut to `valid` bytes and to whole `record`s, into reads of at most `chunk` bytes
/// (a multiple of `record`). `extents` must be contiguous in VCN order. A record that straddles two runs
/// (clusters smaller than records) is read as two pieces of the same chunk.
fn plan_chunks(extents: &[Extent], cluster: u64, record: u64, valid: u64, chunk: u64) -> Vec<Chunk> {
    let covered = extents.last().map_or(0, |e| (e.vcn + e.clusters) * cluster);
    let total = valid.min(covered) / record * record;
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < total {
        let end = (start + chunk).min(total);
        let pieces = extents
            .iter()
            .filter_map(|e| {
                let (run_start, run_end) = (e.vcn * cluster, (e.vcn + e.clusters) * cluster);
                let (from, to) = (start.max(run_start), end.min(run_end));
                (from < to).then(|| Piece { offset: e.lcn.map(|lcn| lcn * cluster + (from - run_start)), len: to - from })
            })
            .collect();
        chunks.push(Chunk { first_record: start / record, pieces });
        start = end;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::walk::walk;
    use std::fs;
    use std::process::Command;

    fn rec(index: u64, parent: u64, name: &str, is_dir: bool, size: u64) -> MftRecord {
        MftRecord { index, parent, name: name.to_string(), is_dir, allocated: size, logical: size / 2, is_reparse: false, extra_links: Vec::new() }
    }

    fn link(index: u64, parent: u64, name: &str, is_dir: bool) -> MftRecord {
        MftRecord { is_reparse: true, ..rec(index, parent, name, is_dir, 0) }
    }

    fn paths(t: &Tree) -> Vec<String> {
        let mut p: Vec<String> = (0..t.nodes.len() as u32).map(|i| t.path(NodeId(i))).collect();
        p.sort();
        p
    }

    fn comps(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn whole_volume_root_is_named_after_drive() {
        let recs = vec![rec(30, 5, "a", true, 0), rec(31, 30, "f.txt", false, 4096)];
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(paths(&t), ["C:", "C:\\a", "C:\\a\\f.txt"]);
        let root = &t.nodes[t.root.0 as usize];
        assert_eq!((root.size, root.logical), (4096, 2048));
    }

    #[test]
    fn orphans_are_dropped() {
        let recs = vec![
            rec(30, 5, "kept.bin", false, 1),
            rec(40, 999, "lost.bin", false, 1),
            rec(41, 998, "gone", true, 0),
            rec(42, 41, "under_gone.bin", false, 1),
            rec(50, 51, "cycle_a", true, 0),
            rec(51, 50, "cycle_b", true, 0),
        ];
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(paths(&t), ["C:", "C:\\kept.bin"]);
    }

    #[test]
    fn metafiles_are_skipped() {
        let recs = vec![
            rec(0, 5, "$MFT", false, 1 << 20),
            rec(5, 5, ".", true, 0),
            rec(11, 5, "$Extend", true, 0),
            rec(25, 11, "$Quota", false, 7),
            rec(30, 5, "user.txt", false, 3),
        ];
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(paths(&t), ["C:", "C:\\user.txt"]);
        assert_eq!(t.nodes[t.root.0 as usize].size, 3);
    }

    #[test]
    fn links_are_not_descended() {
        let recs = vec![
            link(30, 5, "junction", true),
            rec(31, 30, "inside.bin", false, 9),
            link(32, 5, "symlink.txt", false),
            rec(33, 5, "real.bin", false, 4),
        ];
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(paths(&t), ["C:", "C:\\real.bin"]);
    }

    #[test]
    fn subtree_reroots_at_folder_and_keeps_only_descendants() {
        let recs = vec![
            rec(30, 5, "Users", true, 0),
            rec(31, 30, "me", true, 0),
            rec(32, 31, "a.bin", false, 100),
            rec(33, 31, "docs", true, 0),
            rec(34, 33, "b.bin", false, 50),
            rec(35, 30, "outside.bin", false, 999),
            rec(36, 5, "top.bin", false, 999),
        ];
        let t = build_tree(recs, r"C:\Users\me", &comps(&["Users", "me"])).unwrap();
        assert_eq!(paths(&t), [r"C:\Users\me", r"C:\Users\me\a.bin", r"C:\Users\me\docs", r"C:\Users\me\docs\b.bin"]);
        assert_eq!(t.nodes[t.root.0 as usize].size, 150);
    }

    #[test]
    fn subtree_keeps_hard_link_whose_first_name_is_outside() {
        let mut linked = rec(40, 36, "shared.dll", false, 64);
        linked.extra_links = vec![(37, "elsewhere.dll".into()), (33, "shared.dll".into())];
        let recs = vec![
            rec(30, 5, "Users", true, 0),
            rec(31, 30, "me", true, 0),
            rec(33, 31, "docs", true, 0),
            rec(36, 5, "Windows", true, 0),
            rec(37, 5, "Other", true, 0),
            linked,
        ];
        let t = build_tree(recs.clone(), r"C:\Users\me", &comps(&["Users", "me"])).unwrap();
        assert_eq!(paths(&t), [r"C:\Users\me", r"C:\Users\me\docs", r"C:\Users\me\docs\shared.dll"]);
        assert_eq!(t.nodes[t.root.0 as usize].size, 64);

        let whole = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(whole.nodes.iter().filter(|n| !n.is_dir).count(), 1);
        assert!(paths(&whole).contains(&r"C:\Windows\shared.dll".to_string()));
    }

    #[test]
    fn unreachable_subtree_is_err() {
        let recs = vec![
            rec(36, 5, "top.bin", false, 1),
            rec(40, 999, "lost", true, 0),
            rec(41, 40, "x", true, 0),
            link(42, 5, "junction", true),
        ];
        for path in [&["nope"][..], &["lost", "x"], &["top.bin"], &["junction"]] {
            assert!(build_tree(recs.clone(), "C:", &comps(path)).is_err(), "{path:?}");
        }
    }

    #[test]
    fn deep_chain_does_not_overflow_stack() {
        const DEPTH: u64 = 100_000;
        let mut recs: Vec<MftRecord> = (0..DEPTH).map(|i| rec(24 + i, if i == 0 { 5 } else { 23 + i }, "d", true, 0)).collect();
        recs.push(rec(24 + DEPTH, 23 + DEPTH, "leaf.bin", false, 8));
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert_eq!(t.nodes.len() as u64, DEPTH + 2);
        assert_eq!(t.nodes[t.root.0 as usize].size, 8);
    }

    #[test]
    fn non_bmp_and_long_names_survive() {
        let long = "x".repeat(255);
        let recs = vec![rec(30, 5, "\u{10348}.txt", false, 1), rec(31, 5, &long, false, 1)];
        let t = build_tree(recs, "C:", &[]).unwrap();
        assert!(t.nodes.iter().any(|n| n.name == "\u{10348}.txt"));
        assert!(t.nodes.iter().any(|n| n.name == long));
    }

    fn ext(base: u64, data: Option<(u64, u64)>, name: Option<(u64, &str)>) -> Parsed {
        Parsed::Extension(Extension { base, data, name: name.map(|(p, n)| (p, n.to_string())) })
    }

    #[test]
    fn extension_data_fills_a_base_without_data() {
        let parsed = vec![
            Parsed::Base(rec(30, 5, "vm.vhdx", false, 0)),
            ext(30, Some((1 << 30, 1_000_000_000)), None),
            Parsed::Base(rec(31, 5, "own.bin", false, 4096)),
            ext(31, Some((999, 999)), None),
            // Only a named stream in the base: the unnamed stream's sizes are added to it.
            Parsed::Base(MftRecord { logical: 0, ..rec(32, 5, "ads.bin", false, 512) }),
            ext(32, Some((8192, 5000)), None),
        ];
        let t = build_tree(merge_extensions(parsed), "C:", &[]).unwrap();
        let root = &t.nodes[t.root.0 as usize];
        assert_eq!((root.size, root.logical), ((1 << 30) + 4096 + 512 + 8192, 1_000_000_000 + 2048 + 5000));
    }

    #[test]
    fn extension_name_keeps_a_nameless_base_and_its_subtree() {
        let parsed = vec![
            Parsed::Base(rec(30, 0, "", true, 0)),
            ext(30, None, Some((5, "big dir"))),
            Parsed::Base(rec(31, 30, "inside.bin", false, 8)),
            Parsed::Base(rec(32, 0, "", false, 1)),
            ext(77, Some((5, 5)), Some((5, "orphan"))),
        ];
        let t = build_tree(merge_extensions(parsed), "C:", &[]).unwrap();
        assert_eq!(paths(&t), ["C:", r"C:\big dir", r"C:\big dir\inside.bin"]);
    }

    fn pointers(starting_vcn: i64, extents: &[(i64, i64)]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&(extents.len() as u32).to_le_bytes());
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&starting_vcn.to_le_bytes());
        for (next_vcn, lcn) in extents {
            b.extend_from_slice(&next_vcn.to_le_bytes());
            b.extend_from_slice(&lcn.to_le_bytes());
        }
        b
    }

    #[test]
    fn retrieval_pointers_decode_into_extents() {
        let got = parse_retrieval_pointers(&pointers(0, &[(2, 100), (5, -1)])).unwrap();
        assert_eq!(got, [Extent { vcn: 0, lcn: Some(100), clusters: 2 }, Extent { vcn: 2, lcn: None, clusters: 3 }]);
        let got = parse_retrieval_pointers(&pointers(7, &[(9, 50)])).unwrap();
        assert_eq!(got, [Extent { vcn: 7, lcn: Some(50), clusters: 2 }]);
        let mut short = pointers(0, &[(2, 100)]);
        short.truncate(20);
        assert_eq!(parse_retrieval_pointers(&short), None);
    }

    #[test]
    fn retrieval_pointers_layout_matches_windows_struct() {
        use std::mem::{offset_of, size_of};
        use windows::Win32::System::Ioctl::{RETRIEVAL_POINTERS_BUFFER, RETRIEVAL_POINTERS_BUFFER_0};
        assert_eq!(offset_of!(RETRIEVAL_POINTERS_BUFFER, StartingVcn), 8);
        assert_eq!(offset_of!(RETRIEVAL_POINTERS_BUFFER, Extents), 16);
        assert_eq!(size_of::<RETRIEVAL_POINTERS_BUFFER_0>(), 16);
    }

    #[test]
    fn chunks_number_records_by_mft_position_across_fragments() {
        let extents = [Extent { vcn: 0, lcn: Some(100), clusters: 2 }, Extent { vcn: 2, lcn: Some(500), clusters: 3 }];
        let got = plan_chunks(&extents, 4096, 1024, 20480, 12288);
        assert_eq!(
            got,
            [
                Chunk {
                    first_record: 0,
                    pieces: vec![Piece { offset: Some(409_600), len: 8192 }, Piece { offset: Some(2_048_000), len: 4096 }],
                },
                Chunk { first_record: 12, pieces: vec![Piece { offset: Some(2_052_096), len: 8192 }] },
            ]
        );
    }

    #[test]
    fn chunks_stop_at_valid_length_and_extent_end() {
        let extents = [Extent { vcn: 0, lcn: Some(1), clusters: 2 }];
        let got = plan_chunks(&extents, 4096, 1024, 5000, 1 << 22);
        assert_eq!(got, [Chunk { first_record: 0, pieces: vec![Piece { offset: Some(4096), len: 4096 }] }]);
        let got = plan_chunks(&extents, 4096, 1024, 1 << 30, 1 << 22);
        assert_eq!(got, [Chunk { first_record: 0, pieces: vec![Piece { offset: Some(4096), len: 8192 }] }]);
    }

    #[test]
    fn record_straddling_small_clusters_is_stitched_from_two_pieces() {
        let extents = [
            Extent { vcn: 0, lcn: Some(10), clusters: 3 },
            Extent { vcn: 3, lcn: None, clusters: 1 },
            Extent { vcn: 4, lcn: Some(90), clusters: 2 },
        ];
        let got = plan_chunks(&extents, 512, 1024, 3072, 4096);
        assert_eq!(
            got,
            [Chunk {
                first_record: 0,
                pieces: vec![
                    Piece { offset: Some(5120), len: 1536 },
                    Piece { offset: None, len: 512 },
                    Piece { offset: Some(46_080), len: 1024 },
                ],
            }]
        );
    }

    #[test]
    fn root_name_is_target_as_given_like_walk() {
        assert_eq!(root_name('c', None), "C:");
        assert_eq!(root_name('C', Some(Path::new(r"C:\"))), "C:");
        assert_eq!(root_name('C', Some(Path::new(r"C:\Users\ADMIN~1\inner\"))), r"C:\Users\ADMIN~1\inner");
        assert_eq!(root_name('C', Some(Path::new(r"\\?\C:\Users\me"))), r"C:\Users\me");
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(root_name('C', Some(Path::new("rel"))), cwd.join("rel").to_string_lossy());
    }

    #[test]
    fn missing_volume_is_err_not_panic() {
        let letter = ('D'..='Z').find(|l| !Path::new(&format!("{l}:\\")).exists()).expect("a free drive letter");
        let result = scan_volume(letter, None, &AtomicBool::new(false), &mut |_, _| {});
        assert!(result.is_err());
    }

    fn ok(cmd: &mut Command) {
        let status = cmd.output().unwrap().status;
        assert!(status.success(), "{cmd:?} failed");
    }

    /// Flushes every open file on C: so the raw MFT read sees what the test just wrote (needs admin).
    fn flush_volume() {
        OpenOptions::new().read(true).write(true).share_mode(SHARE_ALL).open(r"\\.\C:").unwrap().sync_all().unwrap();
    }

    fn totals(t: &Tree) -> (u64, u64, usize) {
        let root = &t.nodes[t.root.0 as usize];
        (root.size, root.logical, t.nodes.iter().filter(|n| !n.is_dir).count())
    }

    #[test]
    #[ignore = "needs an elevated shell"]
    fn mft_matches_walk_on_same_folder() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert!(d.starts_with("C:\\"), "temp dir {} is not on C:", d.display());
        fs::write(d.join("empty.bin"), b"").unwrap();
        fs::write(d.join("resident.txt"), b"small enough to live in the MFT record").unwrap();
        fs::write(d.join("mid.bin"), vec![1u8; 5000]).unwrap();
        fs::write(d.join("big.bin"), vec![2u8; 3 << 20]).unwrap();
        fs::create_dir(d.join("sub")).unwrap();
        fs::write(d.join("sub").join("inner.bin"), vec![3u8; 70_000]).unwrap();
        fs::hard_link(d.join("mid.bin"), d.join("sub").join("mid_link.bin")).unwrap();
        ok(Command::new("cmd").args(["/C", "mklink", "/J"]).arg(d.join("junction")).arg(d.join("sub")));
        let sparse = d.join("sparse.bin");
        fs::write(&sparse, b"sparse head").unwrap();
        ok(Command::new("fsutil").args(["sparse", "setflag"]).arg(&sparse));
        ok(Command::new("fsutil").args(["file", "seteof"]).arg(&sparse).arg("10485760"));
        let compressed = d.join("compressed.txt");
        fs::write(&compressed, b"dirstat ".repeat(256 * 1024)).unwrap();
        ok(Command::new("compact").arg("/c").arg(&compressed));
        // A hard link whose other name lives outside the scanned folder (the walk counts it once, here).
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("original.bin"), vec![4u8; 20_000]).unwrap();
        fs::hard_link(outside.path().join("original.bin"), d.join("linked_from_outside.bin")).unwrap();
        // WOF (compact /exe) compression: its MFT-mode size is otherwise unverified.
        let wof = d.join("wof.txt");
        fs::write(&wof, b"dirstat wof ".repeat(32 * 1024)).unwrap();
        let wof_ok = Command::new("compact").args(["/c", "/exe:xpress8k"]).arg(&wof).output().is_ok_and(|o| o.status.success());
        if !wof_ok {
            println!("note: compact /exe:xpress8k failed, so the WOF file is uncompressed in this run");
        }
        flush_volume();

        let (walked, skipped) = walk(d, &AtomicBool::new(false), &mut |_, _| {});
        let scanned = scan_volume('C', Some(d), &AtomicBool::new(false), &mut |_, _| {}).unwrap();

        assert_eq!(skipped, 0);
        assert_eq!(totals(&scanned), totals(&walked), "(size, logical, files): MFT vs walk");
    }

    #[test]
    #[ignore = "needs an elevated shell"]
    fn subtree_reroots_at_folder() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        fs::write(inner.join("kept.bin"), b"k").unwrap();
        fs::write(dir.path().join("outside.bin"), b"o").unwrap();
        flush_volume();

        let t = scan_volume('C', Some(&inner), &AtomicBool::new(false), &mut |_, _| {}).unwrap();

        let want = inner.to_string_lossy().into_owned();
        assert_eq!(t.nodes[t.root.0 as usize].name, want);
        assert_eq!(paths(&t), [want.clone(), format!(r"{want}\kept.bin")]);
    }
}
