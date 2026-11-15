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
    compress_list(folder, candidates(folder, mode), mode, progress, log)
}

/// Auto-shrink: AVIF candidates in `folder` last changed more than `days` days ago
/// (online-only OneDrive files are left alone), at most `limit` per run.
pub fn old_enough(folder: &Path, days: u32, limit: usize) -> Vec<PathBuf> {
    let age = std::time::Duration::from_secs(days as u64 * 86400);
    candidates(folder, Mode::Avif)
        .into_iter()
        .filter(|p| !crate::ocr::online_only(p))
        .filter(|p| std::fs::metadata(p).and_then(|m| m.modified()).map(|t| t.elapsed().unwrap_or_default() >= age).unwrap_or(false))
        .take(limit)
        .collect()
}

/// Shrink these files (all under `folder`). -> (converted, bytes_before, bytes_after)
pub fn compress_list(folder: &Path, files: Vec<PathBuf>, mode: Mode, progress: Progress, log: Log) -> (usize, u64, u64) {
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
    let key = format!("{:016x}", fnv(&format!("v2|{}|{}|{}", p.display(), cache_key(p), size)));
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

/// Thumbnail for an online-only OneDrive file from Windows' thumbnail cache / OneDrive,
/// the way File Explorer shows them, without downloading the file. Cached like other thumbs.
/// Needs COM initialised on the calling thread.
pub fn cloud_thumb_bytes(p: &Path, size: u32) -> Option<Vec<u8>> {
    let key = format!("{:016x}", fnv(&format!("v2|{}|{}|{}", p.display(), cache_key(p), size)));
    let f = settings::app_file("thumbs").join(&key[..2]).join(format!("{key}.jpg"));
    if let Ok(b) = std::fs::read(&f) {
        return Some(b);
    }
    let img = shell_thumbnail(p, size)?;
    let img = image::DynamicImage::ImageRgb8(img).thumbnail(size, size).to_rgb8();
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

fn shell_thumbnail(p: &Path, size: u32) -> Option<image::RgbImage> {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_THUMBNAILONLY};
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(crate::ocr::win_path(p)), None).ok()?;
        let hbm = factory.GetImage(SIZE { cx: size as i32, cy: size as i32 }, SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK).ok()?;
        let obj = HGDIOBJ(hbm.0);
        let mut bm = BITMAP::default();
        let ok = GetObjectW(obj, std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut _)) != 0;
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        let mut out = None;
        if ok && w > 0 && h > 0 {
            let mut bi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut px = vec![0u8; (w * h * 4) as usize];
            let dc = GetDC(None);
            let lines = GetDIBits(dc, hbm, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bi, DIB_RGB_COLORS);
            ReleaseDC(None, dc);
            if lines > 0 {
                let rgb: Vec<u8> = px.chunks_exact(4).flat_map(|c| [c[2], c[1], c[0]]).collect();
                out = image::RgbImage::from_raw(w as u32, h as u32, rgb);
            }
        }
        let _ = DeleteObject(obj);
        out
    }
}

/// Make missing grid thumbnails in the background (newest first).
pub fn prewarm_thumbs(mut paths: Vec<PathBuf>, size: u32) {
    paths.retain(|p| !crate::ocr::online_only(p));
    paths.reverse();
    std::thread::spawn(move || {
        let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap();
        pool.install(|| paths.par_iter().for_each(|p| {
            let _ = thumb_bytes(p, size);
        }));
    });
}
