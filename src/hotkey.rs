//! Global hotkey registration.
//!
//! Windows reserves `Win`+Arrow and `Win`+0-9, so bindings on those chords will
//! fail to register. We report the failures instead of silently dropping them.

use crate::config::Keybind;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};

#[derive(Debug, Clone)]
pub struct Registered {
    pub id: i32,
    pub bind: Keybind,
}

/// Map a key name to a virtual-key code.
pub fn vk_for(name: &str) -> Option<u32> {
    let n = name.trim();
    if n.is_empty() {
        return None;
    }
    if n.chars().count() == 1 {
        let c = n.chars().next().unwrap().to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
        return match c {
            '-' => Some(0xBD),
            '=' => Some(0xBB),
            '[' => Some(0xDB),
            ']' => Some(0xDD),
            ';' => Some(0xBA),
            '\'' => Some(0xDE),
            ',' => Some(0xBC),
            '.' => Some(0xBE),
            '/' => Some(0xBF),
            '\\' => Some(0xDC),
            '`' => Some(0xC0),
            _ => None,
        };
    }
    let lower = n.to_ascii_lowercase();
    let vk = match lower.as_str() {
        "tab" => 0x09,
        "space" => 0x20,
        "return" | "enter" => 0x0D,
        "escape" | "esc" => 0x1B,
        "backspace" => 0x08,
        "delete" | "del" => 0x2E,
        "insert" | "ins" => 0x2D,
        "home" => 0x24,
        "end" => 0x23,
        "pageup" | "prior" => 0x21,
        "pagedown" | "next" => 0x22,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        // Punctuation is normally written as the literal character, but accept
        // the spelled-out name too so a config cannot silently drop a binding
        // for writing "Comma" instead of ",".
        "comma" => 0xBC,
        "period" | "dot" => 0xBE,
        "slash" => 0xBF,
        "semicolon" => 0xBA,
        "backslash" => 0xDC,
        "minus" => 0xBD,
        "plus" | "equal" | "equals" => 0xBB,
        "apostrophe" | "quote" => 0xDE,
        "grave" | "backtick" => 0xC0,
        "lbracket" | "leftbracket" => 0xDB,
        "rbracket" | "rightbracket" => 0xDD,
        _ => {
            if let Some(rest) = lower.strip_prefix('f') {
                if let Ok(num) = rest.parse::<u32>() {
                    if (1..=24).contains(&num) {
                        return Some(0x70 + (num - 1));
                    }
                }
            }
            return None;
        }
    };
    Some(vk)
}

fn modifier_bit(token: &str, main_mod: &str) -> Option<u32> {
    let t = token.trim().to_ascii_lowercase();
    let resolved = if t == "main" { main_mod.to_string() } else { t };
    match resolved.as_str() {
        "alt" => Some(MOD_ALT.0),
        "ctrl" | "control" => Some(MOD_CONTROL.0),
        "shift" => Some(MOD_SHIFT.0),
        "win" | "super" | "meta" => Some(MOD_WIN.0),
        _ => None,
    }
}

/// Combine a binding's modifiers, resolving the `main` placeholder.
pub fn mods_for(bind: &Keybind, main_mod: &str) -> Result<HOT_KEY_MODIFIERS, String> {
    let mut bits = MOD_NOREPEAT.0;
    for m in &bind.mods {
        match modifier_bit(m, main_mod) {
            Some(b) => bits |= b,
            None => return Err(format!("unknown modifier '{m}'")),
        }
    }
    Ok(HOT_KEY_MODIFIERS(bits))
}

/// Register every binding, returning the ones that worked and why the rest failed.
pub fn register_all(
    binds: &[Keybind],
    main_mod: &str,
) -> (Vec<Registered>, Vec<(Keybind, String)>) {
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    let mut next_id: i32 = 1;
    for bind in binds {
        let mods = match mods_for(bind, main_mod) {
            Ok(m) => m,
            Err(e) => {
                failed.push((bind.clone(), e));
                continue;
            }
        };
        let vk = match vk_for(&bind.key) {
            Some(v) => v,
            None => {
                failed.push((bind.clone(), format!("unknown key '{}'", bind.key)));
                continue;
            }
        };
        let id = next_id;
        next_id += 1;
        // hwnd = None attaches the hotkey to this thread's message queue.
        match unsafe { RegisterHotKey(None, id, mods, vk) } {
            Ok(()) => ok.push(Registered {
                id,
                bind: bind.clone(),
            }),
            Err(e) => failed.push((
                bind.clone(),
                format!(
                    "RegisterHotKey failed ({}): Windows may already own this chord",
                    e.code()
                ),
            )),
        }
    }
    (ok, failed)
}

