//! Planning where screenshots go, moving them (with undo logs), renaming and search.
use crate::ocr::{self, Log, Progress};
use crate::rules::{self, Rules};
use crate::settings::{self, Settings};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp", "tif", "tiff", "avif", "jxl", "heic", "heif"];

pub fn is_image(p: &Path) -> bool {
    p.extension().map(|e| IMAGE_EXTS.contains(&e.to_string_lossy().to_lowercase().as_str())).unwrap_or(false)
}

pub fn loose_images(folder: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(folder)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_file() && is_image(p)).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn all_images(folder: &Path) -> Vec<PathBuf> {
    fn walk(d: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if is_image(&p) {
                    out.push(p);
                }
            }
        }
    }
    let mut v = Vec::new();
    walk(folder, &mut v);
    v.sort();
    v
}

/// Top-level category folder a file lives in (None when it's loose).
pub fn category_of(folder: &Path, p: &Path) -> Option<String> {
    let rel = p.strip_prefix(folder).ok()?;
    let parts: Vec<_> = rel.components().collect();
    if parts.len() > 1 {
        Some(parts[0].as_os_str().to_string_lossy().to_string())
    } else {
        None
    }
}

// ---------- dates & names ----------

static DATE_RX: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"(\d{4})-(\d{2})-(\d{2})[ _T]?(\d{2})(\d{2})(\d{2})?",
        r"(\d{4})(\d{2})(\d{2})[_-](\d{2})(\d{2})(\d{2})?",
        r"(\d{4})-(\d{2})-(\d{2})",
    ]
    .iter()
    .map(|r| Regex::new(r).unwrap())
    .collect()
});

/// When the screenshot was taken: from the file name if it has a date, else the file time.
pub fn taken_at(p: &Path) -> DateTime<Local> {
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    for rx in DATE_RX.iter() {
        if let Some(c) = rx.captures(&stem) {
            let g = |i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok()).unwrap_or(0);
            if let Some(d) = NaiveDate::from_ymd_opt(g(1) as i32, g(2), g(3)).and_then(|d| d.and_hms_opt(g(4), g(5), g(6))) {
                if let Some(t) = Local.from_local_datetime(&d).earliest() {
                    return t;
                }
            }
        }
    }
    std::fs::metadata(p).and_then(|m| m.modified()).map(DateTime::<Local>::from).unwrap_or_else(|_| Local::now())
}

pub fn short_label(cat: &str) -> &str {
    match cat {
        "League - Stats & Builds" => "League",
        "League - In-Game" => "League game",
        "YouTube - Studio & Analytics" => "YT Studio",
        "YouTube & Twitch - Comments & Streams" => "Stream",
        "Discord & Chats" => "Chat",
        "Code & Terminal" => "Code",
        "Network & PC" => "Network",
        "Games & Steam" => "Games",
        "Web & Shopping" => "Web",
        other => other,
    }
}

static CHAT_NAME: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b([A-Za-z][\w']{2,20})\s+(?:\S\s?\S*\s)?(?:\d{1,2}:\d{2}|Yesterday at|Today at)").unwrap());
static RESULT_RX: Lazy<Regex> =
    Lazy::new(|| RegexBuilder::new(r"\b(Victory|Defeat|Win|Loss)\b").case_insensitive(true).build().unwrap());
static KDA_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b(\d{1,2}) ?/ ?(\d{1,2}) ?/ ?(\d{1,2})\b").unwrap());
static WORD_RX: Lazy<Regex> = Lazy::new(|| Regex::new(r"[A-Za-z][A-Za-z']{2,}").unwrap());
static BAD_CHARS: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap());
static WS: Lazy<Regex> = Lazy::new(|| Regex::new(r"\s+").unwrap());
static RENAMED: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2} \d{6} ").unwrap());
static MONTH_DIR: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\d{4}-\d{2}$").unwrap());

fn title(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase()).unwrap_or_default()
}

