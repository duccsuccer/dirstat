//! File actions on one item: Explorer's own context menu for it, and copying its path.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use windows::core::{w, Interface, Result as WinResult, HSTRING, PCSTR, PCWSTR};
use windows::Win32::Foundation::{FreeLibrary, ERROR_CANCELLED, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::System::SystemInformation::{GetSystemDirectoryW, GetWindowsDirectoryW};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, RemoveWindowSubclass, SHBindToParent,
    SHParseDisplayName, SetWindowSubclass, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_CONTROL_DOWN,
    CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, GetMenuItemCount, TrackPopupMenuEx, HMENU, MF_ENABLED, MF_GRAYED,
    MF_SEPARATOR, MF_STRING, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM, WM_INITMENUPOPUP, WM_MEASUREITEM,
    WM_MENUCHAR,
};

/// `path` as Explorer and the clipboard need it: a drive root `C:` (which means "current folder on C:")
/// becomes `C:\`.
pub fn shell_path(mut path: String) -> String {
    if path.ends_with(':') {
        path.push('\\');
    }
    path
}

pub fn copy_text(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| e.to_string())
}

/// Command ids: dirstat's items get 1 up to `SHELL_FIRST`, the shell's start at `SHELL_FIRST`, so they never clash.
/// 0 is what TrackPopupMenuEx returns when the menu is dismissed.
const APP_FIRST: u32 = 1;
const SHELL_FIRST: u32 = 100;
const LAST_ID: u32 = 0x7FFF;
/// SEE_MASK_NOASYNC, which windows-rs does not name for CMINVOKECOMMANDINFO: run the verb before returning, so a
/// Delete has finished when the tree is checked.
const CMIC_MASK_NOASYNC: u32 = 0x100;
const SUBCLASS_ID: usize = 1;

/// dirstat's own menu items. Each one's command id is its position in `ALL` plus `APP_FIRST`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppItem {
    Explore,
    CommandPrompt,
    CopyPath,
    Export,
    CopyInfo,
}

impl AppItem {
    const ALL: [AppItem; 5] = [AppItem::Explore, AppItem::CommandPrompt, AppItem::CopyPath, AppItem::Export, AppItem::CopyInfo];

    fn label(self) -> &'static str {
        match self {
            AppItem::Explore => "Explore Folder",
            AppItem::CommandPrompt => "Command Prompt Here",
            AppItem::CopyPath => "Copy Path",
            AppItem::Export => "Export to .CSV file...",
            AppItem::CopyInfo => "Copy file and size info to clipboard",
        }
    }

    fn id(self) -> u32 {
        // Declaration order, as `ALL` lists them.
        self as u32 + APP_FIRST
    }

    fn from_id(id: u32) -> Option<AppItem> {
        Self::ALL.get(id.checked_sub(APP_FIRST)? as usize).copied()
    }
}

/// What the user chose in the context menu.
#[derive(Debug, PartialEq, Eq)]
pub enum Chosen {
    /// The menu was dismissed.
    Nothing,
    App(AppItem),
    /// A command of the shell's part ran (a command the user cancelled in its own dialog counts as run).
    Shell,
}

/// Shows dirstat's items `items` (each with whether it is enabled) at the mouse pointer, owned by window `hwnd`.
/// When `shell` is set, Explorer's own items for `path` follow them after a separator; `extended` adds the verbs
/// Explorer shows when Shift is held. A shell command that is chosen runs before this returns.
pub fn show_context_menu(hwnd: HWND, path: &Path, extended: bool, items: &[(AppItem, bool)], shell: bool) -> WinResult<Chosen> {
    let _com = ComApartment::enter();
    let menu = if shell { Some(context_menu(hwnd, path)?) } else { None };
    // Read every time, so a theme switch while dirstat runs shows on the next menu.
    apply_menu_theme(menu_app_mode(apps_use_light_theme()));
    let id = unsafe {
        let popup = CreatePopupMenu()?;
        let chosen = track(menu.as_ref(), items, popup, hwnd, extended);
        let _ = DestroyMenu(popup);
        chosen?
    };
    if id < SHELL_FIRST {
        return Ok(AppItem::from_id(id).map_or(Chosen::Nothing, Chosen::App));
    }
    let Some(menu) = menu else { return Ok(Chosen::Nothing) };
    let held = |key: u16, mask: u32| if unsafe { GetKeyState(i32::from(key)) } < 0 { mask } else { 0 };
    let info = CMINVOKECOMMANDINFO {
        cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
        // Shift held while choosing Delete makes it a permanent delete, as in Explorer.
        fMask: CMIC_MASK_NOASYNC | held(VK_SHIFT.0, CMIC_MASK_SHIFT_DOWN) | held(VK_CONTROL.0, CMIC_MASK_CONTROL_DOWN),
        hwnd,
        // MAKEINTRESOURCE: the command given by its offset from the first shell id.
        lpVerb: PCSTR((id - SHELL_FIRST) as usize as *const u8),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    match unsafe { menu.InvokeCommand(&info) } {
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(Chosen::Shell),
        result => result.map(|()| Chosen::Shell),
    }
}

/// Fills `popup` with `items`, then the shell's items after a separator when `menu` is set, and shows it at the mouse
/// pointer until the user picks one (its id) or dismisses it (0).
unsafe fn track(menu: Option<&IContextMenu>, items: &[(AppItem, bool)], popup: HMENU, hwnd: HWND, extended: bool) -> WinResult<u32> {
    unsafe {
        for &(item, enabled) in items {
            let state = if enabled { MF_ENABLED } else { MF_GRAYED };
            AppendMenuW(popup, MF_STRING | state, item.id() as usize, &HSTRING::from(item.label()))?;
        }
        let mut at = POINT::default();
        if let Some(menu) = menu {
            AppendMenuW(popup, MF_SEPARATOR, 0, PCWSTR::null())?;
            menu.QueryContextMenu(popup, GetMenuItemCount(Some(popup)) as u32, SHELL_FIRST, LAST_ID, menu_flags(extended)).ok()?;
        }
        GetCursorPos(&mut at)?;
        if let Some(menu) = menu {
            // Send To and Open With fill their submenus, and handlers draw their icons, only when the owner window hands
            // them its menu messages, so the window is subclassed while the menu is open. The menu outlives the subclass,
            // which is removed below.
            let _ = SetWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID, menu.as_raw() as usize);
        }
        let id = TrackPopupMenuEx(popup, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, at.x, at.y, hwnd, None).0 as u32;
        let _ = RemoveWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID);
        Ok(id)
    }
}

