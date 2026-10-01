//! The window manager: owns all tiling state and turns commands into retiles.
//!
//! This is the only module that orchestrates layout, workspace and Win32 calls.
//! It deliberately contains no rendering or message-loop code.

use crate::config::{Config, Direction, LayoutKind, Rule};
use crate::layout::{self, DwindleTree, Forest, Rect};
use crate::monitor::{self, Monitor};
use crate::win32;
use crate::workspace::WorkspaceState;
use std::collections::HashMap;
use windows::Win32::Foundation::HWND;

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
    next_id: usize,
    pub tiling_enabled: bool,
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
            next_id: 1,
            tiling_enabled: true,
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
        let exe = win32::exe_name(hwnd);
        let class = win32::class_name(hwnd);
        let title = win32::title(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "ignore")
            .any(|r| rule_matches(r, &exe, &class, &title))
    }

    fn rule_target_workspace(&self, hwnd: HWND) -> Option<u32> {
        let exe = win32::exe_name(hwnd);
        let class = win32::class_name(hwnd);
        let title = win32::title(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "workspace")
            .find(|r| rule_matches(r, &exe, &class, &title))
            .and_then(|r| r.arg.as_ref())
            .and_then(|a| a.parse::<u32>().ok())
    }

    fn rule_floats(&self, hwnd: HWND) -> bool {
        let exe = win32::exe_name(hwnd);
        let class = win32::class_name(hwnd);
        let title = win32::title(hwnd);
        self.cfg
            .rules
            .iter()
            .filter(|r| r.action == "float")
            .any(|r| rule_matches(r, &exe, &class, &title))
    }

    /// Adopt a window. Returns its id, or None if it should not be tiled.
    pub fn manage(&mut self, hwnd: HWND) -> Option<usize> {
        if !self.tiling_enabled || self.is_managed(hwnd) {
            return None;
        }
        if !win32::is_manageable(hwnd) || self.rule_ignores(hwnd) {
            return None;
        }
        let mon = self.monitor_for(hwnd);
        let ws = self
            .rule_target_workspace(hwnd)
            .unwrap_or_else(|| self.workspaces.active(mon));
        let float = self.rule_floats(hwnd);

        let id = self.next_id;
        self.next_id += 1;
        // Remember where the window was before we take it over. `reveal_all`
        // replays these on exit so quitting does not leave the desktop rearranged.
        if let Some(r) = win32::rect_of(hwnd) {
            self.saved_rects.insert(id, r);
        }
        self.workspaces.assign(id, ws);
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

        if !float {
            // Windows on a non-active workspace stay hidden.
            if ws != self.workspaces.active(mon) {
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
    }

    /// Bring `hwnd` to the foreground. Used when the user clicks a window.
    pub fn on_external_focus(&mut self, hwnd: HWND) {
        let Some(id) = self.id_of(hwnd) else {
            return;
        };
        let mon = self.windows[&id].monitor;
        let ws = self.workspaces.workspace_of(id);
        if ws != self.workspaces.active(mon) {
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
        self.workspaces.set_active(mon, target);
        self.retile_monitor(mon);
        self.focus_first_visible(mon);
        self.apply_border_focus();
    }

    fn ensure_tree(&mut self, mon: isize) {
        let ws = self.workspaces.active(mon);
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
        self.windows
            .values()
            .filter(|w| w.monitor == mon && self.workspaces.workspace_of(w.id) == ws)
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
        for id in ids_on_mon {
            self.placed.remove(&id);
        }

        let active = self.workspaces.active(mon);
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
                let should_show = self.workspaces.workspace_of(w.id) == active;
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

    /// Show every window HyprWin manages, whatever workspace it sits on, and
    /// hand back the geometry it had before we tiled it.
    ///
    /// `apply_visibility` is the only writer of visibility, and this is its
    /// inverse for shutdown: HyprWin hides the windows on workspaces you are not
    /// looking at, and if the process simply exits they would stay hidden with no
    /// tray icon left to bring them back. Windows that are minimized are left
    /// alone for the same reason `apply_visibility` skips them.
    pub fn reveal_all(&self) {
        for w in self.windows.values() {
            if win32::is_minimized(w.hwnd) {
                continue;
            }
            win32::clear_border(w.hwnd);
            if let Some(r) = self.saved_rects.get(&w.id) {
                win32::set_pos(w.hwnd, *r);
            }
            if !win32::is_visible(w.hwnd) {
                win32::show(w.hwnd);
            }
        }
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

    // ---------------------------------------------------------------- borders

    pub fn apply_border_focus(&self) {
        if !self.cfg.show_border {
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
            "close" => self.cmd_close(),
            "reload" => self.cmd_reload(),
            other => eprintln!("[hyprwin] unknown action '{other}'"),
        }
    }

    fn focused(&self) -> Option<usize> {
        self.id_of(win32::foreground())
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
        let rects: Vec<(usize, Rect)> = {
            let ws = self.workspaces.active(mon);
            self.placed
                .iter()
                .filter(|(id, _)| self.workspaces.workspace_of(**id) == ws)
                .map(|(id, r)| (*id, *r))
                .collect()
        };
        let Some(target) = layout::neighbor(&rects, focus, dir) else {
            return;
        };
        if swap {
            let active = self.workspaces.active(mon);
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
        let active = self.workspaces.active(mon);
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
        let mon = self
            .focused()
            .map(|id| self.windows[&id].monitor)
            .or_else(|| self.monitors.first().map(|m| m.handle))
            .unwrap_or(0);
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

fn rule_matches(rule: &Rule, exe: &str, class: &str, title: &str) -> bool {
    let check = |pat: &Option<String>, value: &str| -> bool {
        match pat {
            None => true,
            Some(p) => !p.is_empty() && value.to_ascii_lowercase().contains(&p.to_ascii_lowercase()),
        }
    };
    check(&rule.exe, exe) && check(&rule.class, class) && check(&rule.title, title)
}

/// Convenience for `Direction` parsing used by tests.
#[cfg(test)]
use crate::config::Direction as WmDirection;

#[cfg(test)]
mod tests {
    use super::*;

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
}