/// A few words that say what the screenshot is about.
pub fn describe(category: &str, text: &str, your_names: &[String]) -> String {
    let mine: HashSet<String> = your_names.iter().map(|n| n.to_lowercase()).collect();
    if category.starts_with("Discord") {
        for c in CHAT_NAME.captures_iter(text) {
            let n = c[1].to_lowercase();
            if !mine.contains(&n) && !rules::STOP.contains(n.as_str()) {
                return c[1].to_string();
            }
        }
    }
    if category.starts_with("League") {
        let res = RESULT_RX.captures(text).map(|c| title(&c[1])).unwrap_or_default();
        let kda = KDA_RX.captures(text).map(|c| format!("{}-{}-{}", &c[1], &c[2], &c[3])).unwrap_or_default();
        let bits: Vec<String> = [res, kda].into_iter().filter(|b| !b.is_empty()).collect();
        if !bits.is_empty() {
            return bits.join(" ");
        }
    }
    WORD_RX
        .find_iter(text)
        .map(|m| m.as_str())
        .filter(|w| !rules::STOP.contains(w.to_lowercase().as_str()))
        .take(4)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn safe_name(s: &str, limit: usize) -> String {
    let s = BAD_CHARS.replace_all(s, "");
    let s = WS.replace_all(&s, " ");
    let s = s.trim_matches(|c| c == ' ' || c == '.');
    let cut: String = s.chars().take(limit).collect();
    cut.trim_end_matches([' ', '.']).to_string()
}

fn readable_name(p: &Path, category: &str, text: &str, s: &Settings) -> String {
    let desc = safe_name(&describe(category, text, &s.your_names), 50);
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
    let mut name = format!("{} {}", taken_at(p).format("%Y-%m-%d %H%M%S"), short_label(category));
    if !desc.is_empty() {
        name += &format!(" - {desc}");
    }
    name + &ext
}

pub fn target_path(folder: &Path, p: &Path, category: &str, text: &str, s: &Settings) -> PathBuf {
    if category == crate::photo::PHOTOS {
        return crate::photo::target(folder, p, crate::photo::camera(p).as_ref(), s.rename_files);
    }
    let mut dir = folder.join(category);
    if s.month_folders {
        dir = dir.join(taken_at(p).format("%Y-%m").to_string());
    }
    let name = if s.rename_files { readable_name(p, category, text, s) } else { p.file_name().unwrap().to_string_lossy().to_string() };
    dir.join(name)
}

// ---------- planning ----------

#[derive(Serialize, Clone, Debug, Default)]
pub struct Item {
    #[serde(rename = "p")]
    pub path: PathBuf,
    #[serde(rename = "c")]
    pub category: String,
    #[serde(skip)]
    pub hits: Vec<String>,
    #[serde(skip)]
    pub how: String,
    #[serde(skip)]
    pub text: String,
    /// "strong" | "weak" | "none" (nothing matched) | "" (not judged, e.g. moved by you)
    #[serde(skip)]
    pub conf: String,
    /// app or window name that helped (from the file name or the picture's own details)
    #[serde(skip)]
    pub app: String,
}

impl Item {
    pub fn reason(&self) -> String {
        if self.how == "text" {
            let mut seen = Vec::<String>::new();
            if !self.app.is_empty() {
                seen.push(format!("app: {}", self.app));
            }
            for h in self.hits.iter().take(5) {
                let w = rules::pretty_kw(h);
                if !w.is_empty() && !seen.contains(&w) {
                    seen.push(w);
                }
            }
            if seen.is_empty() && !self.hits.is_empty() {
                "text pattern".to_string()
            } else {
                seen.join(", ")
            }
        } else {
            self.how.clone()
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "p": self.path.to_string_lossy(), "n": self.path.file_name().unwrap_or_default().to_string_lossy(),
            "c": self.category, "r": self.reason(), "t": self.text.chars().take(700).collect::<String>(), "conf": self.conf
        })
    }
}

// ---------- app hints: names that come with the picture ----------

static GENERIC_NAME: Lazy<Regex> = Lazy::new(|| {
    RegexBuilder::new(r"^(screenshot|screen ?shot|scr|img|image|photo|pic|capture|snip|clip|untitled|vlcsnap|dsc|pxl|wp|whatsapp image|signal|telegram|photo_)\b|^[\d\W_]+$")
        .case_insensitive(true)
        .build()
        .unwrap()
});
static DATE_BITS: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)\d{1,4}([ _\-.:]\d{1,4}){1,6}( ?[ap]m)?|\b\d+\b|\(\d+\)").unwrap());

