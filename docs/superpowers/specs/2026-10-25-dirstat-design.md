# dirstat: design

An open-source, Windows-only disk usage analyzer in Rust, in the style of WinDirStat, built around a much faster scan.

## Intent and the why

WinDirStat's scan is too slow, and the faster alternatives (WizTree and similar) are not open source. The user wants a free, open-source tool with the same familiar workflow: scan a drive, see which folders and file types use the space, and clean up. **Scan speed is the whole point of the project.** Everything else is "WinDirStat, but nothing is slower than it needs to be".

## Goals

- Scan a full NTFS volume in seconds by reading the NTFS Master File Table (MFT) directly.
- Fall back to a parallel directory walk when the fast path is unavailable (not NTFS, or a folder instead of a whole volume). Slower but correct.
- Classic WinDirStat window (chosen by the user from three mockups): folder tree top-left, file-type legend top-right, treemap along the bottom.
- Open source, written in Rust. C and C++ dependencies are allowed where they are the better tool.

## Non-goals (stage 1)

- Non-Windows platforms (the MFT is a Windows/NTFS concept).
- Anything in stage 2 below.

## Anti-goals

- A UI that blocks or shows a blank window while scanning.
- A "fast" scan whose numbers disagree with what Windows reports for the same files (hard links, sparse/compressed files, reparse points counted wrongly).
- Silent data loss: delete must be recoverable by default.

## Staging

The user wants all WinDirStat features, delivered in stages. **This document specifies stage 1 only.**

**Stage 1 (this design):**
- Fast MFT scan, plus parallel-walk fallback.
- Classic three-pane window with linked selection.
- File actions: open in Explorer, copy path, delete to the Recycle Bin.

**Stage 2 (later, not specified here):** permanent delete, user-defined cleanup commands, empty Recycle Bin, treemap and color options, scanning several drives or folders at once, filters and exclusions.

## The window

- **Top bar:** current path, a Rescan button, and a badge showing whether the scan ran in fast mode (NTFS MFT) or fallback mode.
- **Top left:** folder tree, sorted by size. Each row shows size and a percentage bar relative to its parent.
- **Top right:** file-type legend: color, extension, share of the scanned size.
- **Bottom:** treemap, one block per file, colored by file type.
- **Linked selection:** clicking a treemap block selects that file in the tree; selecting a tree row highlights its blocks in the treemap.
- **Scan feedback (builder's call on details):** the tree and treemap fill in as the scan runs rather than appearing at the end. Exact update cadence is left to the builder.

## Scan behavior

- **Fast mode:** open the volume as a raw handle (requires administrator), read and parse MFT file records, rebuild the folder tree and sizes in memory.
- **Elevation:** the app's manifest requires administrator rights (`requireAdministrator`), so Windows shows the UAC prompt at launch and the app never relaunches itself. *User's decision.* If the user declines the prompt, the app does not start. Fallback mode is therefore only for volumes the MFT path cannot read (non-NTFS, network or removable drives) and for folder scans where the builder judges the walk faster.
- **Fallback mode:** parallel directory walk; works on any filesystem.
- Both modes must produce the same tree model so the UI does not care which ran.

## Constraints and decisions

- **Language:** Rust. *User's decision.*
- **GUI toolkit:** `egui`. *My call, not agreed by the user:* pure Rust, immediate-mode, easy to paint a custom treemap, fine with very large trees. The user was told this and did not object, but it is the builder's to revisit if it proves a poor fit.
- **Dependencies:** no restriction on language; C and C++ libraries are allowed. *User's decision.* Prefer Rust crates where they are as good, to keep the build simple, but pick the best tool for each job.
- **Platform:** Windows only, for now. *User's decision (implied by NTFS/MFT).*
- **Delete:** goes to the Recycle Bin in stage 1. *My call, to honor the "no silent data loss" anti-goal.*

## Left to the builder

- Project layout, crate selection, threading model, and in-memory tree representation.
- Treemap layout algorithm (squarified is the usual choice), color palette, and rendering details.
- Handling of MFT edge cases (hard links, sparse and compressed files, reparse points, alternate data streams, very long paths). The anti-goal above sets the bar: totals should agree with what Windows reports.
- Update cadence and cancel behavior during a scan.
- Testing approach.

## Calls made after the builder check

These were not discussed with the user. They are my calls, made so the builder does not have to guess. The user may change any of them.

- **Choosing what to scan:** the app does not scan automatically at launch. The top bar has a drive dropdown and a "Browse folder" button; before anything is chosen the panes show an empty-state prompt. A folder scan on an NTFS volume reads the whole volume's MFT and keeps only the chosen subtree; otherwise it uses the fallback walk.
- **Headline size:** size on disk (allocated size) is the number shown in the tree, percentage bars, treemap areas and legend shares, because that is what answers "where did my space go". Logical size is shown as a second column in the tree. A hard-linked file is counted once, under the first path found.
- **Delete:** works on files and folders and always asks for confirmation. It goes through the Windows shell with undo (Recycle Bin) enabled. If an item cannot be recycled (too large for the bin, or a volume without a bin such as a network or removable drive), the app refuses and says why; it never falls back to permanent delete in stage 1. After a successful delete the tree and parent sizes update in place without a rescan.
- **Speed target:** the goal is a full scan on the same volume in the same order of magnitude as WizTree and clearly faster than WinDirStat. The builder should add a benchmark that reports scan time and file count on a real volume, not a hard pass/fail threshold.
- **Scale:** the UI must stay responsive on volumes with tens of millions of files. Blocks smaller than a pixel are skipped or merged into their parent's block when drawing the treemap.
