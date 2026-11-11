//! Reading text out of images with Windows' built-in OCR (offline), plus the
//! on-disk cache of what was read (ocr_cache_v2.json, shared with the Python version).
use crate::settings;
use rayon::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;
use windows::core::HSTRING;
use windows::Globalization::Language;
use windows::Graphics::Imaging::BitmapDecoder;
use windows::Media::Ocr::OcrEngine;
use windows::Storage::{FileAccessMode, StorageFile};

pub type Cache = HashMap<String, String>;
static CACHE_LOCK: Mutex<()> = Mutex::new(());

/// "name|size|mtime" - the same key the Python version used.
pub fn cache_key(p: &Path) -> String {
    let (size, mtime) = std::fs::metadata(p)
        .map(|m| {
            let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
            (m.len(), t)
        })
        .unwrap_or((0, 0));
    format!("{}|{}|{}", p.file_name().unwrap_or_default().to_string_lossy(), size, mtime)
}

pub fn load_cache() -> Cache {
    std::fs::read_to_string(settings::cache_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_cache(c: &Cache) {
    let _g = CACHE_LOCK.lock();
    let _ = settings::write_atomic(&settings::cache_file(), serde_json::to_string(c).unwrap().as_bytes());
}

pub fn carry(cache: &mut Cache, old_key: &str, new_path: &Path) {
    if let Some(t) = cache.remove(old_key) {
        cache.insert(cache_key(new_path), t);
    }
}

fn arabic_chars(s: &str) -> usize {
    s.chars().filter(|c| ('\u{0600}'..='\u{06FF}').contains(c)).count()
}

thread_local! {
    static ENGINES: RefCell<Option<(String, Vec<OcrEngine>)>> = const { RefCell::new(None) };
}

pub fn init_thread() {
    unsafe {
        let _ = windows::Win32::System::WinRT::RoInitialize(windows::Win32::System::WinRT::RO_INIT_MULTITHREADED);
    }
}

fn make_engines(langs: &[String]) -> Vec<OcrEngine> {
    let available: Vec<String> = OcrEngine::AvailableRecognizerLanguages()
        .map(|v| v.into_iter().filter_map(|l| l.LanguageTag().ok().map(|t| t.to_string())).collect())
        .unwrap_or_default();
    let mut out: Vec<OcrEngine> = langs
        .iter()
        .filter(|l| available.contains(l))
        .filter_map(|l| {
            Language::CreateLanguage(&HSTRING::from(l.as_str())).ok().and_then(|lang| OcrEngine::TryCreateFromLanguage(&lang).ok())
        })
        .collect();
    if out.is_empty() {
        if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
            out.push(e);
        }
    }
    out
}

/// Windows' file API wants a full path with backslashes (C:/x/y.png is rejected).
pub fn win_path(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().replace('/', "\\")
}

fn ocr_one(engine: &OcrEngine, path: &Path) -> windows::core::Result<String> {
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(win_path(path)))?.get()?;
    let stream = file.OpenAsync(FileAccessMode::Read)?.get()?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.get()?;
    let bitmap = decoder.GetSoftwareBitmapAsync()?.get()?;
    let result = engine.RecognizeAsync(&bitmap)?.get()?;
    let mut parts = Vec::new();
    for line in result.Lines()? {
        parts.push(line.Text()?.to_string());
    }
    let _ = stream.Close();
    Ok(parts.join(" "))
}

/// Read one image with every configured language. Extra languages only add text when
/// they found their own script (the Arabic engine turns English into noise otherwise).
fn read_image(path: &Path, langs: &[String]) -> Result<String, String> {
    init_thread();
    let key = langs.join(",");
    ENGINES.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.as_ref().map(|(k, _)| k != &key).unwrap_or(true) {
            *slot = Some((key.clone(), make_engines(langs)));
        }
        let engines = &slot.as_ref().unwrap().1;
        let mut parts = Vec::new();
        for (i, e) in engines.iter().enumerate() {
            let t = ocr_one(e, path).map_err(|e| e.message().to_string())?;
            if i == 0 || arabic_chars(&t) >= 10 {
                parts.push(t);
            }
        }
        Ok(parts.join(" "))
    })
}

pub type Progress<'a> = &'a (dyn Fn(usize, usize) + Sync);
pub type Log<'a> = &'a (dyn Fn(&str) + Sync);

static POOL: once_cell::sync::Lazy<rayon::ThreadPool> =
    once_cell::sync::Lazy::new(|| rayon::ThreadPoolBuilder::new().num_threads(6).build().unwrap());

/// Text for each path, reading only images that aren't cached yet.
pub fn ocr_files(paths: &[PathBuf], progress: Progress, log: Log) -> HashMap<PathBuf, String> {
    let mut cache = load_cache();
    let mut out = HashMap::new();
    let mut todo = Vec::new();
    for p in paths {
        match cache.get(&cache_key(p)) {
            Some(t) => {
                out.insert(p.clone(), t.clone());
            }
            None => todo.push(p.clone()),
        }
    }
    if !todo.is_empty() {
        log(&format!("Reading text from {} new image(s)...", todo.len()));
        let langs = settings::load().languages;
        let done = AtomicUsize::new(0);
        let total = todo.len();
        let results: Vec<(PathBuf, Option<String>)> = POOL.install(|| {
            todo.par_iter()
                .map(|p| {
                    let text = read_image(p, &langs)
                        .map_err(|e| log(&format!("Could not read {}: {}", p.file_name().unwrap_or_default().to_string_lossy(), e)))
                        .ok();
                    progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
                    (p.clone(), text)
                })
                .collect()
        });
        for (p, t) in results {
            // a failed read isn't remembered, so it's tried again next time
            if let Some(t) = &t {
                cache.insert(cache_key(&p), t.clone());
            }
            out.insert(p, t.unwrap_or_default());
        }
        save_cache(&cache);
    }
    out
}
