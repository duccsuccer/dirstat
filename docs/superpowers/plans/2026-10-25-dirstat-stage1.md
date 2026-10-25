# dirstat Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Windows disk-usage analyzer in Rust: fast NTFS MFT scan, parallel-walk fallback, classic three-pane egui window, and open / copy-path / recycle-delete actions.

**Architecture:** One binary crate. A pure data layer (`model`, `treemap`) is built and tested first. Two scanners (`scan::walk`, `scan::mft`) both produce the same `Tree`. `scan::start` picks the scanner and streams `ScanEvent`s to the egui app over a channel. UI panes are separate modules that read `AppState`.

**Tech Stack:** Rust (MSVC toolchain), `eframe`/`egui`, `windows` (windows-rs), `rayon`, `rfd`, `arboard`, `embed-resource`; `tempfile` for tests. Pin the latest stable version of each when adding it.

**Spec:** `docs/superpowers/specs/2026-10-25-dirstat-design.md`

## Global Constraints

- Windows only. Language is Rust; C and C++ dependencies are allowed, but prefer a Rust crate where it is as good.
- GUI is `egui` (via `eframe`).
- The manifest sets `requireAdministrator`; the app never relaunches itself. Embed it for the binary only (`cargo:rustc-link-arg-bins`), so `cargo test` runs without elevation.
- Headline size is **size on disk (allocated)**: tree, percentage bars, treemap areas, legend shares. Logical size is a second tree column. A hard-linked file counts once, under the first path found.
- Totals must agree with what Windows reports for the same files.
- No auto-scan at launch. Top bar: drive dropdown plus a "Browse folder" button; before a target is chosen the panes show an empty-state prompt.
- A folder scan on an NTFS volume reads the whole volume's MFT and keeps only the chosen subtree. Non-NTFS targets use the walk.
- The UI stays responsive on tens of millions of files. Treemap blocks smaller than a pixel are skipped or merged into their parent's block.
- Delete: files and folders, always asks for confirmation, goes to the Recycle Bin through the shell, refuses (never falls back to permanent delete) when the item cannot be recycled, and updates the tree in place without a rescan.
- Stage 1 only: no permanent delete, cleanups, color options, multi-target scans, or filters.

## Review Focus

Each line below has a test in the owning task.

1. Unreadable or access-denied folders during the walk: skipped, counted, scan continues (Task 3).
2. Junctions, symlinks and other reparse points: never followed, no cycles, no double counting (Tasks 3, 4).
3. Paths over 260 characters and non-BMP Unicode names (Tasks 3, 4).
4. Empty folder, zero-byte files, zero total size: no divide-by-zero in percentages or the treemap (Tasks 1, 2).
5. Rescan or window close while a scan is running: the old scan is cancelled and its events are ignored (Task 6).

## File Structure

- `Cargo.toml`, `build.rs`, `dirstat.manifest`: crate, manifest embedding.
- `src/main.rs`: eframe entry point.
- `src/model.rs`: `Tree`, `NodeId`, `TreeBuilder`, extension stats, `remove`.
- `src/treemap.rs`: squarified layout.
- `src/scan/mod.rs`: `ScanTarget`, `ScanEvent`, `ScanMode`, `start`.
- `src/scan/walk.rs`: parallel directory walk.
- `src/scan/mft_parse.rs`: pure MFT record parser.
- `src/scan/mft.rs`: volume access, MFT reading, tree building.
- `src/app.rs`: `AppState`, top bar, event pump.
- `src/ui_tree.rs`, `src/ui_legend.rs`, `src/ui_treemap.rs`: the three panes.
- `src/actions.rs`: open in Explorer, copy path, recycle delete.
- `examples/scan_bench.rs`: scan timing.

---

### Task 1: Toolchain, skeleton and tree model

**Files:**
- Create: `Cargo.toml`, `build.rs`, `dirstat.manifest`, `src/main.rs`, `src/model.rs`

