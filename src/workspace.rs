//! Per-monitor virtual workspaces and per-window state.
//!
//! Pure data structure: no Win32 calls, so the bookkeeping is unit-testable.

use std::collections::HashMap;

pub const FLOATING_DEFAULT_WORKSPACE: u32 = 1;

#[derive(Debug, Clone, Default)]
pub struct WorkspaceState {
    pub workspace_count: u32,
    /// monitor handle -> active workspace number
    active: HashMap<isize, u32>,
    /// window id -> workspace number
    assignment: HashMap<usize, u32>,
    floating: HashMap<usize, bool>,
    fullscreen: HashMap<usize, bool>,
}

impl WorkspaceState {
    pub fn new(workspace_count: u32) -> Self {
        WorkspaceState {
            workspace_count: workspace_count.max(1),
            ..Default::default()
        }
    }

    pub fn active(&self, monitor: isize) -> u32 {
        *self.active.get(&monitor).unwrap_or(&1)
    }

    pub fn set_active(&mut self, monitor: isize, ws: u32) {
        let ws = ws.clamp(1, self.workspace_count);
        self.active.insert(monitor, ws);
    }

    /// The workspace `step` away from the active one, wrapping around.
    ///
    /// Deliberately pure: it reports where a switch would land without moving
    /// there. Mutating the active workspace here would make the caller's
    /// "have we actually changed?" check fire spuriously and skip the retile.
    pub fn workspace_after(&self, monitor: isize, step: i32) -> u32 {
        let n = self.workspace_count.max(1) as i32;
        let cur = self.active(monitor) as i32;
        (((cur - 1 + step).rem_euclid(n)) + 1) as u32
    }

    pub fn assign(&mut self, id: usize, ws: u32) {
        self.assignment.insert(id, ws.clamp(1, self.workspace_count));
    }

    pub fn workspace_of(&self, id: usize) -> u32 {
        self.floating
            .get(&id)
            .copied()
            .filter(|f| *f)
            .map(|_| FLOATING_DEFAULT_WORKSPACE)
            .unwrap_or_else(|| *self.assignment.get(&id).unwrap_or(&FLOATING_DEFAULT_WORKSPACE))
    }

    pub fn is_floating(&self, id: usize) -> bool {
        self.floating.get(&id).copied().unwrap_or(false)
    }

    pub fn set_floating(&mut self, id: usize, value: bool) {
        self.floating.insert(id, value);
    }

    pub fn toggle_floating(&mut self, id: usize) -> bool {
        let v = !self.is_floating(id);
        self.set_floating(id, v);
        v
    }

    pub fn is_fullscreen(&self, id: usize) -> bool {
        self.fullscreen.get(&id).copied().unwrap_or(false)
    }

    pub fn toggle_fullscreen(&mut self, id: usize) -> bool {
        let v = !self.is_fullscreen(id);
        self.fullscreen.insert(id, v);
        v
    }

    #[allow(dead_code)]
    pub fn set_fullscreen(&mut self, id: usize, value: bool) {
        self.fullscreen.insert(id, value);
    }

    /// Forget everything about a window that has closed.
    pub fn remove(&mut self, id: usize) {
        self.assignment.remove(&id);
        self.floating.remove(&id);
        self.fullscreen.remove(&id);
    }

    /// Windows assigned to a workspace, in insertion-independent order.
    #[allow(dead_code)]
    pub fn on_workspace<'a>(&self, ids: impl Iterator<Item = &'a usize>, ws: u32) -> Vec<usize> {
        ids.copied().filter(|id| self.workspace_of(*id) == ws).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workspace_is_one() {
        let s = WorkspaceState::new(9);
        assert_eq!(s.active(0), 1);
        assert_eq!(s.workspace_of(42), 1);
    }

    #[test]
    fn workspaces_are_tracked_per_monitor() {
        let mut s = WorkspaceState::new(9);
        s.set_active(1, 3);
        assert_eq!(s.active(1), 3);
        assert_eq!(s.active(2), 1, "a second monitor has its own workspace");
    }

    #[test]
    fn active_workspace_is_clamped_to_the_configured_count() {
        let mut s = WorkspaceState::new(4);
        s.set_active(0, 99);
        assert_eq!(s.active(0), 4);
        s.set_active(0, 0);
        assert_eq!(s.active(0), 1);
    }

    #[test]
    fn workspace_after_wraps_in_both_directions() {
        let mut s = WorkspaceState::new(3);
        s.set_active(0, 1);
        assert_eq!(s.workspace_after(0, -1), 3, "going back from 1 wraps to 3");
        assert_eq!(s.workspace_after(0, 1), 2);
        s.set_active(0, 3);
        assert_eq!(s.workspace_after(0, 1), 1, "forward from the last wraps to 1");
    }

    #[test]
    fn workspace_after_does_not_move_the_active_workspace() {
        let mut s = WorkspaceState::new(9);
        s.set_active(0, 4);
        assert_eq!(s.workspace_after(0, 1), 5);
        assert_eq!(s.active(0), 4, "querying must not change the active workspace");
    }

    #[test]
    fn assignment_moves_a_window() {
        let mut s = WorkspaceState::new(9);
        s.assign(7, 5);
        assert_eq!(s.workspace_of(7), 5);
        assert_eq!(s.on_workspace([1, 7, 9].iter(), 5), vec![7]);
    }

    #[test]
    fn floating_windows_are_reported_on_the_default_workspace() {
        let mut s = WorkspaceState::new(9);
        s.assign(3, 6);
        s.set_floating(3, true);
        assert!(s.is_floating(3));
        assert_eq!(s.workspace_of(3), FLOATING_DEFAULT_WORKSPACE);
    }

    #[test]
    fn toggles_flip_and_stick() {
        let mut s = WorkspaceState::new(9);
        assert!(s.toggle_floating(1));
        assert!(s.is_floating(1));
        assert!(!s.toggle_floating(1));
        assert!(s.toggle_fullscreen(1));
        assert!(s.is_fullscreen(1));
    }

    #[test]
    fn remove_forgets_every_trait() {
        let mut s = WorkspaceState::new(9);
        s.assign(5, 4);
        s.set_floating(5, true);
        s.toggle_fullscreen(5);
        s.remove(5);
        assert_eq!(s.workspace_of(5), 1);
        assert!(!s.is_floating(5));
        assert!(!s.is_fullscreen(5));
    }

    #[test]
    fn workspace_assignment_is_clamped() {
        let mut s = WorkspaceState::new(3);
        s.assign(1, 500);
        assert_eq!(s.workspace_of(1), 3);
    }
}
