//! Where the app keeps its files (next to the .exe, so it stays portable) and the
//! settings.json format (same as the Python version).
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

pub static APP_DIR: Lazy<PathBuf> = Lazy::new(|| {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
});

pub fn app_file(name: &str) -> PathBuf {
    APP_DIR.join(name)
}
pub fn rules_file() -> PathBuf {
    app_file("categories.json")
}
pub fn cache_file() -> PathBuf {
    app_file("ocr_cache_v2.json")
}
pub fn settings_file() -> PathBuf {
    app_file("settings.json")
}
pub fn log_dir() -> PathBuf {
    app_file("logs")
}

pub const DEFAULT_RULES: &str = include_str!("../categories.json");

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub folder: String,
    pub month_folders: bool,
    pub rename_files: bool,
    pub your_names: Vec<String>,
    pub languages: Vec<String>,
    pub use_look: bool,
    pub claude_enabled: bool,
    pub claude_api_key: String,
    pub auto_mode: String,
    pub interval_hours: f64,
    pub show_tray_icon: bool,
    pub notifications: bool,
    pub start_tray_at_login: bool,
    pub explorer_menu: bool,
    pub theme: String,
    pub check_updates: bool,
    /// keys we don't know about are kept as they are
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            folder: String::new(),
            month_folders: false,
            rename_files: false,
            your_names: vec![],
            languages: vec!["en-US".into(), "ar-SA".into()],
            use_look: true,
            claude_enabled: false,
            claude_api_key: String::new(),
            auto_mode: "off".into(),
            interval_hours: 6.0,
            show_tray_icon: true,
            notifications: true,
            start_tray_at_login: true,
            explorer_menu: true,
            theme: "system".into(),
            check_updates: true,
            extra: Map::new(),
        }
    }
}

pub fn load() -> Settings {
    std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default()
}

pub fn save(s: &Settings) -> std::io::Result<()> {
    write_atomic(&settings_file(), serde_json::to_string_pretty(s).unwrap().as_bytes())
}

/// Merge a JSON object of changes into the saved settings.
pub fn update(changes: &Map<String, Value>) -> Settings {
    let mut v = serde_json::to_value(load()).unwrap();
    if let Value::Object(m) = &mut v {
        for (k, val) in changes {
            m.insert(k.clone(), val.clone());
        }
    }
    let s: Settings = serde_json::from_value(v).unwrap_or_default();
    let _ = save(&s);
    s
}

pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

pub fn default_folder() -> PathBuf {
    let s = load();
    if !s.folder.is_empty() && Path::new(&s.folder).is_dir() {
        return PathBuf::from(s.folder);
    }
    let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    for c in [
        "OneDrive/Pictures/Screenshots 1",
        "OneDrive/Pictures/Screenshots",
        "Pictures/Screenshots",
        "Pictures",
    ] {
        let p = home.join(c);
        if p.is_dir() {
            return p;
        }
    }
    home
}

pub fn ensure_rules() {
    let f = rules_file();
    if !f.exists() {
        let _ = std::fs::write(f, DEFAULT_RULES);
    }
}

/// Append a timestamped line to a log file in logs/.
pub fn log_line(file: &str, msg: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(log_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(log_dir().join(file)) {
        let _ = writeln!(f, "{} {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg);
    }
}
