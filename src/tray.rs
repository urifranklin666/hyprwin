//! Tray icon with a right-click menu (Reload / Toggle tiling / Quit).
//!
//! The tray owns a hidden window; its proc forwards commands to the main loop
//! through `events::send`, so no Win32 call ever touches the tiling state.

use crate::events::{self, AppEvent, TrayCommand};
use windows::core::w;
use windows::Win32::Foundation::{GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    GetCursorPos, LoadIconW, RegisterClassW, SetForegroundWindow, TrackPopupMenu, HICON,
    IDI_APPLICATION, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_DESTROY,
    WM_RBUTTONUP, WNDCLASSW, WS_EX_TOOLWINDOW, WS_POPUP,
};

const WM_TRAY: u32 = WM_APP + 1;
const CMD_RELOAD: usize = 1;
const CMD_TOGGLE: usize = 2;
const CMD_QUIT: usize = 3;

pub struct Tray {
    hwnd: HWND,
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            // lParam carries the mouse message for the icon.
            if lparam.0 as u32 == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            events::send(AppEvent::Tray(TrayCommand::Quit));
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let _ = AppendMenuW(menu, MF_STRING, CMD_RELOAD, w!("Reload config"));
    let _ = AppendMenuW(menu, MF_STRING, CMD_TOGGLE, w!("Toggle tiling"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, CMD_QUIT, w!("Quit HyprWin"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu dismisses correctly when clicking elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON,
        pt.x,
        pt.y,
        0,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);

    match cmd.0 as usize {
        CMD_RELOAD => events::send(AppEvent::Tray(TrayCommand::Reload)),
        CMD_TOGGLE => events::send(AppEvent::Tray(TrayCommand::ToggleTiling)),
        CMD_QUIT => events::send(AppEvent::Tray(TrayCommand::Quit)),
        _ => {}
    }
}

impl Tray {
    pub fn create() -> Option<Tray> {
        let hinstance = unsafe { GetModuleHandleW(None) }.ok()?;
        let class_name = w!("HyprWinTrayWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: class_name,
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_TOOLWINDOW,
                class_name,
                w!("HyprWin"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                HINSTANCE(hinstance.0),
                None,
            )
            .ok()?;
            let icon: HICON = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
            let mut nid = NOTIFYICONDATAW {
                cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                hWnd: hwnd,
                uID: 1,
                uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
                uCallbackMessage: WM_TRAY,
                hIcon: icon,
                ..Default::default()
            };
            let tip: Vec<u16> = "HyprWin - tiling window manager"
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let n = tip.len().min(nid.szTip.len());
            nid.szTip[..n].copy_from_slice(&tip[..n]);
            if Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                Some(Tray { hwnd })
            } else {
                let _ = GetLastError();
                let _ = DestroyWindow(hwnd);
                None
            }
        }
    }

    /// Handle of the hidden tray window. Unused today; useful if the tray needs
    /// to be destroyed and recreated (e.g. after an explorer restart).
    #[allow(dead_code)]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            ..Default::default()
        };
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Unused today; kept so the tray can be recreated after explorer restarts.
#[allow(dead_code)]
pub fn cursor_pos() -> POINT {
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    pt
}
