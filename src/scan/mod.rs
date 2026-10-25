//! Disk scan: scan targets, the events sent to the UI, and the scanner backends.

pub mod walk;

use crate::model::Tree;
use std::path::PathBuf;

#[derive(Debug)]
pub enum ScanTarget {
    Volume(char),
    Folder(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanMode {
    Mft,
    Walk,
}

#[derive(Debug)]
pub enum ScanEvent {
    Progress { files: u64, bytes: u64 },
    Done { tree: Tree, mode: ScanMode, skipped: u64 },
    Failed(String),
}
