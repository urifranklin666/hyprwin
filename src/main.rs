//! HyprWin — a Hyprland-inspired dynamic tiling window manager for Windows.
//!
//! This file owns process startup and the message loop. All tiling behaviour
//! lives in `wm`.

mod animation;
mod autostart;
mod config;
mod events;
mod hook;
mod hotkey;
mod ipc;
mod layout;
mod log;
mod monitor;
mod overlay;
mod protocol;
mod status;
mod tray;
mod win32;
mod wm;
mod workspace;

use anyhow::{Context, Result};
use config::Config;
use events::{AppEvent, TrayCommand, WinEvent};
use protocol::Response;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use windows::Win32::Foundation::{BOOL, TRUE};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_HOTKEY,
};
use wm::Wm;

/// How long to wait for an idle event before re-checking the displays.
const IDLE_WAIT_MS: u64 = 250;

/// The currently registered global hotkeys.
///
/// Registration is OS state, not tiling state, so it lives in the message loop
/// rather than in `Wm`. Keeping it here is what lets a `reload` re-register:
/// `Wm` can swap its config, but only this can hand the new bindings to Windows
/// and drop the old ones.
struct Hotkeys {
    regs: Vec<hotkey::Registered>,
}

impl Hotkeys {
    fn register(cfg: &Config) -> Self {
        let main_mod = cfg.main_mod_token();
        let (regs, failed) = hotkey::register_all(&cfg.keybinds, &main_mod);
        crate::log_info!(
            "[hyprwin] {}/{} keybinds registered (main mod: {})",
            regs.len(),
            cfg.keybinds.len(),
            main_mod
        );
        for (bind, why) in &failed {
            crate::log_warn!(
                "[hyprwin] keybind {}+{} not registered: {}",
                bind.mods.join("+"),
                bind.key,
                why
            );
        }
        Hotkeys { regs }
    }

    /// Replace the live bindings with those of `cfg`, after a reload.
    ///
    /// The old set is unregistered first so nothing is left bound to an action
    /// the user just deleted, and so the fresh ids cannot collide with the ones
    /// going away.
    fn reload(&mut self, cfg: &Config) {
        hotkey::unregister_all(&self.regs);
        *self = Self::register(cfg);
    }
}

/// Hands the desktop back if `run` exits without doing so itself.
///
/// `reveal_all` is the shutdown inverse of tiling, and it must run on *every*
/// exit — including a panic in the message loop. An explicit call at the end of
/// `run` only covers the path that reaches it, so a `?` added later, or a panic,
/// would leave the user's windows hidden on an off-screen workspace with no tray
/// icon left to bring them back. The guard owns the `Wm` borrow for the rest of
/// the scope, so it cannot be forgotten, and `disarm` marks the normal path.
struct DesktopGuard<'a> {
    wm: &'a mut Wm,
    armed: bool,
}

impl<'a> DesktopGuard<'a> {
    fn new(wm: &'a mut Wm) -> Self {
        DesktopGuard { wm, armed: true }
    }

    fn wm(&mut self) -> &mut Wm {
        self.wm
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for DesktopGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            crate::log_warn!("[hyprwin] exiting without a clean shutdown; restoring the desktop");
            self.wm.reveal_all();
            self.wm.publish_stopped();
        }
    }
}

fn main() {
    if let Err(e) = run() {
        // The logger is only set up once the config loads, so a failure before
        // that has no file to go to; report it on both channels regardless.
        crate::log_error!("[hyprwin] fatal: {e:#}");
        std::process::exit(1);
    }
}

unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    events::send(AppEvent::Tray(TrayCommand::Quit));
    TRUE
}

