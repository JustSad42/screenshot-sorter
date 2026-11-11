//! Clean-up tools and thumbnails: Recycle Bin, PNG -> lossless WebP, and
//! thumbnails kept on disk so the app opens fast next time.
use crate::ocr::{self, cache_key, Log, Progress};
use crate::settings;
use crate::sorter::all_images;
use image::ImageEncoder;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub fn recycle(paths: &[PathBuf]) -> usize {
    let existing: Vec<&PathBuf> = paths.iter().filter(|p| p.exists()).collect();
    let _ = trash::delete_all(&existing);
    existing.iter().filter(|p| !p.exists()).count()
}

/// How to shrink screenshots.
#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// PNG -> WebP, pixel-for-pixel identical. Originals go to the Recycle Bin.
    Lossless,
    /// PNG/WebP/BMP/TIFF -> AVIF quality 80: far smaller, looks the same, not bit-identical.
    /// Originals are moved to the backup folder (the Recycle Bin can silently drop big batches).
    Avif,
}

impl Mode {
    pub fn parse(s: &str) -> Mode {
        if s == "avif" { Mode::Avif } else { Mode::Lossless }
    }
    fn takes(self, ext: &str) -> bool {
        match self {
            Mode::Lossless => ext == "png",
            Mode::Avif => matches!(ext, "png" | "webp" | "bmp" | "tif" | "tiff"),
        }
    }
}

fn ext_of(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn candidates(folder: &Path, mode: Mode) -> Vec<PathBuf> {
    all_images(folder).into_iter().filter(|p| mode.takes(&ext_of(p))).collect()
}

pub fn estimate(folder: &Path, mode: Mode) -> (usize, u64) {
    let f = candidates(folder, mode);
    let size = f.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
    (f.len(), size)
}

pub fn originals_dir() -> PathBuf {
    settings::app_file("originals")
}

/// (files, bytes) currently kept in the originals backup.
pub fn originals_info() -> (usize, u64) {
    let files = crate::sorter::all_images(&originals_dir());
    (files.len(), files.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum())
}

fn convert_one(p: &Path, mode: Mode) -> Result<Option<(PathBuf, u64, u64)>, String> {
    let dest = p.with_extension(if mode == Mode::Avif { "avif" } else { "webp" });
    if dest.exists() {
        return Ok(None);
    }
    let meta = std::fs::metadata(p).map_err(|e| e.to_string())?;
    let img = crate::imageio::open(p).ok_or("could not open image")?;
    let buf = match mode {
        Mode::Lossless => {
            let img = img.to_rgba8();
            let mut buf = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut buf)
                .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgba8)
                .map_err(|e| e.to_string())?;
            // make sure it's pixel-identical before touching the original
            let back = image::load_from_memory_with_format(&buf, image::ImageFormat::WebP).map_err(|e| e.to_string())?.to_rgba8();
            if back.as_raw() != img.as_raw() {
                return Ok(None);
            }
            buf
        }
        Mode::Avif => crate::imageio::encode_avif(&img.to_rgb8(), 80.0)?,
    };
    if buf.len() as u64 >= meta.len() {
        return Ok(None);
    }
    std::fs::write(&dest, &buf).map_err(|e| e.to_string())?;
    if mode == Mode::Avif {
        // make sure Windows can open the new file at the right size before moving the original away
        let ok = crate::imageio::open(&dest).map(|b| (b.width(), b.height()) == (img.width(), img.height())).unwrap_or(false);
        if !ok {
            let _ = std::fs::remove_file(&dest);
            return Err("the converted file didn't open correctly - kept the original".into());
        }
    }
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    let _ = filetime::set_file_mtime(&dest, mtime); // keep the original date
    Ok(Some((dest, meta.len(), buf.len() as u64)))
}

/// Shrink every matching screenshot under folder. -> (converted, bytes_before, bytes_after)
pub fn compress(folder: &Path, mode: Mode, progress: Progress, log: Log) -> (usize, u64, u64) {
    let files = candidates(folder, mode);
    let done = AtomicUsize::new(0);
    let total = files.len();
    let results: Vec<(PathBuf, String, Option<(PathBuf, u64, u64)>)> = files
        .par_iter()
        .map(|p| {
            let key = cache_key(p);
            let r = convert_one(p, mode).unwrap_or_else(|e| {
                log(&format!("Skipped {}: {e}", p.display()));
                None
            });
            progress(done.fetch_add(1, Ordering::Relaxed) + 1, total);
            (p.clone(), key, r)
        })
        .collect();
    let mut cache = ocr::load_cache();
    let mut moved_keys = Vec::new();
    let (mut n, mut before, mut after) = (0, 0, 0);
    let mut originals = Vec::new();
    for (p, key, r) in results {
        if let Some((dest, b, a)) = r {
            if let Some(t) = cache.get(&key).cloned() {
                cache.insert(cache_key(&dest), t);
            }
            moved_keys.push((key, cache_key(&dest)));
            originals.push(p);
            n += 1;
            before += b;
            after += a;
        }
    }
    ocr::save_cache(&cache);
    crate::vision::carry_keys(&moved_keys);
    match mode {
        Mode::Lossless => {
            for chunk in originals.chunks(200) {
                recycle(chunk);
            }
        }
        Mode::Avif => {
            // keep the originals, same folder layout, in the backup folder next to the app
            let root = originals_dir().join(folder.file_name().unwrap_or_default());
            for p in &originals {
                let rel = p.strip_prefix(folder).unwrap_or(p);
                let dest = crate::sorter::unique_dest(&root.join(rel));
                if let Err(e) = crate::sorter::move_file(p, &dest) {
                    log(&format!("Couldn't back up {}: {e}", p.display()));
                }
            }
        }
    }
    (n, before, after)
}

// ---------- thumbnails ----------

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// JPEG thumbnail, made once and kept in thumbs/ next to the app.
pub fn thumb_bytes(p: &Path, size: u32) -> Option<Vec<u8>> {
    let key = format!("{:016x}", fnv(&format!("{}|{}|{}", p.display(), cache_key(p), size)));
    let f = settings::app_file("thumbs").join(&key[..2]).join(format!("{key}.jpg"));
    if let Ok(b) = std::fs::read(&f) {
        return Some(b);
    }
    let img = crate::imageio::open(p)?.thumbnail(size, size).to_rgb8();
    let mut buf = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, 82)
        .write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)
        .ok()?;
    if let Some(d) = f.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let _ = std::fs::write(&f, &buf);
    Some(buf)
}

/// Make missing grid thumbnails in the background (newest first).
pub fn prewarm_thumbs(mut paths: Vec<PathBuf>, size: u32) {
    paths.reverse();
    std::thread::spawn(move || {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        pool.install(|| paths.par_iter().for_each(|p| {
            let _ = thumb_bytes(p, size);
        }));
    });
}
