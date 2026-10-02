use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Tiling layout algorithm.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayoutKind {
    /// Recursive halving; a new window splits the focused one.
    #[default]
    Dwindle,
    /// One master pane on the left, the rest stacked on the right.
    Master,
    /// Recursive halving in one direction, so the panes spiral inward.
    Spiral,
    /// A single column, every window full width.
    Tall,
    /// An even grid of cells, `ceil(sqrt(n))` columns wide.
    Grid,
    /// Every window fills the work area; the focused one is on top.
    Stacking,
}

impl LayoutKind {
    /// The order `layout` cycles through when it is given no argument.
    pub const CYCLE: [LayoutKind; 6] = [
        LayoutKind::Dwindle,
        LayoutKind::Master,
        LayoutKind::Spiral,
        LayoutKind::Tall,
        LayoutKind::Grid,
        LayoutKind::Stacking,
    ];

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dwindle" => Some(LayoutKind::Dwindle),
            "master" => Some(LayoutKind::Master),
            "spiral" => Some(LayoutKind::Spiral),
            "tall" | "columns" => Some(LayoutKind::Tall),
            "grid" => Some(LayoutKind::Grid),
            "stacking" | "stack" => Some(LayoutKind::Stacking),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            LayoutKind::Dwindle => "dwindle",
            LayoutKind::Master => "master",
            LayoutKind::Spiral => "spiral",
            LayoutKind::Tall => "tall",
            LayoutKind::Grid => "grid",
            LayoutKind::Stacking => "stacking",
        }
    }

    /// The next layout in `CYCLE`.
    pub fn next(self) -> Self {
        let i = Self::CYCLE.iter().position(|k| *k == self).unwrap_or(0);
        Self::CYCLE[(i + 1) % Self::CYCLE.len()]
    }
}

/// Direction used for focus/move/resize commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "left" | "l" | "west" => Some(Direction::Left),
            "right" | "r" | "east" => Some(Direction::Right),
            "up" | "u" | "north" => Some(Direction::Up),
            "down" | "d" | "south" => Some(Direction::Down),
            _ => None,
        }
    }
}

/// A single keybinding. `action` is a command name, `arg` its optional parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Keybind {
    #[serde(default)]
    pub mods: Vec<String>,
    pub key: String,
    pub action: String,
    #[serde(default)]
    pub arg: Option<String>,
}

