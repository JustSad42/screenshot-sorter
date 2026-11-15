//! The app: one process with the window (Tauri + WebView2, page in ui/index.html), the tray
//! icon and background auto-sort. The page calls `call(method, args)`; images come from the
//! thumb:// scheme. Launching the app again just brings this window forward.
use crate::ocr::{self, cache_key};
use crate::{activity, background, claude, integration, reports, rules, settings, sorter, tools, vision};
use once_cell::sync::{Lazy, OnceCell};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::http::{Response, StatusCode};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

const PALETTE: [&str; 8] = ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4", "#008300", "#6250d6", "#e34948"];

static FOLDER: Lazy<Mutex<PathBuf>> = Lazy::new(|| Mutex::new(settings::default_folder()));

struct Job {
    kind: String,
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
    c.is_file()
        && (inside(&folder()) || inside(&settings::APP_DIR) || settings::load().folders.iter().any(|f| inside(Path::new(&f.path))))
}

/// The open folder plus every watched one, without repeats.
fn all_folders() -> Vec<PathBuf> {
    let mut out = vec![folder()];
    for f in settings::load().folders {
        let p = PathBuf::from(&f.path);
        if p.is_dir() && !out.iter().any(|o| o.to_string_lossy().eq_ignore_ascii_case(&p.to_string_lossy())) {
            out.push(p);
        }
    }
    out
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
    let root = if p.starts_with(folder) { folder.to_path_buf() } else { settings::root_of(p).unwrap_or_else(|| folder.to_path_buf()) };
    json!({"p": p.to_string_lossy(), "n": p.file_name().unwrap_or_default().to_string_lossy(),
           "c": sorter::category_of(&root, p).unwrap_or_else(|| "Unsorted".into()),
           "f": root.file_name().unwrap_or_default().to_string_lossy(),
           "r": reason, "t": text.chars().take(700).collect::<String>(), "cloud": ocr::online_only(p),
           "d": sorter::taken_at(p).format("%Y-%m-%d %H:%M").to_string()})
}

// ---------- review queue ----------

fn review_state_file() -> PathBuf {
    settings::app_file("review_state.json")
}
/// Files you've said were filed right ("Looks right" on the Review page).
fn reviewed_ok() -> std::collections::HashSet<String> {
    std::fs::read_to_string(review_state_file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["ok"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(|x| x.to_lowercase()).collect()))
        .unwrap_or_default()
}
fn mark_reviewed(paths: &[PathBuf]) {
    let mut ok = reviewed_ok();
    ok.extend(paths.iter().map(|p| p.to_string_lossy().to_lowercase()));
    ok.retain(|p| Path::new(p).exists());
    let _ = settings::write_atomic(&review_state_file(), json!({"ok": ok.into_iter().collect::<Vec<_>>()}).to_string().as_bytes());
}

/// Auto-sorted files marked "weak" in the last 60 days that are still where they were put.
fn weak_moves() -> Vec<(PathBuf, Value)> {
    let ok = reviewed_ok();
    let since = (chrono::Local::now() - chrono::Duration::days(60)).format("%Y-%m-%dT%H:%M:%S").to_string();
    let mut seen = std::collections::HashSet::new();
    activity::recent(50_000)
        .into_iter()
        .filter(|e| e["kind"] == "move" && e["by"] == "auto" && e["conf"] == "weak" && e["t"].as_str().map(|t| t >= since.as_str()).unwrap_or(false))
        .filter_map(|e| {
            let to = PathBuf::from(e["to"].as_str()?);
            let key = to.to_string_lossy().to_lowercase();
            // only files still in a watched folder
            (to.exists() && allowed(&to) && !ok.contains(&key) && seen.insert(key)).then_some((to, e))
        })
        .collect()
}

/// Category folders for leftovers ("Other", "No Text") in every folder.
fn leftovers(which: &str) -> Vec<(PathBuf, PathBuf)> {
    let rules = rules::load();
    let cat = if which == "notext" { rules.no_text } else { rules.fallback };
    all_folders().into_iter().flat_map(|root| sorter::all_images(&root.join(&cat)).into_iter().map(move |p| (root.clone(), p))).collect()
}