/// Text chunks inside a PNG (title, description, software...), read before the image data.
fn png_text(p: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(p) else { return String::new() };
    let mut sig = [0u8; 8];
    if f.read_exact(&mut sig).is_err() || &sig != b"\x89PNG\r\n\x1a\n" {
        return String::new();
    }
    let mut out = Vec::new();
    for _ in 0..64 {
        let mut hd = [0u8; 8];
        if f.read_exact(&mut hd).is_err() {
            break;
        }
        let len = u32::from_be_bytes([hd[0], hd[1], hd[2], hd[3]]) as usize;
        let kind = &hd[4..8];
        if kind == b"IDAT" || kind == b"IEND" {
            break;
        }
        if (kind == b"tEXt" || kind == b"iTXt") && len < 4096 {
            let mut data = vec![0u8; len];
            if f.read_exact(&mut data).is_err() {
                break;
            }
            let _ = f.seek(SeekFrom::Current(4));
            let mut parts = data.splitn(2, |b| *b == 0);
            let key = String::from_utf8_lossy(parts.next().unwrap_or_default()).to_lowercase();
            let mut val = parts.next().unwrap_or_default().to_vec();
            if kind == b"iTXt" {
                // compression flag, method, language\0, translated keyword\0, text
                val = val.get(2..).unwrap_or_default().splitn(3, |b| *b == 0).nth(2).unwrap_or_default().to_vec();
            }
            if ["title", "description", "comment", "software", "source"].contains(&key.as_str()) {
                out.push(String::from_utf8_lossy(&val).chars().take(200).collect::<String>());
            }
        } else if f.seek(SeekFrom::Current(len as i64 + 4)).is_err() {
            break;
        }
    }
    out.join(" ")
}

/// App or window name that comes with a picture: from file names like Game Bar's
/// "League of Legends 11_14_2026 10_15_31 PM.png", and from text stored inside PNGs.
pub fn app_hint(p: &Path) -> String {
    if ocr::online_only(p) {
        return String::new(); // reading it would download it
    }
    let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let mut parts = Vec::new();
    if !RENAMED.is_match(&name) && !GENERIC_NAME.is_match(&stem) {
        let spaced = stem.replace(['_', '.'], " ");
        let cleaned = DATE_BITS.replace_all(&spaced, " ");
        let cleaned = WS.replace_all(cleaned.trim_matches(|c: char| !c.is_alphanumeric()), " ").trim().to_string();
        if cleaned.chars().filter(|c| c.is_alphabetic()).count() >= 3 {
            parts.push(cleaned);
        }
    }
    if p.extension().map(|e| e.eq_ignore_ascii_case("png")).unwrap_or(false) {
        let t = png_text(p);
        if !t.trim().is_empty() {
            parts.push(t.trim().to_string());
        }
    }
    parts.join(" · ")
}

/// Category, reason and confidence for one picture's text (plus app hint).
pub fn judge(rules: &Rules, path: &Path, text: &str) -> Item {
    let hint = app_hint(path);
    let short = text.trim().chars().count() < rules.min_chars;
    let combined = if hint.is_empty() { text.to_string() } else { format!("{text}\n{hint}") };
    let mut v = rules.verdict(&combined, false);
    if short && v.hits.is_empty() {
        v = rules.verdict(text, true); // no text, and the name didn't help either
    }
    let mut it = Item { path: path.to_path_buf(), category: v.cat.clone(), hits: v.hits.clone(), how: "text".into(), text: text.to_string(), ..Default::default() };
    if v.cat == rules.no_text {
        it.how = "no readable text".into();
        it.conf = "none".into();
    } else if v.cat == rules.fallback {
        let words = rules::top_words(text, 3);
        it.how = if words.is_empty() { "no category keywords matched".into() } else { format!("no category keywords matched · top words: {}", words.join(", ")) };
        it.conf = "none".into();
    } else {
        it.conf = v.confidence().into();
        let helped = !hint.is_empty() && rules.cats.iter().find(|c| c.name == v.cat).map(|c| c.keywords.iter().any(|(rx, _, _)| rx.is_match(&hint))).unwrap_or(false);
        if helped {
            it.app = hint.chars().take(60).collect();
        }
    }
    it
}

/// Text rules first, then (optionally) looks, then Claude.
/// Categories for the files whose text could be read. Files that couldn't be read
/// (or online-only OneDrive files) are left out, so they stay where they are.
pub fn classify_files(files: &[PathBuf], folder: &Path, progress: Progress, log: Log, allow_claude: bool) -> Vec<Item> {
    classify_files_ex(files, folder, progress, log, allow_claude, false)
}

