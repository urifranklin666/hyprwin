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
use windows::Win32::Foundation::HWND;

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
}

pub struct Wm {
    pub cfg: Config,
    pub layout_kind: LayoutKind,
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
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

impl Wm {
    pub fn new(cfg: Config) -> Self {
        let layout_kind = cfg.default_layout;
        let workspaces = WorkspaceState::new(cfg.workspaces);
        Wm {
            cfg,
            layout_kind,
            workspaces,
            monitors: monitor::enumerate(),
            windows: HashMap::new(),
            by_hwnd: HashMap::new(),
            forests: HashMap::new(),
            placed: HashMap::new(),
            saved_rects: HashMap::new(),
            scratch_saves: HashMap::new(),
            next_id: 1,
            tiling_enabled: true,
            overlays: Overlays::default(),
        }
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
                .map(|m| (r.overlap_h(&m.work) as i64 * r.overlap_v(&m.work) as i64, m.handle))
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
        let mon = self
            .target_monitor(hwnd, &exe, &class, &title)
            .unwrap_or(0);
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
        // Remember where the window was before we take it over. `reveal_all`
        // replays these on exit so quitting does not leave the desktop rearranged.
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
        // first window it finds, so re-assert theirs afterwards.
        win32::set_foreground(hwnd);
        self.apply_border_focus();
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
                        || (scratch
                            && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE))
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
        let kind = self.layout_kind;
        let ratio = self.cfg.master_ratio;

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
            .filter(|id| {
                !self.workspaces.is_floating(*id) && !self.workspaces.is_fullscreen(*id)
            })
            .collect();
        let order: Vec<usize> = layout_tree
            .leaf_ids()
            .into_iter()
            .filter(|id| tiled.contains(id))
            .collect();

        let area = self.work_area(mon);
        let rects = layout::compute(kind, &layout_tree, &order, area, ratio, go, gi);

        let mut moves: Vec<(HWND, Rect)> = Vec::new();
        let mut new_placed: Vec<(usize, Rect)> = Vec::new();
        for (id, r) in rects {
            if let Some(hwnd) = self.hwnd_of(id) {
                moves.push((hwnd, r));
                new_placed.push((id, r));
            }
        }
        for (id, r) in new_placed {
            self.placed.insert(id, r);
        }
        win32::animate_batch(&moves, self.cfg.animation_ms, &self.cfg.animation_easing);

