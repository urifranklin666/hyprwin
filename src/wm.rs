//! The window manager: owns all tiling state and turns commands into retiles.
//!
//! This is the only module that orchestrates layout, workspace and Win32 calls.
//! It deliberately contains no rendering or message-loop code.

use crate::config::{size_limit, Config, Direction, LayoutKind, Rule};
use crate::layout::{self, DwindleTree, Forest, Rect};
use crate::monitor::{self, Monitor};
use crate::overlay::Overlays;
use crate::status::{FocusedStatus, MonitorStatus, Status};
use crate::win32;
use crate::workspace::{WorkspaceState, SCRATCHPAD_WORKSPACE};
use std::collections::HashMap;
use std::time::Instant;
use windows::Win32::Foundation::HWND;

// The manager is large enough to read best in parts. Each child reaches the
// private state above with `use super::*`, so splitting it moves code without
// widening any visibility.
mod borders;
mod commands;
mod status;

/// Share of the work area the scratchpad covers when it drops down.
const SCRATCHPAD_FRACTION: f32 = 0.7;

/// A window HyprWin is aware of.
#[derive(Debug, Clone)]
pub struct ManagedWindow {
    pub hwnd: HWND,
    /// Dwindle leaf id; also the key for workspace/floating bookkeeping.
    pub id: usize,
    pub monitor: isize,
    pub floating: bool,
    /// Was the window already maximized when we adopted it?
    ///
    /// This decides which way `reveal_all` goes on the way out. A window
    /// maximized *during* a run has to be un-maximized, or it is left zoomed over
    /// the whole monitor. A window that arrived *already* maximized has to come
    /// back maximized: the rectangle we captured at adoption is its maximized
    /// one, so un-maximizing and then restoring that rectangle leaves it at the
    /// maximized size with no maximized state — the half-restored look, arrived
    /// at from the other side.
    pub restore_maximized: bool,
}

pub struct Wm {
    pub cfg: Config,
    /// Layout for each monitor, keyed by handle. Seeded from `default_layout`
    /// and the per-monitor `layouts` map, then moved by the `layout` command.
    /// Kept separate from `cfg` because the command changes it without writing
    /// the config back, the same way the layout survives a reload by default.
    pub layouts: HashMap<isize, LayoutKind>,
    pub workspaces: WorkspaceState,
    /// Monitors, refreshed whenever the display set changes.
    pub monitors: Vec<Monitor>,

    /// id -> window
    windows: HashMap<usize, ManagedWindow>,
    /// hwnd bits -> id
    by_hwnd: HashMap<isize, usize>,
    /// One forest of dwindle trees per monitor, keyed by workspace, so leaving a
    /// workspace parks its layout instead of destroying it.
    forests: HashMap<isize, Forest>,
    /// id -> rectangle from the most recent tiling pass (drives geometric focus).
    placed: HashMap<usize, Rect>,
    /// id -> rectangle the window had immediately before we first tiled it, so
    /// quitting can hand the desktop back as we found it.
    saved_rects: HashMap<usize, Rect>,
    /// id -> rectangle from before it became the scratchpad, restored when pulled up.
    scratch_saves: HashMap<usize, Rect>,
    next_id: usize,
    pub tiling_enabled: bool,
    /// Per-monitor layered windows that draw the thick focused-tile accent.
    overlays: Overlays,
    /// The accent frames currently on screen, and the focused window they were
    /// built for. Reused as the "from" side of a focus cross-fade.
    last_frames: Vec<crate::overlay::Frame>,
    last_focus: Option<isize>,
    /// An in-flight window move. Stepped by `tick` from the message loop rather
    /// than slept through here, so a keybind still answers mid-animation.
    tween: Option<crate::animation::Tween>,
    /// An in-flight focus cross-fade, stepped the same way.
    border_fade: Option<BorderFade>,
    /// Where a mouse drag began, so the gesture can be read when it ends.
    ///
    /// HyprWin is not a compositor, so it cannot follow the cursor; what it can
    /// do is remember the geometry a Windows-initiated move/resize started from
    /// and interpret the end position. That is enough to move a tile to another
    /// display, trade places with a neighbour, or resize the split, without ever
    /// taking the mouse.
    drag: Option<DragState>,
}

/// The start of a move/resize gesture, remembered until it ends.
struct DragState {
    id: usize,
    start: Rect,
    monitor: isize,
}

/// A focus cross-fade in progress: the overlay frames being faded from and to.
///
/// Deliberately its own clock rather than a `Tween`: a fade moves colour, not
/// geometry, so forcing it through the rectangle interpolator would mean
/// inventing rects to carry a fraction.
struct BorderFade {
    from: Vec<crate::overlay::Frame>,
    to: Vec<crate::overlay::Frame>,
    easing: String,
    start: std::time::Instant,
    duration_ms: u32,
}

impl BorderFade {
    /// The eased progress and whether the fade has finished.
    fn fraction(&self) -> (f32, bool) {
        let elapsed = self.start.elapsed().as_millis() as u64;
        if elapsed >= self.duration_ms as u64 {
            return (1.0, true);
        }
        let t = elapsed as f32 / self.duration_ms as f32;
        (crate::animation::ease(&self.easing, t), false)
    }
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

impl Wm {
    pub fn new(cfg: Config) -> Self {
        let monitors = monitor::enumerate();
        let workspaces = WorkspaceState::new(cfg.workspaces);
        let layouts = seed_layouts(&cfg, &monitors);
        Wm {
            cfg,
            layouts,
            workspaces,
            monitors,
            windows: HashMap::new(),
            by_hwnd: HashMap::new(),
            forests: HashMap::new(),
            placed: HashMap::new(),
            saved_rects: HashMap::new(),
            scratch_saves: HashMap::new(),
            next_id: 1,
            tiling_enabled: true,
            overlays: Overlays::default(),
            last_frames: Vec::new(),
            last_focus: None,
            tween: None,
            border_fade: None,
            drag: None,
        }
    }

    /// Start, extend or re-aim the window-move tween with `moves`.
    ///
    /// `moves` are the destinations; the current rectangles are read here, so a
    /// window already in flight continues from where it is on screen rather than
    /// snapping back to a stale origin.
    fn animate_into_tween(&mut self, moves: &[(HWND, Rect)]) {
        if moves.is_empty() {
            return;
        }
        let duration = self.cfg.animation_ms;
        if duration == 0 {
            self.tween = None;
            let pairs: Vec<(isize, Rect)> = moves.iter().map(|(h, r)| (key(*h), *r)).collect();
            win32::set_pos_tween(&pairs);
            return;
        }
        let items: Vec<(isize, Rect, Rect)> = moves
            .iter()
            .map(|(h, r)| (key(*h), win32::rect_of(*h).unwrap_or_default(), *r))
            .collect();
        let easing = self.cfg.animation_easing.clone();
        match &mut self.tween {
            // Keep an in-flight tween's clock so a quick succession of retiles
            // all land together rather than restarting the easing each time.
            Some(t) if t.easing_matches(&easing) && t.duration() != 0 => t.merge(items),
            _ => {
                self.tween = Some(crate::animation::Tween::new(
                    items,
                    duration,
                    &easing,
                    crate::animation::FRAME_MS,
                ))
            }
        }
    }

