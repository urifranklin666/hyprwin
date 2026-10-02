//! Optional "run at login" support via the per-user Run key.

use anyhow::{Context, Result};
use std::os::windows::ffi::OsStrExt;
use windows::core::w;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
    KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
};

const RUN_KEY: windows::core::PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: windows::core::PCWSTR = w!("HyprWin");

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(Some(0))
        .collect()
}

/// Enable or disable autostart. Returns the value that was previously stored.
pub fn set_enabled(enabled: bool) -> Result<()> {
    if enabled {
        let exe = std::env::current_exe().context("locating the running executable")?;
        let exe = exe.to_string_lossy().to_string();
        let data = wide(&exe);
        let bytes: Vec<u8> = data.iter().flat_map(|u| u.to_le_bytes()).collect();
        unsafe {
            let mut hkey = HKEY::default();
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                0,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
            .ok()
            .context("opening the Run registry key")?;
            let res = RegSetValueExW(hkey, VALUE_NAME, 0, REG_SZ, Some(&bytes));
            let _ = RegCloseKey(hkey);
            res.ok().context("writing the autostart value")?;
        }
    } else {
        unsafe {
            let mut hkey = HKEY::default();
            if RegCreateKeyExW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                0,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
            .is_ok()
            {
                let _ = RegDeleteValueW(hkey, VALUE_NAME);
                let _ = RegCloseKey(hkey);
            }
        }
    }
    Ok(())
}

/// Whether the Run key currently points at an executable.
#[allow(dead_code)]
pub fn is_enabled() -> bool {
    use windows::Win32::System::Registry::{RegQueryValueExW, KEY_QUERY_VALUE};
    unsafe {
        let mut hkey = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE,
            None,
            &mut hkey,
            None,
        )
        .is_err()
        {
            return false;
        }
        let mut kind = REG_SZ;
        let mut size = 0u32;
        let res = RegQueryValueExW(
            hkey,
            VALUE_NAME,
            None,
            Some(&mut kind),
            None,
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        res.is_ok() && size > 0
    }
}
