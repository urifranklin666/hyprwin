//! Application-wide event plumbing.
//!
//! WinEvent hooks, the tray window proc and the hotkey message loop all need to
//! hand work to the single thread that owns the tiling state, so they share one
//! channel.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;

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
    /// The user began dragging or resizing a window.
    MoveSizeStart(isize),
    /// The user finished dragging or resizing a window.
    MoveSizeEnd(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    Reload,
    ToggleTiling,
    Quit,
}

/// A command from the control pipe, with the channel its answer goes back on.
///
/// The pipe thread owns the connection, but only the main thread may touch the
/// tiling state, so the request travels here and the reply travels back through
/// `reply`. The sender is dropped if the client gave up, which the pipe thread
/// treats as "nobody is listening" rather than an error.
pub struct ControlCommand {
    pub action: String,
    pub arg: Option<String>,
    pub reply: Sender<crate::protocol::Response>,
}

pub enum AppEvent {
    Win(WinEvent),
    Tray(TrayCommand),
    Control(ControlCommand),
}

static TX: Mutex<Option<Sender<AppEvent>>> = Mutex::new(None);

/// Create the channel. Call once, before installing any hooks.
///
/// Installs a fresh sender, replacing any earlier one. Production calls this
/// exactly once; tests call it per case so each gets its own receiver.
pub fn init() -> Receiver<AppEvent> {
    let (tx, rx) = mpsc::channel();
    if let Ok(mut slot) = TX.lock() {
        *slot = Some(tx);
    }
    rx
}

/// Send from hook callbacks and window procs. Silently drops if nobody listens,
/// which is what happens during shutdown.
pub fn send(ev: AppEvent) {
    if let Ok(slot) = TX.lock() {
        if let Some(tx) = slot.as_ref() {
            let _ = tx.send(ev);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `init` installs a process-wide sender, so these tests must not interleave
    /// or one case would receive another's events.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn events_are_delivered_in_order() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rx = init();
        send(AppEvent::Win(WinEvent::Created(0x1234)));
        send(AppEvent::Tray(TrayCommand::Quit));
        let first = rx.recv().unwrap();
        assert!(matches!(first, AppEvent::Win(WinEvent::Created(0x1234))));
        let second = rx.recv().unwrap();
        assert!(matches!(second, AppEvent::Tray(TrayCommand::Quit)));
    }

    #[test]
    fn sending_without_a_receiver_does_not_panic() {
        // No `init`: the sender slot is whatever another test left, and `send`
        // must tolerate both an absent and a dropped receiver.
        send(AppEvent::Tray(TrayCommand::Reload));
    }

    #[test]
    fn a_control_command_carries_its_reply_channel() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let rx = init();
        let (tx, answer) = mpsc::channel();
        send(AppEvent::Control(ControlCommand {
            action: "workspace".to_string(),
            arg: Some("3".to_string()),
            reply: tx,
        }));
        match rx.recv().unwrap() {
            AppEvent::Control(c) => {
                assert_eq!(c.action, "workspace");
                assert_eq!(c.arg.as_deref(), Some("3"));
                c.reply
                    .send(crate::protocol::Response::Ok("switched".to_string()))
                    .unwrap();
            }
            _ => panic!("expected a control event"),
        }
        assert_eq!(
            answer.recv().unwrap(),
            crate::protocol::Response::Ok("switched".to_string())
        );
    }
}