    /// Advance whatever is animating and return how long to wait before the next
    /// step, or `None` when nothing is left to do.
    ///
    /// The message loop calls this each idle pass. It applies one frame and
    /// returns; blocking the wait here is what keeps the loop responsive between
    /// frames without busy-spinning through them.
    pub fn tick(&mut self) -> Option<u64> {
        let mut wait: Option<u64> = None;
        if let Some(t) = &self.tween {
            let (frames, done) = t.sample();
            win32::set_pos_tween(&frames);
            if done {
                self.tween = None;
            } else {
                wait = Some(t.sleep_ms());
            }
        }
        if let Some(fade) = &self.border_fade {
            let (t, done) = fade.fraction();
            let blend = blend_frames(&fade.from, &fade.to, t);
            self.overlays.redraw(&self.monitors, &blend);
            if done {
                self.overlays.redraw(&self.monitors, &fade.to);
                self.last_frames = fade.to.clone();
                self.border_fade = None;
            } else {
                wait = Some(wait.map_or(crate::animation::FRAME_MS as u64, |w| {
                    w.min(crate::animation::FRAME_MS as u64)
                }));
            }
        }
        wait
    }

    /// The layout in force on `mon`, falling back to the default.
    fn layout_of(&self, mon: isize) -> LayoutKind {
        self.layouts
            .get(&mon)
            .copied()
            .unwrap_or(self.cfg.default_layout)
    }

    /// One monitor's layout, for its line in `status.json` and `monitors`.
    fn layout_name_of(&self, mon: isize) -> &'static str {
        self.layout_of(mon).name()
    }

    /// The master-ratio in force on workspace `ws` of `mon`.
    ///
    /// A per-workspace entry wins over the global default, so a workspace can
    /// keep its own master width; resizing the split writes one back.
    fn master_ratio_for(&self, _mon: isize, ws: u32) -> f32 {
        self.cfg
            .master_ratios
            .get(&ws.to_string())
            .copied()
            .unwrap_or(self.cfg.master_ratio)
    }

    /// Record a master-ratio change for workspace `ws` so a later retile keeps it.
    fn set_master_ratio(&mut self, _mon: isize, ws: u32, ratio: f32) {
        self.cfg
            .master_ratios
            .insert(ws.to_string(), ratio.clamp(0.1, 0.9));
    }

    // ---------------------------------------------------------------- windows

    pub fn id_of(&self, hwnd: HWND) -> Option<usize> {
        self.by_hwnd.get(&key(hwnd)).copied()
    }

    pub fn hwnd_of(&self, id: usize) -> Option<HWND> {
        self.windows.get(&id).map(|w| w.hwnd)
    }

    pub fn is_managed(&self, hwnd: HWND) -> bool {
        self.by_hwnd.contains_key(&key(hwnd))
    }

    pub fn window(&self, id: usize) -> Option<&ManagedWindow> {
        self.windows.get(&id)
    }

    /// The monitor a window most overlaps.
    ///
    /// Comparing the window centre against a monitor's half-width box misfires on
    /// uneven multi-monitor layouts, so measure the overlap directly instead.
    fn monitor_for(&self, hwnd: HWND) -> isize {
        if let Some(r) = win32::rect_of(hwnd) {
            let best = self
                .monitors
                .iter()
                .map(|m| {
                    (
                        r.overlap_h(&m.work) as i64 * r.overlap_v(&m.work) as i64,
                        m.handle,
                    )
                })
                .filter(|(area, _)| *area > 0)
                .max_by_key(|(area, _)| *area);
            if let Some((_, handle)) = best {
                return handle;
            }
        }
        monitor::monitor_for_window(hwnd)
            .map(|m| m.handle)
            .or_else(|| self.monitors.first().map(|m| m.handle))
            .unwrap_or(0)
    }

    /// Does an existing rule say to leave this window alone entirely?
    fn rule_ignores(&self, hwnd: HWND) -> bool {
        let (exe, class, title) = self.rule_fields(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "ignore")
            .any(|r| rule_matches(r, &exe, &class, &title))
    }

    fn rule_target_workspace(&self, hwnd: HWND) -> Option<u32> {
        let (exe, class, title) = self.rule_fields(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "workspace")
            .find(|r| rule_matches(r, &exe, &class, &title))
            .and_then(|r| r.arg.as_ref())
            .and_then(|a| a.parse::<u32>().ok())
    }