fn review_lists() -> Value {
    let rules = rules::load();
    let cache = ocr::load_cache();
    let mk = |root: &Path, p: &Path| {
        let t = cache.get(&cache_key(p)).cloned().unwrap_or_default();
        let it = sorter::judge(&rules, p, &t);
        item_json(root, p, it.reason(), &t)
    };
    let other: Vec<Value> = leftovers("other").iter().map(|(r, p)| mk(r, p)).collect();
    let notext: Vec<Value> = leftovers("notext").iter().map(|(r, p)| mk(r, p)).collect();
    let weak: Vec<Value> = weak_moves()
        .iter()
        .map(|(p, e)| {
            let root = settings::root_of(p).unwrap_or_else(folder);
            let mut v = item_json(&root, p, e["why"].as_str().unwrap_or("").to_string(), cache.get(&cache_key(p)).map(String::as_str).unwrap_or(""));
            v["when"] = e["t"].clone();
            v
        })
        .collect();
    json!({"other": other, "notext": notext, "weak": weak})
}

/// For the sidebar badge: unmatched + worth-a-second-look.
fn review_count() -> Value {
    json!({"other": leftovers("other").len(), "weak": weak_moves().len()})
}

// ---------- smart albums ----------

fn albums_json() -> Value {
    let s = settings::load();
    let cache = ocr::load_cache();
    let files: Vec<PathBuf> = all_folders().iter().flat_map(|r| sorter::all_images(r)).collect();
    let texts: Vec<String> = files.iter().filter_map(|p| cache.get(&cache_key(p)).map(|t| t.to_lowercase())).collect();
    json!(s.albums.iter().map(|a| {
        let q = a.q.to_lowercase();
        json!({"name": a.name, "q": a.q, "count": texts.iter().filter(|t| t.contains(&q)).count()})
    }).collect::<Vec<_>>())
}

fn save_album(name: &str, q: &str) -> Result<Value, String> {
    let (name, q) = (name.trim(), q.trim());
    if name.is_empty() || q.is_empty() {
        return Err("An album needs a name and something to search for.".into());
    }
    let mut s = settings::load();
    s.albums.retain(|a| !a.name.eq_ignore_ascii_case(name));
    s.albums.push(settings::Album { name: name.into(), q: q.into() });
    settings::save(&s).map_err(|e| e.to_string())?;
    Ok(albums_json())
}

// ---------- first run ----------

/// Folders that usually hold screenshots, with how many loose pictures each has.
fn suggest_folders() -> Value {
    let home = std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default();
    let mut out = Vec::new();
    for rel in ["Pictures\\Screenshots", "OneDrive\\Pictures\\Screenshots", "OneDrive\\Pictures\\Screenshots 1", "Videos\\Captures", "OneDrive\\Videos\\Captures", "Desktop", "OneDrive\\Desktop"] {
        let p = home.join(rel);
        if p.is_dir() {
            let loose = sorter::loose_images(&p).len();
            let watched = settings::load().folders.iter().any(|f| f.path.eq_ignore_ascii_case(&p.to_string_lossy()));
            out.push(json!({"path": p.to_string_lossy(), "loose": loose, "watched": watched, "suggest": loose > 0 && !rel.contains("Desktop")}));
        }
    }
    json!(out)
}

/// Per folder: images never read (mostly online-only OneDrive files). Counted at most once a minute.
static UNREAD: Lazy<Mutex<(Option<std::time::Instant>, Value)>> = Lazy::new(|| Mutex::new((None, json!([]))));
fn unread_summary(force: bool) -> Value {
    let mut g = UNREAD.lock().unwrap();
    if !force && g.0.map(|t| t.elapsed().as_secs() < 60).unwrap_or(false) {
        return g.1.clone();
    }
    let v: Vec<Value> = all_folders()
        .iter()
        .filter_map(|f| {
            let un = sorter::unread_images(f);
            let cloud = un.iter().filter(|p| ocr::online_only(p)).count();
            (!un.is_empty()).then(|| json!({"folder": f.to_string_lossy(), "count": un.len(), "cloud": cloud}))
        })
        .collect();
    *g = (Some(std::time::Instant::now()), json!(v));
    g.1.clone()
}

