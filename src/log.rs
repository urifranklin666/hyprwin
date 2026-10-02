//! A tiny level-filtered logger that writes to the console *and* a file.
//!
//! HyprWin is normally started from the tray or the `Run` key, where the console
//! it writes to is either absent or closed the moment the user stops looking at
//! it. A failure in the field is therefore invisible, and the only way to
//! diagnose one is to reproduce it with the binary launched from a terminal. This
//! module keeps a trail on disk so that is not necessary.
//!
//! Every line carries a local timestamp and a level, both the console and the
//! file get the same text, and a failed write is swallowed: a diagnostic aid must
//! never be the thing that takes the manager down.
//!
//! The log lives at `%APPDATA%\hyprwin\hyprwin.log` by default, or wherever
//! `log_path` points. It is rotated once, to `hyprwin.log.old`, when it grows past
//! `MAX_BYTES`, so it cannot grow without bound across reboots.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

/// How large the log may grow before it is rotated once.
pub const MAX_BYTES: u64 = 1 << 20; // 1 MiB

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Failures the user may need to act on.
    Error,
    /// Something was skipped or refused, but the manager keeps working.
    Warn,
    /// Lifecycle milestones: startup, reload, mode changes.
    Info,
    /// Per-event detail, off by default because it is high volume.
    Debug,
}

impl Level {
    pub fn parse(s: &str) -> Option<Option<Level>> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "silent" => Some(None),
            "error" => Some(Some(Level::Error)),
            "warn" | "warning" => Some(Some(Level::Warn)),
            "info" => Some(Some(Level::Info)),
            "debug" | "trace" => Some(Some(Level::Debug)),
            _ => None,
        }
    }

    fn tag(self) -> &'static str {
        match self {
            Level::Error => "error",
            Level::Warn => "warn",
            Level::Info => "info",
            Level::Debug => "debug",
        }
    }
}

struct Sink {
    file: Option<File>,
    /// The minimum level to emit. `None` means logging is off entirely.
    level: Option<Level>,
}

static SINK: OnceLock<Mutex<Sink>> = OnceLock::new();

/// Where the log is written when `log_path` is empty.
pub fn default_path() -> PathBuf {
    crate::config::Config::dir().join("hyprwin.log")
}

/// Open the log for appending, rotating a large one out of the way first.
///
/// A path that cannot be opened is not fatal: the console half of the logger
/// still works, and a read-only `%APPDATA%` is not a reason to refuse to run.
pub fn init(path: &Path, level: Option<Level>) {
    rotate(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    let file = OpenOptions::new().create(true).append(true).open(path).ok();
    // `init` may be called again on a reload, so set it once and then only
    // update the level: replacing the `File` would leak the previous handle.
    // A failed open keeps the earlier handle rather than dropping logging.
    if let Some(slot) = SINK.get() {
        if let Ok(mut s) = slot.lock() {
            if file.is_some() {
                s.file = file;
            }
            s.level = level;
        }
    } else {
        let _ = SINK.set(Mutex::new(Sink { file, level }));
    }
}

/// Change the active level without reopening the file. Used by a config reload.
pub fn set_level(level: Option<Level>) {
    if let Some(slot) = SINK.get() {
        if let Ok(mut s) = slot.lock() {
            s.level = level;
        }
    }
}

/// Move an oversized log aside, keeping exactly one previous file.
fn rotate(path: &Path) {
    let too_big = fs::metadata(path)
        .map(|m| m.len() > MAX_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let old = path.with_extension("log.old");
    let _ = fs::remove_file(&old);
    let _ = fs::rename(path, &old);
}

fn enabled(level: Level) -> bool {
    match SINK.get().and_then(|s| s.lock().ok()).map(|s| s.level) {
        // Before `init`, keep the console informative but stay off debug, which
        // is the one level that would be noisy in a unit test run.
        None => level != Level::Debug,
        Some(None) => false,
        Some(Some(min)) => level <= min,
    }
}

/// Emit one line. Prefer the `log_*!` macros, which wrap this.
pub fn log(level: Level, args: fmt::Arguments) {
    if !enabled(level) {
        return;
    }
    let line = format!("{} [{}] {}", timestamp(), level.tag(), args);
    // The console may be gone (started from the tray) or its handle invalid
    // (started detached); a write failure there is not worth reporting.
    let _ = writeln!(std::io::stdout(), "{line}");
    if let Some(slot) = SINK.get() {
        if let Ok(mut s) = slot.lock() {
            if let Some(file) = s.file.as_mut() {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

fn timestamp() -> String {
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(_) => crate::win32::local_timestamp(),
        Err(_) => String::from("-------- --:--:--"),
    }
}

/// Log an error: something failed and the user may need to act.
#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Error, format_args!($($arg)*))
    };
}

/// Log a warning: something was skipped or refused, but we keep going.
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Warn, format_args!($($arg)*))
    };
}

/// Log a lifecycle milestone.
#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Info, format_args!($($arg)*))
    };
}

/// Log per-event detail, shown only at the `debug` level.
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {
        $crate::log::log($crate::log::Level::Debug, format_args!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_parse_and_off_is_distinct_from_unknown() {
        assert_eq!(
            Level::parse("off"),
            Some(None),
            "off is valid and means silent"
        );
        assert_eq!(Level::parse("  DEBUG "), Some(Some(Level::Debug)));
        assert_eq!(Level::parse("warn"), Some(Some(Level::Warn)));
        assert_eq!(
            Level::parse("banana"),
            None,
            "an unknown level is not `off`"
        );
    }

    #[test]
    fn a_larger_level_number_is_more_verbose() {
        assert!(Level::Error < Level::Warn);
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
    }

    #[test]
    fn rotation_moves_an_oversized_log_aside() {
        let dir = std::env::temp_dir().join(format!("hyprwin-log-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hyprwin.log");
        // A file past the cap must be moved, not deleted, so the previous run's
        // trail survives one rotation.
        let big = vec![b'x'; (MAX_BYTES + 1) as usize];
        fs::write(&path, &big).unwrap();

        rotate(&path);
        assert!(!path.exists(), "the oversized log is moved aside");
        assert!(
            dir.join("hyprwin.log.old").exists(),
            "the previous log is kept"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_small_log_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("hyprwin-log-small-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hyprwin.log");
        fs::write(&path, b"a few lines").unwrap();

        rotate(&path);
        assert!(path.exists(), "a log under the cap is not rotated");
        assert!(!dir.join("hyprwin.log.old").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