    fn rule_floats(&self, hwnd: HWND) -> bool {
        let (exe, class, title) = self.rule_fields(hwnd);
        let size = win32::rect_of(hwnd).map(|r| (r.w, r.h));
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "float")
            .any(|r| rule_matches(r, &exe, &class, &title) && rule_size_allows(r, size))
    }

    /// Is the focused window tagged for the scratchpad?
    fn rule_scratchpads(&self, hwnd: HWND) -> bool {
        let (exe, class, title) = self.rule_fields(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "scratchpad")
            .any(|r| rule_matches(r, &exe, &class, &title))
    }

    /// Which monitor a window should live on: a `monitor` rule if one matches,
    /// otherwise the one it physically overlaps.
    fn target_monitor(&self, hwnd: HWND, exe: &str, class: &str, title: &str) -> Option<isize> {
        let named = self
            .cfg
            .rules
            .iter()
            .filter(|r| r.action == "monitor")
            .find(|r| rule_matches(r, exe, class, title))
            .and_then(|r| r.arg.as_deref())
            .and_then(|a| parse_monitor(a, &self.monitors));
        named.or_else(|| Some(self.monitor_for(hwnd)))
    }

    fn rule_fields(&self, hwnd: HWND) -> (String, String, String) {
        (
            win32::exe_name(hwnd),
            win32::class_name(hwnd),
            win32::title(hwnd),
        )
    }

    /// Put a window back into a tile if Windows has maximised it.
    ///
    /// A window can be maximised at any time — by a title-bar double click, by
    /// `Win`+`Up`, or by the application itself — and a maximised window is
    /// still `IsWindowVisible`, so it stays in the layout. Setting its position
    /// does not un-maximise it, so without this it would sit zoomed over the
    /// whole monitor and ignore the tile it was given.
    fn normalize(&self, id: usize) {
        if self.workspaces.is_floating(id) {
            return;
        }
        if let Some(hwnd) = self.hwnd_of(id) {
            win32::unmaximize(hwnd);
        }
    }

    /// Adopt a window. Returns its id, or None if it should not be tiled.
    pub fn manage(&mut self, hwnd: HWND) -> Option<usize> {
        if !self.tiling_enabled || self.is_managed(hwnd) {
            return None;
        }
        if !win32::is_manageable(hwnd) || self.rule_ignores(hwnd) {
            return None;
        }
        let (exe, class, title) = self.rule_fields(hwnd);
        let mon = self.target_monitor(hwnd, &exe, &class, &title).unwrap_or(0);
        let scratch = self.rule_scratchpads(hwnd);
        let ws = if scratch {
            SCRATCHPAD_WORKSPACE
        } else {
            self.rule_target_workspace(hwnd)
                .unwrap_or_else(|| self.workspaces.active(mon))
        };
        let float = self.rule_floats(hwnd);

        let id = self.next_id;
        self.next_id += 1;
        // Read the window's starting state *before* we touch it. `reveal_all`
        // replays these on exit so quitting does not leave the desktop
        // rearranged. The maximized flag in particular has to be read here: the
        // un-maximize below clears it, and the rectangle captured while it is
        // still set is the maximized one, which is what `reveal_all` must put
        // back for a window that arrived maximized.
        let restore_maximized = win32::is_maximized(hwnd);
        if let Some(r) = win32::rect_of(hwnd) {
            self.saved_rects.insert(id, r);
        }
        self.workspaces.assign_raw(id, ws);
        self.workspaces.set_floating(id, float);
        self.windows.insert(
            id,
            ManagedWindow {
                hwnd,
                id,
                monitor: mon,
                floating: float,
                restore_maximized,
            },
        );
        self.by_hwnd.insert(key(hwnd), id);

        if scratch {
            // A scratchpad-tagged window is dropped down the moment it appears,
            // rather than silently vanishing onto a hidden workspace. The window
            // must already be in `windows` for these to find it.
            self.make_scratchpad(id);
            self.focus_scratchpad(mon);
            self.workspaces.toggle_scratchpad(mon);
        }

        if !float {
            // Windows on a non-active workspace stay hidden. The scratchpad is
            // dropped down by the block above, so its window stays visible.
            if ws != self.workspaces.active(mon)
                && ws != SCRATCHPAD_WORKSPACE
                && !self.workspaces.scratchpad_visible(mon)
            {
                win32::hide(hwnd);
            }
            win32::unmaximize(hwnd);
            if self.cfg.rounding > 0 {
                win32::set_rounding(hwnd, self.cfg.rounding);
            }
        }
        self.retile_monitor(mon);
        self.apply_border_focus();
        Some(id)
    }

    /// Adopt every eligible window that is already open.
    pub fn manage_existing(&mut self) -> usize {
        let mut adopted = 0;
        for hwnd in win32::top_level_windows() {
            if self.manage_if_eligible(hwnd) {
                adopted += 1;
            }
        }
        for m in self.monitors.clone() {
            self.retile_monitor(m.handle);
        }
        self.apply_border_focus();
        adopted
    }

    pub fn manage_if_eligible(&mut self, hwnd: HWND) -> bool {
        if !win32::is_manageable(hwnd) || self.is_managed(hwnd) || self.rule_ignores(hwnd) {
            return false;
        }
        self.manage(hwnd).is_some()
    }

    pub fn unmanage(&mut self, hwnd: HWND) {
        let Some(id) = self.id_of(hwnd) else {
            return;
        };
        let mon = self.windows.get(&id).map(|w| w.monitor).unwrap_or(0);
        self.by_hwnd.remove(&key(hwnd));
        self.windows.remove(&id);
        self.workspaces.remove(id);
        self.placed.remove(&id);
        self.saved_rects.remove(&id);
        self.scratch_saves.remove(&id);
        // Drop it from the parked tree of every workspace, not just the active
        // one, or a stale leaf would come back when you next visit that desktop.
        self.forests.entry(mon).or_default().forget(id);
        self.retile_monitor(mon);
        self.apply_border_focus();
    }

    fn drop_window(&mut self, id: usize) {
        if let Some(w) = self.windows.remove(&id) {
            self.by_hwnd.remove(&key(w.hwnd));
        }
        self.workspaces.remove(id);
        self.placed.remove(&id);
        self.saved_rects.remove(&id);
        self.scratch_saves.remove(&id);
    }

    /// Remember the geometry a move/resize gesture started from.
    ///
    /// Floating windows are left out: the user's drag is the point and nothing
    /// snaps them back. A tiled window is recorded so the end position can be
    /// read against where it began.
    pub fn on_move_size_start(&mut self, hwnd: HWND) {
        let Some(id) = self.id_of(hwnd) else {
            return;
        };
        if self.workspaces.is_floating(id) {
            return;
        }
        let Some(start) = win32::rect_of(hwnd) else {
            return;
        };
        let monitor = self.windows.get(&id).map(|w| w.monitor).unwrap_or(0);
        self.drag = Some(DragState { id, start, monitor });
    }

    /// Interpret a finished move/resize gesture as a layout operation.
    ///
    /// Never a no-op: whatever the gesture did not decide, the retile at the end
    /// undoes, so a drag across empty space simply leaves the window where the
    /// layout says it belongs.
    pub fn on_move_size_end(&mut self, hwnd: HWND) {
        let Some(id) = self.id_of(hwnd) else {
            return;
        };
        // Only a tiled, remembered window participates; anything else is just a
        // floating window being dragged, which is allowed to stay put.
        let Some(drag) = self.drag.take().filter(|d| d.id == id) else {
            if !self.workspaces.is_floating(id) {
                self.retile_window(id);
            }
            return;
        };
        if self.workspaces.is_floating(id) {
            return;
        }
        let Some(end) = win32::rect_of(hwnd) else {
            return;
        };
        // The window's centre before the gesture was cancelled by a mouse-down
        // that never really moved it.
        let center = (end.x + end.w / 2, end.y + end.h / 2);
        let moved = (end.x - drag.start.x).abs() + (end.y - drag.start.y).abs();

        // 1. A cross to another display wins: that is unambiguous, and the most
        //    common reason to drag a tile at all.
        if let Some(target) = self.monitor_at_point(center.0, center.1) {
            if target != drag.monitor {
                self.move_window_to_monitor(id, target);
                return;
            }
        }

        // 2. A small movement is a resize: the size changed but the window did
        //    not travel, so trade the direction the far edge moved for a split
        //    adjustment. A move that only jiggled in place leaves size alone and
        //    falls through to the plain retile below.
        let dw = end.w - drag.start.w;
        let dh = end.h - drag.start.h;
        if moved <= 24 {
            if dw.abs() >= dh.abs() && dw.abs() >= 8 {
                let dir = if dw > 0 {
                    Direction::Right
                } else {
                    Direction::Left
                };
                self.resize_focused_edge(id, dir);
                return;
            }
            if dh.abs() > dw.abs() && dh.abs() >= 8 {
                let dir = if dh > 0 {
                    Direction::Down
                } else {
                    Direction::Up
                };
                self.resize_focused_edge(id, dir);
                return;
            }
        }

        // 3. Dropped over a neighbour: trade places with it.
        if moved >= 40 {
            if let Some(other) = self.neighbor_at(id, center) {
                if other != id && !self.workspaces.is_floating(other) {
                    self.swap_windows(id, other);
                    return;
                }
            }
        }

        // 4. Nothing matched: put it back.
        self.retile_window(id);
    }

    /// The known monitor whose bounds contain `(x, y)`, if any.
    fn monitor_at_point(&self, x: i32, y: i32) -> Option<isize> {
        self.monitors
            .iter()
            .find(|m| {
                x >= m.rect.x && x < m.rect.x + m.rect.w && y >= m.rect.y && y < m.rect.y + m.rect.h
            })
            .map(|m| m.handle)
    }

    /// The currently placed window other than `id` whose rectangle contains the
    /// point, preferring the smallest (topmost, most specific) match.
    fn neighbor_at(&self, id: usize, point: (i32, i32)) -> Option<usize> {
        let mut best: Option<(i64, usize)> = None;
        for (other, r) in &self.placed {
            if *other == id {
                continue;
            }
            if point.0 >= r.x && point.0 < r.x + r.w && point.1 >= r.y && point.1 < r.y + r.h {
                let area = r.w as i64 * r.h as i64;
                if best.map(|(a, _)| area < a).unwrap_or(true) {
                    best = Some((area, *other));
                }
            }
        }
        best.map(|(_, o)| o)
    }

    /// Re-tile the monitor holding `id`, for a gesture that decided nothing.
    fn retile_window(&mut self, id: usize) {
        if let Some(mon) = self.windows.get(&id).map(|w| w.monitor) {
            self.retile_monitor(mon);
        }
    }

    /// Move a specific window to another monitor (the drag counterpart of
    /// `cmd_move_to_monitor`, which acts on the focused window).
    fn move_window_to_monitor(&mut self, id: usize, to: isize) {
        let from = self.windows.get(&id).map(|w| w.monitor).unwrap_or(0);
        if to == from || !self.monitors.iter().any(|m| m.handle == to) {
            self.retile_window(id);
            return;
        }
        let ws = self.workspaces.active(to);
        self.workspaces.assign(id, ws);
        self.forests.entry(from).or_default().forget(id);
        if let Some(w) = self.windows.get_mut(&id) {
            w.monitor = to;
            w.floating = false;
        }
        self.workspaces.set_floating(id, false);
        if let Some(hwnd) = self.hwnd_of(id) {
            win32::unmaximize(hwnd);
        }
        self.retile_monitor(from);
        self.retile_monitor(to);
        if let Some(hwnd) = self.hwnd_of(id) {
            win32::set_foreground(hwnd);
        }
        self.apply_border_focus();
    }

    /// Adjust the split at `id` as though the resize keybind was pressed.
    fn resize_focused_edge(&mut self, id: usize, dir: Direction) {
        let delta = match dir {
            Direction::Left | Direction::Up => -0.04,
            Direction::Right | Direction::Down => 0.04,
        };
        let mon = self.windows.get(&id).map(|w| w.monitor).unwrap_or(0);
        let ws = self.workspaces.active(mon);
        if self.layout_of(mon) == LayoutKind::Master {
            let next = self.master_ratio_for(mon, ws) + delta;
            self.set_master_ratio(mon, ws, next);
        } else {
            let active = self.workspaces.workspace_of(id);
            let forest = self.forests.entry(mon).or_default();
            let mut tree = forest.deactivate(active);
            tree.resize(id, delta);
            forest.activate(&tree, active);
        }
        self.retile_monitor(mon);
        self.apply_border_focus();
    }

    /// Exchange two windows in the layout, keeping their geometry otherwise.
    fn swap_windows(&mut self, id: usize, other: usize) {
        let mon = self.windows.get(&id).map(|w| w.monitor).unwrap_or(0);
        if self.windows.get(&other).map(|w| w.monitor) != Some(mon) {
            self.retile_window(id);
            return;
        }
        let active = self.workspaces.active(mon);
        // Both windows must be in the active workspace's tree for a swap to mean
        // anything; the scratchpad has its own tree.
        if self.workspaces.workspace_of(id) != active
            || self.workspaces.workspace_of(other) != active
        {
            self.retile_window(id);
            return;
        }
        let forest = self.forests.entry(mon).or_default();
        let mut tree = forest.deactivate(active);
        tree.swap(id, other);
        forest.activate(&tree, active);
        self.retile_monitor(mon);
        self.apply_border_focus();
    }

    /// Bring `hwnd` to the foreground. Used when the user clicks a window.
    pub fn on_external_focus(&mut self, hwnd: HWND) {
        let Some(id) = self.id_of(hwnd) else {
            return;
        };
        let mon = self.windows[&id].monitor;
        let ws = self.workspaces.workspace_of(id);
        // The scratchpad deliberately sits outside the numbered workspaces;
        // switching to it would be a switch to a number nobody else uses.
        if ws != SCRATCHPAD_WORKSPACE && ws != self.workspaces.active(mon) {
            self.switch_workspace(mon, ws);
        }
        // The user aimed at one specific window; the workspace switch raises the
        // first window it finds, so re-assert theirs afterwards. Clicking a window
        // is a focus change with the layout already settled, so the accent eases
        // across to it.
        win32::set_foreground(hwnd);
        self.apply_border_focus_animated();
    }

    // ---------------------------------------------------------------- retiling

    /// Park the active workspace's layout and load the one for `target`.
    ///
    /// Every workspace switch must go through here. The forest's active slot
    /// already holds the current tree (each retile persists it), so switching is
    /// just a change of active index followed by a retile, which pulls `target`
    /// out of its parked slot.
    fn switch_workspace(&mut self, mon: isize, target: u32) {
        if self.workspaces.active(mon) == target {
            return;
        }
        // The scratchpad covers the middle of the screen; switching desktops
        // pulls it up rather than leaving it floating over the new one.
        if self.workspaces.scratchpad_visible(mon) {
            self.workspaces.toggle_scratchpad(mon);
        }
        self.workspaces.set_active(mon, target);
        self.retile_monitor(mon);
        self.focus_first_visible(mon);
        self.apply_border_focus();
    }

    fn ensure_tree(&mut self, mon: isize) {
        let ws = self.workspaces.active(mon);
        // Only the active workspace's own windows. The scratchpad has its own
        // tree and must never leak into this one: if it did, the active layout
        // would treat the scratchpad window as a tile, and the retile that runs
        // after the drop-down would overwrite its centred rectangle.
        let desired: Vec<usize> = self
            .windows
            .values()
            .filter(|w| {
                w.monitor == mon
                    && !w.floating
                    // A fullscreen window leaves the tree while it is fullscreen,
                    // so the remaining windows tile the space it vacated.
                    && !self.workspaces.is_fullscreen(w.id)
                    && self.workspaces.workspace_of(w.id) == ws
            })
            .map(|w| w.id)
            .collect();
        let focus = self.frontmost_on(mon);
        // Reconcile the active workspace's own tree; the other workspaces are
        // parked and must not be touched.
        let mut tree = self.forests.entry(mon).or_default().deactivate(ws);
        for id in &desired {
            if !tree.contains(*id) {
                tree.insert(*id, focus.filter(|f| tree.contains(*f)));
            }
        }
        for id in tree.leaf_ids() {
            if !desired.contains(&id) {
                tree.remove(id);
            }
        }
        self.forests.entry(mon).or_default().activate(&tree, ws);
    }

    fn frontmost_on(&self, mon: isize) -> Option<usize> {
        let fg = win32::foreground();
        self.id_of(fg)
            .filter(|id| self.windows.get(id).map(|w| w.monitor) == Some(mon))
    }

    fn visible_ids(&self, mon: isize) -> Vec<usize> {
        let ws = self.workspaces.active(mon);
        let scratch = self.workspaces.scratchpad_visible(mon);
        self.windows
            .values()
            .filter(|w| {
                w.monitor == mon
                    && (self.workspaces.workspace_of(w.id) == ws
                        || (scratch && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE))
            })
            .map(|w| w.id)
            .collect()
    }

    fn work_area(&self, mon: isize) -> Rect {
        self.monitors
            .iter()
            .find(|m| m.handle == mon)
            .map(|m| m.work)
            .unwrap_or(Rect::new(0, 0, 1920, 1080))
    }

    /// Full monitor bounds, including the taskbar. Used for true fullscreen.
    fn monitor_rect(&self, mon: isize) -> Rect {
        self.monitors
            .iter()
            .find(|m| m.handle == mon)
            .map(|m| m.rect)
            .unwrap_or(Rect::new(0, 0, 1920, 1080))
    }

    /// Recompute and apply geometry for one monitor.
    pub fn retile_monitor(&mut self, mon: isize) {
        if !self.tiling_enabled {
            // Nothing will reposition the tiles, so a stale accent would sit
            // over whatever moved or closed underneath it.
            self.overlays.hide_all();
            return;
        }
        self.ensure_tree(mon);

        // Refresh `placed` for this monitor only: a stale rectangle would send
        // the geometric neighbour search to the wrong side of the screen.
        let ids_on_mon: Vec<usize> = self
            .windows
            .values()
            .filter(|w| w.monitor == mon)
            .map(|w| w.id)
            .collect();

        // Un-maximise anything this retile is about to place, so the geometry it
        // is given is the geometry it keeps.
        //
        // Keyed off workspace membership rather than `is_visible`: a window can
        // be maximised while it is hidden on another workspace, and
        // `unmaximize` calls `ShowWindow(SW_RESTORE)`, which *shows* the window.
        // Touching a parked window here would drag it onto the current desktop.
        let active = self.workspaces.active(mon);
        let scratch_down = self.workspaces.scratchpad_visible(mon);
        let to_normalize: Vec<usize> = ids_on_mon
            .iter()
            .copied()
            .filter(|id| {
                self.windows.get(id).is_some_and(|w| {
                    if win32::is_minimized(w.hwnd) || self.workspaces.is_floating(*id) {
                        return false;
                    }
                    // The scratchpad window is only in the tree while it is down.
                    let on = self.workspaces.workspace_of(*id);
                    if on == SCRATCHPAD_WORKSPACE {
                        return scratch_down;
                    }
                    on == active
                })
            })
            .collect();
        for id in &to_normalize {
            self.normalize(*id);
        }
        for id in ids_on_mon {
            self.placed.remove(&id);
        }

        // The scratchpad drops over the middle of the work area, on top of the
        // layout, rather than taking part in it: it has its own tree, so
        // bringing it down and pulling it up never disturbs what is underneath.
        if scratch_down {
            self.layout_scratchpad(mon);
        }

        let (go, gi) = (self.cfg.gaps_outer, self.cfg.gaps_inner);
        let kind = self.layout_of(mon);
        let ratio = self.master_ratio_for(mon, active);
        let masters = self.cfg.master_count;

        let mut forest = self.forests.get(&mon).cloned().unwrap_or_default();
        let tree = forest.deactivate(active);
        let visible = self.visible_ids(mon);

        // Minimized windows stay in the tree so their slot is not lost, but they
        // are pruned from the copy we lay out: the windows you can see reflow to
        // fill what the minimized one vacated, and restoring it later puts it
        // back in exactly the slot it had. Removing it from the tree instead
        // would make a restore splice it in as a new split, permanently
        // reshaping the layout.
        let mut layout_tree = tree.clone();
        for id in tree.leaf_ids() {
            if self
                .windows
                .get(&id)
                .is_some_and(|w| win32::is_minimized(w.hwnd))
            {
                layout_tree.remove(id);
            }
        }

        let tiled: Vec<usize> = visible
            .iter()
            .copied()
            .filter(|id| !self.workspaces.is_floating(*id) && !self.workspaces.is_fullscreen(*id))
            .collect();
        let order: Vec<usize> = layout_tree
            .leaf_ids()
            .into_iter()
            .filter(|id| tiled.contains(id))
            .collect();

        let area = self.work_area(mon);
        let rects = layout::compute(kind, &layout_tree, &order, area, ratio, masters, go, gi);

        let mut moves: Vec<(HWND, Rect)> = Vec::new();
        let mut new_placed: Vec<(usize, Rect)> = Vec::new();
        for (id, r) in rects {
            if let Some(hwnd) = self.hwnd_of(id) {
                moves.push((hwnd, r));
                new_placed.push((id, r));
            }
        }
        // Stacking puts every window at the same rect in list order, and the
        // last move wins the z-order, so raise the focused window last.
        if kind == LayoutKind::Stacking {
            let focused = win32::foreground();
            if let Some(pos) = moves.iter().position(|(h, _)| *h == focused) {
                let entry = moves.remove(pos);
                moves.push(entry);
            }
        }
        for (id, r) in new_placed {
            self.placed.insert(id, r);
        }
        self.animate_into_tween(&moves);

        self.apply_visibility(mon);
        self.persist_tree(mon, active, &tree);
        // Every change to the layout funnels through a retile, so this is the one
        // place the status document needs to be republished.
        self.publish_status();
        // The focused tile's accent may have moved with the layout; the retile
        // repaints it at its new rect rather than easing a stale one.
        self.apply_border_focus();
    }

    /// Write the status document if the user asked for one. Failures are logged
    /// once per write; a dashboard is a convenience, not a reason to stop tiling.
    ///
    /// This also refreshes the control pipe's snapshot, so `hyprwinctl status`
    /// answers from the same facts the file holds without waking the main loop.
    /// The snapshot is refreshed whether or not the file is written, because the
    /// control interface does not depend on `status_file`.
    pub fn publish_status(&self) {
        let doc = self.status_document();
        if self.cfg.status_file {
            if let Err(e) = crate::status::write(&self.cfg.status_path(), &doc) {
                crate::log_warn!("[hyprwin] could not write status file: {e}");
            }
        }
        if let Some(control) = crate::ipc::shared() {
            if let Ok(mut c) = control.lock() {
                c.status = doc.to_json();
                c.monitors = self.monitors_text();
            }
        }
    }

    /// Publish a final document that marks the manager as stopped, so a skin
    /// does not keep showing the last active workspace after HyprWin exits.
    pub fn publish_stopped(&self) {
        if !self.cfg.status_file {
            return;
        }
        let mut doc = self.status_document();
        doc.running = false;
        doc.tiling_enabled = false;
        let _ = crate::status::write(&self.cfg.status_path(), &doc);
    }

    /// Save the active workspace's tree so its layout survives a switch away.
    fn persist_tree(&mut self, mon: isize, active: u32, tree: &DwindleTree) {
        let forest = self.forests.entry(mon).or_default();
        forest.activate(tree, active);
    }

    /// Show exactly the windows that belong to the active workspace, hide the rest.
    ///
    /// This is the only place visibility is decided. Tiled and floating windows
    /// both go through it, so a window hidden on an earlier switch always comes
    /// back when its workspace is shown again.
    fn apply_visibility(&mut self, mon: isize) {
        let active = self.workspaces.active(mon);
        let scratch = self.workspaces.scratchpad_visible(mon);
        let plan: Vec<(HWND, bool)> = self
            .windows
            .values()
            .filter(|w| w.monitor == mon)
            .filter_map(|w| {
                // Never touch a window the user minimized: calling show() would
                // yank it back up, and hide() would fight the taskbar.
                if win32::is_minimized(w.hwnd) {
                    return None;
                }
                let on = self.workspaces.workspace_of(w.id);
                let should_show = on == active || (scratch && on == SCRATCHPAD_WORKSPACE);
                let visible = win32::is_visible(w.hwnd);
                if should_show == visible {
                    None
                } else {
                    Some((w.hwnd, should_show))
                }
            })
            .collect();
        for (hwnd, should_show) in plan {
            if should_show {
                win32::show(hwnd);
            } else {
                win32::hide(hwnd);
            }
        }
    }

    /// Place the scratchpad windows over the middle of the work area.
    ///
    /// The tree is a plain dwindle tree of the scratchpad windows and is laid
    /// out against this centred rectangle; the result feeds `placed`, so the
    /// scratchpad windows are also reachable by geometric focus. They stay
    /// tiled, not floating: `retile_monitor` is the only writer of geometry, so
    /// leaving them in its tree is what lets any later retile re-assert the
    /// drop-down rectangle instead of letting a nudge stick.
    fn layout_scratchpad(&mut self, mon: isize) {
        let ids: Vec<usize> = self
            .windows
            .values()
            .filter(|w| {
                w.monitor == mon
                    && !w.floating
                    && !self.workspaces.is_fullscreen(w.id)
                    && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE
            })
            .map(|w| w.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        let tree = self
            .forests
            .entry(mon)
            .or_default()
            .deactivate(SCRATCHPAD_WORKSPACE);
        let order: Vec<usize> = tree
            .leaf_ids()
            .into_iter()
            .filter(|id| ids.contains(id))
            .collect();
        let area = self.work_area(mon);
        let rect = self.scratchpad_rect(area);
        let (go, gi) = (self.cfg.gaps_outer, self.cfg.gaps_inner);
        let rects = layout::compute(
            self.layout_of(mon),
            &tree,
            &order,
            rect,
            self.master_ratio_for(mon, SCRATCHPAD_WORKSPACE),
            self.cfg.master_count,
            go,
            gi,
        );
        let mut moves: Vec<(HWND, Rect)> = Vec::new();
        for (id, r) in rects {
            if let Some(hwnd) = self.hwnd_of(id) {
                moves.push((hwnd, r));
                self.placed.insert(id, r);
            }
        }
        self.animate_into_tween(&moves);
        // The scratchpad window can hold focus while it is down; its centred
        // rect lands in `placed` here and not in `retile_monitor`.
        self.apply_border_focus();
    }

    /// The centred rectangle the scratchpad occupies inside the work area.
    fn scratchpad_rect(&self, area: Rect) -> Rect {
        let w = ((area.w as f32) * SCRATCHPAD_FRACTION).round() as i32;
        let h = ((area.h as f32) * SCRATCHPAD_FRACTION).round() as i32;
        let w = w.clamp(1, area.w);
        let h = h.clamp(1, area.h);
        Rect::new(area.x + (area.w - w) / 2, area.y + (area.h - h) / 2, w, h)
    }

    /// Drop the scratchpad's window over everything.
    ///
    /// It is deliberately *not* marked floating. The scratchpad window is tiled
    /// like any other window — just against a centred rectangle — so that
    /// `retile_monitor` remains the single authority on its geometry. Floating
    /// it would take it out of the tree and out of every retile, which is how a
    /// stray drag or a workspace switch used to leave it stuck at whatever size
    /// it happened to be.
    fn focus_scratchpad(&mut self, mon: isize) {
        let Some(id) = self.scratchpad_focus(mon) else {
            return;
        };
        let Some(hwnd) = self.hwnd_of(id) else {
            return;
        };
        // Remember the window's own shape so pulling the scratchpad up hands it
        // back rather than leaving it at the drop-down size.
        let saved = win32::rect_of(hwnd).unwrap_or(Rect::new(0, 0, 800, 600));
        self.remember_scratch_shape(id, saved);
        if let Some(w) = self.windows.get_mut(&id) {
            w.floating = false;
        }
        self.workspaces.set_floating(id, false);
        win32::unmaximize(hwnd);
        win32::set_foreground(hwnd);
    }

    /// Remember a scratchpad window's own shape, but never overwrite a save that
    /// already exists.
    ///
    /// A re-drop that follows a workspace switch happens while the window still
    /// sits at the drop-down rect, because the switch pulls the pad up by
    /// flipping the visibility flag rather than going through
    /// `unfocus_scratchpad`. Capturing the shape unconditionally would record the
    /// drop-down rect as the window's own geometry, and `reveal_all` would hand
    /// that back on exit.
    fn remember_scratch_shape(&mut self, id: usize, rect: Rect) {
        self.scratch_saves.entry(id).or_insert(rect);
    }

    /// Pull the scratchpad window back up: restore its own shape and re-tile it.
    fn unfocus_scratchpad(&mut self, mon: isize) {
        let Some(id) = self.scratchpad_focus(mon) else {
            return;
        };
        let Some(hwnd) = self.hwnd_of(id) else {
            return;
        };
        if let Some(w) = self.windows.get_mut(&id) {
            w.floating = false;
        }
        self.workspaces.set_floating(id, false);
        // The retile that follows hides it, so only the window's own shape has
        // to be put back here.
        if let Some(r) = self.scratch_saves.remove(&id) {
            win32::set_pos(hwnd, r);
        }
    }

    /// The scratchpad window to show: an explicit tag wins, then whatever is on
    /// the scratchpad workspace.
    fn scratchpad_focus(&self, mon: isize) -> Option<usize> {
        self.windows
            .values()
            .filter(|w| {
                w.monitor == mon && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE
            })
            .max_by_key(|w| win32::is_maximized(w.hwnd))
            .map(|w| w.id)
    }

    /// Show every window HyprWin manages, whatever workspace it sits on, and
    /// hand back the geometry it had before we tiled it.
    ///
    /// `apply_visibility` is the only writer of visibility, and this is its
    /// inverse for shutdown: HyprWin hides the windows on workspaces you are not
    /// looking at, and if the process simply exits they would stay hidden with no
    /// tray icon left to bring them back. Windows that are minimized are left
    /// alone for the same reason `apply_visibility` skips them.
    ///
    /// It also un-maximizes, because a window that was maximized while managed
    /// is otherwise left zoomed over the whole monitor: restoring its rectangle
    /// alone does not clear the maximized state. A window that was *already*
    /// maximized when we adopted it goes back to maximized instead, the way we
    /// found it.
    pub fn reveal_all(&mut self) {
        self.release_all();
        // Never leave a floating accent behind once the tiles go back.
        self.overlays.hide_all();
    }

    /// Hand every managed window back: its saved geometry, its maximized state,
    /// and its visibility. Shared by shutdown (`reveal_all`) and by turning
    /// tiling off.
    ///
    /// Un-maximizing is *unconditional*, floating windows included. `SW_RESTORE`
    /// is the only call that clears the zoomed flag, so skipping it for a window
    /// the user maximized leaves it at its old rectangle *still zoomed* — the
    /// half-restored state this whole path exists to avoid.
    fn release_all(&self) {
        for w in self.windows.values() {
            self.release_window(w);
        }
    }

    /// Release one window, or return false for a minimized one.
    ///
    /// Minimized windows are left alone for the same reason `apply_visibility`
    /// skips them: touching one would yank it back onto the screen.
    fn release_window(&self, w: &ManagedWindow) -> bool {
        if win32::is_minimized(w.hwnd) {
            return false;
        }
        win32::clear_border(w.hwnd);
        // Un-maximize *before* restoring geometry, and never after.
        //
        // A window maximized while we managed it is still zoomed, and
        // `unmaximize` restores it to the size Windows remembers — not to the
        // geometry we saved. So placing first would only be undone by the
        // restore that followed it, and un-maximizing first makes the placement
        // afterwards the last word.
        win32::unmaximize(w.hwnd);
        // A scratchpad window was floated and blown up; hand back the size it
        // had, which `saved_rects` does not know about.
        let saved = self
            .scratch_saves
            .get(&w.id)
            .or_else(|| self.saved_rects.get(&w.id));
        if let Some(r) = saved {
            win32::set_pos(w.hwnd, *r);
        }
        // A window that arrived maximized has its maximized rectangle in
        // `saved_rects`, so putting the flag back is what completes the restore:
        // without it the window sits at the maximized size with no maximized
        // state, which is the half-restored look from the other side. The
        // rectangle above is ignored for a maximized window, so this has to come
        // last — but only for windows that arrived this way, never for one
        // maximized under our management, which must stay un-maximized.
        if w.restore_maximized {
            win32::maximize(w.hwnd);
        }
        if !win32::is_visible(w.hwnd) {
            win32::show(w.hwnd);
        }
        true
    }

    /// Raise the topmost window of the active workspace into the foreground.
    ///
    /// Without this, the focused window after a switch is a window the user
    /// cannot see: `Win32::hide` does not hand focus on, so the keyboard would
    /// still address the desktop you just left.
    fn focus_first_visible(&mut self, mon: isize) {
        let active = self.workspaces.active(mon);
        let tree = self
            .forests
            .get(&mon)
            .cloned()
            .unwrap_or_default()
            .deactivate(active);
        let target = tree.leaf_ids().into_iter().next().or_else(|| {
            self.windows
                .values()
                .filter(|w| w.monitor == mon && self.workspaces.workspace_of(w.id) == active)
                .map(|w| w.id)
                .next()
        });
        if let Some(id) = target {
            if let Some(hwnd) = self.hwnd_of(id) {
                win32::set_foreground(hwnd);
            }
        }
    }

    pub fn retile_all(&mut self) {
        for m in self.monitors.clone() {
            self.retile_monitor(m.handle);
        }
    }

    /// Reassign all windows to the monitor they physically occupy, then retile.
    /// Called when the display configuration changes.
    pub fn refresh_monitors(&mut self) {
        let new = monitor::enumerate();
        self.monitors = new;
        let mons: Vec<isize> = self.monitors.iter().map(|m| m.handle).collect();
        let stale: Vec<isize> = self
            .workspaces
            .known_monitors()
            .into_iter()
            .filter(|m| !mons.contains(m))
            .collect();
        for mon in stale {
            // A monitor that went away takes its active workspace, its parked
            // trees and its scratchpad state with it, or the memory would grow
            // on every hot-plug and the status output would report ghosts.
            self.workspaces.forget_monitor(mon);
            self.forests.remove(&mon);
        }
        let pairs: Vec<(usize, isize)> = self
            .windows
            .values()
            .map(|w| (w.id, self.monitor_for(w.hwnd)))
            .collect();
        for (id, mon) in pairs {
            if let Some(w) = self.windows.get_mut(&id) {
                w.monitor = if mons.contains(&mon) { mon } else { 0 };
            }
        }
        self.retile_all();
    }
}

