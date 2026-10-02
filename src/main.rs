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
mod monitor;
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

fn main() {
    if let Err(e) = run() {
        eprintln!("[hyprwin] fatal: {e:#}");
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
    println!(
        "[hyprwin] pid {} — config {}",
        unsafe { GetCurrentProcessId() },
        Config::path().display()
    );

    if cfg.autostart {
        match autostart::set_enabled(true) {
            Ok(()) => println!("[hyprwin] autostart registered"),
            Err(e) => eprintln!("[hyprwin] could not register autostart: {e:#}"),
        }
    }

    // Lets us hand focus to the window a keybind selected.
    let saved_timeout = win32::allow_foreground_change();

    // The receiver must exist before any hook or window proc can fire.
    let rx = events::init();

    let hooks = hook::Hooks::install();
    if hooks.count() == 0 {
        eprintln!("[hyprwin] warning: no WinEvent hooks installed; tiling will not track new windows");
    } else {
        println!("[hyprwin] {} event hooks installed", hooks.count());
    }

    let main_mod = cfg.main_mod_token();
    let (regs, failed) = hotkey::register_all(&cfg.keybinds, &main_mod);
    println!(
        "[hyprwin] {}/{} keybinds registered (main mod: {})",
        regs.len(),
        cfg.keybinds.len(),
        main_mod
    );
    for (bind, why) in &failed {
        eprintln!(
            "[hyprwin] keybind {}+{} not registered: {}",
            bind.mods.join("+"),
            bind.key,
            why
        );
    }

    let mut wm = Wm::new(cfg);

    // Control pipe: lets `hyprwinctl` and scripts drive a running instance. It
    // only ever queues events, so tiling state stays on this thread. Started
    // before adoption so the first retile populates its snapshot.
    ipc::spawn();
    println!("[hyprwin] control pipe {}", ipc::full_pipe_path());

    let adopted = if wm.cfg.manage_on_start {
        wm.manage_existing()
    } else {
        println!("[hyprwin] manage_on_start is false; only new windows will be tiled");
        0
    };
    println!(
        "[hyprwin] managing {adopted} existing windows across {} monitor(s)",
        wm.monitors.len()
    );

    let tray = tray::Tray::create();
    if tray.is_none() {
        eprintln!("[hyprwin] warning: tray icon could not be created");
    }

    unsafe {
        let _ = SetConsoleCtrlHandler(Some(ctrl_handler), TRUE);
    }

    // Publish once now that startup is done. The control pipe and the status
    // file are otherwise only refreshed by a retile, so a session that adopts
    // nothing — no windows open yet, or `manage_on_start` off — would answer
    // `status` with `{}` and `monitors` with nothing at all.
    wm.publish_status();

    println!("[hyprwin] running — Ctrl+C here, or tray icon -> Quit, to exit");

    let exit_code = message_loop(&mut wm, &rx, &regs);

    // Hand the desktop back: unhide anything parked on an off-screen workspace
    // and restore the geometry every window had before we tiled it.
    wm.reveal_all();
    wm.publish_stopped();
    println!("[hyprwin] shutting down");

    hotkey::unregister_all(&regs);
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
    regs: &[hotkey::Registered],
) -> i32 {
    let mut quit = false;
    let mut known_monitors: Vec<isize> = wm.monitors.iter().map(|m| m.handle).collect();

    while !quit {
        // Re-check the displays once per iteration, at the top, so a monitor
        // change can never re-enter a retile that is already in flight. The wait
        // below bounds how often this poll actually runs.
        let now: Vec<isize> = monitor::enumerate().iter().map(|m| m.handle).collect();
        if now != known_monitors {
            println!("[hyprwin] display configuration changed; re-tiling");
            known_monitors = now;
            wm.refresh_monitors();
        }

        let mut msg = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool();
        if has_message {
            if msg.message == WM_HOTKEY {
                if let Some(r) = regs.iter().find(|r| r.id == msg.wParam.0 as i32) {
                    wm.command(&r.bind.action, r.bind.arg.as_deref());
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

        // The queue is empty, so nothing can call back into us: block until an
        // event arrives or the timeout expires.
        match rx.recv_timeout(Duration::from_millis(IDLE_WAIT_MS)) {
            Ok(ev) => handle_app_event(wm, ev, regs, &mut quit),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => quit = true,
        }
        while let Ok(ev) = rx.try_recv() {
            handle_app_event(wm, ev, regs, &mut quit);
        }
    }
    0
}

fn handle_app_event(wm: &mut Wm, ev: AppEvent, _regs: &[hotkey::Registered], quit: &mut bool) {
    match ev {
        AppEvent::Win(we) => {
            handle_win_event(wm, we);
            // Safety net: a process killed outright may never emit a destroy
            // event, and a stale entry would corrupt the layout tree.
            wm.prune_dead();
        }
        AppEvent::Tray(TrayCommand::Quit) => *quit = true,
        AppEvent::Tray(TrayCommand::Reload) => wm.command("reload", None),
        AppEvent::Tray(TrayCommand::ToggleTiling) => {
            wm.toggle_tiling();
        }
        AppEvent::Control(c) => handle_control(wm, c, quit),
    }
}

/// Run a control-pipe request and send its answer back to the pipe thread.
///
/// An unknown action is reported as an error instead of the silent log line the
/// hotkey path uses, because here there is a caller waiting to be told.
fn handle_control(wm: &mut Wm, c: events::ControlCommand, quit: &mut bool) {
    let reply = if c.action.eq_ignore_ascii_case("quit") {
        *quit = true;
        Response::Ok("shutting down".to_string())
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
        // The user dragged or resized a window we are tiling; put it back.
        WinEvent::MoveSizeEnd(raw) => {
            let hwnd = win32::hwnd_from(raw);
            if let Some(id) = wm.id_of(hwnd) {
                if !wm.workspaces.is_floating(id) {
                    if let Some(w) = wm.window(id) {
                        let mon = w.monitor;
                        wm.retile_monitor(mon);
                    }
                }
            }
        }
    }
}