/// Window rule: match an application and apply an action.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Rule {
    /// Match against the executable file name, e.g. "explorer.exe".
    #[serde(default)]
    pub exe: Option<String>,
    /// Match against the Win32 window class, e.g. "Notepad".
    #[serde(default)]
    pub class: Option<String>,
    /// Match against a substring of the window title.
    #[serde(default)]
    pub title: Option<String>,
    /// "float" | "ignore" | "workspace"
    pub action: String,
    /// Action argument: a workspace number for "workspace", or a `WxH` size
    /// limit for "float" (float only when the window is smaller than this).
    #[serde(default)]
    pub arg: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Primary modifier: "alt" | "ctrl" | "shift" | "win".
    pub main_mod: String,
    /// Gap between the tiled area and the monitor work area, in pixels.
    pub gaps_outer: i32,
    /// Gap between individual windows, in pixels.
    pub gaps_inner: i32,
    /// Paint the DWM accent border on the focused window.
    pub show_border: bool,
    /// Active window border colour, "#RRGGBB".
    pub border_color_active: String,
    /// Second colour of the focused tile's gradient accent, "#RRGGBB".
    ///
    /// Empty (the default) keeps the accent one flat colour. When set, the
    /// overlay accent runs from `border_color_active` at the top-left corner to
    /// this colour at the bottom-right, the way a Hyprland gradient border does.
    pub border_color_active_alt: String,
    /// Inactive window border colour, "#RRGGBB".
    pub border_color_inactive: String,
    /// Focused-window accent thickness in pixels, drawn by a per-monitor
    /// overlay window. 0 disables the accent; the 1px DWM line stays either way.
    pub border_width: i32,
    /// Accent thickness for unfocused tiles, in pixels.
    ///
    /// 0 (the default) leaves inactive tiles with only their 1px DWM line. A
    /// positive value draws the same overlay frame on every tiled window, in
    /// `border_color_inactive`, which is the Hyprland active/inactive look.
    pub border_width_inactive: i32,
    /// Corner radius of the overlay accent, in pixels. -1 follows `rounding`.
    ///
    /// The DWM owns the window's real corner, so this exists to make the accent
    /// hug it instead of drawing a square corner that overhangs. -1 mirrors the
    /// DWM preference: rounded when `rounding` is 2, square otherwise.
    pub border_radius: i32,
    /// DWM corner preference: 0 = default, 1 = do not round, 2 = round.
    pub rounding: i32,
    /// Animation duration in milliseconds. 0 disables animation.
    pub animation_ms: u32,
    /// Easing: "linear" | "quad" | "cubic" | "expo".
    pub animation_easing: String,
    /// Number of virtual workspaces per monitor.
    pub workspaces: u32,
    pub default_layout: LayoutKind,
    /// Master area share for the "master" layout, 0.1..0.9.
    pub master_ratio: f32,
    /// How many windows hold a master pane in the "master" layout.
    pub master_count: u32,
    /// Per-workspace master-ratio override, keyed by workspace number. A
    /// workspace with no entry uses `master_ratio`; resizing the master split
    /// while in the `master` layout updates the working copy at runtime.
    pub master_ratios: std::collections::HashMap<String, f32>,
    /// Per-monitor layout, overriding `default_layout`. Keys are monitor
    /// numbers (1-based, primary first), the same numbers a `monitor` rule uses;
    /// a monitor with no entry uses `default_layout`. This is a deliberate start
    /// rather than dynamic memory so there is no new file to keep in sync.
    pub layouts: std::collections::HashMap<String, LayoutKind>,
    /// Re-tile all currently open windows on startup. When false, HyprWin only
    /// takes over windows that appear after it starts.
    pub manage_on_start: bool,
    /// Register the binary to run at login (HKCU Run key).
    pub autostart: bool,
    pub keybinds: Vec<Keybind>,
    pub rules: Vec<Rule>,
    /// Write a small status file for external dashboards (e.g. Rainmeter) to read.
    pub status_file: bool,
    /// Path of the status file. Empty means the default in the config directory.
    pub status_path: String,
    /// Remember the arrangement across restarts: each window's workspace,
    /// floating/fullscreen state and, for floating windows, its rectangle, plus
    /// each monitor's active workspace, layout and the per-workspace split trees.
    ///
    /// The file is rewritten on every retile, so a crash still leaves a recent
    /// session behind. Windows are matched by executable, class and title, since
    /// an id is per-run and an HWND does not survive a restart.
    pub persist_session: bool,
    /// Path of the session file. Empty means `session.json` in the config directory.
    pub session_path: String,
    /// Where to write the log. Empty means `hyprwin.log` in the config directory.
    ///
    /// The console is not a usable trail for a tray process, so every line is
    /// also appended here. The file is rotated once, to `.old`, when it grows
    /// past a megabyte.
    pub log_path: String,
    /// Minimum level logged to console and file: "error" | "warn" | "info" |
    /// "debug" | "off".
    pub log_level: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            main_mod: "alt".to_string(),
            gaps_outer: 8,
            gaps_inner: 6,
            show_border: true,
            border_color_active: "#7aa2f7".to_string(),
            border_color_active_alt: String::new(),
            border_color_inactive: "#3b4261".to_string(),
            border_width: 3,
            border_width_inactive: 0,
            border_radius: -1,
            rounding: 2,
            animation_ms: 120,
            animation_easing: "cubic".to_string(),
            workspaces: 9,
            default_layout: LayoutKind::Dwindle,
            master_ratio: 0.6,
            master_count: 1,
            master_ratios: std::collections::HashMap::new(),
            layouts: std::collections::HashMap::new(),
            manage_on_start: true,
            autostart: false,
            keybinds: default_keybinds(),
            rules: default_rules(),
            status_file: true,
            status_path: String::new(),
            persist_session: true,
            session_path: String::new(),
            log_path: String::new(),
            log_level: "info".to_string(),
        }
    }
}

