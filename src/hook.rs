//! WinEvent hooks: this is how HyprWin learns about windows it did not create.

use crate::events::{self, AppEvent, WinEvent};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_SHOW, EVENT_SYSTEM_FOREGROUND,
    EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, OBJID_WINDOW,
    WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

pub struct Hooks {
    handles: Vec<HWINEVENTHOOK>,
}

unsafe extern "system" fn callback(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if hwnd.0.is_null() || id_object != OBJID_WINDOW.0 || id_child != 0 {
        return;
    }
    let translated = match event {
        EVENT_OBJECT_CREATE | EVENT_OBJECT_SHOW => WinEvent::Created(hwnd.0 as isize),
        EVENT_OBJECT_DESTROY => WinEvent::Destroyed(hwnd.0 as isize),
        EVENT_SYSTEM_FOREGROUND => WinEvent::FocusChanged(hwnd.0 as isize),
        EVENT_SYSTEM_MINIMIZESTART => WinEvent::Minimized(hwnd.0 as isize),
        EVENT_SYSTEM_MINIMIZEEND => WinEvent::Restored(hwnd.0 as isize),
        EVENT_SYSTEM_MOVESIZEEND => WinEvent::MoveSizeEnd(hwnd.0 as isize),
        _ => return,
    };
    events::send(AppEvent::Win(translated));
}

impl Hooks {
    /// Install every hook. Must be called on the thread that runs the message
    /// loop, because out-of-context callbacks are dispatched through it.
    pub fn install() -> Self {
        let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
        let mut handles = Vec::new();
        let events = [
            EVENT_OBJECT_CREATE,
            EVENT_OBJECT_SHOW,
            EVENT_OBJECT_DESTROY,
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_MINIMIZESTART,
            EVENT_SYSTEM_MINIMIZEEND,
            EVENT_SYSTEM_MOVESIZEEND,
        ];
        for event in events {
            let h = unsafe { SetWinEventHook(event, event, None, Some(callback), 0, 0, flags) };
            if !h.is_invalid() {
                handles.push(h);
            }
        }
        Hooks { handles }
    }

    pub fn count(&self) -> usize {
        self.handles.len()
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        for h in self.handles.drain(..) {
            unsafe {
                let _ = UnhookWinEvent(h);
            }
        }
    }
}
