//! The app window (Tauri + WebView2, page in ui/index.html) and the tray icon for
//! background mode. The page calls `call(method, args)`; images come from the thumb:// scheme.
use crate::ocr::{self, cache_key};
use crate::{background, claude, integration, reports, rules, settings, sorter, tools, vision};
use once_cell::sync::Lazy;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::http::{Response, StatusCode};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{RunEvent, WebviewUrl, WebviewWindowBuilder};

const PALETTE: [&str; 8] = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"];

static FOLDER: Lazy<Mutex<PathBuf>> = Lazy::new(|| Mutex::new(settings::default_folder()));

struct Job {
    done: bool,
    progress: (usize, usize),
    message: String,
    result: Option<Value>,
    error: Option<String>,
}
static JOBS: Lazy<Mutex<HashMap<String, Job>>> = Lazy::new(Default::default);

fn folder() -> PathBuf {
    FOLDER.lock().unwrap().clone()
}

fn allowed(p: &Path) -> bool {
    let Ok(c) = p.canonicalize() else { return false };
    let inside = |root: &Path| root.canonicalize().map(|r| c.starts_with(r)).unwrap_or(false);
    c.is_file() && (inside(&folder()) || inside(&settings::APP_DIR))
}

fn s_arg(a: &[Value], i: usize) -> String {
    a.get(i).and_then(Value::as_str).unwrap_or("").to_string()
}
fn paths_arg(v: Option<&Value>) -> Vec<PathBuf> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(PathBuf::from).collect()).unwrap_or_default()
}

fn public_settings() -> Value {
    let s = settings::load();
    let mut v = serde_json::to_value(&s).unwrap();
    v["has_claude_key"] = json!(!s.claude_api_key.is_empty());
    v["claude_api_key"] = json!(""); // never sent back to the page
    v
}

fn rules_json() -> Value {
    let data = rules::load_json();
    let colors: Map<String, Value> = data["categories"]
        .as_array()
        .map(|a| a.iter().enumerate().map(|(i, c)| (c["name"].as_str().unwrap_or("").to_string(), json!(PALETTE[i % 8]))).collect())
        .unwrap_or_default();
    json!({"data": data, "colors": colors})
}

fn item_json(folder: &Path, p: &Path, reason: String, text: &str) -> Value {
    json!({"p": p.to_string_lossy(), "n": p.file_name().unwrap_or_default().to_string_lossy(),
           "c": sorter::category_of(folder, p).unwrap_or_else(|| "Unsorted".into()),
           "r": reason, "t": text.chars().take(700).collect::<String>()})
}

fn library_file() -> PathBuf {
    settings::app_file("library.json")
}

// ---------- jobs ----------

