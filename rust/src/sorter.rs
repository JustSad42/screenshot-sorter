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
    cut.trim_end_matches(|c| c == ' ' || c == '.').to_string()
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
    let mut dir = folder.join(category);
    if s.month_folders {
        dir = dir.join(taken_at(p).format("%Y-%m").to_string());
    }
    let name = if s.rename_files { readable_name(p, category, text, s) } else { p.file_name().unwrap().to_string_lossy().to_string() };
    dir.join(name)
}

// ---------- planning ----------

#[derive(Serialize, Clone, Debug)]
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
}

impl Item {
    pub fn reason(&self) -> String {
        if self.how == "text" {
            let mut seen = Vec::<String>::new();
            for h in self.hits.iter().take(5) {
                let w = rules::pretty_kw(h);
                if !w.is_empty() && !seen.contains(&w) {
                    seen.push(w);
                }
            }
            seen.join(", ")
        } else {
            self.how.clone()
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "p": self.path.to_string_lossy(), "n": self.path.file_name().unwrap_or_default().to_string_lossy(),
            "c": self.category, "r": self.reason(), "t": self.text.chars().take(700).collect::<String>()
        })
    }
}

/// Text rules first, then (optionally) looks, then Claude.
pub fn classify_files(files: &[PathBuf], folder: &Path, progress: Progress, log: Log, allow_claude: bool) -> Vec<Item> {
    let texts = ocr::ocr_files(files, progress, log);
    let rules = rules::load();
    let s = settings::load();
    let mut items: Vec<Item> = files
        .iter()
        .map(|p| {
            let t = texts.get(p).cloned().unwrap_or_default();
            let (c, hits, _) = rules.classify(&t);
            Item { path: p.clone(), category: c, hits, how: "text".into(), text: t }
        })
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
                        }
                    }
                }
            }
            Err(e) => log(&format!("Claude step skipped: {e}")),
        }
    }
    items
}

pub fn make_plan(folder: &Path, progress: Progress, log: Log, allow_claude: bool) -> Vec<Item> {
    classify_files(&loose_images(folder), folder, progress, log, allow_claude)
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

/// Moves files, keeps the text cache in step and writes an undo log. -> [(from, to)]
pub fn move_files(pairs: Vec<(PathBuf, PathBuf)>) -> Vec<(PathBuf, PathBuf)> {
    let log_path = new_log();
    let mut cache = ocr::load_cache();
    let mut done = Vec::new();
    for (src, dest) in pairs {
        if !src.exists() || src == dest {
            continue;
        }
        let old_key = ocr::cache_key(&src);
        let dest = unique_dest(&dest);
        if move_file(&src, &dest).is_ok() {
            ocr::carry(&mut cache, &old_key, &dest);
            done.push((src, dest));
        }
    }
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

pub fn apply_plan(folder: &Path, plan: &[Item]) -> Vec<(PathBuf, PathBuf)> {
    let s = settings::load();
    move_files(plan.iter().map(|it| (it.path.clone(), target_path(folder, &it.path, &it.category, &it.text, &s))).collect())
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
                (p.clone(), target_path(folder, p, category, &t, &s))
            })
            .collect(),
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
        pairs.push((p.clone(), p.with_file_name(readable_name(p, &cat, &t, &s))));
    }
    move_files(pairs)
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
    let mut rows = Vec::new();
    if let Ok(mut r) = csv::Reader::from_path(&log_path) {
        for rec in r.records().flatten() {
            if rec.len() >= 2 {
                rows.push((PathBuf::from(&rec[0]), PathBuf::from(&rec[1])));
            }
        }
    }
    let mut cache = ocr::load_cache();
    let mut moved = 0;
    for (from, to) in rows.iter().rev() {
        if to.exists() && !from.exists() {
            let k = ocr::cache_key(to);
            if move_file(to, from).is_ok() {
                ocr::carry(&mut cache, &k, from);
                moved += 1;
            }
        }
    }
    ocr::save_cache(&cache);
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
    let _ = std::fs::rename(&log_path, log_path.with_extension("undone"));
    (moved, true)
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