pub fn classify_files_ex(files: &[PathBuf], folder: &Path, progress: Progress, log: Log, allow_claude: bool, download: bool) -> Vec<Item> {
    let s = settings::load();
    // camera photos aren't screenshots: they go to Photos/YYYY-MM without reading text
    let mut photos: Vec<Item> = Vec::new();
    let mut shots: Vec<PathBuf> = Vec::new();
    for p in files {
        match s.photos_folder.then(|| crate::photo::camera(p)).flatten() {
            Some(cam) => photos.push(Item { path: p.clone(), category: crate::photo::PHOTOS.into(), how: crate::photo::reason(&cam), conf: "strong".into(), ..Default::default() }),
            None => shots.push(p.clone()),
        }
    }
    let files = &shots[..];
    let texts = ocr::ocr_files_ex(files, progress, log, download);
    let rules = rules::load();
    let mut items: Vec<Item> = files
        .iter()
        .filter_map(|p| texts.get(p).map(|t| judge(&rules, p, t)))
        .collect();

    let left = |items: &Vec<Item>, r: &Rules| -> Vec<usize> {
        items.iter().enumerate().filter(|(_, it)| it.category == r.fallback || it.category == r.no_text).map(|(i, _)| i).collect()
    };
    let idx = left(&items, &rules);
    if !idx.is_empty() && s.use_look {
        let paths: Vec<PathBuf> = idx.iter().map(|&i| items[i].path.clone()).collect();
        let guesses = crate::vision::guess(folder, &paths, &rules, log);
        for &i in &idx {
            if let Some((cat, conf)) = guesses.get(&items[i].path) {
                items[i].category = cat.clone();
                items[i].how = format!("looks like {} ({:.0}% sure)", cat, conf * 100.0);
                items[i].conf = if *conf >= 0.9 { "strong" } else { "weak" }.into();
            }
        }
    }
    let idx = left(&items, &rules);
    if !idx.is_empty() && allow_claude && s.claude_enabled {
        let paths: Vec<PathBuf> = idx.iter().map(|&i| items[i].path.clone()).collect();
        match crate::claude::classify(&paths, &rules, &s, progress, log) {
            Ok(answers) => {
                for &i in &idx {
                    if let Some(a) = answers.get(&items[i].path) {
                        if a != &items[i].category {
                            items[i].category = a.clone();
                            items[i].how = "Claude".into();
                            items[i].conf = "weak".into();
                        }
                    }
                }
            }
            Err(e) => log(&format!("Claude step skipped: {e}")),
        }
    }
    items.extend(photos);
    items
}

/// Leftovers (Other / No Text) get a suggestion from the local model, if it's turned on.
/// Called only for sorting you start yourself, never by auto-sort.
pub fn local_ai_pass(items: &mut [Item], progress: Progress, log: Log) {
    let s = settings::load();
    let Some(model) = crate::localai::model(&s) else { return };
    let rules = rules::load();
    let idx: Vec<usize> = items.iter().enumerate().filter(|(_, it)| it.category == rules.fallback || it.category == rules.no_text).map(|(i, _)| i).collect();
    if idx.is_empty() {
        return;
    }
    let asks: Vec<(PathBuf, String)> = idx.iter().map(|&i| (items[i].path.clone(), items[i].text.clone())).collect();
    let answers = crate::localai::classify(&model, &asks, &rules, progress, log);
    for &i in &idx {
        if let Some((cat, why)) = answers.get(&items[i].path) {
            // "none of these" keeps what the rules said (No Text stays No Text)
            if cat != &items[i].category && cat != &rules.fallback {
                items[i].category = cat.clone();
                items[i].how = if why.is_empty() { "local AI".into() } else { format!("local AI: {why}") };
                items[i].conf = "weak".into();
            }
        }
    }
}

pub fn make_plan(folder: &Path, progress: Progress, log: Log, allow_claude: bool) -> Vec<Item> {
    classify_files(&loose_images(folder), folder, progress, log, allow_claude)
}

/// Camera photos that are filed as screenshots (or loose): (root, path, camera)
pub fn misfiled_photos(folders: &[PathBuf]) -> Vec<(PathBuf, PathBuf, crate::photo::Camera)> {
    use rayon::prelude::*;
    let all: Vec<(PathBuf, PathBuf)> = folders
        .iter()
        .flat_map(|root| all_images(root).into_iter().map(move |p| (root.clone(), p)))
        .filter(|(root, p)| category_of(root, p).as_deref() != Some(crate::photo::PHOTOS))
        .collect();
    all.into_par_iter().filter_map(|(root, p)| crate::photo::camera(&p).map(|c| (root, p, c))).collect()
}

