# dirstat

dirstat is an open-source disk usage analyzer for Windows in the style of WinDirStat: pick a drive or folder, see which folders and file types use the space, and act on items through Explorer's own context menu. It is written in Rust with an egui interface.

The scan is the point of the project. On an NTFS drive it reads the Master File Table directly instead of walking every directory, both for the whole drive and for a folder on it. Other filesystems and network paths (`\\server\share`) use a parallel directory walk, as does any scan whose MFT cannot be read (for example without administrator rights).

## Build and run

```
cargo build --release
```

The executable asks for administrator rights when it starts (its manifest requires them). The MFT scan needs them; the app does not relaunch itself.

## The window

There are no screenshots yet.

- Top bar: drive picker, Scan, Browse folder and Rescan; under them the scan status (live file and byte counters while scanning, then how long the scan took and which scanner ran) and a progress bar (measured against the drive's used space for a whole-drive scan, animated for a folder). On the right: the selected path and the scanned volume's total, used and free space.
- Folder tree (left): Folder, % of Parent, Size (logical), Allocated (size on disk), Items, Files and Folders. Rows are ordered by Allocated; the columns cannot be re-sorted. There is no Modified column: the scan does not read timestamps.
- File types (right): one row per extension with its treemap colour, the name Windows gives the type, its share of the total, Size, Allocated and the number of files.
- Treemap (bottom): the scanned path above it; large folders are drawn as a frame with their name and size in a strip at the top; the selection is outlined in white. A status line below gives the selection's file count, Size and Allocated.
- Right-click an item in the tree or the treemap for Explorer's own context menu (Open, Cut, Copy, Delete, Send to, Properties and so on). Hold Shift while right-clicking for the extended menu (for example "Copy as path"). Ctrl+C copies the selected item's path.

## Tests

```
cargo test
```

Two MFT tests are `#[ignore]`d because they read a real volume. Run them from an elevated terminal:

```
cargo test --release scan::mft -- --ignored --test-threads=1
```

## Benchmark

```
cargo run --release --example scan_bench -- C
cargo run --release --example scan_bench -- C:\some\folder
cargo run --release --example scan_bench -- --walk D:
```

To compare the two scan modes file by file on one folder (elevated), run `cargo run --release --example size_diff -- C:\some\folder`.

The benchmark prints the scan mode, elapsed seconds, file count and size on disk. `--walk` forces the directory walk. Run it from an elevated terminal to use the MFT path; unelevated, an NTFS volume falls back to the walk and the output says so.

### Results

| Mode | Target | Files | Size on disk | Seconds |
|------|--------|-------|--------------|---------|
| Mft | `C:` | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) |
| Mft | `C:\Users\Admin\Documents` | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) |
| Walk | `C:\Users\Admin\Documents` | 156,081 | 40.6 GB | 1.85 to 1.88 (warm); 6.01 on the first, cold run |
| Walk | `D:` | 59,262 | 826.2 GB | 0.47 to 0.56 |

Walk rows were measured unelevated on one machine, with the file-cache state noted above. No WinDirStat or WizTree numbers are recorded yet.

## Status and known limits

- Scan, tree, file-type and treemap views, and Explorer's context menu on any item. Other stage 2 features are not built.
- Deleting: dirstat has no delete of its own. Use Delete in the context menu (to the Recycle Bin) or hold Shift while choosing Delete (permanent); Windows asks for confirmation and handles items too large for the bin as Explorer does. When a menu command returns and the item is gone from disk, dirstat removes it from the tree and takes its size off every total. Changes that happen later, such as a Cut that is pasted elsewhere afterwards, show only after a rescan.
- The scan root (often a whole drive) and the `<metadata and unaccounted>` item have no context menu. Rename is not offered in the menu, and there is no "show in Explorer" command for a file (use Open on its folder).
- A whole-drive scan adds one item at the root, `<metadata and unaccounted>`: the drive's used space (as Explorer reports it) less what the scanned files take. It holds what no file in the tree accounts for: NTFS metadata (`$MFT`, `$LogFile`, the change journal and the rest of `$Extend`, `$Bitmap`, `$Secure`), folder indexes, records the MFT scan cannot place in the tree and, in walk mode, everything the walk could not read. It cannot be opened or deleted. Folder scans and network drives have no such item.
- Size on disk counts every data stream of a file in MFT mode (alternate data streams, and the hidden stream where a WOF-compressed file keeps its data). The walk sees only what Windows reports for the main stream, which includes WOF data but not other alternate streams. A small file kept inside its MFT record counts 0 bytes on disk in both modes, as in Explorer.
- Windows and NTFS only for the fast path. Other filesystems (FAT32, exFAT, network shares) use the slower walk.
- Items the walk cannot read (no permission, locked) are left out of the totals; the top bar says how many.
- The walk recurses once per folder level, so an extremely deep folder chain could exhaust a thread stack. The MFT scan has no such limit.
- The MFT scan reads the volume as it is on disk without flushing it first, so changes made seconds before a scan may not show.
- Hard links are counted once. In an MFT folder scan a file is placed under its first link inside the folder.
- The MFT scan has **not yet been verified on a real volume**, and the window and the context menu have not been checked by eye after the latest layout change.
- Large, heavily fragmented files whose data lives in `$ATTRIBUTE_LIST` extension records: the MFT scan takes their sizes from the `$DATA` extent that starts at VCN 0 (the only one that records the stream sizes), wherever it lives, and takes a missing `$FILE_NAME` from an extension record. Only the first name found in extension records is used (further hard links held there are ignored), and a `$REPARSE_POINT` held only in an extension record is not seen.
- WOF-compressed files (`compact /exe`, used by Windows for system files) count their compressed data in both modes; the elevated test compares the two.
- There is no scan-time tree preview: the tree and treemap appear when the scan ends.
