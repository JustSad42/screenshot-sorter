//! Background auto-sort, running inside the app (the same process as the window and tray).
//! Each watched folder is either
//! - "instant":  new screenshots are filed a few seconds after they're saved (Windows tells us
//!               when a folder changes; a slow scan every 30 s catches anything missed), or
//! - "interval": the folder is swept every N hours.
//! The named mutex/event let the installer (and `--stop`) find and close the running app.

use crate::settings::{self, log_line};
use crate::{activity, sorter};
use once_cell::sync::OnceCell;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE, SYNCHRONIZATION_SYNCHRONIZE,
};

const SETTLE: Duration = Duration::from_secs(2); // wait until a new file has stopped changing
const SCAN: Duration = Duration::from_secs(30); // fallback scan of "instant" folders

fn state_file() -> PathBuf {
    settings::app_file("background_state.json")
}

// ---------- files put back with Undo: auto-sort leaves them alone ----------

static SKIP: once_cell::sync::Lazy<Mutex<std::collections::HashSet<String>>> = once_cell::sync::Lazy::new(|| {
    let set = read_state()["skip"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).filter(|p| Path::new(p).exists()).map(String::from).collect())
        .unwrap_or_default();
    Mutex::new(set)
});

fn skip_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Don't auto-sort these files (they were just put back by Undo).
pub fn skip(paths: &[PathBuf]) {
    let mut s = SKIP.lock().unwrap();
    s.extend(paths.iter().map(|p| skip_key(p)));
    s.retain(|p| Path::new(p).exists());
    write_state("skip", json!(s.iter().collect::<Vec<_>>()));
}

fn skipped(p: &Path) -> bool {
    SKIP.lock().unwrap().contains(&skip_key(p))
}

/// Loose screenshots in a folder that auto-sort may move.
fn sortable(folder: &Path) -> Vec<PathBuf> {
    sorter::loose_images(folder).into_iter().filter(|p| !skipped(p)).collect()
}

pub fn read_state() -> Value {
    std::fs::read_to_string(state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}

fn write_state(k: &str, v: Value) {
    let mut s = read_state();
    s[k] = v;
    let _ = std::fs::write(state_file(), s.to_string());
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn folder_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| p.display().to_string())
}

fn last_sweep(st: &Value, f: &Path) -> f64 {
    st["sweeps"][f.to_string_lossy().as_ref()]
        .as_f64()
        .or_else(|| st["last_sweep"].as_f64()) // older versions kept one time for one folder
        .unwrap_or(0.0)
}

// ---------- the app's single-instance handles ----------