/// Opens Explorer on `path`, or with `path` selected in its folder when `select` is set.
pub fn explore(path: &Path, select: bool) -> std::io::Result<()> {
    let mut command = Command::new(system_dir(false).join("explorer.exe"));
    if select {
        // explorer.exe needs the quotes around the path, which Command's own argument quoting would escape.
        command.raw_arg(format!("/select,\"{}\"", path.display()));
    } else {
        command.arg(path);
    }
    command.spawn().map(drop)
}

/// Starts cmd.exe with `dir` as its folder, in a new console window (dirstat itself has none).
pub fn command_prompt(dir: &Path) -> std::io::Result<()> {
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    Command::new(system_dir(true).join("cmd.exe")).current_dir(dir).creation_flags(CREATE_NEW_CONSOLE).spawn().map(drop)
}

/// The Windows directory, or the system directory (System32) when `system` is set, as the system reports it.
fn system_dir(system: bool) -> PathBuf {
    let mut buf = [0u16; 512];
    let len = unsafe {
        if system {
            GetSystemDirectoryW(Some(&mut buf))
        } else {
            GetWindowsDirectoryW(Some(&mut buf))
        }
    } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..len.min(buf.len())]))
}

/// QueryContextMenu flags: what Explorer asks for on a selected item (`extended` = Shift held). No CMF_CANRENAME:
/// rename needs an Explorer view to edit the name in, which dirstat does not have.
fn menu_flags(extended: bool) -> u32 {
    CMF_NORMAL | CMF_EXPLORE | if extended { CMF_EXTENDEDVERBS } else { 0 }
}

/// Window subclass active while the shell menu is open: passes the messages that build and draw its owner-drawn
/// submenus to the menu (`menu` is its IContextMenu pointer), through IContextMenu3 or else IContextMenu2.
unsafe extern "system" fn forward_menu_messages(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, menu: usize) -> LRESULT {
    if matches!(msg, WM_INITMENUPOPUP | WM_DRAWITEM | WM_MEASUREITEM | WM_MENUCHAR) {
        let raw = menu as *mut c_void;
        if let Some(menu) = unsafe { IContextMenu::from_raw_borrowed(&raw) } {
            if let Ok(menu3) = menu.cast::<IContextMenu3>() {
                let mut result = LRESULT(0);
                if unsafe { menu3.HandleMenuMsg2(msg, wparam, lparam, Some(&mut result)) }.is_ok() {
                    return result;
                }
            } else if let Ok(menu2) = menu.cast::<IContextMenu2>() {
                if msg != WM_MENUCHAR && unsafe { menu2.HandleMenuMsg(msg, wparam, lparam) }.is_ok() {
                    // WM_INITMENUPOPUP returns 0 when handled; WM_DRAWITEM and WM_MEASUREITEM return TRUE.
                    return LRESULT(isize::from(msg != WM_INITMENUPOPUP));
                }
            }
        }
    }
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// The system's "app mode" setting (Settings > Personalization > Colors): 0 dark, 1 light; `None` when unset.
fn apps_use_light_theme() -> Option<u32> {
    let mut value = 0u32;
    let mut size = size_of::<u32>() as u32;
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    }
    .ok()
    .ok()
    .map(|()| value)
}

