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

Working, verified by the unit suite (`98 passed`), a clean release build and a
live run against a two-monitor desktop. See
[Manual smoke test](#manual-smoke-test) for the checklist to run yourself.

## What works

| Feature | Notes |
|---|---|
| Dynamic tiling | Dwindle (spiral) and master/stack layouts |
| Minimized windows leave the layout | Minimizing reflows the other windows to fill the gap; restoring puts the window back in the slot it had |
| Maximized windows are re-tiled | A window maximised after it was adopted is un-maximised on the next retile instead of sitting zoomed over its tile |
| Persistent split ratios | Resizing one split survives retiling; other windows do not jump |
| Per-workspace layout memory | Leaving a workspace parks its dwindle tree, so windows return to the position they had |
| Clean exit | Quitting un-hides windows parked on other workspaces and restores their original geometry |
| Gaps | Configurable outer (edge) and inner (between windows) |
| Borders + rounding | DWM accent border, active/inactive colours, Win11 corner rounding |
| Virtual workspaces | Per-monitor, numbered; off-workspace windows are hidden and shown again on return, with one raised into focus |
| Scratchpad | A drop-down window over whatever workspace you are on, hideable with one key and reachable from any workspace |
| Move between monitors | `movetomonitor` sends the focused window to the other display |
| Keyboard escape from an empty workspace | On a monitor showing nothing, a workspace switch or scratchpad toggle follows the mouse instead of always hitting the primary display |
| Keybinds | Configurable, with a `main` placeholder for the primary modifier |
| Window rules | Match exe / class / title, then float (optionally by size), ignore, pin to a workspace, or send to a monitor or the scratchpad |
| Status output | Writes `status.json` (active workspace, layout, focus, window counts, work areas) for a Rainmeter skin or any other dashboard to read |
| Control interface | `hyprwinctl` drives a running instance over a named pipe: switch workspace, toggle tiling, reload, query status |
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

A second binary, `target/release/hyprwinctl.exe`, is built alongside it to drive
a running instance. Both are built by the same `cargo build`.

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
| `main` + `Shift` + `,` / `main` + `Shift` + `.` | Send the focused window to the previous / next monitor |
| `main` + `S` | Move the focused window onto the scratchpad and drop it down |
| `main` + `Shift` + `S` | Show / hide the scratchpad |

> `Alt+Tab` and `Alt+Shift+Tab` are **not** used: Windows reserves them for the
> task switcher, so `RegisterHotKey` rejects them. HyprWin logs every rejected
> binding at startup rather than dropping it silently.

### The scratchpad

The scratchpad is one window — a terminal, a notes pane, whatever — that you can
summon over any workspace and hide again. It is not a tenth desktop: it has its
own layout tree, so dropping it down never disturbs the layout underneath, and
it stays put while you switch workspaces until you pull it back up.

- `main` + `S` puts the focused window on the scratchpad and shows it.
- `main` + `Shift` + `S` toggles it: down over the current workspace, or away.
- With nothing on the scratchpad yet, the toggle simply shows nothing. Put a
  window on it with `main` + `S` first.

It is deliberately explicit: the toggle never picks a window for you, so the
same chord always means the same thing whatever is focused.

### Which monitor a chord acts on

Chords that do not name a window — `workspace`, the previous/next pair, and the
scratchpad toggle — act on the monitor holding the focused window. When nothing
is focused, which is what an empty workspace looks like, they follow the mouse
instead: point at the monitor you mean. A focused window always wins over the
pointer, so this only comes into play when there is no focused window at all.
The primary display is the last resort, used when the cursor cannot be read or
sits on a display HyprWin does not manage.

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
  "status_file": true,
  "status_path": "",
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
| `status_file` | Write the status document described below |
| `status_path` | Where to write it; empty means `status.json` beside `config.json` |

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
or a punctuation key. Actions are `focus`, `move`, `resize`, `togglefloating`,
`fullscreen`, `togglelayout`, `workspace`, `movetoworkspace`, `movetomonitor`,
`scratchpad`, `movetoscratchpad`, `close` and `reload`.

`movetomonitor` takes `next`, `prev`, or a monitor named by index (`1`, `2`,
primary first) or `primary`. Workspace numbers are per-monitor, so a window sent
to the next monitor lands on whatever workspace that monitor is showing — the
only assignment that keeps it visible when it arrives.

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

- `float` — tile-exempt, keeps its own geometry. With no `arg`, it always
  floats. With an `arg` like `"400x300"`, it floats only windows *smaller* than
  that, so one rule can catch every tool palette without touching real windows.
  A rule with no `exe`/`class`/`title` and a size `arg` is the "float anything
  tiny" rule, and ships by default.
- `ignore` — HyprWin never touches it
- `workspace` — send it to `arg` (a workspace number) on creation
- `monitor` — send it to `arg` (`next`, `prev`, `1`, `2`, … or `primary`)
- `scratchpad` — adopt it onto the scratchpad and drop it down

---

## Status output

When `status_file` is true (the default), HyprWin writes
`%APPDATA%\hyprwin\status.json` on every change, for a Rainmeter skin or any
other dashboard to read. It is replaced atomically, so a reader polling it never
sees a half-written file, and it is the only integration a skin needs — HyprWin
has no bar of its own.

```json
{
  "running": true,
  "pid": 4242,
  "tiling_enabled": true,
  "focused": {
    "title": "notes.md - Notepad",
    "exe": "notepad.exe",
    "monitor": 1
  },
  "monitors": [
    {
      "index": 1,
      "primary": true,
      "active_workspace": 3,
      "layout": "dwindle",
      "windows": 2,
      "scratchpad": false,
      "scratchpad_windows": 0,
      "work_area": { "x": 0, "y": 0, "w": 2560, "h": 1392 }
    }
  ]
}
```

- `monitors[].index` is 1-based, primary first, and matches the number a
  `monitor` rule uses.
- `focused.monitor` is `0` when the focused window is not one HyprWin manages.
- `running` goes `false` on a clean exit, so a skin can stop showing live data.

---

## Control interface

`status.json` lets other programs **read** HyprWin. `hyprwinctl.exe` lets them
**act**. It is the `hyprctl` analogue: it talks to a running instance over a
named pipe and exits with a status a script can check.

```sh
hyprwinctl status              # the status document, as status.json holds it
hyprwinctl monitors            # one line per monitor
hyprwinctl workspace 3         # switch the focused monitor to workspace 3
hyprwinctl togglelayout
hyprwinctl reload
hyprwinctl quit                # ask HyprWin to exit cleanly
```

Anything else is passed to the same dispatcher the keybinds use, so the
vocabulary is exactly the keybind action list — `focus left`, `movetomonitor
next`, `movetoscratchpad`, `toggletiling`, and so on — plus `status`,
`monitors` and `quit`. A successful command echoes what it ran; an unknown one
prints `unknown action '...'` and exits `1`.

The pipe is `\\.\pipe\hyprwin` (one instance per user session) and speaks a
single line per request. A reply is tagged `OK`, `DATA` or `ERR`, followed by
the payload; `status` and `monitors` are answered from the same snapshot that
was last written to `status.json`, and everything that changes tiling state is
queued onto the main thread, so the pipe can never touch the layout directly.
Any JSON client can talk to it without `hyprwinctl`, which is what makes it
usable from a Rainmeter action or a startup script.

Requests are answered in order and a client that hangs up is simply dropped.

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
| `status.rs` | The `status.json` document and its atomic write |
| `protocol.rs` | The control-line format, shared by both binaries |
| `ipc.rs` | The control pipe: a listener thread that queues requests onto the main loop |
| `events.rs` | The channel shared by hooks, tray, pipe and the main loop |

Three design points worth knowing:

- **Layouts are parked per workspace.** Each monitor holds a `Forest` — one
  dwindle tree per workspace — so leaving a workspace preserves its split ratios
  and window order. The active workspace's tree is re-synced by `ensure_tree`
  on every retile (additions land, closed windows drop out) and written back.
  A single shared tree would lose the layout on every switch. The scratchpad is
  one more tree in that forest, so it never mixes into the workspace below it.
- **Events carry raw handle values, not `HWND`.** `HWND` wraps a pointer, so it
  is not `Send`; the channel is written from hook callbacks but read on the main
  thread.
- **`retile_monitor` is the only writer of geometry, and `apply_visibility` the
  only writer of visibility.** Keeping those two decisions in one place each is
  what prevents a window from being hidden on a workspace switch and never shown
  again, and it is why the scratchpad stays tiled rather than floating while it
  is down: a floating window is skipped by every retile, so nothing would put it
  back in place.

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
18. `Alt+Shift+.` (or `,`) sends the focused window to the other monitor; it
    lands on whatever workspace that monitor is showing and both monitors
    re-tile.
19. With two or more windows open, `Alt+S` puts the focused window on the
    scratchpad and drops it down as a centred panel; the windows underneath keep
    their layout. `Alt+Shift+S` hides it and `Alt+Shift+S` again brings it back
    to the **same** rectangle, over a different workspace too.
20. Maximise a tiled window with the title-bar button (or `Win`+`↑`), then press
    `Alt+Space`: the window is un-maximised and re-tiled back into the layout
    instead of staying zoomed over its tile.
21. Read `%APPDATA%\hyprwin\status.json` after each of the above — the active
    workspace, layout, focus and window counts track what you see, and `running`
    becomes `false` after you quit.
22. With HyprWin running, use `hyprwinctl` from another terminal: `hyprwinctl
    monitors` lists both displays, `hyprwinctl workspace 2` switches the focused
    monitor, `hyprwinctl bogus` reports `unknown action` and exits non-zero, and
    `hyprwinctl quit` shuts HyprWin down exactly like the tray's Quit. With
    HyprWin stopped, `hyprwinctl monitors` says nothing is listening.
23. On a two-monitor setup, close every window on the second monitor so it shows
    an empty workspace. Put the mouse on that display and press `Alt+2`: the
    **second** monitor switches, not the primary. `Alt+1` brings it back. With
    the pointer on the primary instead, the chord acts there.

## License

MIT.
