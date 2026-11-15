//! Screenshot Sorter
//!   Screenshot Sorter.exe                  the app window
//!   Screenshot Sorter.exe --tray           start in the background (auto-sort; tray icon or hidden)
//!   Screenshot Sorter.exe --gui-folder P   window on folder P, previewing it (Explorer menu)
//!   Screenshot Sorter.exe --setup          apply settings to Windows, start background sorting
//!   Screenshot Sorter.exe --apply [--folder P] | --undo | --search TEXT | --compress avif|webp   command line
//!   Screenshot Sorter.exe --version | --check-update | --update | --stop | --remove-integration
#![windows_subsystem = "windows"]

mod activity;
mod background;
mod claude;
mod gui;
mod imageio;
mod integration;
mod ocr;
mod reports;
mod rules;
mod settings;
mod sorter;
mod tools;
mod trackers;
mod localai;
mod updater;
mod vision;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

/// Where command-line output goes: the console we were started from, or a log file.
static OUT: Mutex<Option<Box<dyn Write + Send>>> = Mutex::new(None);

fn out(s: &str) {
    if let Some(w) = OUT.lock().unwrap().as_mut() {
        let _ = writeln!(w, "{s}");
        let _ = w.flush();
    }
}

fn setup_output() {
    use windows::Win32::System::Console::{AttachConsole, GetStdHandle, ATTACH_PARENT_PROCESS, STD_OUTPUT_HANDLE};
    let has_stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE).map(|h| !h.is_invalid() && !h.0.is_null()).unwrap_or(false) };
    let w: Box<dyn Write + Send> = if has_stdout || unsafe { AttachConsole(ATTACH_PARENT_PROCESS).is_ok() } {
        Box::new(std::io::stdout())
    } else {
        let _ = std::fs::create_dir_all(settings::log_dir());
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(settings::log_dir().join("auto_sort.log")).unwrap();
        let _ = writeln!(f, "\n=== {} ===", chrono::Local::now().format("%Y-%m-%d %H:%M"));
        Box::new(f)
    };
    *OUT.lock().unwrap() = Some(w);
}

fn cli(args: &[String]) {
    setup_output();
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let folder = get("--folder").map(PathBuf::from).unwrap_or_else(settings::default_folder);
    let log = |m: &str| out(m);
    let quiet = |_: usize, _: usize| {};

    if args.iter().any(|a| a == "--version") {
        out(updater::VERSION);
    } else if args.iter().any(|a| a == "--check-update") {
        let v = updater::check(true);
        out(&match v["error"].as_str() {
            Some(e) => e.to_string(),
            None if v["available"] == true => format!("Version {} is available (you have {}).", v["latest"].as_str().unwrap_or(""), updater::VERSION),
            None => format!("Up to date ({}).", updater::VERSION),
        });
    } else if args.iter().any(|a| a == "--update") {
        match updater::install(&log) {
            Ok(v) => out(&format!("Installing version {v}...")),
            Err(e) => out(&e),
        }
    } else if args.iter().any(|a| a == "--stop" || a == "--remove-integration") {
        // used by the installer: stop background sorting, and on uninstall remove the Windows hooks
        if background::request_stop() {
            for _ in 0..40 {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if !background::is_running() {
                    break;
                }
            }
        }
        if args.iter().any(|a| a == "--remove-integration") {
            integration::remove_all();
        }
    } else if args.iter().any(|a| a == "--undo") {
        let (n, had) = sorter::undo_last();
        out(&if had { format!("Moved {n} file(s) back.") } else { "Nothing to undo.".into() });
    } else if let Some(q) = get("--search") {
        let hits = sorter::search(&folder, &q, &quiet, &log);
        for (p, snip) in &hits {
            out(&format!("{}\n    ...{snip}...", p.strip_prefix(&folder).unwrap_or(p).display()));
        }
        out(&format!("\n{} match(es).", hits.len()));
    } else if let Some(m) = get("--compress") {
        let t = std::time::Instant::now();
        let (n, b, a) = tools::compress(&folder, tools::Mode::parse(&m), &quiet, &log);
        out(&format!("Converted {n} file(s): {:.1} MB -> {:.1} MB in {:.0}s", b as f64 / 1e6, a as f64 / 1e6, t.elapsed().as_secs_f64()));
    } else if args.iter().any(|a| a == "--look-test") {
        // diagnostic: how often a confident look-based guess is right, on sorted screenshots
        let rules = rules::load();
        for c in [0.6, 0.7, 0.8, 0.9] {
            if let Some((r, w, u)) = vision::self_test(&folder, &rules, c) {
                out(&format!("conf>={c}: right {r}, wrong {w}, unsure {u} -> precision {:.0}%, coverage {:.0}%",
                    100.0 * r as f64 / (r + w).max(1) as f64, 100.0 * (r + w) as f64 / (r + w + u).max(1) as f64));
            }
        }
    } else if let (Some(cache), Some(dest)) = (get("--parity"), get("--parity-out")) {
        // diagnostic: classify every cached text, to compare with the Python version
        let texts: std::collections::HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(cache).unwrap()).unwrap();
        let rules = rules::load();
        let res: serde_json::Map<String, serde_json::Value> = texts
            .iter()
            .map(|(k, t)| {
                let (c, hits, _) = rules.classify(t);
                let item = sorter::Item { path: PathBuf::new(), category: c.clone(), hits, how: "text".into(), text: String::new(), ..Default::default() };
                (k.clone(), serde_json::json!([c, item.reason()]))
            })
            .collect();
        std::fs::write(dest, serde_json::to_string(&res).unwrap()).unwrap();
        out(&format!("classified {}", res.len()));
    } else {
        let plan = sorter::make_plan(&folder, &quiet, &log, true);
        if plan.is_empty() {
            out(&format!("No unsorted images in {}", folder.display()));
            return;
        }
        let mut counts: Vec<(String, usize)> = Vec::new();
        for it in &plan {
            match counts.iter_mut().find(|c| c.0 == it.category) {
                Some(c) => c.1 += 1,
                None => counts.push((it.category.clone(), 1)),
            }
        }
        counts.sort_by(|a, b| b.1.cmp(&a.1));
        out(&format!("\n{} image(s) in {}\n", plan.len(), folder.display()));
        for (c, n) in &counts {
            out(&format!("  {c:<40} {n:>5}"));
        }
        if args.iter().any(|a| a == "--apply") {
            let done = sorter::apply_plan(&folder, &plan, "you");
            out(&format!("\nMoved {} file(s).", done.len()));
        } else {
            out("\nDry run - nothing moved. Run with --apply to move files.");
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !matches!(args.first().map(String::as_str), Some("--setup" | "--stop" | "--remove-integration")) {
        integration::repair();
    }
    match args.first().map(String::as_str) {
        Some("--setup") => {
            let s = settings::load();
            integration::apply(&s);
            integration::remove_daily_task();
            if s.auto_on() && !background::is_running() {
                integration::spawn(&["--tray"]);
            }
        }
        Some("--tray") => {
            // at login: only start if some folder is sorted in the background
            if settings::load().auto_on() {
                gui::run_app(false, None, false);
            }
        }
        Some("--gui-folder") => gui::run_app(true, args.get(1).map(PathBuf::from), true),
        Some(_) => cli(&args),
        None => gui::run_app(true, None, false),
    }
}
