//! The status document, for external dashboards such as a Rainmeter skin.
//!
//! HyprWin has no bar of its own and does not want to grow one, so the way to
//! display the active workspace or layout is to publish the facts and let a skin
//! read them. This module owns the document's shape and the write itself; `wm`
//! fills one in.

use crate::layout::Rect;
use serde::Serialize;
use std::io;
use std::path::Path;

/// The area a monitor's windows are laid out in, in the skin's own coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct RectStatus {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl From<Rect> for RectStatus {
    fn from(r: Rect) -> Self {
        RectStatus {
            x: r.x,
            y: r.y,
            w: r.w,
            h: r.h,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MonitorStatus {
    /// 1-based, primary first; matches the number a `monitor` rule uses.
    pub index: usize,
    pub primary: bool,
    pub active_workspace: u32,
    pub layout: String,
    /// Managed windows on the active workspace, floating ones included.
    pub windows: usize,
    /// Is the scratchpad dropped down on this monitor?
    pub scratchpad: bool,
    /// Windows on the scratchpad workspace, whether it is down or not.
    pub scratchpad_windows: usize,
    pub work_area: RectStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FocusedStatus {
    /// Empty when the focused window is not one HyprWin manages.
    pub title: String,
    pub exe: String,
    /// 1-based monitor index, or 0 when unknown.
    pub monitor: usize,
}

impl Default for FocusedStatus {
    fn default() -> Self {
        FocusedStatus {
            title: String::new(),
            exe: String::new(),
            monitor: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    /// False once HyprWin has stopped; the file keeps a skin from showing stale
    /// data after a clean exit.
    pub running: bool,
    pub pid: u32,
    pub tiling_enabled: bool,
    pub focused: FocusedStatus,
    pub monitors: Vec<MonitorStatus>,
}

impl Status {
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// Write the status document, replacing any previous one.
///
/// The write goes to a sibling temporary file that is then renamed over the
/// target, because a skin polls this path on a timer: a direct write can be
/// caught half-finished and read as invalid JSON.
pub fn write(path: &Path, status: &Status) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, status.to_json())?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Status {
        Status {
            running: true,
            pid: 4242,
            tiling_enabled: true,
            focused: FocusedStatus {
                title: "notes.md - Notepad".to_string(),
                exe: "notepad.exe".to_string(),
                monitor: 1,
            },
            monitors: vec![MonitorStatus {
                index: 1,
                primary: true,
                active_workspace: 3,
                layout: "dwindle".to_string(),
                windows: 2,
                scratchpad: false,
                scratchpad_windows: 0,
                work_area: Rect::new(0, 0, 2560, 1392).into(),
            }],
        }
    }

    #[test]
    fn the_document_is_valid_json_with_the_fields_a_skin_reads() {
        let json = sample().to_json();
        let v: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["running"], true);
        assert_eq!(v["focused"]["exe"], "notepad.exe");
        assert_eq!(v["monitors"][0]["active_workspace"], 3);
        assert_eq!(v["monitors"][0]["layout"], "dwindle");
        assert_eq!(v["monitors"][0]["work_area"]["w"], 2560);
    }

    #[test]
    fn writing_replaces_the_previous_document_and_leaves_no_temp_file() {
        let dir = std::env::temp_dir().join(format!("hyprwin-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");

        let mut first = sample();
        first.monitors[0].active_workspace = 1;
        write(&path, &first).unwrap();
        // A shorter second document proves the rename replaced rather than appended.
        let second = sample();
        write(&path, &second).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["monitors"][0]["active_workspace"], 3);
        assert!(
            !dir.join("status.json.tmp").exists(),
            "the temporary file must be renamed away"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_rect_maps_straight_across() {
        let r: RectStatus = Rect::new(8, 16, 1280, 696).into();
        assert_eq!(
            r,
            RectStatus {
                x: 8,
                y: 16,
                w: 1280,
                h: 696
            }
        );
    }
}
