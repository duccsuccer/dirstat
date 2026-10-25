//! Parallel directory walk: the fallback scanner used when the MFT cannot be read.

use crate::model::{NodeId, Tree, TreeBuilder};
use rayon::prelude::*;
use std::collections::HashSet;
use std::ffi::OsString;
use std::fs::{self, DirEntry, File, OpenOptions};
use std::mem::size_of;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::Mutex;
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    FileAttributeTagInfo, FileStandardInfo, GetDiskFreeSpaceW, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationW, GetVolumePathNameW,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FILE_STANDARD_INFO,
};

const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;

/// Scans `root` with a parallel directory walk. Calls `on_progress` with running file and allocated-byte totals.
/// Returns the tree and the number of entries that could not be read.
pub fn walk(root: &Path, cancel: &AtomicBool, on_progress: &mut dyn FnMut(u64, u64)) -> (Tree, u64) {
    let name = root_name(root);
    let scan_root = verbatim(root);
    let (tx, rx) = mpsc::channel();

    // The walk runs on a worker thread so this thread can report progress; the callback itself need not be Send.
    let (entries, skipped) = std::thread::scope(|s| {
        let worker = s.spawn(|| {
            let ctx = Ctx { cancel, seen: Mutex::new(HashSet::new()), progress: tx, ntfs_cluster: ntfs_cluster(&scan_root) };
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

/// The root as given, without a trailing `\` or a `\\?\` prefix (so both scan modes name it alike);
/// `\\?\UNC\server\share` becomes `\\server\share`.
fn root_name(root: &Path) -> String {
    let name = root.to_string_lossy();
    let name = match name.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => name.strip_prefix(r"\\?\").unwrap_or(&name).to_string(),
    };
    name.trim_end_matches('\\').to_string()
}

/// Long-path form of the absolute path, so paths past MAX_PATH resolve.
fn verbatim(root: &Path) -> PathBuf {
    let absolute = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let text = absolute.as_os_str().to_string_lossy();
    if text.starts_with(r"\\?\") {
        return absolute;
    }
    let mut path = OsString::from(r"\\?\");
    match text.strip_prefix(r"\\") {
        Some(unc) => {
            path.push(r"UNC\");
            path.push(unc);
        }
        None => path.push(absolute.as_os_str()),
    }
    PathBuf::from(path)
}

struct Ctx<'a> {
    cancel: &'a AtomicBool,
    /// (volume serial, file index) of hard-linked files already counted.
    seen: Mutex<HashSet<(u32, u64)>>,
    /// Per-directory (files, allocated bytes) deltas.
    progress: Sender<(u64, u64)>,
    /// Cluster size when the walked volume is NTFS.
    ntfs_cluster: Option<u64>,
}

/// Cluster size of the volume holding `root` when it is NTFS; `None` for other filesystems or when unreadable.
fn ntfs_cluster(root: &Path) -> Option<u64> {
    let mut volume = [0u16; 261];
    unsafe { GetVolumePathNameW(&HSTRING::from(root), &mut volume) }.ok()?;
    // GetVolumePathNameW wrote a NUL-terminated string into `volume`, which outlives every use below.
    let volume = PCWSTR(volume.as_ptr());
    let mut fs_name = [0u16; 32];
    unsafe { GetVolumeInformationW(volume, None, None, None, None, Some(&mut fs_name)) }.ok()?;
    if String::from_utf16_lossy(&fs_name).trim_end_matches('\0') != "NTFS" {
        return None;
    }
    let (mut sectors, mut bytes) = (0u32, 0u32);
    unsafe { GetDiskFreeSpaceW(volume, Some(&mut sectors), Some(&mut bytes), None, None) }.ok()?;
    Some(u64::from(sectors) * u64::from(bytes)).filter(|&cluster| cluster > 0)
}

/// Size on disk from the allocation NTFS reports. A stream kept inside its MFT record (resident) reports its length
/// rounded up to 8 bytes, never a whole number of clusters, yet takes no clusters; it counts as 0, as in the MFT scan.
/// shortcut: on 512-byte-cluster volumes a resident file of 505 to 512 bytes looks non-resident; upgrade if it matters.
fn size_on_disk(allocated: u64, ntfs_cluster: Option<u64>) -> u64 {
    if ntfs_cluster.is_some_and(|cluster| !allocated.is_multiple_of(cluster)) {
        0
    } else {
        allocated
    }
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
    let path = entry.path();
    // Only links are skipped, so each target is counted where it really lives. Other reparse tags
    // (cloud placeholders, WOF, dedup) are ordinary content that Windows counts.
    if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        match reparse_tag(&path) {
            Some(tag) if is_name_surrogate(tag) => return (None, 0),
            Some(_) => {}
            None => return (None, 1),
        }
    }
    let name = entry.file_name().to_string_lossy().into_owned();
    if meta.is_dir() {
        let (children, skipped) = scan_dir(&path, ctx);
        return (Some(Entry::Dir { name, children }), skipped);
    }
    let Some(info) = file_info(&path) else { return (None, 1) };
    if info.links > 1 && !ctx.seen.lock().unwrap().insert((info.volume, info.index)) {
        return (None, 0);
    }
    (Some(Entry::File { name, size: size_on_disk(info.allocated, ctx.ntfs_cluster), logical: info.logical }), 0)
}

struct FileInfo {
    allocated: u64,
    logical: u64,
    links: u32,
    volume: u32,
    index: u64,
}

/// Opens `path` for attribute reads only. `flags` are extra `FILE_FLAG_*` values.
fn open_attributes(path: &Path, flags: u32) -> Option<File> {
    OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES.0)
        .share_mode(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0 | FILE_SHARE_DELETE.0)
        .custom_flags(flags)
        .open(path)
        .ok()
}

/// Reparse tag of a reparse point, read without following it.
fn reparse_tag(path: &Path) -> Option<u32> {
    let file = open_attributes(path, FILE_FLAG_OPEN_REPARSE_POINT.0 | FILE_FLAG_BACKUP_SEMANTICS.0)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(file.as_raw_handle()),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
        .ok()?;
    }
    Some(info.ReparseTag)
}

/// Symlinks and junctions name another location; the walk must not follow them.
fn is_name_surrogate(tag: u32) -> bool {
    tag == IO_REPARSE_TAG_SYMLINK || tag == IO_REPARSE_TAG_MOUNT_POINT
}

fn file_info(path: &Path) -> Option<FileInfo> {
    let file = open_attributes(path, 0)?;
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
    fn resident_file_takes_no_space_on_disk() {
        // NTFS keeps a file this small inside its MFT record and reports its length rounded to 8 as allocated.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("resident.txt"), b"small enough to live in the MFT record").unwrap();

        let (tree, _) = scan(dir.path());

        let node = &tree.nodes[find(&tree, "resident.txt").unwrap().0 as usize];
        assert_eq!((node.size, node.logical), (0, 38));
    }

    #[test]
    fn only_ntfs_partial_clusters_count_as_resident() {
        assert_eq!(size_on_disk(40, Some(4096)), 0);
        assert_eq!(size_on_disk(8192, Some(4096)), 8192);
        assert_eq!(size_on_disk(0, Some(4096)), 0);
        assert_eq!(size_on_disk(40, None), 40);
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

    fn junction(link: &Path, target: &Path) {
        let status = Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "mklink /J failed");
    }

    #[test]
    fn junction_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("f.bin"), b"data").unwrap();
        junction(&dir.path().join("link"), &target);

        let (tree, _) = scan(dir.path());

        assert_eq!(count_named(&tree, "f.bin"), 1);
        assert!(find(&tree, "link").is_none());
    }

    #[test]
    fn junction_to_parent_terminates_and_counts_once() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("top.bin"), b"t").unwrap();
        let inner = dir.path().join("inner");
        fs::create_dir(&inner).unwrap();
        fs::write(inner.join("deep.bin"), b"d").unwrap();
        junction(&inner.join("up"), dir.path());

        let (tree, _) = scan(dir.path());

        assert_eq!(count_named(&tree, "top.bin"), 1);
        assert_eq!(count_named(&tree, "deep.bin"), 1);
        assert!(find(&tree, "inner\\up").is_none());
    }

    #[test]
    fn only_symlink_and_mount_point_tags_are_skipped() {
        assert!(is_name_surrogate(0xA000_000C)); // symlink
        assert!(is_name_surrogate(0xA000_0003)); // mount point (junction)
        assert!(!is_name_surrogate(0x9000_001A)); // cloud placeholder
        assert!(!is_name_surrogate(0x8000_0017)); // WOF compressed file
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
    fn verbatim_handles_drive_unc_and_verbatim_paths() {
        let v = |p: &str| verbatim(Path::new(p)).to_string_lossy().into_owned();
        assert_eq!(v(r"C:\x\y"), r"\\?\C:\x\y");
        assert_eq!(v(r"\\server\share\dir"), r"\\?\UNC\server\share\dir");
        assert_eq!(v(r"\\?\C:\x"), r"\\?\C:\x");
        assert_eq!(v(r"\\?\UNC\server\share"), r"\\?\UNC\server\share");
    }

    #[test]
    fn root_name_drops_verbatim_prefix() {
        let n = |p: &str| root_name(Path::new(p));
        assert_eq!(n(r"C:\x\"), r"C:\x");
        assert_eq!(n(r"\\?\C:\x"), r"C:\x");
        assert_eq!(n(r"\\server\share"), r"\\server\share");
        assert_eq!(n(r"\\?\UNC\server\share\"), r"\\server\share");
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
