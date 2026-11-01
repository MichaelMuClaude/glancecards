//! Small persistent settings file (`~/.config/glancecards/settings.txt`, `%APPDATA%\glancecards\settings.txt` on Windows).
//! Plain `key=value` lines; `dir` and `recent` may repeat.

use std::path::PathBuf;

/// Where the window sits relative to other windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Below every other window, like a desktop widget (X11 only).
    Bottom,
    /// An ordinary window.
    Normal,
    /// Above other windows.
    Top,
}

impl Level {
    fn name(self) -> &'static str {
        match self {
            Level::Bottom => "bottom",
            Level::Normal => "normal",
            Level::Top => "top",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// Folders the photos are picked from.
    pub dirs: Vec<PathBuf>,
    /// Folders photos were recently moved to, most recent first.
    pub recent: Vec<PathBuf>,
    /// Where cards you already know are moved to; `None` means a `known` folder next to the first source folder.
    pub known: Option<PathBuf>,
    pub interval: u64,
    /// A sentence card (`sentence_*`) or own-word card (`mine_*`) comes after this many other cards;
    /// 0 = shuffled in like the others.
    pub sentence_every: usize,
    pub fit: bool,
    pub level: Level,
}

impl Default for Settings {
    fn default() -> Self {
        Self { dirs: Vec::new(), recent: Vec::new(), known: None, interval: 10, sentence_every: 5, fit: true, level: Level::Bottom }
    }
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

pub fn file_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("APPDATA").map(PathBuf::from))
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("glancecards").join("settings.txt"))
}

pub fn load() -> Settings {
    let mut st = Settings::default();
    let Some(text) = file_path().and_then(|p| std::fs::read_to_string(p).ok()) else { return st };
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key.trim() {
            "dir" => st.dirs.push(PathBuf::from(value)),
            "recent" => st.recent.push(PathBuf::from(value)),
            "known" => st.known = Some(PathBuf::from(value)),
            "interval" => {
                if let Ok(n) = value.trim().parse::<u64>() {
                    st.interval = n.clamp(1, 3600);
                }
            }
            "sentence_every" => {
                if let Ok(n) = value.trim().parse::<usize>() {
                    st.sentence_every = n.min(100);
                }
            }
            "fit" => st.fit = value.trim() != "0",
            "level" => {
                st.level = match value.trim() {
                    "normal" => Level::Normal,
                    "top" => Level::Top,
                    _ => Level::Bottom,
                }
            }
            _ => {}
        }
    }
    st
}

pub fn save(st: &Settings) {
    let Some(path) = file_path() else { return };
    let mut out = String::new();
    for d in &st.dirs {
        out.push_str(&format!("dir={}\n", d.display()));
    }
    for d in &st.recent {
        out.push_str(&format!("recent={}\n", d.display()));
    }
    if let Some(k) = &st.known {
        out.push_str(&format!("known={}\n", k.display()));
    }
    out.push_str(&format!(
        "interval={}\nsentence_every={}\nfit={}\nlevel={}\n",
        st.interval,
        st.sentence_every,
        u8::from(st.fit),
        st.level.name()
    ));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write a temporary file and rename it over the old one, so a crash mid-write never leaves a half file.
    let tmp = path.with_extension("txt.tmp");
    if let Err(e) = std::fs::write(&tmp, out).and_then(|()| std::fs::rename(&tmp, &path)) {
        eprintln!("could not save settings to {}: {e}", path.display());
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Holds a lock on a file in the settings folder for as long as the app runs, so a second copy (from the
/// app menu while the autostart copy is open) can tell and quit. The lock goes away when the process
/// ends, even after a crash. Returns None when another copy holds it.
pub fn single_instance() -> Option<Option<std::fs::File>> {
    let Some(dir) = file_path().and_then(|p| p.parent().map(|d| d.to_path_buf())) else { return Some(None) };
    let _ = std::fs::create_dir_all(&dir);
    let Ok(file) = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("lock")) else {
        return Some(None); // cannot tell, so do not block starting
    };
    match file.try_lock() {
        Ok(()) => Some(Some(file)),
        Err(std::fs::TryLockError::WouldBlock) => None,
        Err(std::fs::TryLockError::Error(_)) => Some(None),
    }
}
