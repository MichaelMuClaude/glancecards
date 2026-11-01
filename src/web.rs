//! `glancecards --serve`: the same cards as a web page for a phone, served from this computer.
//!
//! The page asks for one card at a time (`/api/next`), shows it, and moves on by itself. Got it moves the
//! card to the known folder exactly like the window does, so the phone and the desktop share one set of
//! cards. Listen on 127.0.0.1 and put an HTTPS front in front of it (for example `tailscale serve`); there
//! is no sign-in, so anyone who can reach the port can move cards.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Request, Response, Server};

use crate::cards::{arrange, cambridge_url, card_word, is_featured, is_sentence, known_dir, move_file, unique_path};
use crate::loader;
use crate::settings::Settings;

const PAGE: &str = include_str!("../web/index.html");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const ICON: &[u8] = include_bytes!("../web/icon.png");
/// Cards are sent as JPEGs this wide: plenty for a phone, and about a tenth of the PNG.
const IMG_WIDTH: u32 = 720;
/// How often to look for cards added or removed behind our back (new sentence cards, Got it on the desktop).
const RESCAN: Duration = Duration::from_secs(300);
const THREADS: usize = 4;

struct State {
    dirs: Vec<PathBuf>,
    known: PathBuf,
    every: usize,
    interval: u64,
    queue: Vec<PathBuf>,
    pos: usize,
    files: HashSet<PathBuf>,
    scanned: Instant,
    /// The last card moved to the known folder: where it was and where it went.
    undo: Option<(PathBuf, PathBuf)>,
}

impl State {
    fn rescan(&mut self) {
        let found = loader::scan_dirs(&self.dirs);
        self.scanned = Instant::now();
        let files: HashSet<PathBuf> = found.iter().cloned().collect();
        if files != self.files || self.queue.is_empty() {
            self.files = files;
            self.queue = arrange(found, self.every);
            self.pos = 0;
        }
    }

    fn remove(&mut self, path: &Path) {
        self.files.remove(path);
        while let Some(i) = self.queue.iter().position(|p| p == path) {
            self.queue.remove(i);
            if i < self.pos {
                self.pos -= 1;
            }
        }
    }

    fn next(&mut self) -> Option<PathBuf> {
        if self.scanned.elapsed() >= RESCAN {
            self.rescan();
        }
        for _ in 0..2 {
            while self.pos < self.queue.len() {
                let p = self.queue[self.pos].clone();
                self.pos += 1;
                if p.exists() {
                    return Some(p);
                }
                self.remove(&p);
            }
            // End of a round: pick up new cards and shuffle again.
            self.files.clear();
            self.rescan();
        }
        None
    }

    fn card_json(&self, p: &Path) -> String {
        let word = card_word(p).unwrap_or_default();
        let secs = if is_sentence(p) { self.interval * 2 } else { self.interval };
        format!(
            r#"{{"id":{},"word":{},"lookup":{},"featured":{},"seconds":{}}}"#,
            json(&p.to_string_lossy()),
            json(&word),
            json(&cambridge_url(&word)),
            is_featured(p),
            secs
        )
    }
}

pub fn serve(addr: &str, dirs: Vec<PathBuf>, st: &Settings) {
    let server = match Server::http(addr) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("cannot listen on {addr}: {e}");
            std::process::exit(1);
        }
    };
    let mut state = State {
        known: known_dir(st.known.as_deref(), &dirs),
        dirs,
        every: st.sentence_every,
        interval: st.interval,
        queue: Vec::new(),
        pos: 0,
        files: HashSet::new(),
        scanned: Instant::now(),
        undo: None,
    };
    state.rescan();
    println!("GlanceCards (词卡) web page on http://{addr} with {} cards", state.files.len());
    let state = Arc::new(Mutex::new(state));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (server, state) = (Arc::clone(&server), Arc::clone(&state));
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    handle(req, &state);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

