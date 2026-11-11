//! Windows hooks: start-at-login, Explorer right-click menu, removing the old daily task,
//! and starting background copies. Everything is per-user (HKEY_CURRENT_USER).
use crate::settings::Settings;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use winreg::enums::*;
use winreg::RegKey;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "ScreenshotSorter";
const MENU_KEYS: [&str; 2] = [
    r"Software\Classes\Directory\shell\ScreenshotSorter",
    r"Software\Classes\Directory\Background\shell\ScreenshotSorter",
];
const OLD_TASK: &str = "Screenshot Sorter (daily)";
pub const NO_WINDOW: u32 = 0x0800_0000;
const DETACHED: u32 = 0x0000_0008;

pub fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_default()
}

pub fn launch_command(args: &[&str]) -> String {
    let mut s = format!("\"{}\"", exe().display());
    for a in args {
        s.push(' ');
        s.push_str(a);
    }
    s
}

/// Start another copy of the app (e.g. the background sorter), independent of this one.
pub fn spawn(args: &[&str]) {
    let _ = Command::new(exe()).args(args).creation_flags(NO_WINDOW | DETACHED).spawn();
}

fn hkcu() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

pub fn set_start_at_login(on: bool) {
    if let Ok((k, _)) = hkcu().create_subkey(RUN_KEY) {
        if on {
            let _ = k.set_value(RUN_NAME, &launch_command(&["--tray"]));
        } else {
            let _ = k.delete_value(RUN_NAME);
        }
    }
}

pub fn set_explorer_menu(on: bool) {
    for key in MENU_KEYS {
        if on {
            if let Ok((k, _)) = hkcu().create_subkey(key) {
                let _ = k.set_value("MUIVerb", &"Sort with Screenshot Sorter");
                let _ = k.set_value("Icon", &exe().display().to_string());
            }
            if let Ok((k, _)) = hkcu().create_subkey(format!(r"{key}\command")) {
                let _ = k.set_value("", &launch_command(&["--gui-folder", "\"%V\""]));
            }
        } else {
            let _ = hkcu().delete_subkey_all(key);
        }
    }
}

/// The old once-a-day scheduled task is replaced by background sorting.
pub fn remove_daily_task() -> bool {
    Command::new("schtasks")
        .args(["/Delete", "/TN", OLD_TASK, "/F"])
        .creation_flags(NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn apply(s: &Settings) {
    set_start_at_login(s.start_tray_at_login && s.auto_mode != "off");
    set_explorer_menu(s.explorer_menu);
}

fn exe_in(cmd: &str) -> PathBuf {
    if let Some(rest) = cmd.strip_prefix('"') {
        PathBuf::from(rest.split('"').next().unwrap_or(""))
    } else {
        PathBuf::from(cmd.split(' ').next().unwrap_or(""))
    }
}

/// If the app folder was moved (the registered .exe no longer exists), point the login
/// entry and Explorer menu at this copy. Other copies never take over.
pub fn repair() {
    if let Ok(k) = hkcu().open_subkey(RUN_KEY) {
        if let Ok(cur) = k.get_value::<String, _>(RUN_NAME) {
            if cur != launch_command(&["--tray"]) && !exe_in(&cur).exists() {
                set_start_at_login(true);
            }
        }
    }
    if let Ok(k) = hkcu().open_subkey(format!(r"{}\command", MENU_KEYS[0])) {
        if let Ok(cur) = k.get_value::<String, _>("") {
            if cur != launch_command(&["--gui-folder", "\"%V\""]) && !exe_in(&cur).exists() {
                set_explorer_menu(true);
            }
        }
    }
}

/// Uninstall: remove the login entry and Explorer menu, if they point at this copy.
pub fn remove_all() {
    let ours = |cmd: Result<String, std::io::Error>| cmd.map(|c| exe_in(&c) == exe()).unwrap_or(false);
    if ours(hkcu().open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>(RUN_NAME))) {
        set_start_at_login(false);
    }
    if ours(hkcu().open_subkey(format!(r"{}\command", MENU_KEYS[0])).and_then(|k| k.get_value::<String, _>(""))) {
        set_explorer_menu(false);
    }
}

pub fn status() -> (bool, bool) {
    let login = hkcu().open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>(RUN_NAME)).is_ok();
    let menu = hkcu().open_subkey(MENU_KEYS[0]).is_ok();
    (login, menu)
}