fn kb(mods: &[&str], key: &str, action: &str, arg: Option<&str>) -> Keybind {
    Keybind {
        mods: mods.iter().map(|s| s.to_string()).collect(),
        key: key.to_string(),
        action: action.to_string(),
        arg: arg.map(|s| s.to_string()),
    }
}

/// Default binds use Alt, not Win: Windows reserves Win+Arrow and Win+0-9.
fn default_keybinds() -> Vec<Keybind> {
    let m = "main";
    let mut v = vec![
        // Focus (vim style).
        kb(&[m], "H", "focus", Some("left")),
        kb(&[m], "J", "focus", Some("down")),
        kb(&[m], "K", "focus", Some("up")),
        kb(&[m], "L", "focus", Some("right")),
        kb(&[m], "Left", "focus", Some("left")),
        kb(&[m], "Down", "focus", Some("down")),
        kb(&[m], "Up", "focus", Some("up")),
        kb(&[m], "Right", "focus", Some("right")),
        // Move the focused window through the layout.
        kb(&[m, "shift"], "H", "move", Some("left")),
        kb(&[m, "shift"], "J", "move", Some("down")),
        kb(&[m, "shift"], "K", "move", Some("up")),
        kb(&[m, "shift"], "L", "move", Some("right")),
        // Resize the nearest split.
        kb(&[m, "ctrl"], "H", "resize", Some("left")),
        kb(&[m, "ctrl"], "J", "resize", Some("down")),
        kb(&[m, "ctrl"], "K", "resize", Some("up")),
        kb(&[m, "ctrl"], "L", "resize", Some("right")),
        // Layout.
        kb(&[m], "V", "togglefloating", None),
        kb(&[m], "F", "fullscreen", None),
        kb(&[m], "Space", "togglelayout", None),
        kb(&[m], "Q", "close", None),
        // Workspaces.
        // Not Alt+Tab: Windows owns that chord for the task switcher, so
        // RegisterHotKey rejects it. Comma/period are unclaimed and vim-ish.
        // The key is written as the literal character, which is the form the
        // hotkey parser maps to a virtual-key code.
        kb(&[m], ",", "workspace", Some("prev")),
        kb(&[m], ".", "workspace", Some("next")),
        // Per-monitor workspace targeting and window moves.
        kb(&[m, "shift"], ",", "movetomonitor", Some("prev")),
        kb(&[m, "shift"], ".", "movetomonitor", Some("next")),
        // The scratchpad: one window you can drop over any workspace, and hide.
        kb(&[m], "S", "movetoscratchpad", None),
        kb(&[m, "shift"], "S", "scratchpad", None),
        kb(&[m], "R", "reload", None),
    ];
    for n in 1..=9u32 {
        let k = n.to_string();
        v.push(kb(&[m], &k, "workspace", Some(&k)));
        v.push(kb(&[m, "shift"], &k, "movetoworkspace", Some(&k)));
    }
    v
}

fn default_rules() -> Vec<Rule> {
    vec![
        // Standard Win32 dialogs float instead of being tiled.
        Rule {
            class: Some("#32770".to_string()),
            action: "float".to_string(),
            ..Default::default()
        },
        Rule {
            exe: Some("Taskmgr.exe".to_string()),
            action: "float".to_string(),
            ..Default::default()
        },
        // Tiny windows (tool palettes, splash screens) float instead of being
        // given a tile. `arg` is the size under which a window is "tiny",
        // written `WxH`.
        Rule {
            action: "float".to_string(),
            arg: Some("400x300".to_string()),
            ..Default::default()
        },
        // Picture-in-picture style always-on-top utility windows.
        Rule {
            class: Some("ApplicationFrameWindow".to_string()),
            action: "workspace".to_string(),
            arg: Some("3".to_string()),
            ..Default::default()
        },
    ]
}

impl Config {
    pub fn dir() -> PathBuf {
        let base = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(base).join("hyprwin")
    }

    pub fn path() -> PathBuf {
        Self::dir().join("config.json")
    }

    /// Where the status document is written.
    ///
    /// An empty `status_path` means `status.json` next to `config.json`, which
    /// keeps a Rainmeter skin's path stable without the user having to type one.
    pub fn status_path(&self) -> PathBuf {
        let p = self.status_path.trim();
        if p.is_empty() {
            Self::dir().join("status.json")
        } else {
            PathBuf::from(p)
        }
    }

