# dirstat

dirstat is an open-source disk usage analyzer for Windows in the style of WinDirStat: pick a drive or folder, see which folders and file types use the space, and open, copy or delete items. It is written in Rust with an egui interface.

The scan is the point of the project. On an NTFS volume it reads the Master File Table directly instead of walking every directory. Other filesystems, and folder scans, use a parallel directory walk.

## Build and run

```
cargo build --release
```

The executable asks for administrator rights when it starts (its manifest requires them). The MFT scan needs them; the app does not relaunch itself.

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

The example prints the scan mode, elapsed seconds, file count and size on disk. `--walk` forces the directory walk. Run it from an elevated terminal to use the MFT path; unelevated, an NTFS volume falls back to the walk and the output says so.

### Results

| Mode | Target | Files | Size on disk | Seconds |
|------|--------|-------|--------------|---------|
| Mft | `C:` | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) |
| Mft | `C:\Users\Admin\Documents` | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) | not yet measured (needs an elevated run) |
| Walk | `C:\Users\Admin\Documents` | 156,081 | 40.6 GB | 1.85 to 1.88 (warm); 6.01 on the first, cold run |
| Walk | `D:` | 59,262 | 826.2 GB | 0.47 to 0.56 |

Walk rows were measured unelevated on one machine, with the file-cache state noted above. No WinDirStat or WizTree numbers are recorded yet.

## Status and known limits

- Stage 1 only: scan, tree and treemap views, open, copy path, and delete to the Recycle Bin. Permanent delete and other stage 2 features are not built.
- Windows and NTFS only for the fast path. Other filesystems (FAT32, exFAT, network shares) use the slower walk.
- The MFT scan and the delete-refusal cases have **not yet been verified on a real volume**.
- Large, heavily fragmented files whose data lives in `$ATTRIBUTE_LIST` extension records: the MFT scan takes their sizes from the `$DATA` extent that starts at VCN 0 (the only one that records the stream sizes), wherever it lives, and takes a missing `$FILE_NAME` from an extension record. Only the first name found in extension records is used (further hard links held there are ignored), and a `$REPARSE_POINT` held only in an extension record is not seen.
- There is no scan-time tree preview: the tree and treemap appear when the scan ends.
