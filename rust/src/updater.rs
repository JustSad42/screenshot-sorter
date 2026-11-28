//! Updates come from this app's GitHub releases; the shared `app-update` crate does the work.
use crate::settings;
use serde_json::Value;

pub const REPO: &str = "JustSad42/screenshot-sorter";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn app() -> app_update::App {
    app_update::App {
        repo: REPO,
        id: "ScreenshotSorter",
        version: VERSION,
        state_file: settings::app_file("update_state.json"),
        uninstaller: settings::app_file("unins000.exe"),
    }
}

/// For the window. Uses a result less than 6 hours old unless `force`.
pub fn check(force: bool) -> Value {
    app().check(force)
}

/// Background: at most once a day, tell the user (once per version) that an update is out.
pub fn daily_check(notify: &dyn Fn(&str, &str)) {
    if settings::load().check_updates {
        app().daily_check(&|v| notify("Update available", &format!("Screenshot Sorter {v} is out. Open the app to update.")));
    }
}

/// Downloads, verifies and starts the newest installer.
pub fn install(log: &dyn Fn(&str)) -> Result<String, String> {
    app().install(log)
}