        self.apply_visibility(mon);
        self.persist_tree(mon, active, &tree);
        // Every change to the layout funnels through a retile, so this is the one
        // place the status document needs to be republished.
        self.publish_status();
        // The focused tile's accent may have moved with the layout, and this is
        // reached after the animation has finished, so its new rect is in force.
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
                eprintln!("[hyprwin] could not write status file: {e}");
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
        let rects = layout::compute(self.layout_kind, &tree, &order, rect, self.cfg.master_ratio, go, gi);
        let mut moves: Vec<(HWND, Rect)> = Vec::new();
        for (id, r) in rects {
            if let Some(hwnd) = self.hwnd_of(id) {
                moves.push((hwnd, r));
                self.placed.insert(id, r);
            }
        }
        win32::animate_batch(&moves, self.cfg.animation_ms, &self.cfg.animation_easing);
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
        if !self.scratch_saves.contains_key(&id) {
            self.scratch_saves.insert(id, rect);
        }
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
            .filter(|w| w.monitor == mon && self.workspaces.workspace_of(w.id) == SCRATCHPAD_WORKSPACE)
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
    /// alone does not clear the maximized state.
    pub fn reveal_all(&mut self) {
        for w in self.windows.values() {
            if win32::is_minimized(w.hwnd) {
                continue;
            }
            win32::clear_border(w.hwnd);
            // Un-maximize *before* restoring geometry, and never after.
            //
            // A window maximized while we managed it is still zoomed, and
            // `unmaximize` restores it to the size Windows remembers — not to
            // the geometry we saved. For a window that arrived already maximized,
            // `saved_rects` holds the *maximized* rect, because it is captured at
            // adoption before the first un-maximize. So placing first would only
            // be undone by the restore that followed it. Un-maximizing first also
            // means the placement afterwards is the last word, which is what the
            // caller wants.
            win32::unmaximize(w.hwnd);
            // A scratchpad window was floated and blown up; hand back the size
            // it had, which `saved_rects` does not know about.
            let saved = self
                .scratch_saves
                .get(&w.id)
                .or_else(|| self.saved_rects.get(&w.id));
            if let Some(r) = saved {
                win32::set_pos(w.hwnd, *r);
            }
            if !win32::is_visible(w.hwnd) {
                win32::show(w.hwnd);
            }
        }
        // Never leave a floating accent behind once the tiles go back.
        self.overlays.hide_all();
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
        let target = tree
            .leaf_ids()
            .into_iter()
            .next()
            .or_else(|| {
                self.windows
                    .values()
                    .filter(|w| {
                        w.monitor == mon && self.workspaces.workspace_of(w.id) == active
                    })
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

    // ---------------------------------------------------------------- status

    /// Snapshot the state an external dashboard needs.
    ///
    /// The document is rebuilt from scratch every time rather than patched, so a
    /// workspace that empties out or a window that closes cannot leave stale
    /// counts behind for a skin to display.
    pub fn status_document(&self) -> Status {
        let focused = win32::foreground();
        let focused_id = self.id_of(focused);
        let layout = layout_name(self.layout_kind).to_string();
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
                    layout: layout.clone(),
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
                title: focused_id.map(|_| win32::title(focused)).unwrap_or_default(),
                exe: focused_id.map(|_| win32::exe_name(focused)).unwrap_or_default(),
                monitor: focused_id
                    .and_then(|id| self.windows.get(&id).map(|w| w.monitor))
                    .and_then(|mon| self.monitors.iter().position(|m| m.handle == mon))
                    .map(|i| i + 1)
                    .unwrap_or(0),
            },
            monitors,
        }
    }

    // ---------------------------------------------------------------- borders

    pub fn apply_border_focus(&mut self) {
        if !self.cfg.show_border {
            self.overlays.hide_all();
            return;
        }
        let active = win32::foreground();
        let active_color = crate::config::parse_color(&self.cfg.border_color_active);
        let inactive_color = crate::config::parse_color(&self.cfg.border_color_inactive);
        for w in self.windows.values() {
            if w.floating {
                continue;
            }
            let color = if w.hwnd == active {
                active_color
            } else {
                inactive_color
            };
            win32::set_border(w.hwnd, color);
        }

        // A focused *tiled* window gets the thicker accent from the overlay. A
        // floating or fullscreen window keeps only its 1px DWM line: the overlay
        // frame would not match either's real geometry.
        let frame = self
            .id_of(active)
            .filter(|id| {
                self.windows
                    .get(id)
                    .is_some_and(|w| !w.floating && !self.workspaces.is_fullscreen(*id))
            })
            .and_then(|id| self.placed.get(&id).map(|r| (id, *r)));
        let width = self.cfg.border_width;
        match frame {
            Some((id, rect)) => {
                let mon = self.windows[&id].monitor;
                let color =
                    crate::overlay::pixel(crate::config::parse_rgba(&self.cfg.border_color_active));
                self.overlays
                    .redraw(&self.monitors.clone(), Some((mon, rect)), width, color);
            }
            None => self.overlays.redraw(&self.monitors.clone(), None, width, 0),
        }
    }

    // ---------------------------------------------------------------- commands

    pub fn command(&mut self, action: &str, arg: Option<&str>) {
        match action {
            "focus" => self.cmd_focus(arg, false),
            "move" => self.cmd_focus(arg, true),
            "resize" => self.cmd_resize(arg),
            "togglefloating" => self.cmd_toggle_floating(),
            "fullscreen" => self.cmd_fullscreen(),
            "togglelayout" => self.cmd_toggle_layout(),
            "workspace" => self.cmd_workspace(arg),
            "movetoworkspace" => self.cmd_move_to_workspace(arg),
            "movetomonitor" => self.cmd_move_to_monitor(arg),
            "scratchpad" => self.cmd_scratchpad(),
            "movetoscratchpad" => self.cmd_move_to_scratchpad(),
            "toggletiling" => {
                self.toggle_tiling();
            }
            "close" => self.cmd_close(),
            "reload" => self.cmd_reload(),
            other => eprintln!("[hyprwin] unknown action '{other}'"),
        }
    }

    /// Turn tiling on or off, retiling when it comes back on.
    ///
    /// Lives here rather than in the tray handler so the pipe and the tray share
    /// one implementation and one printed line.
    pub fn toggle_tiling(&mut self) -> bool {
        self.tiling_enabled = !self.tiling_enabled;
        println!(
            "[hyprwin] tiling {}",
            if self.tiling_enabled { "enabled" } else { "disabled" }
        );
        if self.tiling_enabled {
            self.retile_all();
        } else {
            // Disabling does not retile, so the accent would otherwise stay put
            // over tiles that are free to move from under it.
            self.overlays.hide_all();
        }
        // Disabling does not retile, so without this the published document
        // would keep claiming tiling is on until some unrelated retile.
        self.publish_status();
        self.tiling_enabled
    }

    /// The `hyprctl monitors`-style summary: one line per monitor.
    pub fn monitors_text(&self) -> String {
        let mut out = String::new();
        for (i, m) in self.monitors.iter().enumerate() {
            let ws = self.workspaces.active(m.handle);
            let count = self
                .windows
                .values()
                .filter(|w| w.monitor == m.handle && self.workspaces.workspace_of(w.id) == ws)
                .count();
            out.push_str(&format!(
                "monitor {} {} work area {}x{}+{}+{} workspace {} {} windows {}{}\n",
                i + 1,
                if m.primary { "(primary)" } else { "(secondary)" },
                m.work.w,
                m.work.h,
                m.work.x,
                m.work.y,
                ws,
                layout_name(self.layout_kind),
                count,
                if self.workspaces.scratchpad_visible(m.handle) {
                    " scratchpad"
                } else {
                    ""
                }
            ));
        }
        out.trim_end().to_string()
    }

    fn focused(&self) -> Option<usize> {
        self.id_of(win32::foreground())
    }

    /// Which monitor a command that does not carry a window should act on.
    ///
    /// The focused window's monitor is the answer whenever there is a focused
    /// window. It is not, though, always there: a monitor showing an empty
    /// workspace has nothing to focus, and keying off focus alone would then
    /// leave it with no keyboard escape at all — every hotkey would silently
    /// act on some other display. Falling back to the monitor under the cursor
    /// keeps the command pointed at the display the user is looking at.
    fn command_monitor(&self) -> Option<isize> {
        let focused = self
            .focused()
            .and_then(|id| self.windows.get(&id))
            .map(|w| w.monitor);
        let known: Vec<isize> = self.monitors.iter().map(|m| m.handle).collect();
        pick_monitor(focused, monitor::monitor_for_cursor(), &known)
    }

    fn cmd_focus(&mut self, arg: Option<&str>, swap: bool) {
        let Some(focus) = self.focused() else {
            return;
        };
        let dir = match arg.and_then(Direction::parse) {
            Some(d) => d,
            None => return,
        };
        let mon = self.windows[&focus].monitor;
        if self.workspaces.is_fullscreen(focus) && !swap {
            return;
        }
        // While the scratchpad is down, its window is part of what the user can
        // see and point at, so it joins the neighbour search. A swap across the
        // two trees would be meaningless (they are laid out independently), so
        // it only happens within one workspace.
        let rects: Vec<(usize, Rect)> = {
            let ws = self.workspaces.active(mon);
            let scratch = self.workspaces.scratchpad_visible(mon);
            self.placed
                .iter()
                .filter(|(id, _)| {
                    let on = self.workspaces.workspace_of(**id);
                    on == ws || (scratch && on == SCRATCHPAD_WORKSPACE)
                })
                .map(|(id, r)| (*id, *r))
                .collect()
        };
        let Some(target) = layout::neighbor(&rects, focus, dir) else {
            return;
        };
        let same_workspace = self.workspaces.workspace_of(focus) == self.workspaces.workspace_of(target);
        if swap && same_workspace && !self.workspaces.is_floating(target) {
            let active = self.workspaces.workspace_of(focus);
            let forest = self.forests.entry(mon).or_default();
            let mut tree = forest.deactivate(active);
            tree.swap(focus, target);
            forest.activate(&tree, active);
            self.retile_monitor(mon);
        } else {
            if let Some(hwnd) = self.hwnd_of(target) {
                win32::set_foreground(hwnd);
            }
        }
    }

    fn cmd_resize(&mut self, arg: Option<&str>) {
        let Some(focus) = self.focused() else {
            return;
        };
        let Some(dir) = arg.and_then(Direction::parse) else {
            return;
        };
        let mon = self.windows[&focus].monitor;
        let delta = match dir {
            Direction::Left | Direction::Up => -0.04,
            Direction::Right | Direction::Down => 0.04,
        };
        // Resize the split in whichever tree the window actually lives in: the
        // active workspace's, or the scratchpad's while it is down.
        let active = self.workspaces.workspace_of(focus);
        let forest = self.forests.entry(mon).or_default();
        let mut tree = forest.deactivate(active);
        tree.resize(focus, delta);
        forest.activate(&tree, active);
        self.retile_monitor(mon);
    }

    fn cmd_toggle_floating(&mut self) {
        let Some(focus) = self.focused() else {
            return;
        };
        let mon = self.windows[&focus].monitor;
        let floating = self.workspaces.toggle_floating(focus);
        if let Some(w) = self.windows.get_mut(&focus) {
            w.floating = floating;
        }
        if floating {
            if let Some(hwnd) = self.hwnd_of(focus) {
                // Keep the current size but centre the window on the work area.
                let area = self.work_area(mon);
                let cur = win32::rect_of(hwnd).unwrap_or(Rect::new(0, 0, 800, 600));
                let w = cur.w.clamp(320, area.w);
                let h = cur.h.clamp(240, area.h);
                let rect = Rect::new(
                    area.x + (area.w - w) / 2,
                    area.y + (area.h - h) / 2,
                    w,
                    h,
                );
                win32::clear_border(hwnd);
                win32::animate_batch(&[(hwnd, rect)], self.cfg.animation_ms, &self.cfg.animation_easing);
            }
        } else {
            if let Some(hwnd) = self.hwnd_of(focus) {
                if self.cfg.rounding > 0 {
                    win32::set_rounding(hwnd, self.cfg.rounding);
                }
            }
        }
        self.retile_monitor(mon);
    }

    fn cmd_fullscreen(&mut self) {
        let Some(focus) = self.focused() else {
            return;
        };
        let mon = self.windows[&focus].monitor;
        let on = self.workspaces.toggle_fullscreen(focus);
        let Some(hwnd) = self.hwnd_of(focus) else {
            return;
        };
        if on {
            let rect = self.monitor_rect(mon);
            win32::animate_batch(&[(hwnd, rect)], self.cfg.animation_ms, &self.cfg.animation_easing);
            win32::set_foreground(hwnd);
            // The window now fills the monitor, so its tile accent no longer
            // matches where it is; the filter in `apply_border_focus` clears it.
            self.apply_border_focus();
        } else {
            self.retile_monitor(mon);
        }
    }

    fn cmd_toggle_layout(&mut self) {
        self.layout_kind = match self.layout_kind {
            LayoutKind::Dwindle => LayoutKind::Master,
            LayoutKind::Master => LayoutKind::Dwindle,
        };
        self.retile_all();
    }

    fn cmd_workspace(&mut self, arg: Option<&str>) {
        let Some(arg) = arg else { return };
        let Some(mon) = self.command_monitor() else {
            return;
        };
        let target = match arg {
            "next" => self.workspaces.workspace_after(mon, 1),
            "prev" => self.workspaces.workspace_after(mon, -1),
            n => match n.parse::<u32>() {
                Ok(ws) => ws,
                Err(_) => return,
            },
        };
        self.switch_workspace(mon, target);
    }

    fn cmd_move_to_workspace(&mut self, arg: Option<&str>) {
        let Some(arg) = arg else { return };
        let Ok(ws) = arg.parse::<u32>() else { return };
        let Some(focus) = self.focused() else {
            return;
        };
        let mon = self.windows[&focus].monitor;
        self.workspaces.assign(focus, ws);
        // Follow the window, like Hyprland's movetoworkspace.
        self.switch_workspace(mon, ws);
    }

    /// Move the focused window to another monitor.
    ///
    /// Workspace numbers are per-monitor and the target's active workspace has no
    /// reason to match the source's, so the window is reassigned to whatever the
    /// target is showing. That is the only assignment that keeps it visible: any
    /// other number would hide it the moment it arrives.
    fn cmd_move_to_monitor(&mut self, arg: Option<&str>) {
        let Some(focus) = self.focused() else {
            return;
        };
        let from = self.windows[&focus].monitor;
        let Some(to) = self.monitor_step(from, arg.unwrap_or("next")) else {
            return;
        };
        if to == from {
            return;
        }
        let ws = self.workspaces.active(to);
        self.workspaces.assign(focus, ws);
        self.forests.entry(from).or_default().forget(focus);
        if let Some(w) = self.windows.get_mut(&focus) {
            w.monitor = to;
            w.floating = false;
        }
        self.workspaces.set_floating(focus, false);
        if let Some(hwnd) = self.hwnd_of(focus) {
            win32::unmaximize(hwnd);
        }
        self.retile_monitor(from);
        self.retile_monitor(to);
        if let Some(hwnd) = self.hwnd_of(focus) {
            win32::set_foreground(hwnd);
        }
        self.apply_border_focus();
    }

    /// The scratchpad toggle: drop its window over everything, or pull it away.
    ///
    /// Deliberately explicit: it never picks a window for you. `movetoscratchpad`
    /// is the way to put something on the scratchpad, so this chord always means
    /// the same thing whatever is focused. With nothing on the scratchpad it sets
    /// the flag and shows nothing, which is exactly what Hyprland does.
    fn cmd_scratchpad(&mut self) {
        let Some(mon) = self.command_monitor() else {
            return;
        };
        let down = self.workspaces.toggle_scratchpad(mon);
        if down {
            self.focus_scratchpad(mon);
        } else {
            self.unfocus_scratchpad(mon);
        }
        self.retile_monitor(mon);
        self.apply_border_focus();
    }

    /// Move the focused window onto the scratchpad and drop it down.
    fn cmd_move_to_scratchpad(&mut self) {
        let Some(focus) = self.focused() else {
            return;
        };
        let mon = self.windows[&focus].monitor;
        self.make_scratchpad(focus);
        if !self.workspaces.scratchpad_visible(mon) {
            self.workspaces.toggle_scratchpad(mon);
        }
        // Make sure the window's shape is remembered before the retile blows it
        // up, so pulling the scratchpad up can hand it back.
        if let Some(hwnd) = self.hwnd_of(focus) {
            if let Some(r) = win32::rect_of(hwnd) {
                self.remember_scratch_shape(focus, r);
            }
        }
        self.retile_monitor(mon);
        self.apply_border_focus();
    }

    /// Retag a window as the scratchpad's and put it in the scratchpad's tree.
    ///
    /// This does the tree placement rather than leaving it to `ensure_tree`,
    /// because the retile reconciles the active workspace's tree and never looks
    /// at a parked one: a window whose *assignment* changed but which is sitting
    /// in the active workspace's tree would otherwise be pruned out of that tree
    /// and never inserted anywhere.
    fn make_scratchpad(&mut self, id: usize) {
        let mon = self.windows[&id].monitor;
        self.workspaces.assign_raw(id, SCRATCHPAD_WORKSPACE);
        if let Some(w) = self.windows.get_mut(&id) {
            w.floating = false;
        }
        self.workspaces.set_floating(id, false);
        // Drop it from the active workspace's tree, then place it in the
        // scratchpad's own tree. `forget` covers every parked tree too.
        self.forests.entry(mon).or_default().forget(id);
        let forest = self.forests.entry(mon).or_default();
        let mut tree = forest.deactivate(SCRATCHPAD_WORKSPACE);
        tree.insert(id, None);
        forest.activate(&tree, SCRATCHPAD_WORKSPACE);
    }

    /// A monitor by relative position or index: "next", "prev", or "1".."N".
    fn monitor_step(&self, from: isize, arg: &str) -> Option<isize> {
        parse_monitor(arg, &self.monitors).or_else(|| {
            let handles: Vec<isize> = self.monitors.iter().map(|m| m.handle).collect();
            let i = handles.iter().position(|h| *h == from)?;
            let n = handles.len();
            let next = match arg {
                "next" => (i + 1) % n.max(1),
                "prev" => (i + n - 1) % n.max(1),
                _ => return None,
            };
            handles.get(next).copied()
        })
    }

    fn cmd_close(&mut self) {
        if let Some(focus) = self.focused() {
            if let Some(hwnd) = self.hwnd_of(focus) {
                win32::post_close(hwnd);
            }
        }
    }

    fn cmd_reload(&mut self) {
        match Config::load_or_create() {
            Ok(cfg) => {
                self.layout_kind = cfg.default_layout;
                self.cfg = cfg;
                println!("[hyprwin] config reloaded");
            }
            Err(e) => eprintln!("[hyprwin] reload failed: {e:#}"),
        }
    }

    /// Windows that were open but are now gone. Called on every hook event as a
    /// safety net for closings we never received an event for.
    pub fn prune_dead(&mut self) {
        let dead: Vec<(usize, isize)> = self
            .windows
            .values()
            .filter(|w| !win32::is_window(w.hwnd))
            .map(|w| (w.id, w.monitor))
            .collect();
        if dead.is_empty() {
            return;
        }
        let mut mons: Vec<isize> = Vec::new();
        for (id, mon) in dead {
            self.drop_window(id);
            self.forests.entry(mon).or_default().forget(id);
            mons.push(mon);
        }
        mons.sort_unstable();
        mons.dedup();
        for mon in mons {
            self.retile_monitor(mon);
        }
        self.apply_border_focus();
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
            Some(p) => !p.is_empty() && value.to_ascii_lowercase().contains(&p.to_ascii_lowercase()),
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
    match kind {
        LayoutKind::Dwindle => "dwindle",
        LayoutKind::Master => "master",
    }
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
        assert!(rule_matches(&r, "C:\\Windows\\Notepad.exe", "Notepad", "Untitled"));
        assert!(!rule_matches(&r, "explorer.exe", "CabinetWClass", "Desktop"));
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
        assert!(rule_matches(&r, "chrome.exe", "Chrome_WidgetWin_1", "DevTools - Console"));
        assert!(!rule_matches(&r, "chrome.exe", "Chrome_WidgetWin_1", "Gmail"));
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
}
