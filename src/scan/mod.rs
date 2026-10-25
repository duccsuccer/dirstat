//! Disk scan: scan targets, the events sent to the UI, and the scanner backends.

pub mod mft;
pub mod mft_parse;
pub mod walk;

use crate::model::Tree;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

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

const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// Running scan. Dropping the handle cancels the scan.
pub struct ScanHandle {
    cancel: Arc<AtomicBool>,
}

impl ScanHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl Drop for ScanHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Picks the MFT scanner for NTFS volumes and folders on a drive letter; everything else is walked.
pub fn choose_mode(target: &ScanTarget, fs_name: &str) -> ScanMode {
    if fs_name == "NTFS" && drive_letter(target).is_some() {
        ScanMode::Mft
    } else {
        ScanMode::Walk
    }
}

fn drive_letter(target: &ScanTarget) -> Option<char> {
    match target {
        ScanTarget::Volume(letter) => Some(*letter),
        ScanTarget::Folder(path) => match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => Some(letter as char),
                _ => None,
            },
            _ => None,
        },
    }
}

/// The scanner backends, as values so tests can inject them.
struct Scanners {
    mft: Box<dyn Fn(char, Option<&Path>, &AtomicBool, &mut dyn FnMut(u64, u64)) -> Result<Tree, String> + Send>,
    walk: Box<dyn Fn(&Path, &AtomicBool, &mut dyn FnMut(u64, u64)) -> (Tree, u64) + Send>,
    fs_name: Box<dyn Fn(char) -> String + Send>,
    root_check: Box<dyn Fn(&Path) -> std::io::Result<()> + Send>,
}

impl Scanners {
    fn system() -> Self {
        Scanners {
            mft: Box::new(mft::scan_volume),
            walk: Box::new(walk::walk),
            fs_name: Box::new(volume_fs_name),
            root_check: Box::new(|root: &Path| std::fs::read_dir(root).map(|_| ())),
        }
    }
}

/// Filesystem name of volume `letter` (for example "NTFS"), or empty when it cannot be read.
fn volume_fs_name(letter: char) -> String {
    let root: Vec<u16> = format!(r"{letter}:\").encode_utf16().chain(std::iter::once(0)).collect();
    let mut name = [0u16; 32];
    let read = unsafe { GetVolumeInformationW(PCWSTR(root.as_ptr()), None, None, None, None, Some(&mut name[..])) };
    if read.is_err() {
        return String::new();
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..len])
}

/// Starts scanning `target` on a background thread. Events arrive on `tx`; `repaint` runs after each send.
pub fn start(target: ScanTarget, tx: Sender<ScanEvent>, repaint: Box<dyn Fn() + Send + 'static>) -> ScanHandle {
    start_with(target, tx, repaint, Scanners::system())
}

fn start_with(
    target: ScanTarget,
    tx: Sender<ScanEvent>,
    repaint: Box<dyn Fn() + Send + 'static>,
    scanners: Scanners,
) -> ScanHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let thread_cancel = Arc::clone(&cancel);
    thread::spawn(move || run(&target, &tx, repaint.as_ref(), &thread_cancel, &scanners));
    ScanHandle { cancel }
}

