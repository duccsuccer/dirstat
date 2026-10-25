//! File actions on one item: Explorer's own context menu for it, and copying its path.

use std::ffi::c_void;
use std::path::Path;
use windows::core::{w, Interface, Result as WinResult, HSTRING, PCSTR};
use windows::Win32::Foundation::{FreeLibrary, ERROR_CANCELLED, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_CONTROL, VK_SHIFT};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DefSubclassProc, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder, RemoveWindowSubclass, SHBindToParent,
    SHParseDisplayName, SetWindowSubclass, CMF_EXPLORE, CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_CONTROL_DOWN,
    CMIC_MASK_SHIFT_DOWN, CMINVOKECOMMANDINFO,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetCursorPos, TrackPopupMenuEx, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_DRAWITEM,
    WM_INITMENUPOPUP, WM_MEASUREITEM, WM_MENUCHAR,
};

/// `path` as Explorer and the clipboard need it: a drive root `C:` (which means "current folder on C:")
/// becomes `C:\`.
pub fn shell_path(mut path: String) -> String {
    if path.ends_with(':') {
        path.push('\\');
    }
    path
}

pub fn copy_path(path: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(path))
        .map_err(|e| e.to_string())
}

/// Menu command ids the shell may use; 0 is what TrackPopupMenuEx returns when the menu is dismissed.
const FIRST_ID: u32 = 1;
const LAST_ID: u32 = 0x7FFF;
/// SEE_MASK_NOASYNC, which windows-rs does not name for CMINVOKECOMMANDINFO: run the verb before returning, so a
/// Delete has finished when the tree is checked.
const CMIC_MASK_NOASYNC: u32 = 0x100;
const SUBCLASS_ID: usize = 1;

/// Shows Explorer's context menu for `path` at the mouse pointer, owned by window `hwnd`, and runs the chosen
/// command. `extended` adds the verbs Explorer shows when Shift is held. `Ok(false)` when the menu was dismissed;
/// a command the user cancelled (in its own dialog) counts as run.
pub fn show_shell_menu(hwnd: HWND, path: &Path, extended: bool) -> WinResult<bool> {
    let _com = ComApartment::enter();
    let menu = context_menu(hwnd, path)?;
    // Read every time, so a theme switch while dirstat runs shows on the next menu.
    apply_menu_theme(menu_app_mode(apps_use_light_theme()));
    let id = unsafe {
        let popup = CreatePopupMenu()?;
        let chosen = track(&menu, popup, hwnd, extended);
        let _ = DestroyMenu(popup);
        chosen?
    };
    if id < FIRST_ID {
        return Ok(false);
    }
    let held = |key: u16, mask: u32| if unsafe { GetKeyState(i32::from(key)) } < 0 { mask } else { 0 };
    let info = CMINVOKECOMMANDINFO {
        cbSize: size_of::<CMINVOKECOMMANDINFO>() as u32,
        // Shift held while choosing Delete makes it a permanent delete, as in Explorer.
        fMask: CMIC_MASK_NOASYNC | held(VK_SHIFT.0, CMIC_MASK_SHIFT_DOWN) | held(VK_CONTROL.0, CMIC_MASK_CONTROL_DOWN),
        hwnd,
        // MAKEINTRESOURCE: the command given by its offset from the first id.
        lpVerb: PCSTR((id - FIRST_ID) as usize as *const u8),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    match unsafe { menu.InvokeCommand(&info) } {
        Err(e) if e.code() == ERROR_CANCELLED.to_hresult() => Ok(true),
        result => result.map(|()| true),
    }
}

/// Fills `popup` with the shell's items and shows it at the mouse pointer until the user picks one (its id) or
/// dismisses it (0).
unsafe fn track(menu: &IContextMenu, popup: windows::Win32::UI::WindowsAndMessaging::HMENU, hwnd: HWND, extended: bool) -> WinResult<u32> {
    unsafe {
        menu.QueryContextMenu(popup, 0, FIRST_ID, LAST_ID, menu_flags(extended)).ok()?;
        let mut at = POINT::default();
        GetCursorPos(&mut at)?;
        // Send To and Open With fill their submenus, and handlers draw their icons, only when the owner window hands
        // them its menu messages, so the window is subclassed while the menu is open. The menu outlives the subclass,
        // which is removed below.
        let _ = SetWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID, menu.as_raw() as usize);
        let id = TrackPopupMenuEx(popup, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, at.x, at.y, hwnd, None).0 as u32;
        let _ = RemoveWindowSubclass(hwnd, Some(forward_menu_messages), SUBCLASS_ID);
        Ok(id)
    }
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
