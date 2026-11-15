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

/// A folder sorted in the background. mode: "instant" | "interval" | "off"
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct FolderCfg {
    pub path: String,
    pub mode: String,
}

/// A saved search that shows in the sidebar.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Album {
    pub name: String,
    pub q: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    /// folders sorted in the background (`folder` is only the one open in the window)
    pub folders: Vec<FolderCfg>,
    pub ui_zoom: f64,
    /// summary notification: "off" | "daily" | "weekly"
    pub summary: String,
    /// shrink screenshots to AVIF once they're this many days old (0 = off)
    pub auto_avif_days: u32,
    pub albums: Vec<Album>,
    /// the Review page adds a couple of telling words to the category you pick
    pub learn_words: bool,
    /// camera photos go to Photos/YYYY-MM instead of a screenshot category
    pub photos_folder: bool,
    /// ask a model in Ollama (on this PC) about leftovers - only when sorting by hand
    pub local_ai: bool,
    pub local_model: String,
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
            folders: vec![],
            ui_zoom: 1.0,
            summary: "daily".into(),
            auto_avif_days: 0,
            albums: vec![],
            learn_words: true,
            photos_folder: true,
            local_ai: false,
            local_model: String::new(),
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
    let mut s: Settings = std::fs::read_to_string(settings_file())
        .ok()
        .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
        .unwrap_or_default();
    // older versions had one folder with one auto-sort mode
    if s.folders.is_empty() && !s.folder.is_empty() && !s.extra.contains_key("folders_migrated") {
        s.folders.push(FolderCfg { path: s.folder.clone(), mode: s.auto_mode.clone() });
        s.extra.insert("folders_migrated".into(), Value::Bool(true));
        let _ = save(&s);
    }
    if !(0.5..=2.0).contains(&s.ui_zoom) {
        s.ui_zoom = 1.0;
    }
    s
}

impl Settings {
    /// Folders sorted in the background right now: (path, "instant" | "interval")
    pub fn active_folders(&self) -> Vec<(PathBuf, String)> {
        self.folders
            .iter()
            .filter(|f| (f.mode == "instant" || f.mode == "interval") && Path::new(&f.path).is_dir())
            .map(|f| (PathBuf::from(&f.path), f.mode.clone()))
            .collect()
    }
    pub fn auto_on(&self) -> bool {
        !self.active_folders().is_empty()
    }
}

/// The watched (or open) folder that a path is inside, if any.
pub fn root_of(p: &Path) -> Option<PathBuf> {
    let s = load();
    let lower = p.to_string_lossy().to_lowercase();
    s.folders
        .iter()
        .map(|f| f.path.clone())
        .chain(std::iter::once(s.folder.clone()))
        .filter(|r| !r.is_empty())
        .filter(|r| {
            let rl = r.trim_end_matches('\\').to_lowercase();
            lower.len() > rl.len() && lower.starts_with(&rl) && lower[rl.len()..].starts_with('\\')
        })
        .max_by_key(|r| r.len())
        .map(PathBuf::from)
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
