//! Opening any screenshot format, and AVIF encoding.
//! PNG/JPEG/WebP/GIF/BMP/TIFF are decoded in Rust; AVIF, JPEG XL and HEIF are decoded by
//! Windows itself (AV1 / JPEG XL / HEIF extensions), the same decoders Photos uses.
use image::{DynamicImage, RgbImage, RgbaImage};
use std::path::Path;
use windows::core::HSTRING;
use windows::Graphics::Imaging::{
    BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat, BitmapTransform, ColorManagementMode, ExifOrientationMode,
};
use windows::Storage::{FileAccessMode, StorageFile};

/// Formats Windows decodes for us.
pub const WINDOWS_DECODED: &[&str] = &["avif", "jxl", "heic", "heif"];

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn windows_decode(p: &Path) -> windows::core::Result<DynamicImage> {
    crate::ocr::init_thread();
    let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(crate::ocr::win_path(p)))?.get()?;
    let stream = file.OpenAsync(FileAccessMode::Read)?.get()?;
    let decoder = BitmapDecoder::CreateAsync(&stream)?.get()?;
    let (w, h) = (decoder.OrientedPixelWidth()?, decoder.OrientedPixelHeight()?);
    let provider = decoder
        .GetPixelDataTransformedAsync(
            BitmapPixelFormat::Rgba8,
            BitmapAlphaMode::Straight,
            &BitmapTransform::new()?,
            ExifOrientationMode::RespectExifOrientation,
            ColorManagementMode::ColorManageToSRgb,
        )?
        .get()?;
    let data = provider.DetachPixelData()?.to_vec();
    let _ = stream.Close();
    RgbaImage::from_raw(w, h, data)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| windows::core::Error::new(windows::core::HRESULT(-1), "bad pixel data"))
}

/// Open any supported image (falls back to Windows' decoders when Rust can't).
pub fn open(p: &Path) -> Option<DynamicImage> {
    if WINDOWS_DECODED.contains(&ext(p).as_str()) {
        return windows_decode(p).ok();
    }
    image::open(p).ok().or_else(|| windows_decode(p).ok())
}

/// AVIF, full colour detail (4:4:4). quality 80 keeps screenshot text crisp at a fraction of the size.
pub fn encode_avif(img: &RgbImage, quality: f32) -> Result<Vec<u8>, String> {
    let px: Vec<ravif::RGB8> = img.pixels().map(|p| ravif::RGB8::new(p[0], p[1], p[2])).collect();
    ravif::Encoder::new()
        .with_quality(quality)
        .with_speed(6)
        .encode_rgb(ravif::Img::new(px.as_slice(), img.width() as usize, img.height() as usize))
        .map(|r| r.avif_file)
        .map_err(|e| e.to_string())
}