fn library_file() -> PathBuf {
    settings::app_file("library.json")
}

// ---------- jobs ----------

fn run_job(kind: &str, a: &Value, f: &Path, progress: ocr::Progress, log: ocr::Log) -> Result<Value, String> {
    Ok(match kind {
        "preview" => {
            let mut items = sorter::make_plan(f, progress, log, true);
            sorter::local_ai_pass(&mut items, progress, log); // you started this sort yourself
            json!({"items": items.iter().map(|i| i.to_json()).collect::<Vec<_>>()})
        }
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
                                path: p, category: it["c"].as_str().unwrap_or("Other").into(), hits: vec![],
                                // the reason the preview showed (kept as-is), and how sure it was
                                how: it["r"].as_str().filter(|r| !r.is_empty() && *r != "text").unwrap_or("sorted by you").into(),
                                conf: it["conf"].as_str().unwrap_or("").into(),
                                ..Default::default()
                            })
                        })
                        .collect()
                }
                None => {
                    let mut items = sorter::make_plan(f, progress, log, true);
                    sorter::local_ai_pass(&mut items, progress, log);
                    items
                }
            };
            json!({"moved": sorter::apply_plan(f, &plan, "you").len()})
        }
        "browse" => {
            let all = a["all"].as_bool() == Some(true);
            let roots = if all { all_folders() } else { vec![f.to_path_buf()] };
            let files: Vec<(PathBuf, PathBuf)> = roots.iter().flat_map(|r| sorter::all_images(r).into_iter().map(move |p| (r.clone(), p))).collect();
            let paths: Vec<PathBuf> = files.iter().map(|x| x.1.clone()).collect();
            let texts = ocr::ocr_files(&paths, progress, log);
            let items: Vec<Value> = files.iter().map(|(r, p)| item_json(r, p, String::new(), texts.get(p).map(String::as_str).unwrap_or(""))).collect();
            // remember it, so next time the Library shows instantly
            let key = if all { "*".to_string() } else { f.to_string_lossy().to_string() };
            let _ = settings::write_atomic(&library_file(), json!({"folder": key, "items": items}).to_string().as_bytes());
            tools::prewarm_thumbs(paths, 480);
            json!({"items": items, "folders": roots.len()})
        }
        "preview_folders" => {
            // what watching these folders would do right now (nothing is moved)
            let mut out = Vec::new();
            for p in paths_arg(a.get("paths")) {
                let loose = sorter::loose_images(&p);
                let items = sorter::classify_files(&loose, &p, progress, log, false);
                let mut cats: Vec<(String, usize)> = Vec::new();
                for it in &items {
                    match cats.iter_mut().find(|c| c.0 == it.category) {
                        Some(c) => c.1 += 1,
                        None => cats.push((it.category.clone(), 1)),
                    }
                }
                cats.sort_by(|a, b| b.1.cmp(&a.1));
                let weak = items.iter().filter(|i| i.conf == "weak" || i.conf == "none").count();
                out.push(json!({"path": p.to_string_lossy(), "loose": loose.len(), "moves": items.len(), "unsure": weak,
                                "skipped": loose.len() - items.len(), "cats": cats, "sorted": sorter::all_images(&p).len() - loose.len()}));
            }
            json!(out)
        }
        "search" => {
            let q = a["q"].as_str().unwrap_or("");
            let folders = if a["all"].as_bool() == Some(true) { all_folders() } else { vec![f.to_path_buf()] };
            let mut hits = Vec::new();
            for root in &folders {
                hits.extend(sorter::search(root, q, progress, log));
            }
            let cache = ocr::load_cache();
            json!({"folders": folders.len(), "items": hits.iter().map(|(p, snip)| item_json(f, p, format!("…{snip}…"), cache.get(&cache_key(p)).map(String::as_str).unwrap_or(""))).collect::<Vec<_>>()})
        }
        "fetch_cloud" => {
            let folders = match a["folder"].as_str() { Some(x) if !x.is_empty() => vec![PathBuf::from(x)], _ => all_folders() };
            let mut moved = 0;
            for root in &folders {
                moved += sorter::fetch_and_sort(root, progress, log).len();
            }
            unread_summary(true);
            json!({"moved": moved, "left": unread_summary(false)})
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
    JOBS.lock().unwrap().insert(id.clone(), Job { kind: kind.clone(), done: false, progress: (0, 0), message: String::new(), result: None, error: None });
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
    let s = settings::update(&changes);
    applied(&s)
}

