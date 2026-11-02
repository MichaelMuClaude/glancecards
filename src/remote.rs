//! The window as a client of a `glancecards --serve` server (the `server` setting or `--server URL`).
//!
//! The cards then live on the server only: the window asks it for the next cards (`/api/next`), downloads
//! each picture (`/img`), and sends Got it and Undo there (`/api/known`, `/api/undo`), exactly like the
//! phone page. A card's id is its path on the server, so the usual file-name rules (sentence cards, own
//! words, the word to look up) still work on it.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many cards one request asks for.
pub const BATCH: usize = 30;
const TIMEOUT: Duration = Duration::from_secs(15);

pub struct Remote {
    base: String,
    agent: ureq::Agent,
}

impl Remote {
    pub fn new(base: &str) -> Self {
        let agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
        Self { base: base.trim_end_matches('/').to_string(), agent }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// The next `n` cards in the server's order.
    pub fn next(&self, n: usize) -> Result<Vec<PathBuf>, String> {
        let body = self.get(&format!("/api/next?n={n}"))?;
        let list: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        let ids = list.as_array().ok_or("unexpected reply from the server")?;
        Ok(ids.iter().filter_map(card_id).collect())
    }

    /// The picture of a card (a JPEG).
    pub fn image(&self, id: &Path) -> Result<Vec<u8>, String> {
        self.get(&format!("/img?f={}", encode(&id.to_string_lossy())))
    }

    /// Got it: the server moves the card to its known folder.
    pub fn known(&self, id: &Path) -> Result<(), String> {
        self.post(&format!("/api/known?f={}", encode(&id.to_string_lossy()))).map(|_| ())
    }

    /// Moves the last known card back; returns its id.
    pub fn undo(&self) -> Result<PathBuf, String> {
        let body = self.post("/api/undo")?;
        let card: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
        card_id(&card).ok_or_else(|| "unexpected reply from the server".to_string())
    }

    fn get(&self, path: &str) -> Result<Vec<u8>, String> {
        let mut res = self.agent.get(format!("{}{path}", self.base)).call().map_err(|e| e.to_string())?;
        res.body_mut().read_to_vec().map_err(|e| e.to_string())
    }

    fn post(&self, path: &str) -> Result<Vec<u8>, String> {
        let mut res = self.agent.post(format!("{}{path}", self.base)).send_empty().map_err(|e| e.to_string())?;
        res.body_mut().read_to_vec().map_err(|e| e.to_string())
    }
}

fn card_id(card: &serde_json::Value) -> Option<PathBuf> {
    card.get("id")?.as_str().map(PathBuf::from)
}

/// Percent-encodes a query value.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_encoded() {
        assert_eq!(encode("/x/apple (1).png"), "/x/apple%20%281%29.png");
        assert_eq!(encode("/x/词.png"), "/x/%E8%AF%8D.png");
        assert_eq!(encode("a&b=c+d"), "a%26b%3Dc%2Bd");
    }

    /// The client against a real `--serve` server on a scratch folder: cards, a picture, Got it, Undo.
    #[test]
    fn talks_to_a_real_server() {
        let root = std::env::temp_dir().join(format!("glancecards-remote-{}", std::process::id()));
        let learning = root.join("learning");
        std::fs::create_dir_all(&learning).unwrap();
        for name in ["apple.png", "pear (1).png", "sentence_N01_bug.png"] {
            image::RgbImage::from_pixel(40, 30, image::Rgb([200, 100, 50])).save(learning.join(name)).unwrap();
        }
        let addr = format!("127.0.0.1:{}", 20000 + std::process::id() % 20000);
        let (dirs, st) = (vec![learning.clone()], crate::settings::Settings::default());
        let serve_addr = addr.clone();
        std::thread::spawn(move || crate::web::serve(&serve_addr, dirs, &st));
        let r = Remote::new(&format!("http://{addr}/"));
        let mut cards = Err(String::new());
        for _ in 0..50 {
            cards = r.next(5);
            if cards.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let cards = cards.expect("server answers");
        assert!(cards.len() >= 3, "{cards:?}");

        let pear = learning.join("pear (1).png");
        let jpeg = r.image(&pear).unwrap();
        assert_eq!(image::guess_format(&jpeg).unwrap(), image::ImageFormat::Jpeg);
        assert!(r.image(Path::new("/etc/passwd")).is_err(), "only cards are served");

        r.known(&pear).unwrap();
        assert!(!pear.exists() && root.join("known").join("pear (1).png").exists());
        assert_eq!(r.undo().unwrap(), pear);
        assert!(pear.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn card_ids_come_from_the_json() {
        let v: serde_json::Value = serde_json::from_str(r#"{"id":"/x/apple.png","word":"apple"}"#).unwrap();
        assert_eq!(card_id(&v), Some(PathBuf::from("/x/apple.png")));
    }
}
