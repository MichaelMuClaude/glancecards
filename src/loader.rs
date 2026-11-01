//! Background folder scanning and image decoding (including HEIC/HEIF).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui::{self, ColorImage};
use image::ImageDecoder;

/// Longest side photos are shrunk to before they become textures.
const MAX_SIDE: u32 = 2048;
const WORKERS: usize = 2;
#[cfg(feature = "heif")]
const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff", "heic", "heif"];
#[cfg(not(feature = "heif"))]
const EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];

pub enum Loaded {
    Ok(PathBuf, ColorImage),
    Err(PathBuf, String),
}

/// Scans the folders on a background thread and sends back every photo found.
pub fn spawn_scan(ctx: egui::Context, dirs: Vec<PathBuf>) -> Receiver<Vec<PathBuf>> {
    let (scan_tx, scan_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut found = Vec::new();
        for dir in dirs {
            scan(&dir, &mut found);
        }
        let _ = scan_tx.send(found);
        ctx.request_repaint();
    });
    scan_rx
}

/// Starts the decoder threads. Returns (job sender, decoded images).
pub fn spawn_workers(ctx: egui::Context) -> (Sender<PathBuf>, Receiver<Loaded>) {
    let (job_tx, job_rx) = mpsc::channel::<PathBuf>();
    let (res_tx, res_rx) = mpsc::channel();
    let job_rx = Arc::new(Mutex::new(job_rx));
    for _ in 0..WORKERS {
        let job_rx = Arc::clone(&job_rx);
        let res_tx = res_tx.clone();
        let ctx = ctx.clone();
        thread::spawn(move || {
            loop {
                let job = job_rx.lock().ok().and_then(|rx| rx.recv().ok());
                let Some(path) = job else { break };
                // A decoder that panics on a broken file must not take the worker down with it: the card
                // would stay "loading" forever and the slideshow would stop on it.
                let result = std::panic::catch_unwind(|| decode(&path))
                    .unwrap_or_else(|_| Err("the decoder crashed on this file".to_string()));
                let msg = match result {
                    Ok(img) => Loaded::Ok(path, img),
                    Err(e) => Loaded::Err(path, e),
                };
                if res_tx.send(msg).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
    }
    (job_tx, res_rx)
}

fn is_hidden(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn scan(dir: &Path, out: &mut Vec<PathBuf>) {
    let walker = walkdir::WalkDir::new(dir)
        .into_iter()
        // Skip hidden folders such as .dropbox.cache and .thumbnails (but not the root itself).
        .filter_entry(|e| e.depth() == 0 || !is_hidden(e.file_name()));
    for entry in walker.flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let ext = entry.path().extension().map(|e| e.to_string_lossy().to_lowercase());
        if ext.is_some_and(|e| EXTS.contains(&e.as_str())) {
            out.push(entry.into_path());
        }
    }
}

#[cfg(feature = "heif")]
fn is_heif(path: &Path) -> bool {
    path.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| e == "heic" || e == "heif")
}

fn decode(path: &Path) -> Result<ColorImage, String> {
    #[cfg(feature = "heif")]
    let img = if is_heif(path) { decode_heif(path)? } else { decode_std(path)? };
    #[cfg(not(feature = "heif"))]
    let img = decode_std(path)?;
    let img = if img.width().max(img.height()) > MAX_SIDE {
        img.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()))
}

fn decode_std(path: &Path) -> Result<image::DynamicImage, String> {
    let reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    Ok(img)
}

#[cfg(feature = "heif")]
fn decode_heif(path: &Path) -> Result<image::DynamicImage, String> {
    use libheif_rs::{ColorSpace, HeifContext, LibHeif, RgbChroma};

    let lib = LibHeif::new();
    let ctx = HeifContext::read_from_file(path.to_str().ok_or("non-UTF-8 path")?).map_err(|e| e.to_string())?;
    let handle = ctx.primary_image_handle().map_err(|e| e.to_string())?;
    // libheif applies the rotation/mirroring stored in the file by default.
    let decoded = lib
        .decode(&handle, ColorSpace::Rgb(RgbChroma::Rgba), None)
        .map_err(|e| e.to_string())?;
    let planes = decoded.planes();
    let plane = planes.interleaved.ok_or("no interleaved plane")?;
    let (w, h) = (plane.width as usize, plane.height as usize);
    let mut buf = Vec::with_capacity(w * h * 4);
    for row in 0..h {
        let start = row * plane.stride;
        buf.extend_from_slice(&plane.data[start..start + w * 4]);
    }
    let rgba = image::RgbaImage::from_raw(w as u32, h as u32, buf).ok_or("bad buffer size")?;
    Ok(image::DynamicImage::ImageRgba8(rgba))
}