/// After settings change: update Windows hooks, the tray icon and wake auto-sort.
fn applied(s: &settings::Settings) -> Value {
    integration::apply(s);
    if s.auto_on() {
        integration::remove_daily_task();
    }
    if let Some(app) = APP.get() {
        let _ = app.run_on_main_thread(refresh_tray);
    }
    background::wake();
    json!({"settings": public_settings(), "background": background::status_json(), "warnings": []})
}

fn same_path(a: &str, b: &str) -> bool {
    a.trim_end_matches('\\').eq_ignore_ascii_case(b.trim_end_matches('\\'))
}

/// Add a watched folder (or change its mode if it's already there).
fn set_folder_mode(path: &str, mode: &str) -> Result<Value, String> {
    if !Path::new(path).is_dir() {
        return Err(format!("Folder not found: {path}"));
    }
    let mut s = settings::load();
    match s.folders.iter_mut().find(|f| same_path(&f.path, path)) {
        Some(f) => f.mode = mode.to_string(),
        None => s.folders.push(settings::FolderCfg { path: path.to_string(), mode: mode.to_string() }),
    }
    settings::save(&s).map_err(|e| e.to_string())?;
    let name = Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
    activity::info(&format!("{name}: {}", match mode { "instant" => "now sorted instantly", "interval" => "now sorted every few hours", _ => "auto-sort turned off" }));
    Ok(applied(&s))
}