fn run_job(kind: &str, a: &Value, f: &Path, progress: ocr::Progress, log: ocr::Log) -> Result<Value, String> {
    Ok(match kind {
        "preview" => json!({"items": sorter::make_plan(f, progress, log, true).iter().map(|i| i.to_json()).collect::<Vec<_>>()}),
        "sort" => {
            let plan: Vec<sorter::Item> = match a.get("plan").and_then(Value::as_array) {
                Some(planned) => {
                    let cache = ocr::load_cache();
                    planned
                        .iter()
                        .filter_map(|it| {
                            let p = PathBuf::from(it["p"].as_str()?);
                            p.exists().then(|| sorter::Item {
                                text: cache.get(&cache_key(&p)).cloned().unwrap_or_default(),
                                path: p, category: it["c"].as_str().unwrap_or("Other").into(), hits: vec![], how: String::new(),
                            })
                        })
                        .collect()
                }
                None => sorter::make_plan(f, progress, log, true),
            };
            json!({"moved": sorter::apply_plan(f, &plan).len()})
        }
        "browse" => {
            let files = sorter::all_images(f);
            let texts = ocr::ocr_files(&files, progress, log);
            let items: Vec<Value> = files.iter().map(|p| item_json(f, p, String::new(), texts.get(p).map(String::as_str).unwrap_or(""))).collect();
            // remember it, so next time the Library shows instantly
            let _ = settings::write_atomic(&library_file(), json!({"folder": f.to_string_lossy(), "items": items}).to_string().as_bytes());
            tools::prewarm_thumbs(files, 480);
            json!({"items": items})
        }
        "search" => {
            let q = a["q"].as_str().unwrap_or("");
            let cache = ocr::load_cache();
            let hits = sorter::search(f, q, progress, log);
            json!({"items": hits.iter().map(|(p, snip)| item_json(f, p, format!("…{snip}…"), cache.get(&cache_key(p)).map(String::as_str).unwrap_or(""))).collect::<Vec<_>>()})
        }
        "undo" => {
            let (n, had) = sorter::undo_last();
            json!({"moved": n, "had": had})
        }
        "rename" => json!({"moved": sorter::rename_existing(f, progress, log).len()}),
        "dups" => json!({"groups": vision::find_duplicates(f, log).iter().map(|g| g.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>()).collect::<Vec<_>>()}),
        "recycle" => {
            let paths: Vec<PathBuf> = paths_arg(a.get("paths")).into_iter().filter(|p| allowed(p)).collect();
            json!({"removed": tools::recycle(&paths)})
        }
        "shrink" => {
            let mode = tools::Mode::parse(a["mode"].as_str().unwrap_or("webp"));
            let (c, b, af) = tools::compress(f, mode, progress, log);
            json!({"converted": c, "before": b, "after": af})
        }
        "report" => {
            ocr::ocr_files(&sorter::all_images(f), progress, log);
            let path = reports::build(f).map_err(|e| e.to_string())?;
            let _ = open::that(&path);
            json!({"path": path.to_string_lossy()})
        }
        "insights" => sorter::insights(f),
        "testclaude" => {
            let mut s = settings::load();
            if let Some(k) = a["key"].as_str().filter(|k| !k.is_empty()) {
                s.claude_api_key = k.into();
            }
            let (ok, message) = claude::test_connection(&s);
            json!({"ok": ok, "message": message})
        }
        other => return Err(format!("unknown job {other}")),
    })
}

fn start_job(kind: String, args: Value) -> String {
    let id = format!("{:08x}", rand_u32());
    JOBS.lock().unwrap().insert(id.clone(), Job { done: false, progress: (0, 0), message: String::new(), result: None, error: None });
    let f = folder();
    let jid = id.clone();
    std::thread::spawn(move || {
        let set = |upd: &dyn Fn(&mut Job)| {
            if let Some(j) = JOBS.lock().unwrap().get_mut(&jid) {
                upd(j)
            }
        };
        let progress = |d: usize, t: usize| set(&|j| j.progress = (d, t));
        let log = |m: &str| set(&|j| j.message = m.to_string());
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&kind, &args, &f, &progress, &log)));
        let r = r.unwrap_or_else(|_| Err("Unexpected error".into()));
        set(&|j| {
            j.done = true;
            match &r {
                Ok(v) => j.result = Some(v.clone()),
                Err(e) => j.error = Some(e.clone()),
            }
        });
    });
    id
}

fn rand_u32() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    n ^ std::process::id().rotate_left(16) ^ (JOBS.lock().unwrap().len() as u32).wrapping_mul(2654435761)
}

fn job_status(id: &str) -> Value {
    let mut jobs = JOBS.lock().unwrap();
    let Some(j) = jobs.get(id) else { return Value::Null };
    let out = json!({"done": j.done, "progress": [j.progress.0, j.progress.1], "message": j.message,
                     "error": j.error, "has_result": j.result.is_some(), "result": j.result});
    if j.done {
        jobs.remove(id);
    }
    out
}

// ---------- the page's API ----------