pub fn unregister_all(regs: &[Registered]) {
    for r in regs {
        unsafe {
            let _ = UnregisterHotKey(None, r.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(mods: &[&str], key: &str) -> Keybind {
        Keybind {
            mods: mods.iter().map(|s| s.to_string()).collect(),
            key: key.to_string(),
            action: "focus".to_string(),
            arg: Some("left".to_string()),
        }
    }

    #[test]
    fn letters_map_to_their_ascii_codes() {
        assert_eq!(vk_for("A"), Some(0x41));
        assert_eq!(vk_for("a"), Some(0x41));
        assert_eq!(vk_for("Z"), Some(0x5A));
    }

    #[test]
    fn digits_and_named_keys_map_correctly() {
        assert_eq!(vk_for("1"), Some(0x31));
        assert_eq!(vk_for("9"), Some(0x39));
        assert_eq!(vk_for("Space"), Some(0x20));
        assert_eq!(vk_for("TAB"), Some(0x09));
        assert_eq!(vk_for("Left"), Some(0x25));
        assert_eq!(vk_for("pagedown"), Some(0x22));
    }

    #[test]
    fn function_keys_are_ranged() {
        assert_eq!(vk_for("F1"), Some(0x70));
        assert_eq!(vk_for("F12"), Some(0x7B));
        assert_eq!(vk_for("F24"), Some(0x87));
        assert_eq!(vk_for("F25"), None);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert_eq!(vk_for("Banana"), None);
        assert_eq!(vk_for(""), None);
        assert_eq!(vk_for("F0"), None);
    }

    #[test]
    fn punctuation_accepts_both_the_symbol_and_its_name() {
        // The two forms must agree, or a spelled-out bind silently fails to
        // register while the same symbol works.
        assert_eq!(vk_for(","), vk_for("Comma"));
        assert_eq!(vk_for(","), Some(0xBC));
        assert_eq!(vk_for("."), vk_for("Period"));
        assert_eq!(vk_for("."), Some(0xBE));
        assert_eq!(vk_for("/"), vk_for("Slash"));
        assert_eq!(vk_for("["), vk_for("LeftBracket"));
    }

    #[test]
    fn the_default_workspace_cycle_binds_are_registerable() {
        // A default that the hotkey parser rejects would leave `main`+`,` and
        // `main`+`.` dead on a fresh install.
        for b in crate::config::Config::default().keybinds {
            if b.action == "workspace" && matches!(b.arg.as_deref(), Some("next") | Some("prev")) {
                assert!(
                    vk_for(&b.key).is_some(),
                    "default bind {}+{} must map to a virtual key",
                    b.mods.join("+"),
                    b.key
                );
            }
        }
    }

    #[test]
    fn main_modifier_resolves_to_alt() {
        let m = mods_for(&bind(&["main"], "H"), "alt").unwrap();
        assert_ne!(m.0 & MOD_ALT.0, 0);
        assert_ne!(m.0 & MOD_NOREPEAT.0, 0, "repeat must be suppressed");
    }

    #[test]
    fn stacked_modifiers_combine() {
        let m = mods_for(&bind(&["main", "shift"], "1"), "alt").unwrap();
        assert_ne!(m.0 & MOD_ALT.0, 0);
        assert_ne!(m.0 & MOD_SHIFT.0, 0);
    }

    #[test]
    fn super_resolves_to_win() {
        let m = mods_for(&bind(&["main"], "Q"), "win").unwrap();
        assert_ne!(m.0 & MOD_WIN.0, 0);
    }

    #[test]
    fn unknown_modifier_is_an_error() {
        assert!(mods_for(&bind(&["hyper"], "Q"), "alt").is_err());
    }
}
