//! File actions on one item: open in Explorer, copy its path, delete it to the Recycle Bin.

use std::cell::Cell;
use std::ffi::c_void;
use std::io::ErrorKind;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::{implement, Ref, Result as WinResult, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_ABORT, ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD};
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
    // so only a failure to start it counts. Run by full path: this process is elevated, and a bare name would
    // be looked up in the application folder first.
    Command::new(explorer_exe(windows_dir())?)
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
}

/// The Windows folder, from GetWindowsDirectoryW; `None` when it cannot be read.
fn windows_dir() -> Option<PathBuf> {
    let mut buf = [0u16; 261];
    let len = unsafe { GetWindowsDirectoryW(Some(&mut buf)) } as usize;
    (len > 0 && len < buf.len()).then(|| PathBuf::from(String::from_utf16_lossy(&buf[..len])))
}

/// `explorer.exe` in the Windows folder `windows_dir`.
fn explorer_exe(windows_dir: Option<PathBuf>) -> std::io::Result<PathBuf> {
    windows_dir
        .map(|dir| dir.join("explorer.exe"))
        .ok_or_else(|| std::io::Error::other("cannot find the Windows folder to start Explorer"))
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

/// Moves `path` (`size` bytes) to the Recycle Bin. Never deletes permanently: anything that cannot be recycled
/// is refused.
pub fn recycle(path: &Path, size: u64) -> DeleteOutcome {
    // Why this cannot delete permanently (stage 1 forbids it), in the order the guards run:
    // 1. Pre-checks, before the item is touched:
    //    a. The volume must be a fixed local drive (GetDriveTypeW == DRIVE_FIXED), and SHQueryRecycleBinW must
    //       succeed for it. Network, removable, optical and RAM drives are refused.
    //    b. The bin's own settings (`bin_settings`): refused when the volume's NukeOnDelete is on, when the
    //       NoRecycleFiles policy is set, or when `size` reaches the bin's maximum (MaxCapacity, or a
    //       RecycleBinSize policy). These are the causes of "too big to recycle" and "delete permanently".
    //       A setting that exists but cannot be read counts as "refuse": an unreadable NukeOnDelete,
    //       NoRecycleFiles, MaxCapacity or RecycleBinSize value, or a RecycleBinSize policy when the drive's
    //       size cannot be read. A value that is missing means the default.
    // 2. Flags: FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE ask for the Recycle Bin. FOF_NOCONFIRMATION is deliberately
    //    NOT set (it answers "Yes to All", which is how SHFileOperation callers silently nuke too-large files), and
    //    FOF_WANTNUKEWARNING asks the shell to warn before destroying instead of recycling. The docs do not say
    //    what FOFX_RECYCLEONDELETE does with an item too big for the bin, so it is not relied on alone.
    // 3. A progress sink is attached before anything runs (if Advise fails, nothing is performed).
    //    PreDeleteItem cancels the whole operation (documented: an error return cancels the delete and all
    //    pending operations) unless TSF_DELETE_RECYCLE_IF_POSSIBLE is set, i.e. unless the shell is about to
    //    recycle. This is the check Electron's shell.trashItem relies on.
    // 4. Success is reported only when PostDeleteItem returns the item's new location in the Recycle Bin and the
    //    item is gone from its old path. A NULL location means it was destroyed; that is reported, not hidden.
    // Not ruled out: when the bin has no MaxCapacity value for the volume (never configured), its default
    // percentage limit is not guessed, so the size check is skipped. Then a too-large item rests on guards 2 and
    // 3, and the shell might show its own "permanently delete?" prompt before PreDeleteItem. That needs the user
    // to accept the shell's explicit warning; it is never silent.
    let _com = ComApartment::enter();
    if let Some(reason) = volume_refusal(path, size) {
        return DeleteOutcome::Refused(reason);
    }
    shell_recycle(path)
}

/// The Recycle Bin settings that decide whether an item on one volume would be destroyed instead of recycled.
#[derive(Debug, Default)]
pub(crate) struct BinSettings {
    pub nuke_on_delete: bool,
    pub policy_no_recycle: bool,
    /// `None` when no limit is set (the default percentage is not guessed).
    pub max_capacity_bytes: Option<u64>,
    /// What could not be read when a limit exists but its size is unknown; such a bin refuses everything.
    pub unreadable_limit: Option<&'static str>,
}

/// Why the bin would not take an item of `size` bytes; `None` when these settings allow it.
pub(crate) fn bin_refusal(bin: &BinSettings, size: u64) -> Option<String> {
    if bin.policy_no_recycle {
        Some("Not deleted: a policy turns the Recycle Bin off, and this app never deletes permanently.".into())
    } else if bin.nuke_on_delete {
        Some("Not deleted: the Recycle Bin is set to remove files immediately on this drive, and this app never deletes permanently.".into())
    } else if let Some(what) = bin.unreadable_limit {
        Some(format!("Not deleted: cannot read {what}, and this app never deletes permanently."))
    } else if bin.max_capacity_bytes.is_some_and(|max| size >= max) {
        Some("Not deleted: it is too large for this drive's Recycle Bin, and this app never deletes permanently.".into())
    } else {
        None
    }
}

/// A switch read from the registry: a missing value is the default (off); a value that cannot be read counts as on.
pub(crate) fn flag_is_set(read: Result<Option<u32>, ()>) -> bool {
    !matches!(read, Ok(None | Some(0)))
}

/// The bin's size limit in bytes from MaxCapacity (MB) and the RecycleBinSize policies (percent of the
/// `volume_bytes` volume); the smallest wins. `Ok(None)` when none is set. `Err` names what could not be read
/// when a limit exists but its size is unknown.
pub(crate) fn size_limit(
    max_capacity_mb: Result<Option<u32>, ()>,
    percent_policies: &[Result<Option<u32>, ()>],
    volume_bytes: Option<u64>,
) -> Result<Option<u64>, &'static str> {
    const UNREADABLE: &str = "this drive's Recycle Bin limit";
    let mut limit = max_capacity_mb.map_err(|()| UNREADABLE)?.map(|mb| u64::from(mb) * 1024 * 1024);
    for percent in percent_policies {
        if let Some(percent) = percent.map_err(|()| UNREADABLE)? {
            let total = volume_bytes.ok_or("this drive's size, which its Recycle Bin size policy needs")?;
            let bytes = total / 100 * u64::from(percent.min(100));
            limit = Some(limit.map_or(bytes, |l| l.min(bytes)));
        }
    }
    Ok(limit)
}