/// Choose the monitor a windowless command should act on.
///
/// Focus wins when there is any, because that is what the hotkey path has
/// always done. With no focused window — the empty-workspace case — the cursor
/// decides, and only then does it fall back to the primary. A cursor on a
/// display we do not manage is ignored rather than trusted.
fn pick_monitor(focused: Option<isize>, cursor: Option<isize>, known: &[isize]) -> Option<isize> {
    if let Some(mon) = focused {
        return Some(mon);
    }
    cursor
        .filter(|h| known.contains(h))
        .or_else(|| known.first().copied())
}

fn rule_matches(rule: &Rule, exe: &str, class: &str, title: &str) -> bool {
    let check = |pat: &Option<String>, value: &str| -> bool {
        match pat {
            None => true,
            Some(p) => {
                !p.is_empty() && value.to_ascii_lowercase().contains(&p.to_ascii_lowercase())
            }
        }
    };
    check(&rule.exe, exe) && check(&rule.class, class) && check(&rule.title, title)
}

/// Apply a float rule's `WxH` size limit.
///
/// A float rule with no `arg` floats unconditionally (the explicit
/// "Taskmgr.exe floats" case). A rule with an `arg` is a *size threshold*: it
/// floats only windows smaller than that, so one rule can float every tool
/// palette on the desktop without touching normal windows.
fn rule_size_allows(rule: &Rule, size: Option<(i32, i32)>) -> bool {
    match rule.arg.as_deref().map(str::trim) {
        None | Some("") => true,
        Some(arg) => match (size_limit(arg), size) {
            (Some((lw, lh)), Some((w, h))) => w < lw && h < lh,
            // No size to compare (a window we cannot measure) means no match.
            _ => false,
        },
    }
}

