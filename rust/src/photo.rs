//! Camera photos vs screenshots. Photos carry camera details (EXIF make/model);
//! screenshots don't. Photos go to "Photos/YYYY-MM" instead of a screenshot category.
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use std::path::Path;

pub const PHOTOS: &str = "Photos";

pub struct Camera {
    /// "Samsung SM-G950F", "Apple iPhone 12"...
    pub model: String,
    pub taken: Option<DateTime<Local>>,
}

const EXIF_EXTS: &[&str] = &["jpg", "jpeg", "heic", "heif", "tif", "tiff", "png", "webp", "avif"];

/// Camera details, if this picture was taken with a camera or phone camera.
pub fn camera(p: &Path) -> Option<Camera> {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if !EXIF_EXTS.contains(&ext.as_str()) || crate::ocr::online_only(p) {
        return None;
    }
    let mut r = std::io::BufReader::new(std::fs::File::open(p).ok()?);
    let ex = exif::Reader::new().read_from_container(&mut r).ok()?;
    let get = |t: exif::Tag| {
        ex.get_field(t, exif::In::PRIMARY)
            .map(|f| f.display_value().to_string().trim_matches(|c: char| c == '"' || c.is_whitespace() || c == '\0').to_string())
            .filter(|s| !s.is_empty())
    };
    let make = get(exif::Tag::Make);
    let model = get(exif::Tag::Model);
    if make.is_none() && model.is_none() {
        return None;
    }
    // "SAMSUNG" + "SM-G950F" -> "Samsung SM-G950F"; skip the make when the model already says it
    let make = make.map(|m| {
        let m = m.trim().to_string();
        let mut c = m.to_lowercase().chars().collect::<Vec<_>>();
        if let Some(f) = c.first_mut() {
            *f = f.to_ascii_uppercase();
        }
        c.into_iter().collect::<String>()
    });
    let model_s = match (&make, &model) {
        (Some(mk), Some(md)) if md.to_lowercase().starts_with(&mk.to_lowercase()) => md.clone(),
        (Some(mk), Some(md)) => format!("{mk} {md}"),
        (Some(mk), None) => mk.clone(),
        (None, Some(md)) => md.clone(),
        (None, None) => String::new(),
    };
    let taken = get(exif::Tag::DateTimeOriginal).or_else(|| get(exif::Tag::DateTime)).and_then(|s| {
        ["%Y-%m-%d %H:%M:%S", "%Y:%m:%d %H:%M:%S"]
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(s.get(..19).unwrap_or(&s), f).ok())
            .and_then(|d| Local.from_local_datetime(&d).earliest())
    });
    Some(Camera { model: model_s, taken })
}

/// "Photos/2018-06/2018-06-25 154640 Photo.jpg" (or the original name when renaming is off).
pub fn target(root: &Path, p: &Path, cam: Option<&Camera>, rename: bool) -> std::path::PathBuf {
    let when = cam.and_then(|c| c.taken).unwrap_or_else(|| crate::sorter::taken_at(p));
    let dir = root.join(PHOTOS).join(when.format("%Y-%m").to_string());
    let name = if rename {
        let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy().to_lowercase())).unwrap_or_default();
        format!("{} Photo{ext}", when.format("%Y-%m-%d %H%M%S"))
    } else {
        p.file_name().unwrap_or_default().to_string_lossy().to_string()
    };
    dir.join(name)
}

pub fn reason(cam: &Camera) -> String {
    if cam.model.is_empty() { "camera photo".into() } else { format!("camera photo · {}", cam.model) }
}