/// Move camera photos into Photos/YYYY-MM (undoable, as one batch). -> moves made
pub fn file_photos(folders: &[PathBuf], progress: Progress, log: Log) -> Vec<(PathBuf, PathBuf)> {
    log("Looking for camera photos...");
    let found = misfiled_photos(folders);
    progress(found.len(), found.len());
    let s = settings::load();
    let planned: Vec<Planned> = found
        .iter()
        .map(|(root, p, cam)| (p.clone(), crate::photo::target(root, p, Some(cam), s.rename_files), crate::photo::PHOTOS.to_string(), crate::photo::reason(cam), "strong".to_string()))
        .collect();
    log(&format!("Moving {} photo(s)...", planned.len()));
    move_files(planned, "move", "you")
}

/// Images that were never read: online-only OneDrive files that are loose or in the
/// "No Text" folder, plus anything in "No Text" with no cached text.
pub fn unread_images(folder: &Path) -> Vec<PathBuf> {
    let rules = rules::load();
    let cache = ocr::load_cache();
    all_images(folder)
        .into_iter()
        .filter(|p| {
            let cat = category_of(folder, p);
            (cat.is_none() || cat.as_deref() == Some(rules.no_text.as_str())) && !cache.contains_key(&ocr::cache_key(p))
        })
        .collect()
}

/// Download (if needed), read and file the unread images. Files that turn out to have
/// no text stay in (or go to) "No Text". -> moves made
pub fn fetch_and_sort(folder: &Path, progress: Progress, log: Log) -> Vec<(PathBuf, PathBuf)> {
    let files = unread_images(folder);
    if files.is_empty() {
        return Vec::new();
    }
    log(&format!("Downloading and reading {} file(s)...", files.len()));
    let items: Vec<Item> = classify_files_ex(&files, folder, progress, log, true, true)
        .into_iter()
        .filter(|it| category_of(folder, &it.path).as_deref() != Some(it.category.as_str()))
        .collect();
    apply_plan(folder, &items, "auto")
}

// ---------- moving & undo ----------

pub fn unique_dest(dest: &Path) -> PathBuf {
    if !dest.exists() {
        return dest.to_path_buf();
    }
    let stem = dest.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = dest.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|i| dest.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|c| !c.exists())
        .unwrap()
}

pub fn move_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    if let Some(p) = dst.parent() {
        std::fs::create_dir_all(p)?;
    }
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            std::fs::copy(src, dst)?;
            let mtime = filetime::FileTime::from_last_modification_time(&std::fs::metadata(src)?);
            filetime::set_file_mtime(dst, mtime)?;
            std::fs::remove_file(src)
        }
    }
}

fn new_log() -> PathBuf {
    let _ = std::fs::create_dir_all(settings::log_dir());
    settings::log_dir().join(format!("moves_{}.csv", Local::now().format("%Y%m%d_%H%M%S_%6f")))
}

/// One move at a time, whether it comes from auto-sort or the window.
static MOVE_LOCK: Lazy<std::sync::Mutex<()>> = Lazy::new(|| std::sync::Mutex::new(()));

/// A planned move: (from, to, category, why)
/// A planned move: (from, to, category, why, confidence)
pub type Planned = (PathBuf, PathBuf, String, String, String);