fn remove_folder(path: &str) -> Value {
    let mut s = settings::load();
    s.folders.retain(|f| !same_path(&f.path, path));
    let _ = settings::save(&s);
    activity::info(&format!("Stopped watching {}", Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())));
    applied(&s)
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
            rules::add_keywords(&s_arg(a, 0), &words, a.get(2).and_then(Value::as_f64).unwrap_or(2.0))?;
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
        // older page versions called these Start/Stop; now they resume/pause auto-sort
        "start_background" | "resume" => {
            background::set_paused(false);
            background::status_json()
        }
        "stop_background" | "pause" => {
            background::set_paused(true);
            background::status_json()
        }
        "sort_all_now" => {
            let n = background::WORKER.get().map(|w| w.sort_all_now()).unwrap_or(0);
            json!(n)
        }
        "add_folder" => match rfd::FileDialog::new().set_directory(folder()).pick_folder() {
            Some(p) => set_folder_mode(&p.to_string_lossy(), a.get(0).and_then(Value::as_str).unwrap_or("instant"))?,
            None => Value::Null,
        },
        "set_folder_mode" => set_folder_mode(&s_arg(a, 0), &s_arg(a, 1))?,
        "remove_folder" => remove_folder(&s_arg(a, 0)),
        "activity" => {
            let limit = a.first().and_then(Value::as_u64).unwrap_or(1500) as usize;
            let jobs: Vec<Value> = JOBS.lock().unwrap().values().filter(|j| !j.done).map(|j| json!({"kind": j.kind, "progress": [j.progress.0, j.progress.1], "message": j.message})).collect();
            json!({"entries": activity::recent(limit), "status": background::status_json(), "jobs": jobs, "unread": unread_summary(false),
                   "undone": sorter::undone_batches()})
        }
        "undo_batch" => json!(sorter::undo_batch(&s_arg(a, 0))?),
        "start_onedrive" => {
            let local = std::env::var("LOCALAPPDATA").unwrap_or_default();
            let pf = std::env::var("ProgramFiles").unwrap_or_default();
            let exe = [format!("{local}\\Microsoft\\OneDrive\\OneDrive.exe"), format!("{pf}\\Microsoft OneDrive\\OneDrive.exe")]
                .into_iter()
                .find(|p| Path::new(p).exists())
                .ok_or("OneDrive isn't installed in the usual place - start it from the Start menu.")?;
            std::process::Command::new(exe).arg("/background").spawn().map_err(|e| e.to_string())?;
            json!(true)
        }
        "review" => review_lists(),
        "review_count" => review_count(),
        "review_ok" => {
            mark_reviewed(&paths_arg(a.first()));
            Value::Null
        }
        "albums" => albums_json(),
        "save_album" => save_album(&s_arg(a, 0), &s_arg(a, 1))?,
        "delete_album" => {
            let mut s = settings::load();
            s.albums.retain(|x| x.name != s_arg(a, 0));
            settings::save(&s).map_err(|e| e.to_string())?;
            albums_json()
        }
        "league" => crate::trackers::league(&all_folders()),
        "youtube" => crate::trackers::youtube(&all_folders()),
        "suggest_folders" => suggest_folders(),
        "local_models" => match crate::localai::models() {
            Ok(m) => json!({"ok": true, "models": m}),
            Err(e) => json!({"ok": false, "error": e}),
        },
        "local_suggest" => {
            // Review page: what does the local model think this one is?
            let p = PathBuf::from(s_arg(a, 0));
            if !allowed(&p) {
                return Err("Not in a watched folder.".into());
            }
            let s = settings::load();
            let model = crate::localai::model(&s).ok_or("Turn on the local model in Settings first.")?;
            let text = ocr::load_cache().get(&cache_key(&p)).cloned().unwrap_or_default();
            let (c, why) = crate::localai::ask(&model, &p, &text, &rules::load())?;
            json!({"c": c, "why": why, "model": model})
        }
        "pick_folder_path" => match rfd::FileDialog::new().set_directory(folder()).pick_folder() {
            Some(p) => json!(p.to_string_lossy()),
            None => Value::Null,
        },
        "drag_out" => {
            // drag pictures out of the app into Discord, a browser, Explorer...
            let paths: Vec<PathBuf> = paths_arg(a.first()).into_iter().filter(|p| allowed(p)).take(50).collect();
            if paths.is_empty() {
                return Ok(Value::Null);
            }
            let preview = tools::thumb_bytes(&paths[0], 160);
            let app = APP.get().ok_or("not ready")?.clone();
            let a2 = app.clone();
            app.run_on_main_thread(move || {
                // Only while the left button is still held: a drag started after it was released
                // would wait for the next click and then drop the files wherever the mouse is.
                let held = unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState(0x01) } as u16 & 0x8000 != 0;
                if !held {
                    return;
                }
                if let Some(w) = a2.get_webview_window("main") {
                    let image = match preview {
                        Some(b) => drag::Image::Raw(b),
                        None => drag::Image::File(paths[0].clone()),
                    };
                    let _ = drag::start_drag(&w, drag::DragItem::Files(paths), image, |_, _| {}, drag::Options::default());
                }
            })
            .map_err(|e| e.to_string())?;
            Value::Null
        }
        "undo_one" => {
            sorter::undo_one(Path::new(&s_arg(a, 0)), Path::new(&s_arg(a, 1)))?;
            Value::Null
        }
        "set_zoom" => {
            let z = a.first().and_then(Value::as_f64).unwrap_or(1.0).clamp(0.6, 1.6);
            let z = (z * 100.0).round() / 100.0;
            settings::update(&[("ui_zoom".to_string(), json!(z))].into_iter().collect());
            if let Some(w) = APP.get().and_then(|app| app.get_webview_window("main")) {
                let _ = w.set_zoom(z);
            }
            json!(z)
        }
        "library_snapshot" => {
            let snap: Value = std::fs::read_to_string(library_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
            let want = if a.first().and_then(Value::as_bool) == Some(true) { "*".to_string() } else { folder().to_string_lossy().to_string() };
            if snap["folder"].as_str() == Some(want.as_str()) { snap } else { json!({"items": []}) }
        }
        other => return Err(format!("unknown method {other}")),
    })
}

