//! The command vocabulary: what a keybind, the tray or the control pipe can
//! ask the manager to do.
//!
//! Split out of `wm` because the dispatcher and its handlers are a flat list of
//! small, independent operations. `use super::*` reaches the private state.
#![allow(dead_code)]

use super::*;

impl Wm {
    // ---------------------------------------------------------------- commands

    pub fn command(&mut self, action: &str, arg: Option<&str>) {
        match action {
            "focus" => self.cmd_focus(arg, false),
            "move" => self.cmd_focus(arg, true),
            "resize" => self.cmd_resize(arg),
            "togglefloating" => self.cmd_toggle_floating(),
            "fullscreen" => self.cmd_fullscreen(),
            "togglelayout" => self.cmd_toggle_layout(),
            "layout" => self.cmd_layout(arg),
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
            other => crate::log_warn!("[hyprwin] unknown action '{other}'"),
        }
    }

    /// Turn tiling on or off, retiling when it comes back on.
    ///
    /// Lives here rather than in the tray handler so the pipe and the tray share
    /// one implementation and one printed line.
    pub fn toggle_tiling(&mut self) -> bool {
        self.tiling_enabled = !self.tiling_enabled;
        crate::log_info!(
            "[hyprwin] tiling {}",
            if self.tiling_enabled { "enabled" } else { "disabled" }
        );
        if self.tiling_enabled {
            self.retile_all();
        } else {
            // Turning tiling off hands every window back the way `reveal_all`
            // does on exit, so nothing is left pinned where HyprWin last put it
            // and no window the user maximized stays zoomed over the desktop.
            // The tiles keep their current rectangles; they are simply free to
            // be moved again.
            self.release_all();
            // Disabling does not retile, so the accent would otherwise stay put
            // over tiles that are free to move from under it.
            self.overlays.hide_all();
        }
        // Disabling does not retile, so without this the published document
        // would keep claiming tiling is on until some unrelated retile.
        self.publish_status();
        self.tiling_enabled
    }

    /// The `hyprctl clients`-style listing: one line per managed window.
    ///
    /// The read-side counterpart to `monitors_text`, and the only way to see what
    /// HyprWin has adopted without moving focus. Ids match the ones the status
    /// document's counts are drawn from, so a script can correlate the two.
    pub fn clients_text(&self) -> String {
        let focused = win32::foreground();
        let mut rows: Vec<(usize, String)> = Vec::with_capacity(self.windows.len());
        for w in self.windows.values() {
            let id = w.id;
            let rect = self.placed.get(&id).copied().unwrap_or_default();
            let mon = self
                .monitors
                .iter()
                .position(|m| m.handle == w.monitor)
                .map(|i| i + 1)
                .unwrap_or(0);
            let ws = self.workspaces.workspace_of(id);
            let workspace = if ws == SCRATCHPAD_WORKSPACE {
                "scratchpad".to_string()
            } else {
                ws.to_string()
            };
            let mut traits = Vec::new();
            if self.workspaces.is_floating(id) {
                traits.push("floating");
            }
            if self.workspaces.is_fullscreen(id) {
                traits.push("fullscreen");
            }
            if win32::is_maximized(w.hwnd) {
                traits.push("maximized");
            }
            if win32::is_minimized(w.hwnd) {
                traits.push("minimized");
            }
            let traits = if traits.is_empty() {
                "tiled".to_string()
            } else {
                traits.join(",")
            };
            let mark = if w.hwnd == focused { "*" } else { " " };
            rows.push((
                id,
                format!(
                    "{mark} id {id} monitor {mon} workspace {workspace} {traits} \
                     rect {}+{}+{}+{} {}",
                    rect.x,
                    rect.y,
                    rect.w,
                    rect.h,
                    win32::title(w.hwnd).trim()
                ),
            ));
        }
        rows.sort_by_key(|(id, _)| *id);
        let mut out = String::new();
        for (_, line) in rows {
            out.push_str(&line);
            out.push('\n');
        }
        out
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
                layout_name(self.layout_of(m.handle)),
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
            // Focus moved between tiles with the layout untouched, so the accent
            // eases onto the new tile rather than snapping.
            self.apply_border_focus_animated();
        }
    }

    fn cmd_resize(&mut self, arg: Option<&str>) {
        let Some(focus) = self.focused() else {
            return;
        };
        let Some(dir) = arg.and_then(Direction::parse) else {
            return;
        };
        self.resize_focused_edge(focus, dir);
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
                self.animate_into_tween(&[(hwnd, rect)]);
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
            self.animate_into_tween(&[(hwnd, rect)]);
            win32::set_foreground(hwnd);
            // The window now fills the monitor, so its tile accent no longer
            // matches where it is; the filter in `apply_border_focus` clears it.
            self.apply_border_focus();
        } else {
            self.retile_monitor(mon);
        }
    }

    fn cmd_toggle_layout(&mut self) {
        // Kept as the argument-less "cycle" entry point so an existing bind or
        // `hyprwinctl togglelayout` keeps working; `layout <name>` sets one.
        self.cmd_layout(None);
    }

    /// Set or cycle the layout. `arg` absent, `next` or `cycle` cycles; a name
    /// (`dwindle`, `master`, `spiral`, `tall`, `grid`, `stacking`) sets it on
    /// every monitor, and `next`/`prev` alone move only the focused one.
    fn cmd_layout(&mut self, arg: Option<&str>) {
        let Some(mon) = self.command_monitor() else {
            return;
        };
        let owned = arg.map(|a| a.trim().to_ascii_lowercase());
        let token = owned.as_deref().filter(|s| !s.is_empty());
        let chosen = match token {
            None | Some("next") | Some("cycle") => {
                let next = self.layout_of(mon).next();
                self.layouts.insert(mon, next);
                next
            }
            Some("prev") => {
                let i = LayoutKind::CYCLE
                    .iter()
                    .position(|k| *k == self.layout_of(mon))
                    .unwrap_or(0);
                let n = LayoutKind::CYCLE.len();
                let prev = LayoutKind::CYCLE[(i + n - 1) % n];
                self.layouts.insert(mon, prev);
                prev
            }
            Some(name) => match LayoutKind::parse(name) {
                Some(kind) => {
                    for m in self.monitors.clone() {
                        self.layouts.insert(m.handle, kind);
                    }
                    kind
                }
                None => {
                    crate::log_warn!("[hyprwin] unknown layout '{name}'");
                    return;
                }
            },
        };
        crate::log_info!("[hyprwin] layout {}", chosen.name());
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
        self.move_window_to_monitor(focus, to);
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
    pub(super) fn make_scratchpad(&mut self, id: usize) {
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
                self.cfg = cfg;
                // Re-seed each monitor's layout from the new config, keeping the
                // monitors we already know about.
                self.layouts = seed_layouts(&self.cfg, &self.monitors);
                crate::log_info!("[hyprwin] config reloaded");
            }
            Err(e) => crate::log_error!("[hyprwin] reload failed: {e:#}"),
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

