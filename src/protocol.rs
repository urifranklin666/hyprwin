//! The control protocol: what a client may ask HyprWin to do over the pipe.
//!
//! Deliberately tiny and line-oriented, so `hyprwinctl` and any other client can
//! speak it with nothing but string formatting. Parsing lives here, pure and
//! tested, and both binaries include this file so there is exactly one
//! definition of the wire format.
//!
//! The client only formats and tags replies; the parsing half is the server's.
//! Both halves stay in one file so they cannot drift apart, which is why a few
//! items are unused in the client binary.
#![allow(dead_code)]

/// Name of the named pipe, without the `\\.\pipe\` prefix.
pub const PIPE_NAME: &str = "hyprwin";

/// How long a client waits for a reply before giving up, in milliseconds.
pub const TIMEOUT_MS: u32 = 5000;

/// Cap on an inbound request, so a client cannot make the server allocate
/// without bound. Commands are short; 8 KiB is generous.
pub const MAX_REQUEST: usize = 8192;

/// A request from a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Run a `Wm::command` action with an optional argument.
    Command { action: String, arg: Option<String> },
    /// Return the status document as `status.json` would hold it.
    Status,
    /// Return the `hyprctl monitors`-style summary.
    Monitors,
    /// Ask the process to exit cleanly.
    Quit,
}

/// What the server tells the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Ok(String),
    Data(String),
    Err(String),
}

impl Response {
    /// The wire form: `OK`/`DATA`/`ERR`, then a space, then the payload.
    ///
    /// The payload may itself contain newlines (the status document does), so
    /// the client is expected to read to end-of-stream rather than by line.
    pub fn render(&self) -> String {
        match self {
            Response::Ok(msg) => format!("OK {msg}"),
            Response::Data(body) => format!("DATA {body}"),
            Response::Err(msg) => format!("ERR {msg}"),
        }
    }
}

/// Parse a single request line.
///
/// The vocabulary is the command name followed by an optional argument. Bare
/// words that are not actions are read as the `status`/`monitors`/`quit`
/// pseudo-commands, so the common case is a single word on its own.
pub fn parse(line: &str) -> Result<Request, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty request".to_string());
    }
    let mut parts = line.splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    let rest = parts.next().map(str::trim).filter(|s| !s.is_empty());

    match head.to_ascii_lowercase().as_str() {
        "status" => Ok(Request::Status),
        "monitors" => Ok(Request::Monitors),
        "quit" | "exit" => Ok(Request::Quit),
        // Everything else is a command, with the rest of the line as its arg.
        action => {
            let action = action.to_ascii_lowercase();
            if action.is_empty() {
                return Err("no action".to_string());
            }
            Ok(Request::Command {
                action,
                arg: rest.map(|s| s.to_string()),
            })
        }
    }
}

/// Is this the action vocabulary `Wm::command` understands? Used to answer a
/// typo with a useful error instead of a silent no-op.
pub const ACTIONS: [&str; 14] = [
    "focus",
    "move",
    "resize",
    "togglefloating",
    "fullscreen",
    "togglelayout",
    "workspace",
    "movetoworkspace",
    "movetomonitor",
    "scratchpad",
    "movetoscratchpad",
    "toggletiling",
    "close",
    "reload",
];

/// The help text `hyprwinctl` prints when asked, and the reply to a bad action.
pub fn help() -> String {
    let mut s = String::from("hyprwinctl - drive a running HyprWin\n\nusage: hyprwinctl <command> [arg]\n\n");
    s.push_str("  status                     print status.json\n");
    s.push_str("  monitors                   print the per-monitor summary\n");
    s.push_str("  quit                       ask HyprWin to exit cleanly\n\n");
    s.push_str("window commands (act on the focused window):\n");
    for a in ACTIONS {
        s.push_str("  ");
        s.push_str(a);
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pseudo_commands_are_recognised_case_insensitively() {
        assert_eq!(parse("status").unwrap(), Request::Status);
        assert_eq!(parse("STATUS\n").unwrap(), Request::Status);
        assert_eq!(parse("  monitors  ").unwrap(), Request::Monitors);
        assert_eq!(parse("quit").unwrap(), Request::Quit);
        assert_eq!(parse("exit").unwrap(), Request::Quit);
    }

    #[test]
    fn a_command_carries_the_rest_of_the_line_as_its_argument() {
        assert_eq!(
            parse("workspace 3").unwrap(),
            Request::Command {
                action: "workspace".to_string(),
                arg: Some("3".to_string())
            }
        );
        assert_eq!(
            parse("MOVETOMONITOR Next").unwrap(),
            Request::Command {
                action: "movetomonitor".to_string(),
                arg: Some("Next".to_string())
            }
        );
        // The argument keeps everything after the first run of whitespace, so a
        // title-looking argument is not silently truncated.
        assert_eq!(
            parse("workspace 3 extra").unwrap(),
            Request::Command {
                action: "workspace".to_string(),
                arg: Some("3 extra".to_string())
            }
        );
    }

    #[test]
    fn a_bare_command_has_no_argument() {
        assert_eq!(
            parse("togglelayout").unwrap(),
            Request::Command {
                action: "togglelayout".to_string(),
                arg: None
            }
        );
        assert_eq!(
            parse("scratchpad   ").unwrap(),
            Request::Command {
                action: "scratchpad".to_string(),
                arg: None
            }
        );
    }

    #[test]
    fn empty_input_is_an_error_rather_than_a_silent_no_op() {
        assert!(parse("").is_err());
        assert!(parse("   \t ").is_err());
    }

    #[test]
    fn responses_render_with_their_tag_and_survive_multiline_payloads() {
        assert_eq!(Response::Ok("done".to_string()).render(), "OK done");
        assert_eq!(
            Response::Err("unknown action 'foo'".to_string()).render(),
            "ERR unknown action 'foo'"
        );
        let doc = Response::Data("{\n  \"running\": true\n}".to_string()).render();
        assert!(doc.starts_with("DATA {"));
        assert!(doc.ends_with('}'));
        // The tag is on the first line only; the payload keeps its own newlines.
        assert_eq!(doc.lines().count(), 3);
    }

    #[test]
    fn the_advertised_actions_match_the_dispatcher() {
        // Guards against documenting an action the dispatcher does not know.
        let source = include_str!("wm.rs");
        for action in ACTIONS {
            assert!(
                source.contains(&format!("\"{action}\" =>")),
                "{action} is advertised but not dispatched"
            );
        }
    }
}