#[tauri::command]
async fn call(method: String, args: Vec<Value>) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || dispatch(&method, &args)).await.map_err(|e| e.to_string())?
}

// ---------- thumb:// image scheme ----------

/// A few worker threads answer image requests, newest request first (what's on screen
/// right now), instead of one thread per request. Online-only OneDrive files have their
/// own lane so they never hold up local ones.
mod thumbq {
    use once_cell::sync::Lazy;
    use std::sync::{Condvar, Mutex};
    use tauri::UriSchemeResponder;

    pub struct Q {
        items: Mutex<Vec<(String, UriSchemeResponder)>>,
        cv: Condvar,
    }
    fn new_q(workers: usize, com: bool) -> &'static Q {
        let q: &'static Q = Box::leak(Box::new(Q { items: Mutex::new(Vec::new()), cv: Condvar::new() }));
        for _ in 0..workers {
            std::thread::spawn(move || {
                if com {
                    unsafe {
                        let _ = windows::Win32::System::Com::CoInitializeEx(None, windows::Win32::System::Com::COINIT_APARTMENTTHREADED);
                    }
                }
                loop {
                    let (uri, r) = {
                        let mut g = q.items.lock().unwrap();
                        while g.is_empty() {
                            g = q.cv.wait(g).unwrap();
                        }
                        g.pop().unwrap()
                    };
                    let resp = std::panic::catch_unwind(|| super::image_response(&uri))
                        .unwrap_or_else(|_| tauri::http::Response::builder().status(500).body(Vec::new()).unwrap());
                    r.respond(resp);
                }
            });
        }
        q
    }
    static LOCAL: Lazy<&'static Q> = Lazy::new(|| new_q(4, false));
    static CLOUD: Lazy<&'static Q> = Lazy::new(|| new_q(2, true));

    pub fn submit(uri: String, r: UriSchemeResponder) {
        let cloud = super::query_path(&uri).map(|p| crate::ocr::online_only(&p)).unwrap_or(false);
        let q: &Q = if cloud { &CLOUD } else { &LOCAL };
        q.items.lock().unwrap().push((uri, r));
        q.cv.notify_one();
    }
}

fn query_path(uri: &str) -> Option<PathBuf> {
    let (_, q) = uri.split_once('?')?;
    q.split('&').filter_map(|kv| kv.split_once('=')).find(|(k, _)| *k == "p").map(|(_, v)| PathBuf::from(urlencoding::decode(v).map(|c| c.into_owned()).unwrap_or_default()))
}

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
    } else if ocr::online_only(&p) {
        (tools::cloud_thumb_bytes(&p, size), "image/jpeg".to_string())
    } else {
        (tools::thumb_bytes(&p, size), "image/jpeg".to_string())
    };
    match body {
        Some(b) => Response::builder().header("Content-Type", ctype).header("Cache-Control", "max-age=3600").body(b).unwrap(),
        None => nf(),
    }
}

// ---------- the app: one process with the window, tray icon and auto-sort ----------

static APP: OnceCell<AppHandle> = OnceCell::new();

fn notify(title: &str, body: &str) {
    let _ = notify_rust::Notification::new().summary(title).body(body).appname("Screenshot Sorter").show();
}

fn geometry_file() -> PathBuf {
    settings::app_file("window_state.json")
}