fn save_settings(values: &Value) -> Value {
    let mut changes: Map<String, Value> = values.as_object().cloned().unwrap_or_default();
    changes.remove("has_claude_key");
    if changes.get("claude_api_key").and_then(Value::as_str).map(str::is_empty).unwrap_or(true) {
        changes.remove("claude_api_key"); // keep the stored key unless a new one is typed
    }
    if changes.remove("clear_claude_key").and_then(|v| v.as_bool()).unwrap_or(false) {
        changes.insert("claude_api_key".into(), json!(""));
    }
    let old = settings::load();
    let s = settings::update(&changes);
    integration::apply(&s);
    if s.auto_mode != "off" {
        integration::remove_daily_task();
    }
    if s.auto_mode == "off" {
        background::request_stop();
    } else if old.show_tray_icon != s.show_tray_icon && background::is_running() {
        // restart so the tray icon appears/disappears
        background::request_stop();
        std::thread::sleep(std::time::Duration::from_millis(1500));
        start_background();
    }
    json!({"settings": public_settings(), "background": background::status_json(), "warnings": []})
}

fn start_background() -> Value {
    if !background::is_running() {
        integration::spawn(&["--tray"]);
        for _ in 0..20 {
            std::thread::sleep(std::time::Duration::from_millis(250));
            if background::is_running() {
                break;
            }
        }
    }
    background::status_json()
}

fn stop_background() -> Value {
    background::request_stop();
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if !background::is_running() {
            break;
        }
    }
    background::status_json()
}

fn dispatch(method: &str, a: &[Value]) -> Result<Value, String> {
    Ok(match method {
        "init" => json!({"version": crate::updater::VERSION, "folder": folder().to_string_lossy(), "settings": public_settings(), "rules": rules_json(), "background": background::status_json()}),
        "pick_folder" => match rfd::FileDialog::new().set_directory(folder()).pick_folder() {
            Some(p) => dispatch("set_folder", &[json!(p.to_string_lossy())])?,
            None => Value::Null,
        },
        "set_folder" => {
            let p = PathBuf::from(s_arg(a, 0));
            if !p.is_dir() {
                return Err(format!("Folder not found: {}", p.display()));
            }
            *FOLDER.lock().unwrap() = p.clone();
            settings::update(&[("folder".to_string(), json!(p.to_string_lossy()))].into_iter().collect());
            json!(p.to_string_lossy())
        }
        "open" => {
            let p = PathBuf::from(s_arg(a, 0));
            if p.exists() {
                let _ = open::that_detached(&p);
            }
            Value::Null
        }
        "reveal" => {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", s_arg(a, 0))).spawn();
            Value::Null
        }
        "open_folder" => {
            let _ = open::that_detached(folder());
            Value::Null
        }
        "start_job" => json!(start_job(s_arg(a, 0), a.get(1).cloned().unwrap_or(json!({})))),
        "job" => job_status(&s_arg(a, 0)),
        "move" => {
            let paths: Vec<PathBuf> = paths_arg(a.first()).into_iter().filter(|p| allowed(p)).collect();
            let done = sorter::move_to_category(&folder(), &paths, &s_arg(a, 1));
            Value::Object(done.iter().map(|(x, y)| (x.to_string_lossy().to_string(), json!(y.to_string_lossy()))).collect())
        }
        "suggest" => {
            let cache = ocr::load_cache();
            let texts: Vec<String> = paths_arg(a.first()).iter().filter(|p| p.exists()).filter_map(|p| cache.get(&cache_key(p)).cloned()).collect();
            json!(if texts.iter().any(|t| !t.is_empty()) { rules::suggest_keywords(&texts, &s_arg(a, 1), 8) } else { vec![] })
        }
        "add_keywords" => {
            let words: Vec<String> = a.get(1).and_then(Value::as_array).map(|w| w.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
            rules::add_keywords(&s_arg(a, 0), &words, 2.0)?;
            rules_json()
        }
        "size_estimate" => {
            let mode = tools::Mode::parse(&s_arg(a, 0));
            let (count, size) = tools::estimate(&folder(), mode);
            let (bk_count, bk_size) = tools::originals_info();
            json!({"count": count, "size": size, "backup_count": bk_count, "backup_size": bk_size})
        }
        "open_originals" => {
            let d = tools::originals_dir();
            let _ = std::fs::create_dir_all(&d);
            let _ = open::that_detached(d);
            Value::Null
        }
        "empty_originals" => {
            // permanent: the page asks first
            let d = tools::originals_dir();
            let (n, size) = tools::originals_info();
            let _ = std::fs::remove_dir_all(&d);
            json!({"removed": n, "size": size})
        }
        "get_rules" => rules_json(),
        "save_rules" => {
            rules::save_json(a.first().unwrap_or(&Value::Null))?;
            rules_json()
        }
        "save_settings" => save_settings(a.first().unwrap_or(&json!({}))),
        "background_status" => background::status_json(),
        "check_update" => crate::updater::check(a.first().and_then(Value::as_bool).unwrap_or(false)),
        "install_update" => {
            let v = crate::updater::install(&|_| {})?;
            // give the page its answer, then get out of the installer's way
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(800));
                std::process::exit(0);
            });
            json!(v)
        }
        "open_release_page" => {
            let _ = open::that_detached(format!("https://github.com/{}/releases/latest", crate::updater::REPO));
            Value::Null
        }
        "start_background" => start_background(),
        "stop_background" => stop_background(),
        "library_snapshot" => {
            let snap: Value = std::fs::read_to_string(library_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
            if snap["folder"].as_str() == Some(folder().to_string_lossy().as_ref()) { snap } else { json!({"items": []}) }
        }
        other => return Err(format!("unknown method {other}")),
    })
}

