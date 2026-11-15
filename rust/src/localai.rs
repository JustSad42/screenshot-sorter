//! Optional: a vision model running in Ollama on this PC suggests a folder for screenshots the
//! rules couldn't place. Nothing leaves the computer. Only used when you sort by hand (Sort page
//! preview / Sort now, and the Review page) - never by background auto-sort.
use crate::ocr::{Log, Progress};
use crate::rules::Rules;
use crate::settings::Settings;
use base64::Engine;
use image::ImageEncoder;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const HOST: &str = "http://127.0.0.1:11434";
const MAX_SIDE: u32 = 1024;

fn agent(secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(secs))).build().into()
}

fn get(path: &str, secs: u64) -> Result<Value, String> {
    let mut r = agent(secs).get(format!("{HOST}{path}")).call().map_err(|_| not_running())?;
    r.body_mut().read_json::<Value>().map_err(|e| e.to_string())
}

fn post(path: &str, body: &Value, secs: u64) -> Result<Value, String> {
    match agent(secs).post(format!("{HOST}{path}")).send_json(body) {
        Ok(mut r) => r.body_mut().read_json::<Value>().map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(404)) => Err("That model isn't installed in Ollama any more.".into()),
        Err(ureq::Error::StatusCode(c)) => Err(format!("Ollama returned HTTP {c}")),
        Err(ureq::Error::Timeout(_)) => Err("The local model took too long to answer.".into()),
        Err(_) => Err(not_running()),
    }
}

fn not_running() -> String {
    "Ollama isn't running. Start it from the Start menu, then try again.".into()
}

/// The model to use, if the local model is turned on.
pub fn model(s: &Settings) -> Option<String> {
    (s.local_ai && !s.local_model.trim().is_empty()).then(|| s.local_model.trim().to_string())
}

/// Chat models installed in Ollama: [{name, vision, gb}], ones that can see images first.
pub fn models() -> Result<Vec<Value>, String> {
    let tags = get("/api/tags", 4)?;
    let mut out: Vec<Value> = tags["models"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?.to_string();
            let caps = post("/api/show", &json!({"model": name}), 10).ok()?["capabilities"].clone();
            let has = |c: &str| caps.as_array().map(|a| a.iter().any(|x| x == c)).unwrap_or(false);
            // embedding-only models can't answer questions
            (has("completion") || caps.is_null()).then(|| {
                json!({"name": name, "vision": has("vision"), "gb": (m["size"].as_f64().unwrap_or(0.0) / 1e8).round() / 10.0})
            })
        })
        .collect();
    out.sort_by_key(|m| (m["vision"] != true, m["name"].as_str().unwrap_or("").to_string()));
    Ok(out)
}

fn jpeg_b64(p: &Path) -> Option<String> {
    let img = crate::imageio::open(p)?.thumbnail(MAX_SIDE, MAX_SIDE).to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)
        .ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(buf))
}

/// Ask about one screenshot. -> (folder, short reason)
pub fn ask(model: &str, p: &Path, text: &str, rules: &Rules) -> Result<(String, String), String> {
    let names: Vec<String> = rules.cats.iter().map(|c| c.name.clone()).chain([rules.fallback.clone()]).collect();
    let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(800).collect();
    let mut msg = json!({"role": "user", "content": format!(
        "Which one of these folders does this screenshot belong in?\n{}\n\n{}Answer with the folder name exactly as written, and a reason of at most eight words.",
        crate::claude::category_list(rules),
        if text.is_empty() { "It has no readable text.\n\n".to_string() } else { format!("Text found in it (may be messy): {text}\n\n") })});
    if let Some(b64) = jpeg_b64(p) {
        msg["images"] = json!([b64]);
    }
    let body = json!({
        "model": model,
        "stream": false,
        "think": false,
        "keep_alive": "10m",
        "options": {"temperature": 0},
        "format": {"type": "object", "properties": {
            "folder": {"type": "string", "enum": names},
            "reason": {"type": "string"}}, "required": ["folder", "reason"]},
        "messages": [
            {"role": "system", "content": "You sort a person's screenshots into folders. Pick the single best folder from the list."},
            msg]
    });
    let r = post("/api/chat", &body, 240)?;
    if let Some(e) = r["error"].as_str() {
        return Err(e.to_string());
    }
    let answer: Value = serde_json::from_str(r["message"]["content"].as_str().unwrap_or("")).map_err(|_| "The local model gave an answer that couldn't be read.".to_string())?;
    let said = answer["folder"].as_str().unwrap_or("").trim().to_lowercase();
    let folder = names.into_iter().find(|n| n.to_lowercase() == said).ok_or("The local model picked a folder that isn't in the list.")?;
    let reason: String = answer["reason"].as_str().unwrap_or("").trim().chars().take(80).collect();
    Ok((folder, reason))
}

/// {path: (folder, reason)} for several screenshots, one at a time (one GPU).
pub fn classify(model: &str, files: &[(PathBuf, String)], rules: &Rules, progress: Progress, log: Log) -> HashMap<PathBuf, (String, String)> {
    let mut out = HashMap::new();
    log(&format!("Asking the local model ({model}) about {} leftover screenshot(s)...", files.len()));
    progress(0, files.len());
    for (i, (p, t)) in files.iter().enumerate() {
        match ask(model, p, t, rules) {
            Ok(a) => {
                out.insert(p.clone(), a);
            }
            Err(e) => {
                log(&format!("Local model: {e}"));
                if e.contains("isn't running") || e.contains("isn't installed") {
                    break;
                }
            }
        }
        progress(i + 1, files.len());
    }
    out
}