/// Remember where the window was, to open it there next time.
fn save_geometry(w: &WebviewWindow) {
    let max = w.is_maximized().unwrap_or(false);
    let mut g: Value = std::fs::read_to_string(geometry_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    g["max"] = json!(max);
    if !max && !w.is_minimized().unwrap_or(false) {
        if let (Ok(p), Ok(sz)) = (w.outer_position(), w.inner_size()) {
            g["x"] = json!(p.x);
            g["y"] = json!(p.y);
            g["w"] = json!(sz.width);
            g["h"] = json!(sz.height);
        }
    }
    let _ = settings::write_atomic(&geometry_file(), g.to_string().as_bytes());
}

/// Show the window (create it if needed). `preview`: start a preview of the open folder.
pub fn open_window(app: &AppHandle, preview: bool) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let f = serde_json::to_string(&folder().to_string_lossy()).unwrap_or_default();
        let _ = w.eval(&format!("window.__switchFolder && window.__switchFolder({f}, {preview})"));
        return;
    }
    let mut b = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Screenshot Sorter")
        .min_inner_size(760.0, 520.0)
        .visible(false)
        .background_color(tauri::webview::Color(15, 17, 21, 255));
    if preview {
        b = b.initialization_script("window.__AUTO = 'preview';");
    }
    // a size that fits this screen: up to 1320x860, at most ~90% of it
    let (mut lw, mut lh) = (1320.0_f64, 860.0_f64);
    if let Ok(Some(m)) = app.primary_monitor() {
        let sz = m.size().to_logical::<f64>(m.scale_factor());
        lw = lw.min(sz.width * 0.9);
        lh = lh.min(sz.height * 0.88);
    }
    b = b.inner_size(lw, lh).center();
    let Ok(w) = b.build() else { return };
    // put it back where it was last time, if that spot is still on a screen
    let g: Value = std::fs::read_to_string(geometry_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
    if let (Some(x), Some(y), Some(gw), Some(gh)) = (g["x"].as_i64(), g["y"].as_i64(), g["w"].as_u64(), g["h"].as_u64()) {
        let on_screen = w.available_monitors().unwrap_or_default().iter().any(|m| {
            let (p, s) = (m.position(), m.size());
            let (cx, cy) = (x as i32 + 120, y as i32 + 40);
            cx >= p.x && cy >= p.y && cx < p.x + s.width as i32 && cy < p.y + s.height as i32
        });
        if on_screen && gw >= 400 && gh >= 300 {
            let _ = w.set_size(tauri::PhysicalSize::new(gw as u32, gh as u32));
            let _ = w.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
        }
    }
    if g["max"].as_bool() == Some(true) {
        let _ = w.maximize();
    }
    let _ = w.set_zoom(settings::load().ui_zoom);
    let w2 = w.clone();
    w.on_window_event(move |ev| {
        if let WindowEvent::CloseRequested { .. } = ev {
            save_geometry(&w2);
        }
    });
    let _ = w.show();
    let _ = w.set_focus();
}

fn refresh_tray() {
    if let Some(app) = APP.get() {
        if let Some(t) = app.tray_by_id("tray") {
            let _ = t.set_tooltip(Some(background::summary()));
            let _ = t.set_visible(settings::load().show_tray_icon);
        }
    }
}