/// uxtheme's PreferredAppMode for menus: ForceDark (2) when apps use the dark theme, ForceLight (3) otherwise
/// (also when the setting is missing, as Windows then shows light apps).
fn menu_app_mode(apps_use_light_theme: Option<u32>) -> i32 {
    if apps_use_light_theme == Some(0) { 2 } else { 3 }
}

/// Sets the process's menu theme to `mode` through uxtheme's undocumented SetPreferredAppMode (ordinal 135) and
/// FlushMenuThemes (ordinal 136), exported by ordinal only since Windows 10 1903 (1809 has AllowDarkModeForApp(BOOL)
/// at 135, which takes the same call harmlessly). Does nothing when they are missing.
fn apply_menu_theme(mode: i32) {
    type SetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
    type FlushMenuThemes = unsafe extern "system" fn();
    unsafe {
        // System32 only: this process is elevated, so a uxtheme.dll beside the exe must never load.
        let Ok(uxtheme) = LoadLibraryExW(w!("uxtheme.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) else { return };
        let by_ordinal = |n: usize| GetProcAddress(uxtheme, PCSTR(n as *const u8));
        if let (Some(set_mode), Some(flush)) = (by_ordinal(135), by_ordinal(136)) {
            let set_mode = std::mem::transmute::<unsafe extern "system" fn() -> isize, SetPreferredAppMode>(set_mode);
            let flush = std::mem::transmute::<unsafe extern "system" fn() -> isize, FlushMenuThemes>(flush);
            set_mode(mode);
            flush();
        }
        let _ = FreeLibrary(uxtheme);
    }
}

/// The shell's context menu object for `path`; `hwnd` owns any window its commands show. Nothing is run.
fn context_menu(hwnd: HWND, path: &Path) -> WinResult<IContextMenu> {
    let pidl = Pidl::parse(path)?;
    let mut child = std::ptr::null_mut();
    unsafe {
        let folder: IShellFolder = SHBindToParent(pidl.0, Some(&mut child))?;
        // `child` points into `pidl`, which lives until the end of this function.
        folder.GetUIObjectOf(hwnd, &[child.cast_const()], None)
    }
}

/// An item id list from SHParseDisplayName, freed on drop.
struct Pidl(*mut ITEMIDLIST);

impl Pidl {
    fn parse(path: &Path) -> WinResult<Self> {
        let mut pidl = Pidl(std::ptr::null_mut());
        unsafe { SHParseDisplayName(&HSTRING::from(path), None, &mut pidl.0, 0, None) }?;
        Ok(pidl)
    }
}

impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast_const().cast())) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_root_gets_a_backslash() {
        assert_eq!(shell_path("C:".into()), r"C:\");
        assert_eq!(shell_path(r"C:\Users".into()), r"C:\Users");
        assert_eq!(shell_path(r"\\server\share".into()), r"\\server\share");
    }

    #[test]
    fn app_item_ids_stay_below_the_shell_range() {
        for item in AppItem::ALL {
            assert_eq!(AppItem::from_id(item.id()), Some(item));
            assert!(item.id() < SHELL_FIRST);
        }
        assert_eq!(AppItem::from_id(0), None);
    }

    #[test]
    fn menu_follows_the_system_app_theme() {
        assert_eq!(menu_app_mode(Some(0)), 2, "dark apps: ForceDark");
        assert_eq!(menu_app_mode(Some(1)), 3, "light apps: ForceLight");
        assert_eq!(menu_app_mode(None), 3, "unset means light");
    }

    #[test]
    fn menu_flags_ask_for_explorer_verbs_without_rename() {
        use windows::Win32::UI::Shell::{CMF_CANRENAME, CMF_DEFAULTONLY, CMF_NOVERBS, CMF_VERBSONLY};
        for extended in [false, true] {
            let flags = menu_flags(extended);
            assert_ne!(flags & CMF_EXPLORE, 0);
            assert_eq!(flags & (CMF_CANRENAME | CMF_DEFAULTONLY | CMF_NOVERBS | CMF_VERBSONLY), 0);
            assert_eq!(flags & CMF_EXTENDEDVERBS != 0, extended);
        }
    }

    #[test]
    fn shell_parses_existing_paths_and_rejects_missing_ones() {
        let _com = ComApartment::enter();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dirstat menu test.txt");
        std::fs::write(&file, b"x").unwrap();

        assert!(!Pidl::parse(&file).unwrap().0.is_null());
        assert!(Pidl::parse(dir.path()).is_ok());
        assert!(Pidl::parse(&dir.path().join("missing.txt")).is_err());
    }

    #[test]
    fn context_menu_is_built_without_running_anything() {
        // Only the menu object is created: no menu is shown and no command is invoked.
        let _com = ComApartment::enter();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("dirstat menu test.txt");
        std::fs::write(&file, b"x").unwrap();

        assert!(context_menu(HWND::default(), &file).is_ok());
        assert!(context_menu(HWND::default(), &dir.path().join("missing.txt")).is_err());
        assert!(file.exists());
    }
}