/// Is the app (with background sorting) running?
pub fn is_running() -> bool {
    unsafe {
        match OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, w!("ScreenshotSorterBackground")) {
            Ok(h) => {
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

/// Ask the running app to quit (used by the installer through --stop).
pub fn request_stop() -> bool {
    unsafe {
        match OpenEventW(EVENT_MODIFY_STATE, false, w!("ScreenshotSorterBackgroundStop")) {
            Ok(h) => {
                let _ = SetEvent(h);
                let _ = CloseHandle(h);
                true
            }
            Err(_) => false,
        }
    }
}

struct Handle(#[allow(dead_code)] HANDLE);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
static APP_MUTEX: OnceCell<Handle> = OnceCell::new();

/// Mark this process as the running app. An older copy still running in the background is
/// asked to quit first, so there is only ever one.
pub fn claim_app(on_quit_request: impl FnOnce() + Send + 'static) {
    for attempt in 0..2 {
        let h = unsafe { CreateMutexW(None, false, w!("ScreenshotSorterBackground")) };
        let taken = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if let Ok(h) = h {
            if !taken || attempt == 1 {
                let _ = APP_MUTEX.set(Handle(h));
                break;
            }
            unsafe {
                let _ = CloseHandle(h);
            }
        }
        request_stop();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(200));
            if !is_running() {
                break;
            }
        }
    }
    std::thread::spawn(move || unsafe {
        if let Ok(ev) = CreateEventW(None, true, false, w!("ScreenshotSorterBackgroundStop")) {
            WaitForSingleObject(ev, INFINITE);
            on_quit_request();
        }
    });
}

// ---------- the worker ----------

pub struct Worker {
    stop: AtomicBool,
    pub paused: AtomicBool,
    wake: (Mutex<bool>, Condvar),
    seen: Mutex<HashMap<PathBuf, (u64, u64, Instant)>>,
    notify: Option<Box<dyn Fn(&str, &str) + Send + Sync>>,
    /// what it's doing right now, for the Activity page
    busy: Mutex<String>,
    watching: Mutex<Vec<PathBuf>>,
}

pub static WORKER: OnceCell<Arc<Worker>> = OnceCell::new();
/// Called after something changes (a sort, pause), e.g. to refresh the tray tooltip.
pub static ON_CHANGE: OnceCell<Box<dyn Fn() + Send + Sync>> = OnceCell::new();

fn changed() {
    if let Some(f) = ON_CHANGE.get() {
        f();
    }
}

/// Start the worker thread (once per process).
pub fn start(notify: Option<Box<dyn Fn(&str, &str) + Send + Sync>>) -> Arc<Worker> {
    WORKER
        .get_or_init(|| {
            let w = Arc::new(Worker {
                stop: false.into(),
                paused: false.into(),
                wake: (Mutex::new(false), Condvar::new()),
                seen: Default::default(),
                notify,
                busy: Default::default(),
                watching: Default::default(),
            });
            let w2 = w.clone();
            std::thread::spawn(move || w2.run());
            w
        })
        .clone()
}

/// Wake the worker now (settings changed, a folder changed).
pub fn wake() {
    if let Some(w) = WORKER.get() {
        w.wake();
    }
}

pub fn set_paused(p: bool) {
    if let Some(w) = WORKER.get() {
        w.paused.store(p, Ordering::Relaxed);
        activity::info(if p { "Auto-sort paused" } else { "Auto-sort resumed" });
        w.wake();
        changed();
    }
}

pub fn is_paused() -> bool {
    WORKER.get().map(|w| w.paused.load(Ordering::Relaxed)).unwrap_or(false)
}

impl Worker {
    fn wake(&self) {
        let (m, c) = &self.wake;
        *m.lock().unwrap() = true;
        c.notify_all();
    }

    /// Sleep up to `d` (or until woken). -> true when stopping.
    fn wait(&self, d: Duration) -> bool {
        let (m, c) = &self.wake;
        let mut woke = m.lock().unwrap();
        if !*woke {
            woke = c.wait_timeout(woke, d).unwrap().0;
        }
        *woke = false;
        self.stop.load(Ordering::Relaxed)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.wake();
    }

    fn set_busy(&self, s: &str) {
        *self.busy.lock().unwrap() = s.to_string();
        changed();
    }

    fn ready_files(&self, folder: &Path) -> Vec<PathBuf> {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        let mut ready = Vec::new();
        let current = sortable(folder);
        for p in &current {
            let Ok(m) = std::fs::metadata(p) else { continue };
            let mtime = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0);
            let age = m.modified().ok().and_then(|t| t.elapsed().ok()).unwrap_or_default();
            match seen.get(p) {
                Some(&(s, t, first)) if s == m.len() && t == mtime => {
                    if now - first >= SETTLE && age >= SETTLE {
                        ready.push(p.clone());
                    }
                }
                _ => {
                    seen.insert(p.clone(), (m.len(), mtime, now));
                }
            }
        }
        seen.retain(|p, _| p.parent() != Some(folder) || current.contains(p));
        ready
    }

    /// Sort these files in `folder`. -> how many moved
    pub fn sort_files(&self, folder: &Path, files: &[PathBuf], several_folders: bool) -> usize {
        if files.is_empty() {
            return 0;
        }
        self.set_busy(&format!("Sorting {} new screenshot{} in {}", files.len(), if files.len() == 1 { "" } else { "s" }, folder_name(folder)));
        let log = |m: &str| log_line("background.log", m);
        let items = sorter::classify_files(files, folder, &|_, _| {}, &log, true);
        let done = sorter::apply_plan(folder, &items, "auto");
        {
            let mut seen = self.seen.lock().unwrap();
            for p in files {
                seen.remove(p);
            }
        }
        let n = done.len();
        if n > 0 {
            let mut cats: Vec<String> = items.iter().map(|i| i.category.clone()).collect();
            cats.sort();
            cats.dedup();
            log(&format!("Sorted {n} in {} into {}", folder.display(), cats.join(", ")));
            write_state("last_sorted", json!(chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()));
            write_state("last_count", json!(n));
            if let Some(notify) = &self.notify {
                if settings::load().notifications {
                    let place = if several_folders { format!(" ({})", folder_name(folder)) } else { String::new() };
                    if n == 1 {
                        notify("Screenshot sorted", &format!("Filed in {}{place}", cats[0]));
                    } else {
                        notify("Screenshots sorted", &format!("Sorted {n} into {}{place}", cats.join(", ")));
                    }
                }
            }
        }
        self.set_busy("");
        n
    }

    /// Sort every loose screenshot in all active folders now (tray "Sort now").
    pub fn sort_all_now(&self) -> usize {
        let s = settings::load();
        let folders = s.active_folders();
        let several = folders.len() > 1;
        let mut n = 0;
        for (f, _) in &folders {
            n += self.sort_files(f, &sortable(f), several);
        }
        n
    }

    fn run(self: Arc<Self>) {
        activity::info("Auto-sort started");
        log_line("background.log", "Background sorting started");
        let me = Arc::downgrade(&self);
        let mut watcher = ::notify::recommended_watcher(move |res: ::notify::Result<::notify::Event>| {
            if res.is_ok() {
                if let Some(w) = me.upgrade() {
                    w.wake();
                }
            }
        })
        .ok();
        let mut watched: Vec<PathBuf> = Vec::new();
        let mut timeout = Duration::from_secs(1);
        while !self.wait(timeout) {
            timeout = SCAN;
            let s = settings::load();
            let active = s.active_folders();

            // follow "instant" folders with Windows change notifications
            let want: Vec<PathBuf> = active.iter().filter(|(_, m)| m == "instant").map(|(p, _)| p.clone()).collect();
            if want != watched {
                if let Some(wt) = watcher.as_mut() {
                    use ::notify::Watcher;
                    for p in &watched {
                        let _ = wt.unwatch(p);
                    }
                    for p in &want {
                        if let Err(e) = wt.watch(p, ::notify::RecursiveMode::NonRecursive) {
                            activity::error(&format!("Can't watch {}: {e}", p.display()), None);
                        }
                    }
                }
                watched = want;
                *self.watching.lock().unwrap() = watched.clone();
                changed();
            }
            if self.paused.load(Ordering::Relaxed) || active.is_empty() {
                continue;
            }

            let several = active.len() > 1;
            let st = read_state();
            for (folder, mode) in &active {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match mode.as_str() {
                    "instant" => {
                        let ready = self.ready_files(folder);
                        self.sort_files(folder, &ready, several);
                    }
                    _ => {
                        let next = last_sweep(&st, folder) + s.interval_hours * 3600.0;
                        if now_secs() >= next {
                            let files: Vec<PathBuf> = sortable(folder)
                                .into_iter()
                                .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(5)).unwrap_or(false))
                                .collect();
                            self.sort_files(folder, &files, several);
                            let mut sweeps = read_state()["sweeps"].clone();
                            if !sweeps.is_object() {
                                sweeps = json!({});
                            }
                            sweeps[folder.to_string_lossy().as_ref()] = json!(now_secs());
                            write_state("sweeps", sweeps);
                        }
                    }
                }));
                if r.is_err() {
                    self.set_busy("");
                    activity::error(&format!("Something went wrong while sorting {} (will retry)", folder.display()), None);
                    timeout = Duration::from_secs(10);
                }
            }
            if !self.seen.lock().unwrap().is_empty() {
                timeout = timeout.min(Duration::from_secs(1)); // a new file is settling
            }
            if let Some(notify) = &self.notify {
                if s.notifications {
                    crate::updater::daily_check(notify.as_ref());
                }
            }
        }
        log_line("background.log", "Background sorting stopped");
    }
}

