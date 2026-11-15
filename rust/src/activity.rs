//! What the app did, one JSON line per event in logs/activity.jsonl:
//! moves (by auto-sort or by you), renames, undos, errors and start/stop notes.
//! Shown on the Activity page.
use crate::settings;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const MAX_BYTES: u64 = 4 * 1024 * 1024;
static LOCK: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

fn file() -> PathBuf {
    settings::log_dir().join("activity.jsonl")
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn append(entries: &[Value]) {
    if entries.is_empty() {
        return;
    }
    let _g = LOCK.lock().unwrap();
    let f = file();
    let _ = std::fs::create_dir_all(settings::log_dir());
    // keep it small: when it's big, start a new file and keep one old one
    if std::fs::metadata(&f).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&f, f.with_extension("old.jsonl"));
    }
    if let Ok(mut w) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
        let mut buf = String::new();
        for e in entries {
            buf.push_str(&e.to_string());
            buf.push('\n');
        }
        let _ = w.write_all(buf.as_bytes());
    }
}

fn s(p: &Path) -> String {
    p.to_string_lossy().to_string()
}

/// One moved/renamed/undone file.
pub struct Moved<'a> {
    pub from: &'a Path,
    pub to: &'a Path,
    pub category: &'a str,
    pub why: &'a str,
    /// "strong" | "weak" | "none" | ""
    pub conf: &'a str,
}

/// kind: "move" | "rename" | "undo"; by: "auto" | "you"; batch: the undo log it belongs to
pub fn moved(kind: &str, by: &str, batch: &str, items: &[Moved]) {
    let t = now();
    let entries: Vec<Value> = items
        .iter()
        .map(|m| {
            let root = settings::root_of(m.from).or_else(|| settings::root_of(m.to));
            json!({"t": t, "kind": kind, "by": by, "batch": batch, "from": s(m.from), "to": s(m.to),
                   "cat": m.category, "why": m.why, "conf": m.conf, "folder": root.map(|r| s(&r)).unwrap_or_default()})
        })
        .collect();
    append(&entries);
}

pub fn error(msg: &str, file: Option<&Path>) {
    let folder = file.and_then(settings::root_of).map(|r| s(&r)).unwrap_or_default();
    append(&[json!({"t": now(), "kind": "error", "msg": msg, "file": file.map(s), "folder": folder})]);
}

pub fn info(msg: &str) {
    append(&[json!({"t": now(), "kind": "info", "msg": msg})]);
}

/// Newest first, up to `limit` entries.
pub fn recent(limit: usize) -> Vec<Value> {
    let _g = LOCK.lock().unwrap();
    let mut lines: Vec<String> = Vec::new();
    for f in [file().with_extension("old.jsonl"), file()] {
        if let Ok(text) = std::fs::read_to_string(&f) {
            lines.extend(text.lines().map(String::from));
        }
    }
    lines.iter().rev().take(limit).filter_map(|l| serde_json::from_str(l).ok()).collect()
}
