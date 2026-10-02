//! The control pipe: a listener thread that accepts client connections and runs
//! their requests.
//!
//! The thread owns the whole conversation and pushes finished work onto the
//! existing `events` channel, so the tiling state is still only ever touched on
//! the main thread. It is detached: it holds `Arc<Mutex<Control>>` rather than a
//! `JoinHandle`, and when the process exits the pipe dies with it.

use crate::events::{AppEvent, ControlCommand};
use crate::protocol::{self, Request, Response, MAX_REQUEST, PIPE_NAME};
use std::fs::File;
use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;
use std::sync::{Arc, Mutex, OnceLock};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

/// The snapshot shared with the main loop, once `spawn` has been called.
static SHARED: OnceLock<Arc<Mutex<Control>>> = OnceLock::new();

/// The snapshot, if the control pipe is running.
///
/// The tiling code refreshes this from `publish_status`, and tolerates its
/// absence because the tests build a `Wm` without ever starting a pipe.
pub fn shared() -> Option<Arc<Mutex<Control>>> {
    SHARED.get().cloned()
}

/// Shared with the main loop: the last status document, so `status` can be
/// answered without a round trip through the event channel.
pub struct Control {
    /// Pretty-printed `status.json`, refreshed whenever the main loop publishes.
    pub status: String,
    /// `hyprctl monitors`-style summary, refreshed alongside `status`.
    pub monitors: String,
}

impl Default for Control {
    fn default() -> Self {
        Control {
            status: "{}".to_string(),
            monitors: String::new(),
        }
    }
}

/// The reply to a request is carried by `events::ControlCommand`, which owns the
/// channel it travels back on. `status`/`monitors` are answered by the shared
/// snapshot instead, without waking the main loop.
pub fn full_pipe_path() -> String {
    format!(r"\\.\pipe\{PIPE_NAME}")
}

/// Start the listener. Returns immediately; the thread runs until the process
/// exits.
pub fn spawn() {
    let control: Arc<Mutex<Control>> = Arc::new(Mutex::new(Control::default()));
    if SHARED.set(control.clone()).is_err() {
        // Already running; a second call is a no-op rather than a second pipe.
        return;
    }
    let path = full_pipe_path();
    std::thread::Builder::new()
        .name("hyprwin-ipc".to_string())
        .spawn(move || listen(path, control))
        .expect("spawning the control pipe thread");
}

fn listen(path: String, control: Arc<Mutex<Control>>) {
    use windows::core::HSTRING;
    let wide: Vec<u16> = HSTRING::from(path.as_str())
        .as_wide()
        .iter()
        .copied()
        .chain(Some(0))
        .collect();

    loop {
        // One fresh instance per connection, so a client that dies mid-request
        // cannot wedge the server.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                0,                    // output buffer: let the system pick
                0,                    // input buffer
                protocol::TIMEOUT_MS, // busy timeout, so a stuck client is dropped
                None,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            crate::log_error!("[hyprwin] control pipe could not be created; control is disabled");
            return;
        }
        unsafe {
            let _ = ConnectNamedPipe(handle, None);
        }
        serve(handle, &control);
        unsafe {
            let _ = CloseHandle(handle);
        }
    }
}

/// Serve one client, then return so the loop can wait for the next.
fn serve(handle: HANDLE, control: &Arc<Mutex<Control>>) {
    // Wrapping the pipe handle in a `File` gives ordinary `Read`/`Write` with
    // automatic cleanup, instead of hand-rolling ReadFile/WriteFile here.
    let mut stream = unsafe { File::from_raw_handle(handle.0) };
    let mut buf = [0u8; 256];
    let mut request = String::new();
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if request.len() + n > MAX_REQUEST {
                    let _ = stream.write_all(
                        Response::Err("request too large".into())
                            .render()
                            .as_bytes(),
                    );
                    break;
                }
                request.push_str(&String::from_utf8_lossy(&buf[..n]));
                if request.contains('\n') {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let line = request.lines().next().unwrap_or("").trim().to_string();
    let reply = handle_request(&line, control);
    let _ = stream.write_all(reply.render().as_bytes());
    let _ = stream.flush();
    // The `File` owns the handle and would close it; `listen` closes it too, so
    // hand the handle back rather than letting both do it.
    std::mem::forget(stream);
}

fn handle_request(line: &str, control: &Arc<Mutex<Control>>) -> Response {
    let request = match protocol::parse(line) {
        Ok(r) => r,
        Err(e) => return Response::Err(e),
    };
    match request {
        // Answered straight from the snapshot: no need to wake the main loop.
        Request::Status => Response::Data(snapshot(control).0),
        Request::Monitors => Response::Data(snapshot(control).1),
        // Everything that touches state goes through the main loop, so it is
        // applied at a single point and the caller can be told what happened.
        Request::Command { action, arg } => {
            let (tx, rx) = std::sync::mpsc::channel();
            crate::events::send(AppEvent::Control(ControlCommand {
                action,
                arg,
                reply: tx,
            }));
            match rx.recv_timeout(std::time::Duration::from_millis(
                protocol::TIMEOUT_MS as u64,
            )) {
                Ok(message) => message,
                Err(_) => Response::Err("HyprWin did not answer".to_string()),
            }
        }
        Request::Quit => {
            let (tx, rx) = std::sync::mpsc::channel();
            crate::events::send(AppEvent::Control(ControlCommand {
                action: "quit".to_string(),
                arg: None,
                reply: tx,
            }));
            match rx.recv_timeout(std::time::Duration::from_millis(
                protocol::TIMEOUT_MS as u64,
            )) {
                Ok(_) => Response::Ok("shutting down".to_string()),
                Err(_) => Response::Ok("shutting down".to_string()),
            }
        }
    }
}

fn snapshot(control: &Arc<Mutex<Control>>) -> (String, String) {
    match control.lock() {
        Ok(c) => (c.status.clone(), c.monitors.clone()),
        Err(_) => ("{}".to_string(), String::new()),
    }
}