#[tauri::command]
async fn call(method: String, args: Vec<Value>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || dispatch(&method, &args)).await.map_err(|e| e.to_string())?
}

// ---------- thumb:// image scheme ----------

fn image_response(uri: &str) -> Response<Vec<u8>> {
    let q: HashMap<String, String> = uri
        .split_once('?')
        .map(|(_, q)| q.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default())).collect())
        .unwrap_or_default();
    let p = PathBuf::from(q.get("p").cloned().unwrap_or_default());
    let size: u32 = q.get("s").and_then(|s| s.parse().ok()).unwrap_or(0);
    let nf = || Response::builder().status(StatusCode::NOT_FOUND).body(Vec::new()).unwrap();
    if !allowed(&p) {
        return nf();
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let (body, ctype) = if size == 0 && matches!(ext.as_str(), "jxl" | "heic" | "heif") {
        // the window can't show these itself: hand it a full-size PNG made by Windows' decoder
        let png = crate::imageio::open(&p).and_then(|img| {
            let mut buf = std::io::Cursor::new(Vec::new());
            img.write_to(&mut buf, image::ImageFormat::Png).ok().map(|_| buf.into_inner())
        });
        (png, "image/png".to_string())
    } else if size == 0 {
        (std::fs::read(&p).ok(), format!("image/{}", if ext == "jpg" { "jpeg" } else { ext.as_str() }))
    } else {
        (tools::thumb_bytes(&p, size), "image/jpeg".to_string())
    };
    match body {
        Some(b) => Response::builder().header("Content-Type", ctype).header("Cache-Control", "max-age=3600").body(b).unwrap(),
        None => nf(),
    }
}

pub fn run_gui(start_folder: Option<PathBuf>, auto_preview: bool) {
    if let Some(f) = start_folder.filter(|f| f.is_dir()) {
        *FOLDER.lock().unwrap() = f;
    }
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![call])
        .register_asynchronous_uri_scheme_protocol("thumb", |_ctx, req, responder| {
            let uri = req.uri().to_string();
            std::thread::spawn(move || responder.respond(image_response(&uri)));
        })
        .setup(move |app| {
            let mut b = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Screenshot Sorter")
                .inner_size(1320.0, 860.0)
                .min_inner_size(980.0, 640.0)
                .background_color(tauri::webview::Color(15, 17, 21, 255));
            if auto_preview {
                b = b.initialization_script("window.__AUTO = 'preview';");
            }
            b.build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("could not start the window");
}

// ---------- tray (background mode with icon) ----------

fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).appname("Screenshot Sorter").show();
}

