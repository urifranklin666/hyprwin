//! hyprwinctl — drive a running HyprWin over its control pipe.
//!
//! The `hyprctl` analogue: send one request, print the answer, exit with a
//! status a script can check. It shares `protocol.rs` with the server, so the
//! wire format has exactly one definition.

use std::io::{Read, Write};

#[path = "../protocol.rs"]
mod protocol;

fn main() {
    let code = run();
    std::process::exit(code);
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "-h" | "--help" | "help") {
        print!("{}", protocol::help());
        // No request made, so exit non-zero when the user typed nothing at all.
        return if args.is_empty() { 2 } else { 0 };
    }

    let request = args.join(" ");
    match send(&request) {
        Ok(reply) => print_reply(&reply),
        Err(e) => {
            eprintln!("hyprwinctl: {e}");
            1
        }
    }
}

/// Connect, send one line, and read the whole answer.
fn send(request: &str) -> Result<String, String> {
    let path = format!(r"\\.\pipe\{}", protocol::PIPE_NAME);
    let mut stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                format!("no HyprWin is listening on {path} (is it running?)")
            }
            _ => format!("could not open {path}: {e}"),
        })?;

    stream
        .write_all(format!("{request}\n").as_bytes())
        .map_err(|e| format!("could not send the request: {e}"))?;
    stream
        .flush()
        .map_err(|e| format!("could not send the request: {e}"))?;

    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .map_err(|e| format!("could not read the answer: {e}"))?;
    Ok(answer)
}

/// Print the payload and map the tag to an exit code.
fn print_reply(raw: &str) -> i32 {
    let raw = raw.trim_end_matches(['\r', '\n']);
    match raw.split_once(' ') {
        Some(("OK", payload)) => {
            if !payload.is_empty() {
                println!("{payload}");
            }
            0
        }
        Some(("DATA", payload)) => {
            println!("{payload}");
            0
        }
        Some(("ERR", message)) => {
            eprintln!("hyprwinctl: {message}");
            1
        }
        // No tag: something answered that is not a HyprWin control pipe.
        _ => {
            eprintln!("hyprwinctl: unexpected reply: {raw}");
            1
        }
    }
}
