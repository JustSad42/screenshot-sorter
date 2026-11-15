//! Optional: ask Claude which category a leftover screenshot belongs to. Off by default.
//! Only screenshots the text rules and look-matching couldn't place are sent, downscaled.
//! There is no official Rust SDK, so this calls the Messages API over HTTPS directly.
use crate::ocr::{Log, Progress};
use crate::rules::{pretty_kw, Rules};
use crate::settings::Settings;
use base64::Engine;
use image::ImageEncoder;
use rayon::prelude::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const URL: &str = "https://api.anthropic.com/v1/messages";
const MODEL: &str = "claude-opus-5-5";
const MAX_SIDE: u32 = 1568; // larger images are downscaled by the API anyway

fn api_key(s: &Settings) -> Result<String, String> {
    if !s.claude_api_key.trim().is_empty() {
        return Ok(s.claude_api_key.trim().to_string());
    }
    std::env::var("ANTHROPIC_API_KEY").map_err(|_| "No API key - add one in Settings, or set ANTHROPIC_API_KEY.".to_string())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(180))).build().into()
}

fn post(key: &str, body: &Value) -> Result<Value, String> {
    match agent()
        .post(URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2026-10-21")
        .header("anthropic-beta", "server-side-fallback-2026-07-01")
        .header("content-type", "application/json")
        .send_json(body)
    {
        Ok(mut r) => r.body_mut().read_json::<Value>().map_err(|e| e.to_string()),
        Err(ureq::Error::StatusCode(401)) => Err("That API key wasn't accepted.".into()),
        Err(ureq::Error::StatusCode(c)) => Err(format!("Claude returned HTTP {c}")),
        Err(e) => Err(format!("Couldn't reach Claude: {e}")),
    }
}

fn image_block(p: &Path) -> Result<Value, String> {
    let img = crate::imageio::open(p).ok_or("could not open image")?.thumbnail(MAX_SIDE, MAX_SIDE).to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 85)
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)
        .map_err(|e| e.to_string())?;
    Ok(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg",
        "data": base64::engine::general_purpose::STANDARD.encode(buf)}}))
}

pub(crate) fn category_list(rules: &Rules) -> String {
    let mut lines: Vec<String> = rules
        .cats
        .iter()
        .map(|c| {
            let mut kws: Vec<&(regex::Regex, String, f64)> = c.keywords.iter().collect();
            kws.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
            let hints: Vec<String> = kws.iter().take(6).map(|k| pretty_kw(&k.1)).filter(|h| !h.is_empty()).collect();
            format!("- {} (clues: {})", c.name, hints.join(", "))
        })
        .collect();
    lines.push(format!("- {} (none of the above)", rules.fallback));
    lines.join("\n")
}

fn ask(key: &str, p: &Path, rules: &Rules, cats: &str) -> Result<Option<String>, String> {
    let body = json!({
        "model": MODEL,
        "max_tokens": 2048,
        "fallbacks": "default",
        "output_config": {"effort": "low"},
        "system": "You sort a person's screenshots into folders. Answer with the folder name only.",
        "messages": [{"role": "user", "content": [
            image_block(p)?,
            {"type": "text", "text": format!("Which one of these folders does this screenshot belong in?\n{cats}\n\nReply with exactly one folder name from the list.")}
        ]}]
    });
    let r = post(key, &body)?;
    if r["stop_reason"] == "refusal" {
        return Ok(None);
    }
    let text: String = r["content"].as_array().map(|a| {
        a.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join(" ")
    }).unwrap_or_default().to_lowercase();
    // longest name first, so "League - In-Game" isn't mistaken for a shorter match
    let mut names: Vec<String> = rules.cats.iter().map(|c| c.name.clone()).chain([rules.fallback.clone()]).collect();
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    Ok(names.into_iter().find(|n| text.contains(&n.to_lowercase())))
}

/// {path: category} for leftover screenshots.
pub fn classify(paths: &[PathBuf], rules: &Rules, s: &Settings, progress: Progress, log: Log) -> Result<HashMap<PathBuf, String>, String> {
    let key = api_key(s)?;
    let cats = category_list(rules);
    log(&format!("Asking Claude about {} leftover screenshot(s)...", paths.len()));
    let done = AtomicUsize::new(0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(4).build().unwrap();
    let results: Vec<(PathBuf, Result<Option<String>, String>)> = pool.install(|| {
        paths.par_iter().map(|p| {
            let r = ask(&key, p, rules, &cats);
            progress(done.fetch_add(1, Ordering::Relaxed) + 1, paths.len());
            (p.clone(), r)
        }).collect()
    });
    let mut out = HashMap::new();
    for (p, r) in results {
        match r {
            Ok(Some(c)) => { out.insert(p, c); }
            Ok(None) => {}
            Err(e) if e.contains("API key") => return Err(e),
            Err(e) => log(&format!("Claude couldn't check {}: {e}", p.display())),
        }
    }
    Ok(out)
}

/// One tiny text request. -> (ok, message)
pub fn test_connection(s: &Settings) -> (bool, String) {
    let key = match api_key(s) { Ok(k) => k, Err(e) => return (false, e) };
    let body = json!({"model": MODEL, "max_tokens": 64, "output_config": {"effort": "low"},
        "messages": [{"role": "user", "content": "Reply with OK."}]});
    match post(&key, &body) {
        Ok(_) => (true, "Connected to Claude.".into()),
        Err(e) => (false, e),
    }
}