/// A monitor named by index (`"1"`..`"N"`, primary-first) or `"primary"`.
fn parse_monitor(arg: &str, monitors: &[Monitor]) -> Option<isize> {
    match arg.trim().to_ascii_lowercase().as_str() {
        "primary" | "main" => monitors.iter().find(|m| m.primary).map(|m| m.handle),
        n => n
            .parse::<usize>()
            .ok()
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| monitors.get(i))
            .map(|m| m.handle),
    }
}

fn layout_name(kind: LayoutKind) -> &'static str {
    kind.name()
}

/// Blend each destination overlay frame towards its source frame by `t`.
///
/// Both lists are built in `windows` order, which is stable between calls, so
/// index `i` is the same window in each. A caller must only cross-fade lists of
/// equal length; a change in length means the layout itself moved and the fade
/// is not meaningful.
fn blend_frames(
    from: &[crate::overlay::Frame],
    to: &[crate::overlay::Frame],
    t: f32,
) -> Vec<crate::overlay::Frame> {
    to.iter()
        .zip(from.iter())
        .map(|(d, f)| crate::overlay::Frame {
            color_a: crate::overlay::blend_pixel(f.color_a, d.color_a, t),
            color_b: crate::overlay::blend_pixel(f.color_b, d.color_b, t),
            ..*d
        })
        .collect()
}