fn run() -> Result<()> {
    // Per-monitor DPI awareness, so GetMonitorInfo and SetWindowPos agree on
    // physical pixels on mixed-DPI setups.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let cfg = Config::load_or_create().context("loading configuration")?;
    // Start logging as soon as the config names the sink and the level. Every
    // milestone and failure below is written to the console *and* the file, so a
    // session started from the tray still leaves a trail.
    log::init(&cfg.log_path(), log::Level::parse(&cfg.log_level).flatten());
    crate::log_info!(
        "[hyprwin] pid {} — config {}",
        unsafe { GetCurrentProcessId() },
        Config::path().display()
    );
    crate::log_info!("[hyprwin] log {}", cfg.log_path().display());

    if cfg.autostart {
        match autostart::set_enabled(true) {
            Ok(()) => crate::log_info!("[hyprwin] autostart registered"),
            Err(e) => crate::log_error!("[hyprwin] could not register autostart: {e:#}"),
        }
    }

    // Lets us hand focus to the window a keybind selected.
    let saved_timeout = win32::allow_foreground_change();

    // The receiver must exist before any hook or window proc can fire.
    let rx = events::init();

    let hooks = hook::Hooks::install();
    if hooks.count() == 0 {
        crate::log_warn!(
            "[hyprwin] no WinEvent hooks installed; tiling will not track new windows"
        );
    } else {
        crate::log_info!("[hyprwin] {} event hooks installed", hooks.count());
    }

    let mut hotkeys = Hotkeys::register(&cfg);

    let mut wm = Wm::new(cfg);

    // Control pipe: lets `hyprwinctl` and scripts drive a running instance. It
    // only ever queues events, so tiling state stays on this thread. Started
    // before adoption so the first retile populates its snapshot.
    ipc::spawn();
    crate::log_info!("[hyprwin] control pipe {}", ipc::full_pipe_path());

    let adopted = if wm.cfg.manage_on_start {
        wm.manage_existing()
    } else {
        crate::log_info!("[hyprwin] manage_on_start is false; only new windows will be tiled");
        0
    };
    crate::log_info!(
        "[hyprwin] managing {adopted} existing windows across {} monitor(s)",
        wm.monitors.len()
    );

    let tray = tray::Tray::create();
    if tray.is_none() {
        crate::log_warn!("[hyprwin] tray icon could not be created");
    }

    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), TRUE);
    }

    // From here on the desktop is tiled, so wrap the manager in a guard that
    // hands it back even if the message loop panics or a later `?` returns early.
    let mut guard = DesktopGuard::new(&mut wm);

    // Publish once now that startup is done. The control pipe and the status
    // file are otherwise only refreshed by a retile, so a session that adopts
    // nothing — no windows open yet, or `manage_on_start` off — would answer
    // `status` with `{}` and `monitors` with nothing at all.
    guard.wm().publish_status();

    crate::log_info!("[hyprwin] running — Ctrl+C here, or tray icon -> Quit, to exit");

    let exit_code = message_loop(guard.wm(), &rx, &mut hotkeys);

    // Hand the desktop back: unhide anything parked on an off-screen workspace
    // and restore the geometry every window had before we tiled it. Disarming
    // after makes the guard's own Drop a no-op on the normal path.
    guard.wm().reveal_all();
    guard.wm().publish_stopped();
    guard.disarm();
    crate::log_info!("[hyprwin] shutting down");

    hotkey::unregister_all(&hotkeys.regs);
    drop(tray);
    drop(hooks);
    win32::restore_foreground_timeout(saved_timeout);

    if exit_code == 0 {
        Ok(())
    } else {
        anyhow::bail!("message loop failed with code {exit_code}")
    }
}

