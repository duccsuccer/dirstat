//! Parallel directory walk: the fallback scanner used when the MFT cannot be read.

use crate::model::{NodeId, Tree, TreeBuilder};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, DirEntry, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileStandardInfo, GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO,
};

/// Scans `root` with a parallel directory walk. Calls `on_progress` with running file and allocated-byte totals.
/// Returns the tree and the number of entries that could not be read.
pub fn walk(root: &Path, cancel: &AtomicBool, on_progress: &mut dyn FnMut(u64, u64)) -> (Tree, u64) {
    let name = root.to_string_lossy().trim_end_matches('\\').to_string();
    let scan_root = verbatim(root);
    let (tx, rx) = mpsc::channel();

    // The walk runs on a worker thread so this thread can report progress; the callback itself need not be Send.
    let (entries, skipped) = std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let ctx = Ctx { cancel, seen: Mutex::new(HashSet::new()), progress: tx };
            scan_dir(&scan_root, &ctx)
        });
        let (mut files, mut bytes) = (0, 0);
        for (df, db) in rx {
            files += df;
            bytes += db;
            on_progress(files, bytes);
        }
        worker.join().unwrap()
    });

    let mut builder = TreeBuilder::new(&name);
    add_entries(&mut builder, NodeId(0), &entries);
    (builder.finish(), skipped)
}

/// Long-path form of the absolute path, so paths past MAX_PATH resolve.
fn verbatim(root: &Path) -> PathBuf {
    let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut path = OsString::from(r"\\?\");
    path.push(absolute.as_os_str());
    PathBuf::from(path)
}

struct Ctx<'a> {
    cancel: &'a AtomicBool,
    /// (volume serial, file index) of hard-linked files already counted.
    seen: Mutex<HashSet<(u32, u64)>>,
    /// Per-directory (files, allocated bytes) deltas.
    progress: Sender<(u64, u64)>,
}

enum Entry {
    File { name: String, size: u64, logical: u64 },
    Dir { name: String, children: Vec<Entry> },
}

/// Scans the entries of `dir` in parallel. Returns them with the number of entries that could not be read.
fn scan_dir(dir: &Path, ctx: &Ctx) -> (Vec<Entry>, u64) {
    if ctx.cancel.load(Ordering::Relaxed) {
        return (Vec::new(), 0);
    }
    let Ok(read) = fs::read_dir(dir) else { return (Vec::new(), 1) };
    let results: Vec<(Option<Entry>, u64)> = read
        .collect::<Vec<_>>()
        .into_par_iter()
        .map(|entry| match entry {
            Ok(entry) => scan_entry(&entry, ctx),
            Err(_) => (None, 1),
        })
        .collect();

    let mut entries = Vec::new();
    let mut skipped = 0;
    let (mut files, mut bytes) = (0, 0);
    for (entry, n) in results {
        skipped += n;
        if let Some(entry) = entry {
            if let Entry::File { size, .. } = &entry {
                files += 1;
                bytes += size;
            }
            entries.push(entry);
        }
    }
    let _ = ctx.progress.send((files, bytes));
    (entries, skipped)
}

fn scan_entry(entry: &DirEntry, ctx: &Ctx) -> (Option<Entry>, u64) {
    let Ok(meta) = entry.metadata() else { return (None, 1) };
    // Junctions and symlinks are skipped, so each target is counted only where it really lives.
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return (None, 0);
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if meta.is_dir() {
        let (children, skipped) = scan_dir(&entry.path(), ctx);
        return (Some(Entry::Dir { name, children }), skipped);
    }
    let Some(info) = file_info(&entry.path()) else { return (None, 1) };
    if info.links > 1 && !ctx.seen.lock().unwrap().insert((info.volume, info.index)) {
        return (None, 0);
    }
    (Some(Entry::File { name, size: info.allocated, logical: info.logical }), 0)
}

struct FileInfo {
    allocated: u64,
    logical: u64,
    links: u32,
    volume: u32,
    index: u64,
}

fn file_info(path: &Path) -> Option<FileInfo> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .open(path)
        .ok()?;
    let handle = HANDLE(file.as_raw_handle());
    let mut std_info = FILE_STANDARD_INFO::default();
    let mut by_handle = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileStandardInfo,
            (&mut std_info as *mut FILE_STANDARD_INFO).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
        .ok()?;
        GetFileInformationByHandle(handle, &mut by_handle).ok()?;
    }
    Some(FileInfo {
        allocated: std_info.AllocationSize as u64,
        logical: ((by_handle.nFileSizeHigh as u64) << 32) | by_handle.nFileSizeLow as u64,
        links: by_handle.nNumberOfLinks,
        volume: by_handle.dwVolumeSerialNumber,
        index: ((by_handle.nFileIndexHigh as u64) << 32) | by_handle.nFileIndexLow as u64,
    })
}