/// Each monitor's starting layout: `layouts["N"]` if present, else the default.
///
/// Monitor "N" is the `monitors` index, 1-based and primary first, so the key
/// matches the number a `monitor` rule and `status.json` already use.
fn seed_layouts(cfg: &Config, monitors: &[Monitor]) -> HashMap<isize, LayoutKind> {
    let mut out = HashMap::new();
    for (i, m) in monitors.iter().enumerate() {
        let key = (i + 1).to_string();
        let kind = cfg.layouts.get(&key).copied().unwrap_or(cfg.default_layout);
        out.insert(m.handle, kind);
    }
    out
}

/// Convenience for `Direction` parsing used by tests.
#[cfg(test)]
use crate::config::Direction as WmDirection;

#[cfg(test)]
mod tests {
    use super::*;

    /// A re-drop that follows a workspace switch happens while the window still
    /// sits at the drop-down rect, so the save must survive it. Without this,
    /// `reveal_all` restores the drop-down geometry on exit.
    #[test]
    fn a_scratchpad_reshape_never_overwrites_the_saved_shape() {
        let mut wm = Wm::new(Config::default());
        let own = Rect::new(100, 120, 900, 700);
        let dropped = Rect::new(-1621, 331, 1322, 700);

        wm.remember_scratch_shape(1, own);
        wm.remember_scratch_shape(1, dropped);
        assert_eq!(
            wm.scratch_saves.get(&1).copied(),
            Some(own),
            "the window's own shape must win, not the drop-down rect"
        );

        // A different window is remembered independently.
        wm.remember_scratch_shape(2, dropped);
        assert_eq!(wm.scratch_saves.get(&2).copied(), Some(dropped));
    }