/// Moves files, keeps the text cache in step, writes an undo log and the activity log.
/// kind: "move" | "rename"; by: "auto" | "you". -> [(from, to)]
pub fn move_files(pairs: Vec<Planned>, kind: &str, by: &str) -> Vec<(PathBuf, PathBuf)> {
    let _g = MOVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let log_path = new_log();
    let batch = log_path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let mut cache = ocr::load_cache();
    let mut done = Vec::new();
    let mut logged: Vec<Planned> = Vec::new();
    for (src, dest, cat, why, conf) in pairs {
        if !src.exists() || src == dest {
            continue;
        }
        let old_key = ocr::cache_key(&src);
        let dest = unique_dest(&dest);
        match move_file(&src, &dest) {
            Ok(()) => {
                ocr::carry(&mut cache, &old_key, &dest);
                logged.push((src.clone(), dest.clone(), cat, why, conf));
                done.push((src, dest));
            }
            Err(e) => crate::activity::error(&format!("Couldn't move {}: {e}", src.file_name().unwrap_or_default().to_string_lossy()), Some(&src)),
        }
    }
    let items: Vec<crate::activity::Moved> =
        logged.iter().map(|(f, t, c, w, k)| crate::activity::Moved { from: f, to: t, category: c, why: w, conf: k }).collect();
    crate::activity::moved(kind, by, &batch, &items);
    if !done.is_empty() {
        if let Ok(mut w) = csv::Writer::from_path(&log_path) {
            let _ = w.write_record(["from", "to"]);
            for (a, b) in &done {
                let _ = w.write_record([a.to_string_lossy().as_ref(), b.to_string_lossy().as_ref()]);
            }
            let _ = w.flush();
        }
        ocr::save_cache(&cache);
    }
    done
}

/// by: "auto" (background) or "you"
pub fn apply_plan(folder: &Path, plan: &[Item], by: &str) -> Vec<(PathBuf, PathBuf)> {
    let s = settings::load();
    move_files(
        plan.iter()
            .map(|it| (it.path.clone(), target_path(folder, &it.path, &it.category, &it.text, &s), it.category.clone(), it.reason(), it.conf.clone()))
            .collect(),
        "move",
        by,
    )
}

/// A correction: move files into another category folder.
pub fn move_to_category(folder: &Path, paths: &[PathBuf], category: &str) -> Vec<(PathBuf, PathBuf)> {
    let s = settings::load();
    let cache = ocr::load_cache();
    move_files(
        paths
            .iter()
            .map(|p| {
                let t = cache.get(&ocr::cache_key(p)).cloned().unwrap_or_default();
                let root = settings::root_of(p).unwrap_or_else(|| folder.to_path_buf());
                (p.clone(), target_path(&root, p, category, &t, &s), category.to_string(), "moved by you".to_string(), String::new())
            })
            .collect(),
        "move",
        "you",
    )
}

/// Give already-sorted files readable names (undoable).
pub fn rename_existing(folder: &Path, progress: Progress, log: Log) -> Vec<(PathBuf, PathBuf)> {
    let files: Vec<PathBuf> = all_images(folder).into_iter().filter(|p| p.parent() != Some(folder)).collect();
    let texts = ocr::ocr_files(&files, progress, log);
    let s = settings::load();
    let mut pairs = Vec::new();
    for p in &files {
        let parent = p.parent().unwrap();
        let pname = parent.file_name().unwrap_or_default().to_string_lossy().to_string();
        let cat = if MONTH_DIR.is_match(&pname) {
            parent.parent().and_then(|g| g.file_name()).unwrap_or_default().to_string_lossy().to_string()
        } else {
            pname
        };
        let name = p.file_name().unwrap().to_string_lossy();
        if RENAMED.is_match(&name) {
            continue;
        }
        let t = texts.get(p).cloned().unwrap_or_default();
        pairs.push((p.clone(), p.with_file_name(readable_name(p, &cat, &t, &s)), cat, "readable name".to_string(), String::new()));
    }
    move_files(pairs, "rename", "you")
}

/// Put one file back where it was (from the Activity page).
pub fn undo_one(from: &Path, to: &Path) -> Result<(), String> {
    let _g = MOVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !to.exists() {
        return Err("The file isn't there any more.".into());
    }
    if from.exists() {
        return Err("Another file already has its old name.".into());
    }
    let mut cache = ocr::load_cache();
    let k = ocr::cache_key(to);
    move_file(to, from).map_err(|e| e.to_string())?;
    ocr::carry(&mut cache, &k, from);
    ocr::save_cache(&cache);
    if let Some(d) = to.parent() {
        if std::fs::read_dir(d).map(|mut r| r.next().is_none()).unwrap_or(false) {
            let _ = std::fs::remove_dir(d);
        }
    }
    crate::activity::moved("undo", "you", "", &[crate::activity::Moved { from: to, to: from, category: "", why: "undone", conf: "" }]);
    crate::background::skip(&[from.to_path_buf()]);
    Ok(())
}