fn run(target: &ScanTarget, tx: &Sender<ScanEvent>, repaint: &dyn Fn(), cancel: &AtomicBool, scanners: &Scanners) {
    let mut last_sent: Option<Instant> = None;
    let mut on_progress = |files: u64, bytes: u64| {
        if last_sent.is_none_or(|sent| sent.elapsed() >= PROGRESS_INTERVAL) {
            last_sent = Some(Instant::now());
            let _ = tx.send(ScanEvent::Progress { files, bytes });
            repaint();
        }
    };

    let letter = drive_letter(target);
    let fs_name = letter.map(|letter| (scanners.fs_name)(letter)).unwrap_or_default();
    let mft_result = match (choose_mode(target, &fs_name), letter) {
        (ScanMode::Mft, Some(letter)) => {
            let subtree = match target {
                ScanTarget::Volume(_) => None,
                ScanTarget::Folder(path) => Some(path.as_path()),
            };
            Some((scanners.mft)(letter, subtree, cancel, &mut on_progress))
        }
        _ => None,
    };

    let (tree, mode, skipped) = match mft_result {
        Some(Ok(tree)) => (tree, ScanMode::Mft, 0),
        // A cancelled MFT scan must not fall back to the walk.
        Some(Err(_)) if cancel.load(Ordering::SeqCst) => return,
        _ => {
            let root = match target {
                ScanTarget::Volume(letter) => PathBuf::from(format!(r"{letter}:\")),
                ScanTarget::Folder(path) => path.clone(),
            };
            if let Err(err) = (scanners.root_check)(&root) {
                if !cancel.load(Ordering::SeqCst) {
                    let _ = tx.send(ScanEvent::Failed(format!("cannot read {}: {err}", root.display())));
                    repaint();
                }
                return;
            }
            let (tree, skipped) = (scanners.walk)(&root, cancel, &mut on_progress);
            (tree, ScanMode::Walk, skipped)
        }
    };

    if cancel.load(Ordering::SeqCst) {
        return;
    }
    let _ = tx.send(ScanEvent::Done { tree, mode, skipped });
    repaint();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TreeBuilder;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
    use std::time::{Duration, Instant};

    fn tree() -> Tree {
        TreeBuilder::new("test").finish()
    }

    fn no_repaint() -> Box<dyn Fn() + Send> {
        Box::new(|| {})
    }

    fn failing_mft(_: char, _: Option<&Path>, _: &AtomicBool, _: &mut dyn FnMut(u64, u64)) -> Result<Tree, String> {
        Err("needs administrator".into())
    }

    fn cancelling_failed_mft(_: char, _: Option<&Path>, cancel: &AtomicBool, _: &mut dyn FnMut(u64, u64)) -> Result<Tree, String> {
        cancel.store(true, Ordering::SeqCst);
        Err("needs administrator".into())
    }

    fn readable(_: &Path) -> std::io::Result<()> {
        Ok(())
    }

    fn fake_walk(_: &Path, _: &AtomicBool, _: &mut dyn FnMut(u64, u64)) -> (Tree, u64) {
        (tree(), 0)
    }

    /// A walk that waits for cancellation, reports the cancel flag it saw, then returns.
    fn blocking_walk(seen: Sender<bool>) -> Scanners {
        Scanners {
            mft: Box::new(failing_mft),
            walk: Box::new(move |_: &Path, cancel: &AtomicBool, _: &mut dyn FnMut(u64, u64)| {
                let start = Instant::now();
                while !cancel.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let _ = seen.send(cancel.load(Ordering::SeqCst));
                (tree(), 0)
            }),
            fs_name: Box::new(|_| "FAT32".into()),
            root_check: Box::new(readable),
        }
    }

    #[test]
    fn ntfs_chooses_mft_others_walk() {
        assert_eq!(choose_mode(&ScanTarget::Volume('C'), "NTFS"), ScanMode::Mft);
        assert_eq!(choose_mode(&ScanTarget::Folder(r"C:\data".into()), "NTFS"), ScanMode::Mft);
        assert_eq!(choose_mode(&ScanTarget::Volume('C'), "FAT32"), ScanMode::Walk);
        assert_eq!(choose_mode(&ScanTarget::Folder(r"\server\share".into()), "NTFS"), ScanMode::Walk);
    }

    #[test]
    fn mft_error_falls_back_to_walk() {
        let (tx, rx) = channel();
        let scanners = Scanners {
            mft: Box::new(failing_mft),
            walk: Box::new(fake_walk),
            fs_name: Box::new(|_| "NTFS".into()),
            root_check: Box::new(readable),
        };
        let _handle = start_with(ScanTarget::Volume('C'), tx, no_repaint(), scanners);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ScanEvent::Done { mode, .. } => assert_eq!(mode, ScanMode::Walk),
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn cancel_suppresses_done() {
        let (tx, rx) = channel();
        let (seen_tx, seen_rx) = channel();
        let handle = start_with(ScanTarget::Volume('C'), tx, no_repaint(), blocking_walk(seen_tx));
        handle.cancel();
        assert!(seen_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn drop_cancels() {
        let (tx, rx) = channel();
        let (seen_tx, seen_rx) = channel();
        let handle = start_with(ScanTarget::Volume('C'), tx, no_repaint(), blocking_walk(seen_tx));
        drop(handle);
        assert!(seen_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(RecvTimeoutError::Disconnected)));
    }

    #[test]
    fn cancel_after_mft_error_does_not_fall_back() {
        let (tx, rx) = channel();
        let (walk_tx, walk_rx) = channel::<()>();
        let scanners = Scanners {
            mft: Box::new(cancelling_failed_mft),
            walk: Box::new(move |_: &Path, _: &AtomicBool, _: &mut dyn FnMut(u64, u64)| {
                let _ = walk_tx.send(());
                (tree(), 0)
            }),
            fs_name: Box::new(|_| "NTFS".into()),
            root_check: Box::new(readable),
        };
        let _handle = start_with(ScanTarget::Volume('C'), tx, no_repaint(), scanners);
        assert!(matches!(rx.recv_timeout(Duration::from_secs(5)), Err(RecvTimeoutError::Disconnected)));
        assert!(walk_rx.try_recv().is_err());
    }

    #[test]
    fn unreadable_root_sends_failed_not_done() {
        let (tx, rx) = channel();
        let scanners = Scanners {
            mft: Box::new(failing_mft),
            walk: Box::new(fake_walk),
            fs_name: Box::new(|_| "FAT32".into()),
            root_check: Box::new(|_: &Path| Err(std::io::Error::from(std::io::ErrorKind::NotFound))),
        };
        let _handle = start_with(ScanTarget::Volume('Z'), tx, no_repaint(), scanners);
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            ScanEvent::Failed(message) => assert!(message.starts_with("cannot read Z:\\"), "{message}"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(matches!(rx.recv_timeout(Duration::from_secs(2)), Err(RecvTimeoutError::Disconnected)));
    }
}