    #[test]
    fn rule_matching_is_case_insensitive_and_substring_based() {
        let r = Rule {
            exe: Some("notepad.exe".to_string()),
            action: "float".to_string(),
            ..Default::default()
        };
        assert!(rule_matches(
            &r,
            "C:\\Windows\\Notepad.exe",
            "Notepad",
            "Untitled"
        ));
        assert!(!rule_matches(
            &r,
            "explorer.exe",
            "CabinetWClass",
            "Desktop"
        ));
    }

    #[test]
    fn an_empty_rule_matches_everything() {
        let r = Rule::default();
        assert!(rule_matches(&r, "a.exe", "Cls", "Title"));
    }

    #[test]
    fn rules_require_all_specified_fields_to_match() {
        let r = Rule {
            exe: Some("chrome.exe".to_string()),
            title: Some("DevTools".to_string()),
            action: "float".to_string(),
            ..Default::default()
        };
        assert!(rule_matches(
            &r,
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "DevTools - Console"
        ));
        assert!(!rule_matches(
            &r,
            "chrome.exe",
            "Chrome_WidgetWin_1",
            "Gmail"
        ));
    }

    #[test]
    fn wm_direction_alias_is_reachable() {
        assert_eq!(WmDirection::parse("left"), Some(Direction::Left));
    }

