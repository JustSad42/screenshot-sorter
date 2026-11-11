//! Background sorting, started at login with --tray:
//! - "instant":  files each new screenshot a few seconds after it's saved
//! - "interval": sweeps the folder every N hours
//! With a tray icon (right-click for options) or fully hidden. Uses the same mutex/event
//! names and state files as the Python version, so either window can see and stop it.

use crate::settings::{self, log_line};
use crate::sorter;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, SYNCHRONIZATION_SYNCHRONIZE,
};

const POLL: Duration = Duration::from_secs(2);
const SETTLE: Duration = Duration::from_secs(2); // wait until the file has stopped changing

fn state_file() -> PathBuf {
    settings::app_file("background_state.json")
}
fn pid_file() -> PathBuf {
    settings::app_file("background.pid")
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

pub struct Worker {
    stop: HANDLE,
    pub paused: std::sync::atomic::AtomicBool,
    seen: std::sync::Mutex<HashMap<PathBuf, (u64, u64, Instant)>>,
    notify: Option<Box<dyn Fn(&str, &str) + Send + Sync>>,
}
unsafe impl Send for Worker {}
unsafe impl Sync for Worker {}

impl Worker {
    pub fn new(notify: Option<Box<dyn Fn(&str, &str) + Send + Sync>>) -> Worker {
        let stop = unsafe { CreateEventW(None, true, false, w!("ScreenshotSorterBackgroundStop")).unwrap_or_default() };
        Worker { stop, paused: false.into(), seen: Default::default(), notify }
    }

    pub fn stopped(&self, wait: Duration) -> bool {
        unsafe { WaitForSingleObject(self.stop, wait.as_millis() as u32) == WAIT_OBJECT_0 }
    }

    pub fn signal_stop(&self) {
        unsafe {
            let _ = SetEvent(self.stop);
        }
    }

    fn ready_files(&self, folder: &Path) -> Vec<PathBuf> {
        let mut seen = self.seen.lock().unwrap();
        let now = Instant::now();
        let mut ready = Vec::new();
        let current = sorter::loose_images(folder);
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
        seen.retain(|p, _| current.contains(p));
        ready
    }

    pub fn sort_files(&self, folder: &Path, files: &[PathBuf]) -> usize {
        if files.is_empty() {
            return 0;
        }
        let log = |m: &str| log_line("background.log", m);
        let items = sorter::classify_files(files, folder, &|_, _| {}, &log, true);
        let done = sorter::apply_plan(folder, &items);
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
            log(&format!("Sorted {n} into {}", cats.join(", ")));
            write_state("last_sorted", json!(chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()));
            write_state("last_count", json!(n));
            if let Some(notify) = &self.notify {
                if settings::load().notifications {
                    if n == 1 {
                        notify("Screenshot sorted", &format!("Filed in {}", cats[0]));
                    } else {
                        notify("Screenshots sorted", &format!("Sorted {n} into {}", cats.join(", ")));
                    }
                }
            }
        }
        n
    }

    pub fn run(&self) {
        log_line("background.log", "Background sorting started");
        while !self.stopped(POLL) {
            let s = settings::load();
            if self.paused.load(std::sync::atomic::Ordering::Relaxed) || s.auto_mode == "off" {
                continue;
            }
            let folder = settings::default_folder();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match s.auto_mode.as_str() {
                "instant" => {
                    let ready = self.ready_files(&folder);
                    self.sort_files(&folder, &ready);
                }
                "interval" => {
                    let last = read_state()["last_sweep"].as_f64().unwrap_or(0.0);
                    if now_secs() - last >= s.interval_hours * 3600.0 {
                        let files: Vec<PathBuf> = sorter::loose_images(&folder)
                            .into_iter()
                            .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(5)).unwrap_or(false))
                            .collect();
                        self.sort_files(&folder, &files);
                        write_state("last_sweep", json!(now_secs()));
                    }
                }
                _ => {}
            }));
            if let Some(notify) = &self.notify {
                if s.notifications {
                    crate::updater::daily_check(notify.as_ref());
                }
            }
            if r.is_err() {
                log_line("background.log", "Error while sorting (will retry)");
                self.stopped(Duration::from_secs(10));
            }
        }
        log_line("background.log", "Background sorting stopped");
    }
}

/// Entry point for --tray. Returns when stopped (Quit, or Stop from the app window).
pub fn main() {
    let mutex = unsafe { CreateMutexW(None, false, w!("ScreenshotSorterBackground")) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return; // already running
    }
    let _ = std::fs::write(pid_file(), std::process::id().to_string());
    if settings::load().show_tray_icon {
        crate::gui::run_tray();
    } else {
        Worker::new(None).run();
    }
    let _ = std::fs::remove_file(pid_file());
    if let Ok(h) = mutex {
        unsafe {
            let _ = CloseHandle(h);
        }
    }
}

pub fn status_json() -> Value {
    let st = read_state();
    let s = settings::load();
    let (login, menu) = crate::integration::status();
    json!({"running": is_running(), "mode": s.auto_mode, "hours": s.interval_hours,
           "last_sorted": st.get("last_sorted"), "last_count": st.get("last_count"),
           "start_at_login": login, "explorer_menu": menu})
}
