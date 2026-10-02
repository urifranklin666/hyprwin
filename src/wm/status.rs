//! The status document: a snapshot of tiling state for external dashboards.
//!
//! Split out of `wm` so the shape of what a skin reads is one small file rather
//! than buried in the middle of the tiling logic. `use super::*` is deliberate:
//! as a child of `wm` this reaches the manager's private state and helpers
//! without widening their visibility.

use super::*;

impl Wm {
    /// Snapshot the state an external dashboard needs.
    ///
    /// The document is rebuilt from scratch every time rather than patched, so a
    /// workspace that empties out or a window that closes cannot leave stale
    /// counts behind for a skin to display.
    pub fn status_document(&self) -> Status {
        let focused = win32::foreground();
        let focused_id = self.id_of(focused);
        let monitors = self
            .monitors
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let ws = self.workspaces.active(m.handle);
                MonitorStatus {
                    index: i + 1,
                    primary: m.primary,
                    active_workspace: ws,
                    layout: self.layout_name_of(m.handle).to_string(),
                    windows: self
                        .windows
                        .values()
                        .filter(|w| {
                            w.monitor == m.handle && self.workspaces.workspace_of(w.id) == ws
                        })
                        .count(),
                    scratchpad: self.workspaces.scratchpad_visible(m.handle),
                    scratchpad_windows: self
                        .windows
                        .values()
                        .filter(|w| {
                            w.monitor == m.handle
                                && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE
                        })
                        .count(),
                    work_area: m.work.into(),
                }
            })
            .collect();
        Status {
            running: true,
            pid: std::process::id(),
            tiling_enabled: self.tiling_enabled,
            focused: FocusedStatus {
                title: focused_id
                    .map(|_| win32::title(focused))
                    .unwrap_or_default(),
                exe: focused_id
                    .map(|_| win32::exe_name(focused))
                    .unwrap_or_default(),
                monitor: focused_id
                    .and_then(|id| self.windows.get(&id).map(|w| w.monitor))
                    .and_then(|mon| self.monitors.iter().position(|m| m.handle == mon))
                    .map(|i| i + 1)
                    .unwrap_or(0),
            },
            monitors,
        }
    }
}