fn handle(req: Request, state: &Mutex<State>) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let file = param(query, "f").map(PathBuf::from);
    let res = match (req.method(), path) {
        (Method::Get, "/") => text(PAGE, "text/html; charset=utf-8"),
        (Method::Get, "/manifest.webmanifest") => text(MANIFEST, "application/manifest+json"),
        (Method::Get, "/icon.png") => bytes(ICON.to_vec(), "image/png"),
        (Method::Get, "/api/next") => {
            let mut s = lock(state);
            match s.next() {
                Some(p) => text(&s.card_json(&p), "application/json"),
                None => text(r#"{"error":"没有卡片"}"#, "application/json").with_status_code(404),
            }
        }
        (Method::Get, "/img") => {
            // Only pictures that are cards right now; never an arbitrary path.
            let ok = file.as_ref().is_some_and(|f| lock(state).files.contains(f));
            match file.filter(|_| ok).map(|f| jpeg(&f)) {
                Some(Ok(data)) => bytes(data, "image/jpeg")
                    .with_header(header("Cache-Control", "private, max-age=86400")),
                Some(Err(e)) => text(&e, "text/plain; charset=utf-8").with_status_code(500),
                None => text("not a card", "text/plain").with_status_code(404),
            }
        }
        (Method::Post, "/api/known") => {
            let mut s = lock(state);
            match file.filter(|f| s.files.contains(f)) {
                Some(f) => match mark_known(&mut s, &f) {
                    Ok(()) => text(r#"{"ok":true}"#, "application/json"),
                    Err(e) => text(&format!(r#"{{"error":{}}}"#, json(&e)), "application/json").with_status_code(500),
                },
                None => text(r#"{"error":"not a card"}"#, "application/json").with_status_code(404),
            }
        }
        (Method::Post, "/api/undo") => {
            let mut s = lock(state);
            match undo(&mut s) {
                Ok(p) => text(&s.card_json(&p), "application/json"),
                Err(e) => text(&format!(r#"{{"error":{}}}"#, json(&e)), "application/json").with_status_code(409),
            }
        }
        _ => text("not found", "text/plain").with_status_code(404),
    };
    let _ = req.respond(res.with_header(header("X-Content-Type-Options", "nosniff")));
}

fn mark_known(s: &mut State, from: &Path) -> Result<(), String> {
    std::fs::create_dir_all(&s.known).map_err(|e| e.to_string())?;
    let name = from.file_name().ok_or("no file name")?;
    let mut to = s.known.join(name);
    if to.exists() {
        to = unique_path(&to);
    }
    move_file(from, &to).map_err(|e| e.to_string())?;
    s.remove(from);
    s.undo = Some((from.to_path_buf(), to));
    Ok(())
}

/// Moves the last known card back and makes it the next card.
fn undo(s: &mut State) -> Result<PathBuf, String> {
    let (back, from) = s.undo.take().ok_or("nothing to undo")?;
    let back = if back.exists() { unique_path(&back) } else { back };
    move_file(&from, &back).map_err(|e| e.to_string())?;
    s.files.insert(back.clone());
    let at = s.pos.min(s.queue.len());
    s.queue.insert(at, back.clone());
    s.pos = at + 1;
    Ok(back)
}

fn jpeg(path: &Path) -> Result<Vec<u8>, String> {
    let img = loader::open_image(path)?;
    let img = if img.width() > IMG_WIDTH { img.resize(IMG_WIDTH, u32::MAX, image::imageops::FilterType::Triangle) } else { img };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82)
        .encode_image(&img.to_rgb8())
        .map_err(|e| e.to_string())?;
    Ok(out)
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn text(body: &str, kind: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    bytes(body.as_bytes().to_vec(), kind)
}

fn bytes(body: Vec<u8>, kind: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(body).with_header(header("Content-Type", kind))
}

/// The value of `key` in a query string, percent-decoded.
fn param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    (c as char).to_digit(16).map(|d| d as u8)
}

/// A JSON string literal.
fn json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(param("f=%2Fx%2Fapple%20(1).png&y=2", "f").as_deref(), Some("/x/apple (1).png"));
        assert_eq!(param("f=a+b", "f").as_deref(), Some("a b"));
        assert_eq!(param("f=%E8%AF%8D", "f").as_deref(), Some("词"));
        assert_eq!(param("f=50%", "f").as_deref(), Some("50%"));
        assert_eq!(param("g=1", "f"), None);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json("x\ny"), r#""x\u000ay""#);
    }
}