fn message_loop(
    wm: &mut Wm,
    rx: &std::sync::mpsc::Receiver<AppEvent>,
    hotkeys: &mut Hotkeys,
) -> i32 {
    let mut quit = false;
    let mut known_monitors: Vec<isize> = wm.monitors.iter().map(|m| m.handle).collect();

    while !quit {
        // Re-check the displays once per iteration, at the top, so a monitor
        // change can never re-enter a retile that is already in flight. The wait
        // below bounds how often this poll actually runs.
        let now: Vec<isize> = monitor::enumerate().iter().map(|m| m.handle).collect();
        if now != known_monitors {
            crate::log_info!("[hyprwin] display configuration changed; re-tiling");
            known_monitors = now;
            wm.refresh_monitors();
        }

        let mut msg = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
        if has_message {
            if msg.message == WM_HOTKEY {
                // Copy the binding out before dispatching: a `reload` binding
                // re-registers the hotkeys through the shared helper, which needs
                // `hotkeys` mutably, and the lookup holds an immutable borrow.
                let fired = hotkeys
                    .regs
                    .iter()
                    .find(|r| r.id == msg.wParam.0 as i32)
                    .map(|r| (r.bind.action.clone(), r.bind.arg.clone()));
                if let Some((action, arg)) = fired {
                    // `wm.command("reload")` alone only swaps the config; the
                    // helper re-registers the keybinds, exactly as the tray and
                    // the control pipe do.
                    if action.eq_ignore_ascii_case("reload") {
                        reload(wm, hotkeys);
                    } else {
                        wm.command(&action, arg.as_deref());
                    }
                }
            } else {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            // There may be more messages queued; do not sleep on them.
            continue;
        }

        // The queue is empty. Step any animation first and use its next-frame
        // delay as the wait, so a move advances on time while a keypress is still
        // serviced between frames rather than after the whole animation.
        //
        // This has to happen inside the idle branch rather than at the top of the
        // loop: a steady stream of messages keeps hitting the `continue` above,
        // and an animation stepped from there could never progress.
        let wait = wm.tick().unwrap_or(IDLE_WAIT_MS).min(IDLE_WAIT_MS);
        match rx.recv_timeout(Duration::from_millis(wait)) {
            Ok(ev) => handle_app_event(wm, ev, hotkeys, &mut quit),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => quit = true,
        }
        while let Ok(ev) = rx.try_recv() {
            handle_app_event(wm, ev, hotkeys, &mut quit);
        }
    }
    0
}

fn handle_app_event(wm: &mut Wm, ev: AppEvent, hotkeys: &mut Hotkeys, quit: &mut bool) {
    match ev {
        AppEvent::Win(we) => {
            handle_win_event(wm, we);
            // Safety net: a process killed outright may never emit a destroy
            // event, and a stale entry would corrupt the layout tree.
            wm.prune_dead();
        }
        AppEvent::Tray(TrayCommand::Quit) => *quit = true,
        AppEvent::Tray(TrayCommand::Reload) => reload(wm, hotkeys),
        AppEvent::Tray(TrayCommand::ToggleTiling) => {
            wm.toggle_tiling();
        }
        AppEvent::Control(c) => handle_control(wm, c, hotkeys, quit),
    }
}

/// Re-read the config and make every part of it live.
///
/// `Wm` applies what it can to its own state, but a changed keybind or `main_mod`
/// only takes effect if the bindings are handed to Windows again, and a changed
/// `log_level` only if the sink is told. Both are OS state, so both are refreshed
/// here rather than in `Wm::command`.
fn reload(wm: &mut Wm, hotkeys: &mut Hotkeys) {
    let before = wm.cfg.clone();
    wm.command("reload", None);

    // A parse failure leaves `wm.cfg` untouched (`Wm::command` logs it), so the
    // comparisons below see no change and nothing is dropped.
    crate::log::set_level(crate::log::Level::parse(&wm.cfg.log_level).flatten());

    if wm.cfg.main_mod_token() != before.main_mod_token() {
        crate::log_warn!(
            "[hyprwin] main_mod changed to {}; the running bindings cannot be rebound in \
             place, so a restart is needed to apply the new primary modifier fully",
            wm.cfg.main_mod_token()
        );
    }
    if wm.cfg.keybinds == before.keybinds {
        return;
    }
    hotkeys.reload(&wm.cfg);
}

/// Run a control-pipe request and send its answer back to the pipe thread.
///
/// An unknown action is reported as an error instead of the silent log line the
/// hotkey path uses, because here there is a caller waiting to be told.
fn handle_control(wm: &mut Wm, c: events::ControlCommand, hotkeys: &mut Hotkeys, quit: &mut bool) {
    let reply = if c.action.eq_ignore_ascii_case("quit") {
        *quit = true;
        Response::Ok("shutting down".to_string())
    } else if c.action.eq_ignore_ascii_case("reload") {
        // `reload` is dispatched through here so the pipe re-registers hotkeys
        // exactly as the tray does, instead of only swapping `Wm`'s config.
        reload(wm, hotkeys);
        Response::Ok("reload".to_string())
    } else if c.action.eq_ignore_ascii_case("clients") {
        Response::Data(wm.clients_text())
    } else if protocol::ACTIONS.contains(&c.action.as_str()) {
        wm.command(&c.action, c.arg.as_deref());
        let echo = match c.arg.as_deref() {
            Some(arg) => format!("{} {arg}", c.action),
            None => c.action.clone(),
        };
        Response::Ok(echo)
    } else {
        Response::Err(format!("unknown action '{}'", c.action))
    };
    // The client may have hung up; that is not an error for us.
    let _ = c.reply.send(reply);
}

fn handle_win_event(wm: &mut Wm, ev: WinEvent) {
    match ev {
        // A window appeared, or was restored from the taskbar.
        WinEvent::Created(raw) | WinEvent::Restored(raw) => {
            let hwnd = win32::hwnd_from(raw);
            if wm.manage_if_eligible(hwnd) {
                return;
            }
            if let Some(id) = wm.id_of(hwnd) {
                if let Some(w) = wm.window(id) {
                    let mon = w.monitor;
                    wm.retile_monitor(mon);
                }
            }
        }
        WinEvent::Destroyed(raw) => wm.unmanage(win32::hwnd_from(raw)),
        // Minimizing frees the window's slot so the others can expand into it.
        // Nothing else retiles on a minimize, so without this the layout keeps
        // reserving the space and leaves a hole until some unrelated event
        // happens to trigger a retile.
        WinEvent::Minimized(raw) => {
            let hwnd = win32::hwnd_from(raw);
            if let Some(id) = wm.id_of(hwnd) {
                if let Some(mon) = wm.window(id).map(|w| w.monitor) {
                    // Only the active workspace's tree contains the slot.
                    if wm.workspaces.workspace_of(id) == wm.workspaces.active(mon) {
                        wm.retile_monitor(mon);
                    }
                }
            }
        }
        WinEvent::FocusChanged(raw) => wm.on_external_focus(win32::hwnd_from(raw)),
        // The user grabbed a tiled window with the mouse. Remember where it
        // started so the release can be read as a move, a swap or a resize.
        WinEvent::MoveSizeStart(raw) => wm.on_move_size_start(win32::hwnd_from(raw)),
        // The gesture ended. `on_move_size_end` decides whether it meant a move
        // to another monitor, a swap with a neighbour, a resize, or nothing at
        // all (in which case the window snaps back to where the layout wants it).
        WinEvent::MoveSizeEnd(raw) => wm.on_move_size_end(win32::hwnd_from(raw)),
    }
}