    /// A monitor showing an empty workspace has no focused window, so a
    /// workspace switch used to fall through to the primary monitor and could
    /// never reach any other display. The cursor now decides in that case.
    #[test]
    fn a_windowless_command_targets_the_monitor_under_the_cursor() {
        let known = [10, 20];
        assert_eq!(pick_monitor(None, Some(20), &known), Some(20));
        // Cursor on a display we do not manage, or no cursor reading at all:
        // the primary is the last resort rather than nothing happening.
        assert_eq!(pick_monitor(None, Some(99), &known), Some(10));
        assert_eq!(pick_monitor(None, None, &known), Some(10));
        assert_eq!(pick_monitor(None, None, &[]), None);
    }

    /// Focus still outranks the cursor: aiming a hotkey at the window you are
    /// on must not be redirected because the pointer happens to sit elsewhere.
    #[test]
    fn a_focused_window_outranks_the_cursor() {
        assert_eq!(pick_monitor(Some(20), Some(10), &[10, 20]), Some(20));
    }

    #[test]
    fn a_master_ratio_override_is_remembered_per_workspace() {
        let mut wm = Wm::new(Config::default());
        let mon = 0;
        // Workspace 1 has no override, so the global default applies.
        assert_eq!(wm.master_ratio_for(mon, 1), wm.cfg.master_ratio);
        // Setting one applies to that workspace alone.
        wm.set_master_ratio(mon, 1, 0.75);
        assert_eq!(wm.master_ratio_for(mon, 1), 0.75);
        assert_eq!(wm.master_ratio_for(mon, 2), wm.cfg.master_ratio);
        // The clamp keeps a stray drag from collapsing the master column.
        wm.set_master_ratio(mon, 1, 0.01);
        assert_eq!(wm.master_ratio_for(mon, 1), 0.1);
        wm.set_master_ratio(mon, 1, 5.0);
        assert_eq!(wm.master_ratio_for(mon, 1), 0.9);
    }

    #[test]
    fn the_monitor_under_a_point_is_the_one_whose_bounds_contain_it() {
        use crate::monitor::Monitor;
        let mut wm = Wm::new(Config::default());
        wm.monitors = vec![
            Monitor {
                handle: 10,
                rect: Rect::new(0, 0, 1920, 1080),
                work: Rect::new(0, 0, 1920, 1040),
                primary: true,
            },
            Monitor {
                handle: 20,
                rect: Rect::new(-1920, 0, 1920, 1080),
                work: Rect::new(-1920, 0, 1920, 1040),
                primary: false,
            },
        ];
        assert_eq!(wm.monitor_at_point(100, 100), Some(10));
        // The right edge is exclusive, matching how the rectangles tile.
        assert_eq!(wm.monitor_at_point(1919, 100), Some(10));
        assert_eq!(wm.monitor_at_point(-1, 100), Some(20));
        // A point in no monitor's bounds (a gap between differently-shaped
        // displays) yields nothing rather than a wrong guess.
        assert_eq!(wm.monitor_at_point(5000, 5000), None);
    }

    #[test]
    fn the_smallest_placed_rect_under_a_point_wins_a_swap_target() {
        let mut wm = Wm::new(Config::default());
        // Window 2 fills the left half, window 3 a small pane on top of it.
        wm.placed.insert(2, Rect::new(0, 0, 960, 1080));
        wm.placed.insert(3, Rect::new(20, 20, 200, 200));
        // A point inside both picks the smaller/topmost window.
        assert_eq!(wm.neighbor_at(1, (100, 100)), Some(3));
        // A point inside only the big one picks that.
        assert_eq!(wm.neighbor_at(1, (800, 800)), Some(2));
        // A point inside neither has no target.
        assert_eq!(wm.neighbor_at(1, (2000, 2000)), None);
        // The dragged window is never its own target.
        assert_eq!(wm.neighbor_at(3, (100, 100)), Some(2));
    }
}