fn add_entries(builder: &mut TreeBuilder, parent: NodeId, entries: &[Entry]) {
    for entry in entries {
        match entry {
            Entry::File { name, size, logical } => {
                builder.add(parent, name, false, *size, *logical);
            }
            Entry::Dir { name, children } => {
                let id = builder.add(parent, name, true, 0, 0);
                add_entries(builder, id, children);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn scan(dir: &Path) -> (Tree, u64) {
        walk(dir, &AtomicBool::new(false), &mut |_, _| {})
    }

    /// Finds the node whose path below the scan root is `rel` (backslash separated).
    fn find(tree: &Tree, rel: &str) -> Option<NodeId> {
        let want = format!("{}\\{}", tree.path(tree.root), rel);
        (0..tree.nodes.len() as u32).map(NodeId).find(|&id| tree.path(id) == want)
    }

    fn count_named(tree: &Tree, name: &str) -> usize {
        tree.nodes.iter().filter(|n| n.name == name).count()
    }

    fn user() -> String {
        std::env::var("USERNAME").unwrap()
    }

    /// Denies the current user read access to a folder until dropped.
    struct DenyRead(PathBuf);

    impl DenyRead {
        fn new(dir: &Path) -> Self {
            let status = Command::new("icacls")
                .arg(dir)
                .arg("/deny")
                .arg(format!("{}:(OI)(CI)(R)", user()))
                .output()
                .unwrap()
                .status;
            assert!(status.success(), "icacls /deny failed");
            DenyRead(dir.to_path_buf())
        }
    }

    impl Drop for DenyRead {
        fn drop(&mut self) {
            let _ = Command::new("icacls").arg(&self.0).arg("/remove:d").arg(user()).output();
        }
    }

    #[test]
    fn sizes_match_fixture() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.bin"), vec![7u8; 5000]).unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub").join("small.txt"), b"abc").unwrap();

        let (tree, skipped) = scan(dir.path());

        assert_eq!(skipped, 0);
        assert_eq!(tree.path(tree.root), dir.path().to_string_lossy().into_owned());
        let big = &tree.nodes[find(&tree, "big.bin").unwrap().0 as usize];
        assert_eq!(big.logical, 5000);
        assert!(big.size >= 5000 && big.size % 4096 == 0, "allocated size {}", big.size);
        let small = &tree.nodes[find(&tree, "sub\\small.txt").unwrap().0 as usize];
        assert_eq!(small.logical, 3);
        assert_eq!(tree.nodes[tree.root.0 as usize].logical, 5003);
    }

    #[test]
    fn empty_folder_and_zero_byte_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("empty")).unwrap();
        fs::write(dir.path().join("zero.bin"), b"").unwrap();

        let (tree, skipped) = scan(dir.path());
        assert_eq!(skipped, 0);
        let zero = &tree.nodes[find(&tree, "zero.bin").unwrap().0 as usize];
        assert_eq!((zero.size, zero.logical), (0, 0));
        let empty = &tree.nodes[find(&tree, "empty").unwrap().0 as usize];
        assert!(empty.is_dir && empty.children.is_empty());

        let (empty_root, _) = scan(&dir.path().join("empty"));
        assert!(empty_root.nodes[0].children.is_empty());
    }

    #[test]
    fn unreadable_folder_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::write(locked.join("hidden.bin"), b"secret").unwrap();
        fs::write(dir.path().join("open.bin"), b"visible").unwrap();
        // Declared after `dir`, so the ACL is restored before tempfile deletes the folder.
        let _deny = DenyRead::new(&locked);

        let (tree, skipped) = scan(dir.path());

        assert!(skipped >= 1, "skipped = {skipped}");
        assert!(find(&tree, "open.bin").is_some());
        assert!(find(&tree, "locked\\hidden.bin").is_none());
    }

    #[test]
    fn junction_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f.bin"), b"data").unwrap();
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(dir.path().join("link"))
            .arg(&target)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "mklink /J failed");

        let (tree, _) = scan(dir.path());

        assert_eq!(count_named(&tree, "f.bin"), 1);
        assert!(find(&tree, "link").is_none());
    }

    #[test]
    fn hard_link_counted_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.bin");
        fs::write(&a, vec![1u8; 1000]).unwrap();
        fs::hard_link(&a, dir.path().join("b.bin")).unwrap();

        let (tree, _) = scan(dir.path());

        let files = tree.nodes.iter().filter(|n| !n.is_dir).count();
        assert_eq!(files, 1);
        assert_eq!(tree.nodes[tree.root.0 as usize].logical, 1000);
    }

    #[test]
    fn long_and_non_bmp_names() {
        let dir = tempfile::tempdir().unwrap();
        let mut deep = dir.path().to_path_buf();
        for _ in 0..5 {
            deep.push("d".repeat(60));
        }
        fs::create_dir_all(&deep).unwrap();
        assert!(deep.as_os_str().len() > 260);
        fs::write(deep.join("deep.txt"), b"x").unwrap();
        fs::write(dir.path().join("\u{10348}.txt"), b"y").unwrap();

        let (tree, skipped) = scan(dir.path());

        assert_eq!(skipped, 0);
        assert_eq!(count_named(&tree, "deep.txt"), 1);
        assert_eq!(count_named(&tree, "\u{10348}.txt"), 1);
    }

    #[test]
    fn cancel_stops_early() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.bin"), b"a").unwrap();
        fs::write(dir.path().join("b.bin"), b"b").unwrap();
        let cancel = AtomicBool::new(false);
        cancel.store(true, Ordering::Relaxed);

        let (tree, _) = walk(dir.path(), &cancel, &mut |_, _| {});

        assert!(tree.nodes[0].children.is_empty());
    }
}
