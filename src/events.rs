//! Application-wide event plumbing.
//!
//! WinEvent hooks, the tray window proc and the hotkey message loop all need to
//! hand work to the single thread that owns the tiling state, so they share one
//! channel.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::OnceLock;

/// Something happened to a top-level window.
///
/// Windows are identified by their raw handle value rather than by `HWND`,
/// because `HWND` wraps a pointer and is therefore not `Send`; the events are
/// produced on the hook thread but consumed on the main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinEvent {
    Created(isize),
    Destroyed(isize),
    FocusChanged(isize),
    Minimized(isize),
    Restored(isize),
    /// The user finished dragging or resizing a window.
    MoveSizeEnd(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Reload,
    ToggleTiling,
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEvent {
    Win(WinEvent),
    Tray(TrayCommand),
}

static TX: OnceLock<Sender<AppEvent>> = OnceLock::new();

/// Create the channel. Call once, before installing any hooks.
pub fn init() -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel();
    let _ = TX.set(tx);
    rx
}

/// Send from hook callbacks and window procs. Silently drops if nobody listens,
/// which is what happens during shutdown.
pub fn send(ev: AppEvent) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_delivered_in_order() {
        let rx = init();
        send(AppEvent::Win(WinEvent::Created(0x1234)));
        send(AppEvent::Tray(TrayCommand::Quit));
        assert_eq!(rx.recv().unwrap(), AppEvent::Win(WinEvent::Created(0x1234)));
        assert_eq!(rx.recv().unwrap(), AppEvent::Tray(TrayCommand::Quit));
    }

    #[test]
    fn sending_without_a_receiver_does_not_panic() {
        send(AppEvent::Tray(TrayCommand::Reload));
    }
}
