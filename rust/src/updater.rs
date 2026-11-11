//! Update check against the public GitHub repo's releases.
//! A release is expected to carry an installer asset named "...-setup.exe".
use crate::settings;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const REPO: &str = "JustSad42/screenshot-sorter";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const DAY: f64 = 24.0 * 3600.0;

#[derive(Clone, Debug)]
pub struct Release {
    pub version: String,
    pub notes: String,
    pub page: String,
    pub setup_url: Option<String>,
    pub setup_sha256: Option<String>,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).build().into()
}

fn now() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn parse_ver(s: &str) -> Vec<u64> {
    s.trim().trim_start_matches(['v', 'V']).split(['.', '-', '+']).map_while(|p| p.parse().ok()).collect()
}

/// true if `a` is a newer version than `b`
pub fn is_newer(a: &str, b: &str) -> bool {
    let (mut x, mut y) = (parse_ver(a), parse_ver(b));
    let n = x.len().max(y.len());
    x.resize(n, 0);
    y.resize(n, 0);
    x > y
}

/// The newest published release.
pub fn latest() -> Result<Release, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut r = agent()
        .get(&url)
        .header("User-Agent", "ScreenshotSorter-updater")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| match e {
            ureq::Error::StatusCode(404) => "No releases published yet.".to_string(),
            ureq::Error::StatusCode(c) => format!("GitHub returned HTTP {c}"),
            e => format!("Couldn't reach GitHub: {e}"),
        })?;
    let v: Value = r.body_mut().read_json().map_err(|e| e.to_string())?;
    let asset = v["assets"].as_array().and_then(|a| {
        a.iter().find(|x| x["name"].as_str().map(|n| n.to_lowercase().ends_with("-setup.exe")).unwrap_or(false))
    });
    Ok(Release {
        version: v["tag_name"].as_str().unwrap_or("").trim_start_matches(['v', 'V']).to_string(),
        notes: v["body"].as_str().unwrap_or("").to_string(),
        page: v["html_url"].as_str().unwrap_or("").to_string(),
        setup_url: asset.and_then(|a| a["browser_download_url"].as_str()).map(String::from),
        setup_sha256: asset
            .and_then(|a| a["digest"].as_str())
            .and_then(|d| d.strip_prefix("sha256:"))
            .map(|d| d.to_lowercase()),
    })
}

/// Installed with the installer (rather than a loose portable copy)?
pub fn is_installed() -> bool {
    settings::app_file("unins000.exe").exists()
}

fn state_file() -> PathBuf {
    settings::app_file("update_state.json")
}
fn read_state() -> Value {
    std::fs::read_to_string(state_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}))
}
fn write_state(v: &Value) {
    let _ = settings::write_atomic(&state_file(), v.to_string().as_bytes());
}

fn to_json(r: &Release) -> Value {
    json!({"current": VERSION, "latest": r.version, "available": is_newer(&r.version, VERSION),
           "notes": r.notes, "page": r.page, "can_install": is_installed() && r.setup_url.is_some(),
           "installed": is_installed()})
}

/// For the window. Uses a result less than 6 hours old unless `force`.
pub fn check(force: bool) -> Value {
    let mut st = read_state();
    let fresh = now() - st["checked"].as_f64().unwrap_or(0.0) < 6.0 * 3600.0;
    if !force && fresh && st["release"].is_object() {
        let mut v = st["release"].clone();
        let latest = v["latest"].as_str().unwrap_or("").to_string();
        v["current"] = json!(VERSION);
        v["available"] = json!(is_newer(&latest, VERSION));
        return v;
    }
    match latest() {
        Ok(r) => {
            let v = to_json(&r);
            st["checked"] = json!(now());
            st["release"] = v.clone();
            write_state(&st);
            v
        }
        Err(e) => json!({"current": VERSION, "available": false, "error": e}),
    }
}

/// Background: at most once a day, tell the user (once per version) that an update is out.
pub fn daily_check(notify: &dyn Fn(&str, &str)) {
    if !settings::load().check_updates {
        return;
    }
    let mut st = read_state();
    if now() - st["checked"].as_f64().unwrap_or(0.0) < DAY {
        return;
    }
    let Ok(r) = latest() else {
        st["checked"] = json!(now() - DAY + 3600.0); // offline: try again in an hour
        write_state(&st);
        return;
    };
    st["checked"] = json!(now());
    st["release"] = to_json(&r);
    if is_newer(&r.version, VERSION) && st["notified"].as_str() != Some(&r.version) {
        notify("Update available", &format!("Screenshot Sorter {} is out. Open the app to update.", r.version));
        st["notified"] = json!(r.version);
    }
    write_state(&st);
}

/// Download the newest installer, check its SHA-256 against GitHub's, and start it silently.
/// The installer closes this app, updates it and opens it again.
pub fn install(log: &dyn Fn(&str)) -> Result<String, String> {
    if !is_installed() {
        return Err("This copy wasn't installed with the installer, so it can't update itself. Download the new version from GitHub.".into());
    }
    let r = latest()?;
    if !is_newer(&r.version, VERSION) {
        return Err(format!("You already have the newest version ({VERSION})."));
    }
    let url = r.setup_url.clone().ok_or("That release has no installer attached.")?;
    log(&format!("Downloading version {}...", r.version));
    let dl: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(600))).build().into();
    let mut resp = dl
        .get(&url)
        .header("User-Agent", "ScreenshotSorter-updater")
        .call()
        .map_err(|e| format!("Download failed: {e}"))?;
    let bytes = resp.body_mut().with_config().limit(300 * 1024 * 1024).read_to_vec().map_err(|e| format!("Download failed: {e}"))?;
    if let Some(want) = &r.setup_sha256 {
        let got: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        if &got != want {
            return Err("The download didn't match GitHub's checksum, so it wasn't installed.".into());
        }
    }
    let dir = std::env::temp_dir().join("ScreenshotSorterUpdate");
    let _ = std::fs::create_dir_all(&dir);
    let file = dir.join(format!("ScreenshotSorter-{}-setup.exe", r.version));
    std::fs::write(&file, &bytes).map_err(|e| format!("Couldn't save the installer: {e}"))?;
    log("Installing...");
    std::process::Command::new(&file)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/CLOSEAPPLICATIONS"])
        .spawn()
        .map_err(|e| format!("Couldn't start the installer: {e}"))?;
    Ok(r.version)
}

#[cfg(test)]
mod tests {
    use super::is_newer;
    #[test]
    fn versions() {
        assert!(is_newer("2.0.1", "2.0.0"));
        assert!(is_newer("v2.1", "2.0.9"));
        assert!(is_newer("10.0.0", "9.9.9"));
        assert!(!is_newer("2.0.0", "2.0.0"));
        assert!(!is_newer("2.0", "2.0.0"));
        assert!(!is_newer("1.9.9", "2.0.0"));
    }
}
