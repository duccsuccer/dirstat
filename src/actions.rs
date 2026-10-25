//! File actions on one item: open in Explorer, copy its path, delete it to the Recycle Bin.

use std::cell::Cell;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use windows::core::{implement, Ref, Result as WinResult, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::E_ABORT;
use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::WindowsProgramming::{DRIVE_CDROM, DRIVE_FIXED, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE};
use windows::Win32::UI::Shell::{
    FileOperation, IFileOperation, IFileOperationProgressSink, IFileOperationProgressSink_Impl, IShellItem,
    SHCreateItemFromParsingName, SHQueryRecycleBinW, FOFX_RECYCLEONDELETE, FOF_ALLOWUNDO, FOF_NOERRORUI, FOF_SILENT,
    FOF_WANTNUKEWARNING, SHQUERYRBINFO, TSF_DELETE_RECYCLE_IF_POSSIBLE,
};
use windows_core::ComObject;

/// Opens an Explorer window with `path` selected.
pub fn open_in_explorer(path: &Path) -> std::io::Result<()> {
    // Explorer wants `/select,"path"` as one argument, which Rust's own quoting does not produce; Windows paths
    // cannot contain `"`, so the quotes cannot be broken out of. Explorer exits with code 1 even on success,
    // so only a failure to start it counts.
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
}

pub fn copy_path(path: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(path))
        .map_err(|e| e.to_string())
}

#[derive(Debug, PartialEq)]
pub enum DeleteOutcome {
    Recycled,
    Refused(String),
    Failed(String),
}

/// Moves `path` to the Recycle Bin. Never deletes permanently: anything that cannot be recycled is refused.
pub fn recycle(path: &Path) -> DeleteOutcome {
    // Why this cannot delete permanently (stage 1 forbids it), in the order the guards run:
    // 1. Pre-check, before the item is touched: the volume must be a fixed local drive (GetDriveTypeW ==
    //    DRIVE_FIXED) and SHQueryRecycleBinW must succeed for it. Network, removable, optical and RAM drives are
    //    refused here.
    // 2. Flags: FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE ask for the Recycle Bin. FOF_NOCONFIRMATION is deliberately
    //    NOT set (it answers "Yes to All", which is how SHFileOperation callers silently nuke too-large files), and
    //    FOF_WANTNUKEWARNING asks the shell to warn before destroying instead of recycling. The docs do not say
    //    what FOFX_RECYCLEONDELETE does with an item too big for the bin, so it is not relied on alone.
    // 3. A progress sink is attached before anything runs (if Advise fails, nothing is performed).
    //    PreDeleteItem cancels the whole operation (documented: an error return cancels the delete and all
    //    pending operations) unless TSF_DELETE_RECYCLE_IF_POSSIBLE is set, i.e. unless the shell is about to
    //    recycle. This is the check Electron's shell.trashItem relies on.
    // 4. PostDeleteItem: success is reported only when the shell returns the item's new location in the Recycle
    //    Bin; a NULL location means it was destroyed, which is reported as a failure, not hidden.
    // Not ruled out: the shell showing its own "permanently delete?" prompt before PreDeleteItem with the recycle
    // flag still set. Then the user has to accept the shell's explicit warning; it is never silent.
    if let Some(reason) = volume_refusal(path) {
        return DeleteOutcome::Refused(reason);
    }
    let _com = ComApartment::enter();
    shell_recycle(path)
}

/// Why `drive_type` (a GetDriveTypeW result) has no usable Recycle Bin; `None` for a fixed local drive.
pub(crate) fn drive_type_refusal(drive_type: u32) -> Option<String> {
    let kind = match drive_type {
        DRIVE_FIXED => return None,
        DRIVE_REMOVABLE => "a removable drive",
        DRIVE_REMOTE => "a network drive",
        DRIVE_CDROM => "an optical drive",
        DRIVE_RAMDISK => "a RAM disk",
        _ => "a drive of unknown type",
    };
    Some(format!("Not deleted: it is on {kind}, which has no Recycle Bin, and this app never deletes permanently."))
}