/// Another launch of the app (Start menu, Explorer menu, login) ends up here.
fn second_launch(app: &AppHandle, argv: Vec<String>) {
    let args: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
    match args.first() {
        Some(&"--tray") => {}
        Some(&"--gui-folder") => {
            if let Some(f) = args.get(1).map(PathBuf::from).filter(|f| f.is_dir()) {
                *FOLDER.lock().unwrap() = f;
            }
            open_window(app, true);
        }
        _ => open_window(app, false),
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let open_i = MenuItem::with_id(app, "open", "Open Screenshot Sorter", true, None::<&str>)?;
    let sort_i = MenuItem::with_id(app, "sort", "Sort now", true, None::<&str>)?;
    let pause_i = CheckMenuItem::with_id(app, "pause", "Pause auto-sort", true, background::is_paused(), None::<&str>)?;
    let undo_i = MenuItem::with_id(app, "undo", "Undo last sort", true, None::<&str>)?;
    let upd_i = MenuItem::with_id(app, "update", "Check for updates", true, None::<&str>)?;
    let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(app, &[&open_i, &sort_i, &pause_i, &undo_i, &upd_i, &sep, &quit_i])?;
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?;
    let _ = PAUSE_ITEM.set(pause_i.clone());
    TrayIconBuilder::with_id("tray")
        .icon(icon)
        .tooltip(background::summary())
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, ev| match ev.id().as_ref() {
            "open" => open_window(app, false),
            "sort" => {
                std::thread::spawn(|| {
                    if let Some(w) = background::WORKER.get() {
                        if w.sort_all_now() == 0 {
                            notify("Screenshot Sorter", "Nothing new to sort.");
                        }
                    }
                });
            }
            "pause" => background::set_paused(!background::is_paused()),
            "undo" => {
                let (n, had) = sorter::undo_last();
                notify("Screenshot Sorter", &if had { format!("Moved {n} file(s) back.") } else { "Nothing to undo.".into() });
            }
            "update" => {
                let app = app.clone();
                std::thread::spawn(move || {
                    let v = crate::updater::check(true);
                    if let Some(e) = v["error"].as_str() {
                        notify("Couldn't check for updates", e);
                    } else if v["available"] == true {
                        notify("Update available", &format!("Version {} is out.", v["latest"].as_str().unwrap_or("")));
                        let a2 = app.clone();
                        let _ = app.run_on_main_thread(move || open_window(&a2, false));
                    } else {
                        notify("Screenshot Sorter", &format!("You have the newest version ({}).", crate::updater::VERSION));
                    }
                });
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } = ev {
                open_window(tray.app_handle(), false);
            }
        })
        .build(app)?;
    if let Some(t) = app.tray_by_id("tray") {
        let _ = t.set_visible(settings::load().show_tray_icon);
    }
    Ok(())
}

static PAUSE_ITEM: OnceCell<CheckMenuItem<tauri::Wry>> = OnceCell::new();

/// The app. `show_window` false = start in the background (at login).
pub fn run_app(show_window: bool, start_folder: Option<PathBuf>, preview: bool) {
    if let Some(f) = start_folder.filter(|f| f.is_dir()) {
        *FOLDER.lock().unwrap() = f;
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| second_launch(app, argv)))
        .invoke_handler(tauri::generate_handler![call])
        .register_asynchronous_uri_scheme_protocol("thumb", |_ctx, req, responder| {
            thumbq::submit(req.uri().to_string(), responder);
        })
        .setup(move |app| {
            let _ = APP.set(app.handle().clone());
            let h = app.handle().clone();
            background::claim_app(move || h.exit(0));
            let _ = background::ON_CHANGE.set(Box::new(|| {
                if let Some(app) = APP.get() {
                    let _ = app.run_on_main_thread(|| {
                        refresh_tray();
                        if let Some(i) = PAUSE_ITEM.get() {
                            let _ = i.set_checked(background::is_paused());
                        }
                    });
                }
            }));
            build_tray(app)?;
            background::start(Some(Box::new(notify)));
            if show_window {
                open_window(app.handle(), preview);
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not start Screenshot Sorter");
    app.run(|app, ev| match ev {
        // closing the window keeps auto-sort running in the background; with nothing to do, quit
        RunEvent::ExitRequested { api, code: None, .. } => {
            if settings::load().auto_on() {
                api.prevent_exit();
            }
        }
        RunEvent::Exit => {
            if let Some(w) = app.get_webview_window("main") {
                save_geometry(&w);
            }
            if let Some(w) = background::WORKER.get() {
                w.shutdown();
            }
        }
        _ => {}
    });
}