    /// Where the log is written. Empty means `hyprwin.log` beside the config.
    pub fn log_path(&self) -> PathBuf {
        let p = self.log_path.trim();
        if p.is_empty() {
            crate::log::default_path()
        } else {
            PathBuf::from(p)
        }
    }

    /// Where the remembered session is written.
    ///
    /// Empty means `session.json` next to `config.json`. It is not written at all
    /// when `persist_session` is false.
    pub fn session_path(&self) -> PathBuf {
        let p = self.session_path.trim();
        if p.is_empty() {
            Self::dir().join("session.json")
        } else {
            PathBuf::from(p)
        }
    }

    /// Load the config, writing a default file on first run.
    pub fn load_or_create() -> Result<Self> {
        let path = Self::path();
        if path.exists() {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let cfg: Config = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?;
            cfg.validate()?;
            Ok(cfg)
        } else {
            let cfg = Config::default();
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).ok();
            }
            let text = serde_json::to_string_pretty(&cfg)?;
            fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
            Ok(cfg)
        }
    }

    /// Reject configurations that would panic or behave nonsensically.
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.workspaces >= 1, "workspaces must be >= 1");
        anyhow::ensure!(self.workspaces <= 36, "workspaces must be <= 36");
        anyhow::ensure!(self.gaps_outer >= 0, "gaps_outer must be >= 0");
        anyhow::ensure!(self.gaps_inner >= 0, "gaps_inner must be >= 0");
        anyhow::ensure!(
            (0..=64).contains(&self.border_width),
            "border_width must be between 0 and 64"
        );
        anyhow::ensure!(
            (0..=64).contains(&self.border_width_inactive),
            "border_width_inactive must be between 0 and 64"
        );
        anyhow::ensure!(
            (-1..=64).contains(&self.border_radius),
            "border_radius must be -1 (follow rounding) or between 0 and 64"
        );
        anyhow::ensure!(
            self.master_ratio > 0.1 && self.master_ratio < 0.9,
            "master_ratio must be between 0.1 and 0.9"
        );
        anyhow::ensure!(self.master_count >= 1, "master_count must be at least 1");
        for key in self.layouts.keys() {
            anyhow::ensure!(
                key.trim().parse::<usize>().map(|n| n >= 1).unwrap_or(false),
                "layouts keys must be monitor numbers like \"1\", got {key:?}"
            );
        }
        for (key, ratio) in &self.master_ratios {
            anyhow::ensure!(
                key.trim().parse::<usize>().map(|n| n >= 1).unwrap_or(false),
                "master_ratios keys must be workspace numbers like \"1\", got {key:?}"
            );
            anyhow::ensure!(
                *ratio > 0.1 && *ratio < 0.9,
                "master_ratios[{key:?}] must be between 0.1 and 0.9"
            );
        }
        anyhow::ensure!(
            matches!(
                self.main_mod.trim().to_ascii_lowercase().as_str(),
                "alt" | "ctrl" | "control" | "shift" | "win" | "super" | "meta"
            ),
            "main_mod must be one of alt, ctrl, shift, win"
        );
        anyhow::ensure!(
            matches!(
                self.animation_easing.trim().to_ascii_lowercase().as_str(),
                "linear" | "quad" | "cubic" | "expo"
            ),
            "animation_easing must be linear, quad, cubic or expo"
        );
        anyhow::ensure!(
            crate::log::Level::parse(&self.log_level).is_some(),
            "log_level must be error, warn, info, debug or off"
        );
        for rule in &self.rules {
            if rule.action == "float" {
                if let Some(arg) = &rule.arg {
                    if !arg.trim().is_empty() {
                        anyhow::ensure!(
                            size_limit(arg).is_some(),
                            "float rule arg must be a size like \"400x300\", got {arg:?}"
                        );
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve `main_mod` to a lowercase canonical token.
    pub fn main_mod_token(&self) -> String {
        match self.main_mod.trim().to_ascii_lowercase().as_str() {
            "control" => "ctrl".to_string(),
            "super" | "meta" => "win".to_string(),
            other => other.to_string(),
        }
    }
}

/// Parse a `WxH` size limit, e.g. `"400x300"`. Case-insensitive, spaces allowed.
pub fn size_limit(s: &str) -> Option<(i32, i32)> {
    let t = s.trim().to_ascii_lowercase();
    let (w, h) = t.split_once('x')?;
    let w: i32 = w.trim().parse().ok()?;
    let h: i32 = h.trim().parse().ok()?;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

/// Parse "#RRGGBB" or "#AARRGGBB" into a Win32 COLORREF (0x00BBGGRR).
pub fn parse_color(s: &str) -> u32 {
    let t = s.trim().trim_start_matches('#').trim_start_matches("0x");
    let v = u32::from_str_radix(t, 16).unwrap_or(0);
    let r = (v >> 16) & 0xFF;
    let g = (v >> 8) & 0xFF;
    let b = v & 0xFF;
    (b << 16) | (g << 8) | r
}

/// Parse "#RRGGBB" / "#AARRGGBB" into `(r, g, b, a)` for drawing on a 32-bit
/// DIBSection, whose pixels are BGRA little-endian.
///
/// Only the overlay uses this: DWM border colours are COLORREFs. An absent
/// alpha byte means fully opaque, which is what a six-digit hex colour means.
pub fn parse_rgba(s: &str) -> (u8, u8, u8, u8) {
    let t = s.trim().trim_start_matches('#').trim_start_matches("0x");
    let v = u32::from_str_radix(t, 16).unwrap_or(0);
    if t.len() >= 8 {
        ((v >> 16) as u8, (v >> 8) as u8, v as u8, (v >> 24) as u8)
    } else {
        ((v >> 16) as u8, (v >> 8) as u8, v as u8, 0xFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_parsing_is_bgr() {
        assert_eq!(parse_color("#FF8000"), 0x0080FF);
        assert_eq!(parse_color("0000FF"), 0xFF0000);
        // Alpha is ignored: DWM border colours do not support transparency.
        assert_eq!(parse_color("#80FF0000"), 0x0000FF);
    }

    #[test]
    fn rgba_parsing_keeps_order_and_defaults_to_opaque() {
        assert_eq!(parse_rgba("#7AA2F7"), (0x7A, 0xA2, 0xF7, 0xFF));
        assert_eq!(parse_rgba("#807AA2F7"), (0x7A, 0xA2, 0xF7, 0x80));
        assert_eq!(parse_rgba("garbage"), (0, 0, 0, 0xFF));
    }

    #[test]
    fn default_config_is_valid() {
        Config::default()
            .validate()
            .expect("defaults must validate");
    }

    #[test]
    fn main_mod_canonicalises_super() {
        let c = Config {
            main_mod: "Super".to_string(),
            ..Default::default()
        };
        assert_eq!(c.main_mod_token(), "win");
    }

    #[test]
    fn rejects_insane_master_ratio() {
        let c = Config {
            master_ratio: 0.95,
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn a_master_ratio_override_is_validated_per_workspace() {
        let mut c = Config::default();
        // A sensible per-workspace ratio is accepted.
        c.master_ratios.insert("3".to_string(), 0.7);
        assert!(c.validate().is_ok());
        // Its key must be a workspace number, and its value within the range.
        c.master_ratios.insert("left".to_string(), 0.5);
        assert!(
            c.validate().is_err(),
            "a non-numeric workspace key is rejected"
        );
        c.master_ratios.remove("left");
        c.master_ratios.insert("3".to_string(), 0.99);
        assert!(c.validate().is_err(), "an out-of-range ratio is rejected");
    }

    #[test]
    fn keybinds_compare_by_value_so_a_reload_can_tell_they_changed() {
        // `reload` re-registers the global hotkeys only when the parsed bindings
        // actually differ; the comparison has to see a changed key, modifier,
        // action and argument alike.
        let base = vec![
            Keybind {
                mods: vec!["main".to_string()],
                key: "Q".to_string(),
                action: "close".to_string(),
                arg: None,
            },
            Keybind {
                mods: vec!["main".to_string(), "shift".to_string()],
                key: "1".to_string(),
                action: "movetoworkspace".to_string(),
                arg: Some("1".to_string()),
            },
        ];
        assert_eq!(base.clone(), base.clone(), "an identical set is equal");

        let mut other_key = base.clone();
        other_key[0].key = "W".to_string();
        assert_ne!(base, other_key, "a changed key must be noticed");

        let mut other_mods = base.clone();
        other_mods[0].mods = vec!["main".to_string(), "ctrl".to_string()];
        assert_ne!(base, other_mods, "a changed modifier must be noticed");

        let mut other_arg = base.clone();
        other_arg[1].arg = Some("2".to_string());
        assert_ne!(base, other_arg, "a changed argument must be noticed");
    }

    #[test]
    fn a_bad_log_level_is_rejected_but_off_is_allowed() {
        let mut c = Config {
            log_level: "verbose".to_string(),
            ..Default::default()
        };
        assert!(c.validate().is_err());
        c.log_level = "off".to_string();
        assert!(
            c.validate().is_ok(),
            "`off` is a deliberate choice, not a typo"
        );
    }

    #[test]
    fn the_log_path_defaults_next_to_the_config() {
        let c = Config::default();
        assert_eq!(c.log_path(), Config::dir().join("hyprwin.log"));
        let c = Config {
            log_path: "D:\\logs\\hyprwin.log".to_string(),
            ..Default::default()
        };
        assert_eq!(c.log_path(), PathBuf::from("D:\\logs\\hyprwin.log"));
    }

    #[test]
    fn default_binds_cover_every_workspace() {
        let binds = default_keybinds();
        for n in 1..=9u32 {
            let k = n.to_string();
            assert!(binds
                .iter()
                .any(|b| b.action == "workspace" && b.arg.as_deref() == Some(k.as_str())));
            assert!(binds
                .iter()
                .any(|b| b.action == "movetoworkspace" && b.arg.as_deref() == Some(k.as_str())));
        }
    }

    #[test]
    fn direction_parsing() {
        assert_eq!(Direction::parse("LEFT"), Some(Direction::Left));
        assert_eq!(Direction::parse(" hmm "), None);
    }

    #[test]
    fn size_limits_parse_and_reject_nonsense() {
        assert_eq!(size_limit("400x300"), Some((400, 300)));
        assert_eq!(size_limit(" 1280 x 720 "), Some((1280, 720)));
        assert_eq!(size_limit("400X300"), Some((400, 300)));
        assert_eq!(size_limit("400"), None);
        assert_eq!(size_limit("wide"), None);
        assert_eq!(size_limit("0x300"), None, "a zero dimension is not a limit");
    }

    #[test]
    fn the_default_config_floats_tiny_windows() {
        // The size rule has no exe/class/title, so it must be the `arg` that
        // carries the meaning; a bare float rule matching everything would float
        // every window on the desktop.
        let c = Config::default();
        let tiny = c
            .rules
            .iter()
            .find(|r| r.action == "float" && r.arg.is_some())
            .expect("a size-based float rule ships by default");
        assert_eq!(size_limit(tiny.arg.as_deref().unwrap()), Some((400, 300)));
        assert!(tiny.exe.is_none() && tiny.class.is_none() && tiny.title.is_none());
    }

    #[test]
    fn a_malformed_float_rule_arg_is_rejected() {
        let c = Config {
            rules: vec![Rule {
                action: "float".to_string(),
                arg: Some("not-a-size".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }

    #[test]
    fn the_status_path_defaults_next_to_the_config() {
        let c = Config::default();
        assert_eq!(c.status_path(), Config::dir().join("status.json"));
        let c = Config {
            status_path: "D:\\skins\\hyprwin.json".to_string(),
            ..Default::default()
        };
        assert_eq!(c.status_path(), PathBuf::from("D:\\skins\\hyprwin.json"));
    }

    #[test]
    fn the_scratchpad_and_monitor_binds_are_registerable() {
        use crate::hotkey::vk_for;
        let binds = default_keybinds();
        for action in ["scratchpad", "movetoscratchpad", "movetomonitor"] {
            let b = binds
                .iter()
                .find(|b| b.action == action)
                .unwrap_or_else(|| panic!("a default bind for {action} must exist"));
            assert!(
                vk_for(&b.key).is_some(),
                "{action} bind key {} must map to a virtual key",
                b.key
            );
        }
    }
}