fn volume_refusal(path: &Path) -> Option<String> {
    let mut root = [0u16; 261];
    if unsafe { GetVolumePathNameW(&HSTRING::from(path), &mut root) }.is_err() {
        return Some(format!("Not deleted: cannot tell which drive {} is on.", path.display()));
    }
    // GetVolumePathNameW wrote a NUL-terminated string into `root`, which outlives both calls.
    let root = PCWSTR(root.as_ptr());
    if let Some(reason) = drive_type_refusal(unsafe { GetDriveTypeW(root) }) {
        return Some(reason);
    }
    let mut info = SHQUERYRBINFO { cbSize: size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    if unsafe { SHQueryRecycleBinW(root, &mut info) }.is_err() {
        return Some("Not deleted: its drive has no Recycle Bin, and this app never deletes permanently.".into());
    }
    None
}

fn shell_recycle(path: &Path) -> DeleteOutcome {
    let sink = ComObject::new(RecycleSink::default());
    let performed = unsafe {
        (|| -> WinResult<()> {
            let item: IShellItem = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
            let op: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
            op.SetOperationFlags(FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOF_WANTNUKEWARNING | FOF_NOERRORUI | FOF_SILENT)?;
            op.Advise(&sink.to_interface::<IFileOperationProgressSink>())?;
            op.DeleteItem(&item, None)?;
            op.PerformOperations()
        })()
    };
    let shown = path.display();
    if sink.refused.get() {
        DeleteOutcome::Refused(format!(
            "Not deleted: {shown} cannot go to the Recycle Bin (it may be too large for it), and this app never deletes permanently."
        ))
    } else if sink.destroyed.get() {
        DeleteOutcome::Failed(format!("{shown} was deleted permanently by the shell instead of being recycled."))
    } else if sink.recycled.get() {
        DeleteOutcome::Recycled
    } else {
        match performed {
            Err(e) => DeleteOutcome::Failed(format!("Could not delete {shown}: {}", e.message())),
            Ok(()) => DeleteOutcome::Failed(format!("Could not delete {shown}: the shell did not move it to the Recycle Bin.")),
        }
    }
}

/// Holds this thread in a COM apartment for as long as it lives.
struct ComApartment(bool);

impl ComApartment {
    fn enter() -> Self {
        // S_OK and S_FALSE (already initialised) both take a reference that must be released. RPC_E_CHANGED_MODE
        // (the thread is already in another apartment) takes none, and COM stays usable as it is.
        ComApartment(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok())
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Watches the delete: cancels it before an item would be destroyed rather than recycled, and records the result.
#[implement(IFileOperationProgressSink)]
#[derive(Default)]
struct RecycleSink {
    refused: Cell<bool>,
    recycled: Cell<bool>,
    destroyed: Cell<bool>,
}

impl IFileOperationProgressSink_Impl for RecycleSink_Impl {
    fn PreDeleteItem(&self, dwflags: u32, _item: Ref<IShellItem>) -> WinResult<()> {
        if dwflags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            self.refused.set(true);
            return Err(E_ABORT.into());
        }
        Ok(())
    }

    fn PostDeleteItem(&self, _flags: u32, _item: Ref<IShellItem>, hrdelete: HRESULT, newly_created: Ref<IShellItem>) -> WinResult<()> {
        if hrdelete.is_ok() {
            // The newly created item is the one now in the Recycle Bin; NULL means the item was destroyed.
            let flag = if newly_created.as_ref().is_some() { &self.recycled } else { &self.destroyed };
            flag.set(true);
        }
        Ok(())
    }

    fn StartOperations(&self) -> WinResult<()> { Ok(()) }
    fn FinishOperations(&self, _: HRESULT) -> WinResult<()> { Ok(()) }
    fn PreRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> WinResult<()> { Ok(()) }
    fn PostRenameItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<IShellItem>) -> WinResult<()> { Ok(()) }
    fn PreMoveItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> WinResult<()> { Ok(()) }
    fn PostMoveItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<IShellItem>) -> WinResult<()> { Ok(()) }
    fn PreCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR) -> WinResult<()> { Ok(()) }
    fn PostCopyItem(&self, _: u32, _: Ref<IShellItem>, _: Ref<IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<IShellItem>) -> WinResult<()> { Ok(()) }
    fn PreNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR) -> WinResult<()> { Ok(()) }
    fn PostNewItem(&self, _: u32, _: Ref<IShellItem>, _: &PCWSTR, _: &PCWSTR, _: u32, _: HRESULT, _: Ref<IShellItem>) -> WinResult<()> { Ok(()) }
    fn UpdateProgress(&self, _: u32, _: u32) -> WinResult<()> { Ok(()) }
    fn ResetTimer(&self) -> WinResult<()> { Ok(()) }
    fn PauseTimer(&self) -> WinResult<()> { Ok(()) }
    fn ResumeTimer(&self) -> WinResult<()> { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE, DRIVE_UNKNOWN,
    };

    #[test]
    fn recycled_file_disappears_and_is_in_bin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dirstat-recycle-test.txt");
        std::fs::write(&file, b"dirstat recycle test").unwrap();

        // Recycled is reported only when the shell hands back the item's new location in the Recycle Bin.
        assert_eq!(recycle(&file), DeleteOutcome::Recycled);
        assert!(!file.exists());
    }

    #[test]
    fn recycled_folder_is_reported_recycled() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("dirstat-recycle-folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inner.txt"), b"x").unwrap();

        assert_eq!(recycle(&folder), DeleteOutcome::Recycled);
        assert!(!folder.exists());
    }

    #[test]
    fn missing_path_returns_failed_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.txt");
        assert!(matches!(recycle(&missing), DeleteOutcome::Failed(_)));
    }

    #[test]
    fn only_fixed_drives_are_allowed() {
        assert_eq!(drive_type_refusal(DRIVE_FIXED), None);
        for kind in [DRIVE_REMOVABLE, DRIVE_REMOTE, DRIVE_CDROM, DRIVE_RAMDISK, DRIVE_UNKNOWN, DRIVE_NO_ROOT_DIR] {
            assert!(drive_type_refusal(kind).is_some(), "drive type {kind} must be refused");
        }
    }

    #[test]
    fn network_drive_refusal_says_why() {
        let reason = drive_type_refusal(DRIVE_REMOTE).unwrap();
        assert!(reason.contains("network drive"), "{reason}");
    }
}