const POLICIES_EXPLORER: &str = r"Software\Microsoft\Windows\CurrentVersion\Policies\Explorer";

/// Reads the bin settings for the volume mounted at `root` (as returned by GetVolumePathNameW).
/// Per-volume values live under HKCU `...\Explorer\BitBucket\Volume\{volume GUID}`: NukeOnDelete (0/1) and
/// MaxCapacity (MB). Policies are read from both HKCU and HKLM: NoRecycleFiles (1 = bin off) and RecycleBinSize
/// (maximum as a percent of the volume). When several limits exist the smallest wins.
fn bin_settings(root: PCWSTR) -> BinSettings {
    let mut name = [0u16; 64];
    let volume_key = unsafe { GetVolumeNameForVolumeMountPointW(root, &mut name) }.ok().and_then(|()| {
        // `name` is `\\?\Volume{GUID}\`; the key uses `{GUID}`.
        let name = String::from_utf16_lossy(&name);
        let guid = &name[name.find('{')?..=name.find('}')?];
        Some(format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\BitBucket\Volume\{guid}"))
    });
    // Without the volume key the per-volume values cannot be read, which counts as unreadable.
    let per_volume = |value: &str| volume_key.as_deref().map_or(Err(()), |key| read_dword(HKEY_CURRENT_USER, key, value));
    let policy = |value: &str| [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].map(|hive| read_dword(hive, POLICIES_EXPLORER, value));

    let mut total = 0u64;
    let volume_bytes = unsafe { GetDiskFreeSpaceExW(root, None, Some(&mut total), None) }.ok().map(|()| total);
    let limit = size_limit(per_volume("MaxCapacity"), &policy("RecycleBinSize"), volume_bytes);
    BinSettings {
        nuke_on_delete: flag_is_set(per_volume("NukeOnDelete")),
        policy_no_recycle: policy("NoRecycleFiles").into_iter().any(flag_is_set),
        max_capacity_bytes: limit.unwrap_or(None),
        unreadable_limit: limit.err(),
    }
}

/// A REG_DWORD value: `Ok(None)` when the key or value does not exist, `Err` for any other failure.
fn read_dword(hive: HKEY, key: &str, value: &str) -> Result<Option<u32>, ()> {
    let (mut data, mut len) = (0u32, size_of::<u32>() as u32);
    let status = unsafe {
        RegGetValueW(
            hive,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut u32 as *mut c_void),
            Some(&mut len),
        )
    };
    match status {
        ERROR_SUCCESS => Ok(Some(data)),
        ERROR_FILE_NOT_FOUND => Ok(None),
        _ => Err(()),
    }
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

fn volume_refusal(path: &Path, size: u64) -> Option<String> {
    let mut root = [0u16; 261];
    if unsafe { GetVolumePathNameW(&HSTRING::from(path), &mut root) }.is_err() {
        return Some(format!("Not deleted: cannot tell which drive {} is on.", path.display()));
    }
    // GetVolumePathNameW wrote a NUL-terminated string into `root`, which outlives every use below.
    let root = PCWSTR(root.as_ptr());
    if let Some(reason) = drive_type_refusal(unsafe { GetDriveTypeW(root) }) {
        return Some(reason);
    }
    let mut info = SHQUERYRBINFO { cbSize: size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    if unsafe { SHQueryRecycleBinW(root, &mut info) }.is_err() {
        return Some("Not deleted: its drive has no Recycle Bin, and this app never deletes permanently.".into());
    }
    bin_refusal(&bin_settings(root), size)
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
    let still_exists = std::fs::symlink_metadata(path).map_or_else(|e| e.kind() != ErrorKind::NotFound, |_| true);
    outcome(&path.display().to_string(), &sink, performed, still_exists)
}

/// What the user is told, from what the sink saw, what PerformOperations returned and whether the item is still there.
fn outcome(shown: &str, sink: &RecycleSink, performed: WinResult<()>, still_exists: bool) -> DeleteOutcome {
    if sink.refused.get() {
        DeleteOutcome::Refused(format!(
            "Not deleted: {shown} cannot go to the Recycle Bin (it may be too large for it), and this app never deletes permanently."
        ))
    } else if sink.destroyed.get() {
        DeleteOutcome::Failed(format!("{shown} was deleted permanently by the shell instead of being recycled."))
    } else if sink.recycled.get() && still_exists {
        DeleteOutcome::Failed(format!("{shown}: the shell reported success but the item still exists."))
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
    use windows::Win32::Foundation::S_OK;
    use windows::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE, DRIVE_UNKNOWN,
    };

    #[test]
    fn recycled_file_disappears_and_is_in_bin() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dirstat-recycle-test.txt");
        std::fs::write(&file, b"dirstat recycle test").unwrap();

        // Recycled is reported only when the shell hands back the item's new location in the Recycle Bin.
        assert_eq!(recycle(&file, 20), DeleteOutcome::Recycled);
        assert!(!file.exists());
    }

    #[test]
    fn recycled_folder_is_reported_recycled() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("dirstat-recycle-folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("inner.txt"), b"x").unwrap();

        assert_eq!(recycle(&folder, 1), DeleteOutcome::Recycled);
        assert!(!folder.exists());
    }

    #[test]
    fn oversized_item_is_refused_before_the_shell_runs() {
        // Relies on the temp volume having a MaxCapacity value, which Windows writes once the bin has been used.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dirstat-oversized.txt");
        std::fs::write(&file, b"x").unwrap();

        let outcome = recycle(&file, u64::MAX);

        assert!(matches!(&outcome, DeleteOutcome::Refused(m) if m.contains("too large")), "{outcome:?}");
        assert!(file.exists());
    }

    #[test]
    fn missing_path_returns_failed_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.txt");
        assert!(matches!(recycle(&missing, 0), DeleteOutcome::Failed(_)));
    }

    #[test]
    fn only_fixed_drives_are_allowed() {
        assert_eq!(drive_type_refusal(DRIVE_FIXED), None);
        for kind in [DRIVE_REMOVABLE, DRIVE_REMOTE, DRIVE_CDROM, DRIVE_RAMDISK, DRIVE_UNKNOWN, DRIVE_NO_ROOT_DIR] {
            assert!(drive_type_refusal(kind).is_some(), "drive type {kind} must be refused");
        }
    }

    const MB: u64 = 1024 * 1024;

    #[test]
    fn nuke_on_delete_is_refused() {
        let bin = BinSettings { nuke_on_delete: true, ..Default::default() };
        assert!(bin_refusal(&bin, 1).is_some());
    }

    #[test]
    fn policy_is_refused() {
        let bin = BinSettings { policy_no_recycle: true, ..Default::default() };
        assert!(bin_refusal(&bin, 1).is_some());
    }

    #[test]
    fn item_larger_than_max_is_refused() {
        let bin = BinSettings { max_capacity_bytes: Some(10 * MB), ..Default::default() };
        let reason = bin_refusal(&bin, 10 * MB + 1).unwrap();
        assert!(reason.contains("too large"), "{reason}");
    }

    #[test]
    fn item_within_max_is_allowed() {
        let bin = BinSettings { max_capacity_bytes: Some(10 * MB), ..Default::default() };
        assert_eq!(bin_refusal(&bin, 10 * MB - 1), None);
    }

    #[test]
    fn unknown_max_is_not_guessed() {
        let bin = BinSettings::default();
        assert_eq!(bin_refusal(&bin, u64::MAX), None);
    }

    #[test]
    fn unreadable_limit_is_refused() {
        let bin = BinSettings { unreadable_limit: Some("this drive's Recycle Bin limit"), ..Default::default() };
        let reason = bin_refusal(&bin, 1).unwrap();
        assert!(reason.contains("cannot read this drive's Recycle Bin limit"), "{reason}");
    }

    #[test]
    fn size_limit_from_each_setting() {
        let none = [Ok(None), Ok(None)];
        // Missing everywhere: the default limit is not guessed.
        assert_eq!(size_limit(Ok(None), &none, None), Ok(None));
        assert_eq!(size_limit(Ok(Some(10)), &none, None), Ok(Some(10 * MB)));
        // A policy percent of the volume; the smallest limit wins.
        assert_eq!(size_limit(Ok(None), &[Ok(Some(5)), Ok(None)], Some(100_000)), Ok(Some(5_000)));
        assert_eq!(size_limit(Ok(Some(1)), &[Ok(None), Ok(Some(50))], Some(100 * MB)), Ok(Some(MB)));
    }

    #[test]
    fn size_limit_that_exists_but_cannot_be_read_is_err() {
        let none = [Ok(None), Ok(None)];
        let limit = Err("this drive's Recycle Bin limit");
        assert_eq!(size_limit(Err(()), &none, Some(MB)), limit);
        assert_eq!(size_limit(Ok(Some(10)), &[Ok(None), Err(())], Some(MB)), limit);
        let disk = size_limit(Ok(Some(10)), &[Ok(Some(5)), Ok(None)], None);
        assert!(disk.is_err_and(|what| what.contains("size")), "{disk:?}");
        assert_ne!(disk, limit);
    }

    #[test]
    fn missing_flag_is_default_off_and_unreadable_flag_counts_as_set() {
        assert!(!flag_is_set(Ok(None)));
        assert!(!flag_is_set(Ok(Some(0))));
        assert!(flag_is_set(Ok(Some(1))));
        assert!(flag_is_set(Err(())));
    }

    fn sink_interface(sink: &ComObject<RecycleSink>) -> IFileOperationProgressSink {
        sink.to_interface()
    }

    #[test]
    fn sink_aborts_delete_that_would_not_recycle() {
        let sink = ComObject::new(RecycleSink::default());
        let result = unsafe { sink_interface(&sink).PreDeleteItem(0, None::<&IShellItem>) };
        assert_eq!(result.unwrap_err().code(), E_ABORT);
        assert!(sink.refused.get());
    }

    #[test]
    fn sink_allows_delete_that_recycles() {
        let sink = ComObject::new(RecycleSink::default());
        let flags = TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32;
        assert!(unsafe { sink_interface(&sink).PreDeleteItem(flags, None::<&IShellItem>) }.is_ok());
        assert!(!sink.refused.get());
    }

    #[test]
    fn sink_records_destroyed_item() {
        let sink = ComObject::new(RecycleSink::default());
        unsafe { sink_interface(&sink).PostDeleteItem(0, None::<&IShellItem>, S_OK, None::<&IShellItem>) }.unwrap();
        assert!(sink.destroyed.get());
        assert!(!sink.recycled.get());
    }

    fn sink_with(refused: bool, destroyed: bool, recycled: bool) -> RecycleSink {
        RecycleSink { refused: Cell::new(refused), destroyed: Cell::new(destroyed), recycled: Cell::new(recycled) }
    }

    #[test]
    fn outcome_maps_each_case() {
        let ok = || Ok(());
        assert!(matches!(outcome("x", &sink_with(true, false, true), ok(), true), DeleteOutcome::Refused(_)));
        assert!(matches!(outcome("x", &sink_with(false, true, false), ok(), false), DeleteOutcome::Failed(m) if m.contains("permanently")));
        assert_eq!(outcome("x", &sink_with(false, false, true), ok(), false), DeleteOutcome::Recycled);
        assert!(matches!(outcome("x", &sink_with(false, false, true), ok(), true), DeleteOutcome::Failed(m) if m.contains("still exists")));
        assert!(matches!(outcome("x", &sink_with(false, false, false), Err(E_ABORT.into()), true), DeleteOutcome::Failed(_)));
        assert!(matches!(outcome("x", &sink_with(false, false, false), ok(), true), DeleteOutcome::Failed(_)));
    }

    #[test]
    fn explorer_is_run_from_the_windows_folder() {
        let exe = explorer_exe(Some(PathBuf::from(r"C:\Windows"))).unwrap();
        assert_eq!(exe, Path::new(r"C:\Windows\explorer.exe"));
        assert!(explorer_exe(None).is_err());
        assert!(explorer_exe(windows_dir()).unwrap().is_file());
    }

    #[test]
    fn network_drive_refusal_says_why() {
        let reason = drive_type_refusal(DRIVE_REMOTE).unwrap();
        assert!(reason.contains("network drive"), "{reason}");
    }
}