pub fn run_tray() {
    let worker = std::sync::Arc::new(background::Worker::new(Some(Box::new(notify))));
    let app = tauri::Builder::default()
        .setup({
            let worker = worker.clone();
            move |app| {
                let open_i = MenuItem::with_id(app, "open", "Open Screenshot Sorter", true, None::<&str>)?;
                let sort_i = MenuItem::with_id(app, "sort", "Sort now", true, None::<&str>)?;
                let pause_i = CheckMenuItem::with_id(app, "pause", "Pause", true, false, None::<&str>)?;
                let undo_i = MenuItem::with_id(app, "undo", "Undo last sort", true, None::<&str>)?;
                let upd_i = MenuItem::with_id(app, "update", "Check for updates", true, None::<&str>)?;
                let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
                let sep = PredefinedMenuItem::separator(app)?;
                let menu = Menu::with_items(app, &[&open_i, &sort_i, &pause_i, &undo_i, &upd_i, &sep, &quit_i])?;
                let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
                let w = worker.clone();
                TrayIconBuilder::with_id("tray")
                    .icon(icon)
                    .tooltip("Screenshot Sorter")
                    .menu(&menu)
                    .show_menu_on_left_click(false)
                    .on_menu_event(move |app, ev| match ev.id().as_ref() {
                        "open" => integration::spawn(&[]),
                        "sort" => {
                            let w = w.clone();
                            std::thread::spawn(move || {
                                let f = settings::default_folder();
                                if w.sort_files(&f, &sorter::loose_images(&f)) == 0 {
                                    notify("Screenshot Sorter", "Nothing new to sort.");
                                }
                            });
                        }
                        "pause" => {
                            let p = !w.paused.load(std::sync::atomic::Ordering::Relaxed);
                            w.paused.store(p, std::sync::atomic::Ordering::Relaxed);
                        }
                        "undo" => {
                            let (n, had) = sorter::undo_last();
                            notify("Screenshot Sorter", &if had { format!("Moved {n} file(s) back.") } else { "Nothing to undo.".into() });
                        }
                        "update" => {
                            std::thread::spawn(|| {
                                let v = crate::updater::check(true);
                                if let Some(e) = v["error"].as_str() {
                                    notify("Couldn't check for updates", e);
                                } else if v["available"] == true {
                                    notify("Update available", &format!("Version {} is out. Opening Screenshot Sorter...", v["latest"].as_str().unwrap_or("")));
                                    integration::spawn(&[]);
                                } else {
                                    notify("Screenshot Sorter", &format!("You have the newest version ({}).", crate::updater::VERSION));
                                }
                            });
                        }
                        "quit" => {
                            w.signal_stop();
                            app.exit(0);
                        }
                        _ => {}
                    })
                    .on_tray_icon_event(|tray, ev| {
                        if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = ev {
                            let _ = tray.app_handle();
                            integration::spawn(&[]);
                        }
                    })
                    .build(app)?;
                // watcher thread; when it stops (Quit or Stop from the window) the app exits
                let handle = app.handle().clone();
                let w = worker.clone();
                std::thread::spawn(move || {
                    w.run();
                    handle.exit(0);
                });
                Ok(())
            }
        })
        .build(tauri::generate_context!())
        .expect("could not start the tray icon");
    app.run(|_, ev| {
        if let RunEvent::ExitRequested { api, code: None, .. } = ev {
            api.prevent_exit(); // no windows is normal for the tray
        }
    });
}