**Interfaces:**
- Produces (`model.rs`):
  - `pub struct NodeId(pub u32)`, derives `Copy, Eq, Hash, Ord`
  - `pub struct Node { pub name: String, pub parent: Option<NodeId>, pub is_dir: bool, pub size: u64, pub logical: u64, pub ext: u16, pub children: Vec<NodeId> }` (`size` = size on disk; for directories, the total of all descendants)
  - `pub struct Tree { pub nodes: Vec<Node>, pub root: NodeId, pub exts: Vec<ExtStat> }`
  - `pub struct ExtStat { pub ext: String, pub size: u64, pub count: u64 }`, sorted by `size` descending after `finish`
  - `pub struct TreeBuilder`: `new(root_name: &str) -> Self`, `add(&mut self, parent: NodeId, name: &str, is_dir: bool, size: u64, logical: u64) -> NodeId`, `finish(self) -> Tree` (sums directories, sorts each node's children by `size` descending, fills `exts`)
  - `Tree::path(&self, id: NodeId) -> String`, `Tree::remove(&mut self, id: NodeId)` (detaches the node and subtracts its sizes from every ancestor), `Tree::percent_of_parent(&self, id: NodeId) -> f32` (0.0 when the parent's size is 0)

- [ ] **Step 1:** Install Rust (rustup, `stable-x86_64-pc-windows-msvc`) and confirm MSVC Build Tools with the C++ workload are present. This changes the machine, so ask the user before installing. Verify: `cargo --version` and `cargo new --bin` in a scratch directory builds.
- [ ] **Step 2:** `git init` in the project root; add `.gitignore` (`target/`, `.superpowers/`).
- [ ] **Step 3: Write failing tests in `src/model.rs`:** `finish_sums_directories_and_sorts_children`, `ext_stats_sorted_by_size`, `remove_subtracts_from_all_ancestors`, `empty_tree_percent_is_zero_not_nan` (a root with no children and a zero-byte file, `percent_of_parent` returns `0.0`), `path_joins_names_from_root`.
- [ ] **Step 4:** Run `cargo test model` and see them fail.
- [ ] **Step 5:** Implement the model. Arena of `Vec<Node>`; extension is the lower-cased text after the last `.` ("" when none), interned into `exts`.
- [ ] **Step 6:** Run `cargo test model` and see them pass.
- [ ] **Step 7:** `build.rs` embeds `dirstat.manifest` (`requestedExecutionLevel level="requireAdministrator"`) with `embed-resource`, bins only. Verify: `cargo build` succeeds, and running `target\debug\dirstat.exe` from a non-elevated shell raises the UAC prompt (or error 740).
- [ ] **Step 8:** Commit: `feat: tree model, manifest, skeleton`.

---

### Task 2: Treemap layout

**Files:**
- Create: `src/treemap.rs`

**Interfaces:**
- Consumes: `Tree`, `NodeId` from Task 1.
- Produces: `pub struct Rect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }`; `pub fn layout(tree: &Tree, root: NodeId, bounds: Rect) -> Vec<(NodeId, Rect)>` returns one rect per drawn **file** (leaf) block, in draw order. Directories are laid out recursively by squarified subdivision of their children by `size`. A child whose rect is under 1 px in width or height is dropped, and its area stays as the parent's gap.

- [ ] **Step 1: Write failing tests:** `areas_proportional_to_size` (three files 50/30/20 in a 100x100 rect, areas within 1% of 5000/3000/2000), `blocks_stay_inside_bounds_and_do_not_overlap`, `subpixel_blocks_are_dropped` (one huge file plus 1,000 one-byte files in 100x100: the result has fewer than 1,001 entries and still contains the huge file), `zero_total_returns_empty` (no panic, empty vec), `nested_directories_recurse`.
- [ ] **Step 2:** Run `cargo test treemap` and see them fail.
- [ ] **Step 3:** Implement `layout` (squarified, Bruls et al.). It is iterative or depth-bounded so a 1,000-level directory chain does not overflow the stack.
- [ ] **Step 4:** Run `cargo test treemap` and see them pass.
- [ ] **Step 5:** Commit: `feat: squarified treemap layout`.

---

### Task 3: Fallback directory walk

**Files:**
- Create: `src/scan/mod.rs` (types only, below), `src/scan/walk.rs`

**Interfaces:**
- Produces (`scan/mod.rs`):
  - `pub enum ScanTarget { Volume(char), Folder(PathBuf) }`
  - `pub enum ScanMode { Mft, Walk }`
  - `pub enum ScanEvent { Progress { files: u64, bytes: u64 }, Partial(Tree), Done { tree: Tree, mode: ScanMode, skipped: u64 }, Failed(String) }`
- Produces (`walk.rs`): `pub fn walk(root: &Path, cancel: &AtomicBool, on_progress: &mut dyn FnMut(u64, u64)) -> (Tree, u64)`; the `u64` is the number of unreadable entries skipped.

- [ ] **Step 1: Write failing tests** in `walk.rs` using `tempfile`:
  - `sizes_match_fixture` (known file sizes; `logical` equals the written length; `size` is at least `logical` and a multiple of the cluster size for non-empty files)
  - `empty_folder_and_zero_byte_file` (no panic)
  - `unreadable_folder_is_skipped_and_counted` (deny read via `icacls` on a subfolder; `skipped >= 1`, other files still counted)
  - `junction_is_not_followed` (create with `mklink /J`; target's files appear once)
  - `hard_link_counted_once` (`fs::hard_link`; total equals one copy)
  - `long_and_non_bmp_names` (a path over 260 characters, and a name containing `𐍈`; both found)
  - `cancel_stops_early` (flag set before the call returns promptly with a partial tree)
- [ ] **Step 2:** Run `cargo test walk` and see them fail.
- [ ] **Step 3:** Implement with `rayon` over directory entries. Use the `\\?\` prefix for long paths. Take allocated size from `GetFileInformationByHandleEx(FileStandardInfo).AllocationSize`. Deduplicate hard links by (volume serial, file index) when the link count is above 1. Skip reparse points (`FILE_ATTRIBUTE_REPARSE_POINT`) without descending.
- [ ] **Step 4:** Run `cargo test walk` and see them pass.
- [ ] **Step 5:** Commit: `feat: parallel directory walk scanner`.

---

### Task 4: MFT record parser

**Files:**
- Create: `src/scan/mft_parse.rs`

**Interfaces:**
- Produces: `pub struct MftRecord { pub index: u64, pub parent: u64, pub name: String, pub is_dir: bool, pub allocated: u64, pub logical: u64, pub is_reparse: bool }` and `pub fn parse_record(buf: &mut [u8], index: u64) -> Option<MftRecord>`. It returns `None` for unused, extension (non-base) and corrupt records. `parent` is the 48-bit record number, with the sequence number stripped.

- [ ] **Step 1: Write failing tests** against synthetic 1024-byte records built by a test helper (`make_record(flags, names, data_attr)`) that applies the update-sequence fixups:
  - `parses_name_parent_and_sizes`
  - `prefers_win32_name_over_dos_name` (a record with both an 8.3 and a long name returns the long one)
  - `hard_links_use_first_non_dos_name`
  - `non_resident_data_uses_allocated_and_real_size`, `resident_data_uses_value_length_and_zero_allocation`
  - `sparse_and_compressed_use_allocated_not_logical` (allocated is what the attribute header reports)
  - `unused_record_returns_none`, `bad_fixup_returns_none`, `extension_record_returns_none`
  - `utf16_surrogate_pair_name_decodes` (`𐍈`)
  - `reparse_flag_is_reported`
- [ ] **Step 2:** Run `cargo test mft_parse` and see them fail.
- [ ] **Step 3:** Implement the parser: validate `FILE` magic, apply fixups, read the flags (in use, directory), walk the attributes for `$FILE_NAME` (0x30) and the unnamed `$DATA` (0x80), and read the standard-information/file-name flags for reparse. Records whose `$DATA` sits only in an `$ATTRIBUTE_LIST` extension keep the size from the base record; document this limit with a `shortcut:` comment.
- [ ] **Step 4:** Run `cargo test mft_parse` and see them pass.
- [ ] **Step 5:** Commit: `feat: NTFS MFT record parser`.

---

### Task 5: MFT volume scan

**Files:**
- Create: `src/scan/mft.rs`

**Interfaces:**
- Consumes: `parse_record`, `TreeBuilder`, `ScanTarget`.
- Produces: `pub fn scan_volume(letter: char, subtree: Option<&Path>, cancel: &AtomicBool, on_progress: &mut dyn FnMut(u64, u64)) -> Result<Tree, String>`. Opens `\\.\X:`, calls `FSCTL_GET_NTFS_VOLUME_DATA` and `FSCTL_GET_RETRIEVAL_POINTERS` for `$MFT`, reads the MFT in large sequential chunks across worker threads (`rayon`), parses with `parse_record`, resolves parent links into a `Tree`, and, when `subtree` is given, re-roots the tree at that folder. Returns `Err` when the volume is not NTFS or cannot be opened.

- [ ] **Step 1: Write a failing, `#[ignore]`d test** `mft_matches_walk_on_same_folder` (needs an elevated shell): create a fixture folder on `C:` with files, a subfolder, a hard link and a junction, then compare `scan_volume('C', Some(fixture))` against `walk(fixture)`. Assert: equal total `size`, equal total `logical`, equal file count. Add a second test `subtree_reroots_at_folder` (the root name is the folder name).
- [ ] **Step 2:** Run `cargo test --release mft -- --ignored` from an elevated shell and see them fail.
- [ ] **Step 3:** Implement. Orphans whose parent chain never reaches the root (deleted parents) are dropped. Skip the NTFS system metafiles (record numbers below 24, including `$MFT` itself). Check `cancel` between chunks.
- [ ] **Step 4:** Re-run the ignored tests and see them pass. If totals differ from the walk, fix the parser or the size rules until they agree (the "totals agree with Windows" anti-goal).
- [ ] **Step 5:** Commit: `feat: MFT volume scanner`.

---

### Task 6: Scan dispatch, events and cancellation

**Files:**
- Modify: `src/scan/mod.rs`

**Interfaces:**
- Consumes: `walk`, `scan_volume`.
- Produces: `pub fn choose_mode(target: &ScanTarget, fs_name: &str) -> ScanMode` (`Mft` when `fs_name == "NTFS"`, otherwise `Walk`); `pub fn start(target: ScanTarget, tx: Sender<ScanEvent>, ctx: egui::Context) -> ScanHandle`; `pub struct ScanHandle { cancel: Arc<AtomicBool> }` with `cancel(&self)`, also cancelled on `Drop`. The scan thread sends `Progress` at most every 100 ms, calls `ctx.request_repaint()` after each send, and falls back to `Walk` (reporting `mode: Walk`) when `scan_volume` returns `Err`. Walk mode sends a `Partial` tree about every 500 ms; MFT mode sends `Progress` only and the tree arrives with `Done`.

- [ ] **Step 1: Write failing tests:** `ntfs_chooses_mft_others_walk`, `mft_error_falls_back_to_walk` (inject a failing scanner function; `Done.mode == Walk`), `cancel_suppresses_done` (cancel immediately; no `Done` arrives within 2 s), `drop_cancels`.
- [ ] **Step 2:** Run `cargo test scan::` and see them fail.
- [ ] **Step 3:** Implement the dispatch; look up the filesystem name with `GetVolumeInformationW`. Make the scanner functions injectable for the tests (function pointers on a private struct are enough).
- [ ] **Step 4:** Run `cargo test scan::` and see them pass.
- [ ] **Step 5:** Commit: `feat: scan dispatch and events`.

---

### Task 7: App shell, top bar and empty state

**Files:**
- Create: `src/app.rs`, `src/ui_tree.rs`, `src/ui_legend.rs`, `src/ui_treemap.rs` (empty `pub fn show(...)` stubs for now)
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `pub struct AppState { pub tree: Option<Tree>, pub selected: Option<NodeId>, pub expanded: HashSet<NodeId>, pub mode: Option<ScanMode>, pub progress: (u64, u64), pub scan: Option<ScanHandle>, pub rx: Receiver<ScanEvent>, pub target: Option<ScanTarget>, pub error: Option<String> }`; `impl eframe::App for AppState`; `AppState::begin_scan(&mut self, target: ScanTarget, ctx: &egui::Context)` (drops the old `ScanHandle`, which cancels it, and replaces `rx` with a fresh channel so stale events can never arrive); `AppState::pump(&mut self)` applies queued events. Each pane module exposes `pub fn show(ui: &mut egui::Ui, state: &mut AppState)`.

- [ ] **Step 1: Write failing tests** (no GUI needed): `stale_events_after_rescan_are_ignored` (`begin_scan` twice; an event sent on the first channel never changes state), `partial_event_replaces_tree`, `done_event_sets_tree_and_mode`, `failed_event_sets_error`.
- [ ] **Step 2:** Run `cargo test app` and see them fail.
- [ ] **Step 3:** Implement `AppState` and the top bar: drive dropdown (`GetLogicalDriveStringsW`), "Browse folder" (`rfd::FileDialog`), Rescan (disabled with no target), and a badge reading "Fast (MFT)" or "Fallback (walk)". Show the empty-state prompt until a target is chosen, and a progress line (`files`, bytes) while scanning. Lay the panes out as in the spec: tree top-left, legend top-right, treemap bottom (`egui::TopBottomPanel` and `SidePanel`).
- [ ] **Step 4:** Run `cargo test app` and see them pass. Then, from an elevated shell, `cargo run` and confirm the window opens, the drive dropdown lists drives, and nothing scans until a target is chosen.
- [ ] **Step 5:** Commit: `feat: app shell and top bar`.

---

### Task 8: Tree pane and legend pane

**Files:**
- Modify: `src/ui_tree.rs`, `src/ui_legend.rs`

**Interfaces:**
- Consumes: `AppState`, `Tree::percent_of_parent`, `Tree::exts`.
- Produces: `ui_tree::visible_rows(tree: &Tree, expanded: &HashSet<NodeId>) -> Vec<(NodeId, usize)>` (node and depth, in display order); `ui_tree::ensure_visible(tree: &Tree, expanded: &mut HashSet<NodeId>, id: NodeId)` (expands every ancestor); `ui_legend::color_for(ext_index: u16) -> egui::Color32` (stable palette; ranks beyond the palette share a grey).

- [ ] **Step 1: Write failing tests:** `visible_rows_respects_expansion_and_depth`, `ensure_visible_expands_all_ancestors`, `color_for_is_stable_and_distinct_for_top_ten`.
- [ ] **Step 2:** Run `cargo test ui_` and see them fail.
- [ ] **Step 3:** Implement. The tree pane uses `ScrollArea::show_rows` over `visible_rows` so only on-screen rows are built. Columns: name (with expander), size on disk, percentage bar from `percent_of_parent`, logical size. Clicking a row sets `state.selected`. The legend lists color, extension, share of the scanned size.
- [ ] **Step 4:** Run `cargo test ui_` and see them pass. In an elevated `cargo run`, scan a folder and confirm the tree and legend show it and scroll smoothly.
- [ ] **Step 5:** Commit: `feat: tree and legend panes`.

---

### Task 9: Treemap pane and linked selection

**Files:**
- Modify: `src/ui_treemap.rs`

**Interfaces:**
- Consumes: `treemap::layout`, `color_for`, `ensure_visible`.
- Produces: `ui_treemap::hit_test(blocks: &[(NodeId, Rect)], x: f32, y: f32) -> Option<NodeId>`; `ui_treemap::blocks_under(tree: &Tree, blocks: &[(NodeId, Rect)], id: NodeId) -> Vec<Rect>` (the blocks of a file, or of every descendant file of a directory). The layout is cached in `AppState` and recomputed only when the pane size, the tree, or a deletion changes.

- [ ] **Step 1: Write failing tests:** `hit_test_finds_block_and_misses_gap`, `blocks_under_directory_returns_descendant_blocks`.
- [ ] **Step 2:** Run `cargo test ui_treemap` and see them fail.
- [ ] **Step 3:** Implement. Paint each block as a filled rect colored by extension. Clicking a block sets `selected` and calls `ensure_visible`, and the tree pane scrolls to it. Selecting a tree row outlines its blocks via `blocks_under`. Show the path and sizes of the hovered block in a tooltip.
- [ ] **Step 4:** Run `cargo test ui_treemap` and see them pass. In an elevated `cargo run`, confirm that clicking a block selects the tree row and that clicking a row highlights the blocks.
- [ ] **Step 5:** Commit: `feat: treemap pane and linked selection`.

---

### Task 10: File actions

**Files:**
- Create: `src/actions.rs`
- Modify: `src/ui_tree.rs`, `src/ui_treemap.rs` (context menu on rows and blocks)

**Interfaces:**
- Produces: `pub fn open_in_explorer(path: &Path) -> std::io::Result<()>` (runs `explorer.exe /select,<path>`); `pub fn copy_path(path: &str) -> Result<(), String>` (`arboard`); `pub enum DeleteOutcome { Recycled, Refused(String), Failed(String) }`; `pub fn recycle(path: &Path) -> DeleteOutcome` using `IFileOperation` with `FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE` and no UI of its own, so an item that cannot be recycled is reported as `Refused` and never permanently deleted. `AppState::delete_selected(&mut self, confirmed: bool)` calls `recycle` and, only on `Recycled`, calls `Tree::remove` and invalidates the cached layout.

- [ ] **Step 1: Write failing tests:** `recycled_file_disappears_and_is_in_bin` (create a temp file, `recycle`, assert the file is gone and `Recycled`), `missing_path_returns_failed_not_panic`, `delete_selected_requires_confirmation` (with `confirmed = false`, the file and tree are untouched), `refused_delete_leaves_tree_unchanged` (inject a fake `recycle` returning `Refused`).
- [ ] **Step 2:** Run `cargo test actions` and see them fail.
- [ ] **Step 3:** Implement. The context menu has "Open in Explorer", "Copy path" and "Delete to Recycle Bin"; the last opens a confirmation dialog naming the item and its size, and shows the reason in a dialog when `Refused`. Deleting the scan root is not offered.
- [ ] **Step 4:** Run `cargo test actions` and see them pass. Manually, verify against the real shell and record the results in the commit message: a file larger than the Recycle Bin limit is refused; an item on a removable or network drive is refused.
- [ ] **Step 5:** Commit: `feat: open, copy path and recycle delete`.

---

### Task 11: Benchmark and final verification

**Files:**
- Create: `examples/scan_bench.rs`

**Interfaces:**
- Consumes: `scan::start` or the scanner functions directly.
- Produces: `cargo run --release --example scan_bench -- <drive letter or folder>` prints mode, elapsed seconds, file count and total size.

- [ ] **Step 1:** Implement the example. Run it from an elevated shell against `C:` and against a non-NTFS or removable drive if one is available.
- [ ] **Step 2:** Record the numbers in `README.md` next to the same volume's WinDirStat time, and WizTree's if installed. The goal is the same order of magnitude as WizTree and clearly faster than WinDirStat; this is reported, not a pass/fail gate.
- [ ] **Step 3:** Final checks: `cargo test`, `cargo test --release -- --ignored` (elevated), `cargo clippy -- -D warnings`. Run the app on `C:` and confirm the window stays responsive while scanning, totals look right against Explorer's Properties for a few folders, and no auto-scan happens at launch.
- [ ] **Step 4:** Commit: `docs: benchmark results`.
