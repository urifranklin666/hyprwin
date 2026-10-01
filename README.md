# HyprWin

A Hyprland-inspired **dynamic tiling window manager for Windows**, written in Rust
on top of `windows-rs`.

HyprWin is a supervisor process, not a compositor. Windows does not let a
third-party program own rendering, so HyprWin hooks window events
(`SetWinEventHook`), grabs global hotkeys (`RegisterHotKey`) and repositions
other applications' windows with `SetWindowPos`. This is the same model used by
komorebi and GlazeWM, and it is the only model Windows permits.

---

## Status

Working, verified by the unit suite (`71 passed`), a clean release build and a
live run against a two-monitor desktop. See
[Manual smoke test](#manual-smoke-test) for the checklist to run yourself.

## What works

| Feature | Notes |
|---|---|
| Dynamic tiling | Dwindle (spiral) and master/stack layouts |
| Minimized windows leave the layout | Minimizing reflows the other windows to fill the gap; restoring puts the window back in the slot it had |
| Persistent split ratios | Resizing one split survives retiling; other windows do not jump |
| Per-workspace layout memory | Leaving a workspace parks its dwindle tree, so windows return to the position they had |
| Clean exit | Quitting un-hides windows parked on other workspaces and restores their original geometry |
| Gaps | Configurable outer (edge) and inner (between windows) |
| Borders + rounding | DWM accent border, active/inactive colours, Win11 corner rounding |
| Virtual workspaces | Per-monitor, numbered; off-workspace windows are hidden and shown again on return, with one raised into focus |
| Keybinds | Configurable, with a `main` placeholder for the primary modifier |
| Window rules | Match exe / class / title, then float, ignore, or pin to a workspace |
| Animations | Eased interpolation of moves and resizes (best effort) |
| Tray icon | Reload config / toggle tiling / quit |
| Autostart | Optional `HKCU\...\Run` registration |
| Multi-monitor | Per-monitor workspaces, hot-plug re-detection, per-monitor DPI awareness; windows are assigned by greatest overlap with a monitor |

## Known limitations

**These are Windows platform limits, not missing features.**

- **No blur.** A compositor can blur behind other apps' windows; a supervisor
  process cannot. Per-app acrylic/mica is the only thing available, and it is
  the app's decision, not ours.
- **Animations are approximate.** They are produced by stepping `SetWindowPos`
  on a ~8 ms loop, so they can stutter on heavy windows and will never match
  Hyprland's compositor-native fluidity. Set `animation_ms` to `0` to disable.
- **`Win` is a poor choice for `main_mod`.** Windows reserves `Win`+Arrow
  (Snap) and `Win`+`0`–`9` (taskbar). `RegisterHotKey` will refuse those chords
  and HyprWin will log each refusal rather than silently dropping it. The
  default primary modifier is therefore **Alt**.

---

## Build

Requires the Rust toolchain and the MSVC C++ build tools.

```sh
cargo build --release
cargo test
```

The binary lands at `target/release/hyprwin.exe`.

If `cargo` is not on your `PATH` after a fresh rustup install, use
`%USERPROFILE%\.cargo\bin\cargo.exe`.

## Run

```sh
target\release\hyprwin.exe
```

On first run HyprWin writes a default config to
`%APPDATA%\hyprwin\config.json`, then tiles the windows already open. It only
tiles windows that are visible, unminimized, titled, not tool windows and not
owned by another window, so dialogs, shell surfaces and the hidden helper
windows browsers create are left alone.

Quit from the tray icon, or with `Ctrl+C` in the console.

> **Before your first run**, close anything you cannot afford to have moved.
> HyprWin rearranges every eligible window it can see.

## Default keybinds

`main` is the primary modifier, `alt` by default.

| Binding | Action |
|---|---|
| `main` + `H` `J` `K` `L` | Focus left / down / up / right (vim order) |
| `main` + `←` `↓` `↑` `→` | Focus left / down / up / right |
| `main` + `Shift` + `H/J/K/L` | Swap the focused window through the layout |
| `main` + `Ctrl` + `H/J/K/L` | Resize the nearest split |
| `main` + `V` | Toggle floating |
| `main` + `F` | Toggle fullscreen |
| `main` + `Space` | Toggle dwindle / master layout |
| `main` + `Q` | Close the focused window |
| `main` + `R` | Reload `config.json` |
| `main` + `1`..`9` | Switch to workspace 1..9 |
| `main` + `Shift` + `1`..`9` | Move the focused window to workspace 1..9 |
| `main` + `,` / `main` + `.` | Previous / next workspace |

> `Alt+Tab` and `Alt+Shift+Tab` are **not** used: Windows reserves them for the
> task switcher, so `RegisterHotKey` rejects them. HyprWin logs every rejected
> binding at startup rather than dropping it silently.

## Configuration

`%APPDATA%\hyprwin\config.json`. Every field may be omitted; omitted fields take
the documented default.

```json
{
  "main_mod": "alt",
  "gaps_outer": 8,
  "gaps_inner": 6,
  "show_border": true,
  "border_color_active": "#7aa2f7",
  "border_color_inactive": "#3b4261",
  "rounding": 2,
  "animation_ms": 120,
  "animation_easing": "cubic",
  "workspaces": 9,
  "default_layout": "dwindle",
  "master_ratio": 0.6,
  "manage_on_start": true,
  "autostart": false,
  "keybinds": [],
  "rules": []
}
```

| Field | Meaning |
|---|---|
| `main_mod` | `alt`, `ctrl`, `shift` or `win` |
| `gaps_outer` | Gap between the tiled area and the monitor work area, pixels |
| `gaps_inner` | Gap between individual windows, pixels |
| `show_border` | Paint the DWM accent border on the focused window |
| `border_color_active` / `border_color_inactive` | `#RRGGBB` |
| `rounding` | `0` default, `1` square, `2` rounded (Windows 11) |
| `animation_ms` | Duration; `0` disables animation |
| `animation_easing` | `linear`, `quad`, `cubic` or `expo` |
| `workspaces` | Workspaces per monitor, 1–36 |
| `default_layout` | `dwindle` or `master` |
| `master_ratio` | Master share for `master` layout, between 0.1 and 0.9 |
| `manage_on_start` | Adopt already-open windows on startup; when `false`, only windows that appear after startup are tiled |
| `autostart` | Register the binary in `HKCU\...\Run` |

### Keybinds

```json
{
  "mods": ["main", "shift"],
  "key": "H",
  "action": "move",
  "arg": "left"
}
```

`mods` accepts `main`, `alt`, `ctrl`, `shift`, `win`. `key` is a single
character (`1`, `Q`), a named key (`Left`, `Space`, `Tab`, `PageUp`, `F1`–`F24`)
or a punctuation key. Actions are `focus`, `move`, `resize`,
`togglefloating`, `fullscreen`, `togglelayout`, `workspace`,
`movetoworkspace`, `close` and `reload`.

### Window rules

```json
{
  "exe": "chrome.exe",
  "title": "DevTools",
  "action": "float"
}
```

All specified fields must match; matching is case-insensitive substring.
`action` is one of:

- `float` — tile-exempt, keeps its own geometry
- `ignore` — HyprWin never touches it
- `workspace` — send it to `arg` (a workspace number) on creation

---

## Architecture

| Module | Responsibility |
|---|---|
| `main.rs` | Startup, message loop, wiring |
| `wm.rs` | The state machine: tiling, focus, workspaces, commands |
| `layout.rs` | Dwindle tree, per-workspace forest, master/stack, gaps, geometric neighbour search |
| `workspace.rs` | Per-monitor workspaces, floating/fullscreen bookkeeping |
| `win32.rs` | FFI wrappers: window queries, `SetWindowPos`, DWM attributes |
| `hook.rs` | `SetWinEventHook` callbacks |
| `hotkey.rs` | `RegisterHotKey` and keybind parsing |
| `monitor.rs` | `EnumDisplayMonitors`, work areas |
| `animation.rs` | Easing curves and interpolation (pure, tested) |
| `config.rs` | Config structs, defaults, validation, colour parsing |
| `tray.rs` | Tray icon and menu |
| `autostart.rs` | `HKCU\...\Run` helper |
| `events.rs` | The channel shared by hooks, tray and the main loop |

Three design points worth knowing:

- **Layouts are parked per workspace.** Each monitor holds a `Forest` — one
  dwindle tree per workspace — so leaving a workspace preserves its split ratios
  and window order. The active workspace's tree is re-synced by `ensure_tree`
  on every retile (additions land, closed windows drop out) and written back.
  A single shared tree would lose the layout on every switch.
- **Events carry raw handle values, not `HWND`.** `HWND` wraps a pointer, so it
  is not `Send`; the channel is written from hook callbacks but read on the main
  thread.
- **`retile_monitor` is the only writer of geometry, and `apply_visibility` the
  only writer of visibility.** Keeping those two decisions in one place each is
  what prevents a window from being hidden on a workspace switch and never shown
  again.

## Manual smoke test

Run `target\release\hyprwin.exe`, then check:

1. Open Notepad twice — the two windows tile side by side, with gaps.
2. `Alt+J` / `Alt+K` move focus between them; the active window shows the accent border.
3. `Alt+Shift+L` swaps their positions.
4. `Alt+Ctrl+L` grows the focused split; `Alt+L` then tiles still line up.
5. Open a third window — dwindle spirals it into the focused half.
6. `Alt+2`, then `Alt+1` — the windows disappear, come back, and **return to the
   positions they had**; the focused window is the one you can see.
7. `Alt+Ctrl+L` to widen a split, switch away with `Alt+2` and back with `Alt+1`
   — the widened split is still widened.
8. `Alt+Shift+2` — the window moves to workspace 2 and focus follows it. Close it
   there, then `Alt+1`: the window left behind must still be visible.
9. `Alt+V` floats the focused window and centres it; `Alt+V` again re-tiles it.
10. `Alt+F` fills the monitor; `Alt+F` again restores the tiling.
11. `Alt+Space` switches to master layout; the first window takes the wide pane.
12. `Alt+Q` closes the focused window and the survivors expand to fill the space.
13. Close every window on a monitor — no gaps remain and nothing crashes.
14. Right-click the tray icon — Reload / Toggle tiling / Quit all work.
15. Edit `%APPDATA%\hyprwin\config.json`, change `gaps_inner` and `animation_ms`,
    press `Alt+R` — the change applies.
16. Unplug or mirror a display, or drag the window between monitors — HyprWin
    re-tiles without losing windows.
17. Start HyprWin with several windows already open, on a multi-monitor setup.

## License

MIT.