/// Move everything from the most recent action back. -> (count, had_something_to_undo)
pub fn undo_last() -> (usize, bool) {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(settings::log_dir())
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let n = p.file_name().unwrap_or_default().to_string_lossy();
                    n.starts_with("moves_") && n.ends_with(".csv")
                })
                .collect()
        })
        .unwrap_or_default();
    logs.sort();
    let Some(log_path) = logs.pop() else { return (0, false) };
    (undo_log(&log_path), true)
}

/// Undo one whole sort (a batch on the Activity page). -> files moved back
pub fn undo_batch(batch: &str) -> Result<usize, String> {
    if batch.is_empty() || batch.contains(['/', '\\', '.']) {
        return Err("Unknown batch".into());
    }
    let p = settings::log_dir().join(format!("{batch}.csv"));
    if !p.exists() {
        return Err(if p.with_extension("undone").exists() { "That sort was already undone.".into() } else { "Its undo log is gone.".into() });
    }
    Ok(undo_log(&p))
}

/// Undo batches already undone, for the Activity page.
pub fn undone_batches() -> Vec<String> {
    std::fs::read_dir(settings::log_dir())
        .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".undone")).map(String::from)).collect())
        .unwrap_or_default()
}

fn undo_log(log_path: &Path) -> usize {
    let mut rows = Vec::new();
    if let Ok(mut r) = csv::Reader::from_path(log_path) {
        for rec in r.records().flatten() {
            if rec.len() >= 2 {
                rows.push((PathBuf::from(&rec[0]), PathBuf::from(&rec[1])));
            }
        }
    }
    let _g = MOVE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut cache = ocr::load_cache();
    let mut moved = 0;
    let mut back: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (from, to) in rows.iter().rev() {
        if to.exists() && !from.exists() {
            let k = ocr::cache_key(to);
            if move_file(to, from).is_ok() {
                ocr::carry(&mut cache, &k, from);
                back.push((to.clone(), from.clone()));
                moved += 1;
            }
        }
    }
    ocr::save_cache(&cache);
    let items: Vec<crate::activity::Moved> =
        back.iter().map(|(a, b)| crate::activity::Moved { from: a, to: b, category: "", why: "undone", conf: "" }).collect();
    crate::activity::moved("undo", "you", "", &items);
    crate::background::skip(&back.iter().map(|(_, b)| b.clone()).collect::<Vec<_>>());
    let mut dirs: Vec<PathBuf> = rows.iter().filter_map(|(_, to)| to.parent().map(Path::to_path_buf)).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    for d in dirs {
        for dd in [d.clone(), d.parent().map(Path::to_path_buf).unwrap_or_default()] {
            if dd.is_dir() && std::fs::read_dir(&dd).map(|mut r| r.next().is_none()).unwrap_or(false) {
                let _ = std::fs::remove_dir(&dd);
            }
        }
    }
    let _ = std::fs::rename(log_path, log_path.with_extension("undone"));
    moved
}

/// Images under folder whose text contains query -> [(path, snippet)]
pub fn search(folder: &Path, query: &str, progress: Progress, log: Log) -> Vec<(PathBuf, String)> {
    let files = all_images(folder);
    let texts = ocr::ocr_files(&files, progress, log);
    let q = query.to_lowercase();
    let mut hits = Vec::new();
    for p in files {
        let text = texts.get(&p).cloned().unwrap_or_default();
        let lower = text.to_lowercase();
        if let Some(byte) = lower.find(&q) {
            // snippet by characters so multi-byte text never splits
            let ci = lower[..byte].chars().count();
            let chars: Vec<char> = text.chars().collect();
            let a = ci.saturating_sub(40);
            let b = (ci + 60).min(chars.len());
            hits.push((p, chars[a..b].iter().collect()));
        }
    }
    hits
}

/// Counts for the Insights page.
pub fn insights(folder: &Path) -> serde_json::Value {
    let files = all_images(folder);
    let mut cats: HashMap<String, usize> = HashMap::new();
    let mut months: BTreeMap<String, usize> = BTreeMap::new();
    let mut size = 0u64;
    for p in &files {
        *cats.entry(category_of(folder, p).unwrap_or_else(|| "Unsorted".into())).or_default() += 1;
        *months.entry(taken_at(p).format("%Y-%m").to_string()).or_default() += 1;
        size += std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    }
    let mut cats: Vec<(String, usize)> = cats.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    serde_json::json!({"total": files.len(), "size": size, "cats": cats, "months": months.into_iter().collect::<Vec<_>>()})
}