/// For the window: what auto-sort is doing.
pub fn status_json() -> Value {
    let st = read_state();
    let s = settings::load();
    let (login, menu) = crate::integration::status();
    let w = WORKER.get();
    let watching: Vec<PathBuf> = w.map(|w| w.watching.lock().unwrap().clone()).unwrap_or_default();
    let folders: Vec<Value> = s
        .folders
        .iter()
        .map(|f| {
            let p = PathBuf::from(&f.path);
            let last = last_sweep(&st, &p);
            json!({"path": f.path, "mode": f.mode, "exists": p.is_dir(), "watching": watching.contains(&p),
                   "next": if f.mode == "interval" { json!(last + s.interval_hours * 3600.0) } else { Value::Null },
                   "last_sweep": if last > 0.0 { json!(last) } else { Value::Null }})
        })
        .collect();
    json!({"running": w.is_some() && s.auto_on() && !is_paused(), "paused": is_paused(), "on": s.auto_on(),
           "active": s.active_folders().len(), "busy": w.map(|w| w.busy.lock().unwrap().clone()).unwrap_or_default(),
           "folders": folders, "hours": s.interval_hours, "now": now_secs(),
           "last_sorted": st.get("last_sorted"), "last_count": st.get("last_count"),
           "start_at_login": login, "explorer_menu": menu})
}

/// One line for the tray tooltip.
pub fn summary() -> String {
    let s = settings::load();
    let st = read_state();
    let n = s.active_folders().len();
    let state = if is_paused() {
        "Paused".to_string()
    } else if n == 0 {
        "Auto-sort off".to_string()
    } else {
        format!("Watching {n} folder{}", if n == 1 { "" } else { "s" })
    };
    let busy = WORKER.get().map(|w| w.busy.lock().unwrap().clone()).unwrap_or_default();
    let last = match (st["last_count"].as_u64(), st["last_sorted"].as_str()) {
        _ if !busy.is_empty() => busy,
        (Some(c), Some(t)) if t.len() >= 16 => format!("Last: {c} sorted at {} on {}", &t[11..16], &t[5..10]),
        _ => "Nothing sorted yet".into(),
    };
    format!("Screenshot Sorter\n{state}\n{last}")
}
