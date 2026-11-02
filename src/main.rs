//! A small, resizable desktop card window for learning words: shows random word cards (pictures)
//! from a folder and lets you move the ones you already know to another folder with one click.

// A release build on Windows is a window app: no console window next to it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cards;
mod loader;
mod remote;
mod settings;
mod web;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Id, Pos2, Rect, ResizeDirection, Sense, TextureHandle,
    TextureOptions, ViewportCommand, vec2,
};

use cards::{arrange, cambridge_url, card_word, is_sentence, move_file, unique_path};
use loader::Loaded;
use remote::Remote;
use settings::{Level, Settings, home_dir};

/// Background behind the card: the same cream as the strip under each picture, so a card that does not
/// fill the window (no "fit to card") blends in instead of showing bars.
const CARD_BG: Color32 = Color32::from_rgb(250, 247, 240);
const EDGE: f32 = 7.0;
const TOAST_SECS: u64 = 8;
/// The longest a card waits after Look up for you to come back from the dictionary.
const LOOKUP_HOLD: Duration = Duration::from_secs(60);
/// With a server: ask for more cards once fewer than this many are left ahead.
const AHEAD: usize = 10;
/// With a server: how long to wait before asking again after it could not be reached.
const RETRY: Duration = Duration::from_secs(30);
/// With a server: cards kept behind the current one (for Back) before the oldest are dropped.
const BEHIND: usize = 200;

/// Whether the interface is in Chinese. It is, unless no Chinese font can be found on the system.
static ZH: AtomicBool = AtomicBool::new(false);

/// Picks the Chinese or the English text for the interface.
fn tr(zh: &'static str, en: &'static str) -> &'static str {
    if ZH.load(Ordering::Relaxed) { zh } else { en }
}

/// Loads a system font with Chinese characters as a fallback for the built-in fonts. Returns false when none is
/// found (the interface then falls back to English instead of showing empty boxes).
fn install_cjk_font(ctx: &egui::Context) -> bool {
    // (path, face index inside a .ttc collection; for Noto Sans CJK 2 is Simplified Chinese)
    const CANDIDATES: &[(&str, u32)] = &[
        ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc", 2),
        ("/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc", 2),
        ("/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc", 2),
        ("/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc", 2),
        ("/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf", 0),
        ("/usr/share/fonts/truetype/wqy/wqy-microhei.ttc", 0),
        ("/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc", 0),
        ("/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc", 0),
        ("C:\\Windows\\Fonts\\msyh.ttc", 0),
        ("C:\\Windows\\Fonts\\simhei.ttf", 0),
        ("/System/Library/Fonts/PingFang.ttc", 0),
        ("/System/Library/Fonts/STHeiti Medium.ttc", 0),
        ("/Library/Fonts/Arial Unicode.ttf", 0),
    ];
    for (path, index) in CANDIDATES {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let mut data = egui::FontData::from_owned(bytes);
        data.index = *index;
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert("cjk".to_owned(), Arc::new(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.push("cjk".to_owned());
            }
        }
        ctx.set_fonts(fonts);
        return true;
    }
    false
}

fn window_level(level: Level) -> egui::WindowLevel {
    match level {
        Level::Bottom => egui::WindowLevel::AlwaysOnBottom,
        Level::Normal => egui::WindowLevel::Normal,
        Level::Top => egui::WindowLevel::AlwaysOnTop,
    }
}

/// What the Undo button in the toast can reverse.
enum Undo {
    Moved { from: PathBuf, to: PathBuf },
    /// Got it on the server: the server remembers what to move back.
    Remote,
}

/// Answers from the server, sent back by the threads that talk to it.
enum Net {
    Batch(Result<Vec<PathBuf>, String>),
    Known(PathBuf, Result<(), String>),
    Undone(Result<PathBuf, String>),
}

/// What a folder-picker dialog was opened for.
enum Pick {
    AddSource,
}

struct Config {
    dirs: Vec<PathBuf>,
    interval: Duration,
    /// Where the window sits relative to other windows (the command line overrides the saved setting).
    level: Level,
    settings: Settings,
    /// `--serve [ADDR]`: run the web page for phones instead of the window.
    serve: Option<String>,
    /// `--server URL` or the `server` setting: show that server's cards instead of the folders.
    server: Option<String>,
}

struct App {
    queue: Vec<PathBuf>,
    pos: usize,
    scan_rx: Receiver<Vec<PathBuf>>,
    scanned: bool,
    job_tx: Sender<PathBuf>,
    res_rx: Receiver<Loaded>,
    cache: HashMap<PathBuf, TextureHandle>,
    pending: HashSet<PathBuf>,
    shown: Option<(PathBuf, TextureHandle)>,
    shown_at: Instant,
    /// When the card on screen first appeared. Unlike `shown_at`, hovering does not reset it.
    shown_since: Instant,
    /// Set by "Look up" (when it was clicked): the card stays until the pointer comes back over it, Next is
    /// used, or `LOOKUP_HOLD` has passed.
    hold: Option<Instant>,
    interval: Duration,
    paused: bool,
    toast: Option<(String, Instant, bool)>,
    last_action: Option<Undo>,
    dirs: Vec<PathBuf>,
    settings: Settings,
    settings_open: bool,
    /// Whether the pointer was over the photo last frame; the slideshow waits while it is.
    hover_photo: bool,
    pick: Option<(Pick, Receiver<Option<PathBuf>>)>,
    frames: u32,
    level: Level,
    /// Resize the frame to match each photo's orientation.
    fit: bool,
    /// Aspect ratio of a newly shown photo that the window has not been fitted to yet.
    fit_aspect: Option<f32>,
    /// Photos shown before the current one, most recent last.
    history: Vec<PathBuf>,
    /// The server the cards come from, if any (then `dirs` is not read).
    remote: Option<Arc<Remote>>,
    net_tx: Sender<Net>,
    net_rx: Receiver<Net>,
    /// A request for more cards is on its way.
    fetching: bool,
    /// The last time the server could not be reached, and why.
    net_error: Option<(String, Instant)>,
}

impl App {
    fn new(ctx: &egui::Context, cfg: Config) -> Self {
        ZH.store(install_cjk_font(ctx), Ordering::Relaxed);
        let remote = cfg.server.as_deref().map(|s| Arc::new(Remote::new(s)));
        let scan_rx = if remote.is_some() { mpsc::channel().1 } else { loader::spawn_scan(ctx.clone(), cfg.dirs.clone()) };
        let (job_tx, res_rx) = loader::spawn_workers(ctx.clone(), remote.clone());
        let (net_tx, net_rx) = mpsc::channel();
        Self {
            remote,
            net_tx,
            net_rx,
            fetching: false,
            net_error: None,
            queue: Vec::new(),
            pos: 0,
            scan_rx,
            scanned: false,
            job_tx,
            res_rx,
            cache: HashMap::new(),
            pending: HashSet::new(),
            shown: None,
            shown_at: Instant::now(),
            shown_since: Instant::now(),
            hold: None,
            interval: cfg.interval,
            paused: false,
            toast: None,
            last_action: None,
            dirs: cfg.dirs,
            hover_photo: false,
            settings_open: false,
            pick: None,
            frames: 0,
            level: cfg.level,
            fit: cfg.settings.fit,
            fit_aspect: None,
            history: Vec::new(),
            settings: cfg.settings,
        }
    }

    /// Reshapes the window to the card's aspect ratio, keeping its centre and card area.
    fn fit_window(&self, ctx: &egui::Context, aspect: f32) {
        let Some(inner) = ctx.input(|i| i.viewport().inner_rect) else { return };
        let cur = inner.size();
        let pad = note_pad(cur);
        let area = (cur.x - pad).max(60.0) * (cur.y - pad).max(60.0);
        let pw = (area * aspect).sqrt();
        let ph = pw / aspect;
        let mut size = vec2(pw + pad, ph + pad);
        for _ in 0..3 {
            let q = note_pad(size);
            size = vec2(pw + q, ph + q);
        }
        let limit = ctx
            .input(|i| i.viewport().monitor_size)
            .map_or(vec2(2400.0, 1600.0), |m| m * 0.9);
        size = size.max(vec2(200.0, 160.0)).min(limit);
        if (size - cur).abs().max_elem() < 2.0 {
            return;
        }
        ctx.send_viewport_cmd(ViewportCommand::InnerSize(size));
        if let Some(outer) = ctx.input(|i| i.viewport().outer_rect) {
            ctx.send_viewport_cmd(ViewportCommand::OuterPosition(outer.center() - size / 2.0));
        }
    }

    fn current_path(&self) -> Option<&PathBuf> {
        self.queue.get(self.pos)
    }

    fn request(&mut self, path: PathBuf) {
        if !self.cache.contains_key(&path) && self.pending.insert(path.clone()) {
            let _ = self.job_tx.send(path);
        }
    }

    fn prefetch(&mut self) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        for off in 0..=1 {
            let p = self.queue[(self.pos + off) % n].clone();
            self.request(p);
        }
        // Keep only the photos near the cursor in memory.
        let keep: HashSet<&PathBuf> = [n - 1, 0, 1, 2]
            .iter()
            .map(|o| &self.queue[(self.pos + o) % n])
            .collect();
        self.cache.retain(|k, _| keep.contains(k));
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let picked = self.pick.as_ref().and_then(|(_, rx)| match rx.try_recv() {
            Ok(res) => Some(res),
            Err(mpsc::TryRecvError::Disconnected) => Some(None),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(res) = picked {
            if let Some((kind, _)) = self.pick.take() {
                if let Some(dir) = res {
                    match kind {
                        Pick::AddSource => {
                            if !self.dirs.contains(&dir) {
                                self.dirs.push(dir);
                                self.save_dirs(ctx);
                            }
                        }
                    }
                }
            }
        }
        while let Ok(batch) = self.scan_rx.try_recv() {
            self.queue = arrange(batch, self.settings.sentence_every);
            self.pos = 0;
            self.scanned = true;
        }
        while let Ok(msg) = self.net_rx.try_recv() {
            self.on_net(msg);
        }
        self.fetch_more(ctx);
        while let Ok(res) = self.res_rx.try_recv() {
            match res {
                Loaded::Ok(path, img) => {
                    self.pending.remove(&path);
                    let tex = ctx.load_texture(path.to_string_lossy(), img, TextureOptions::LINEAR);
                    self.cache.insert(path, tex);
                }
                Loaded::Err(path, err) => {
                    eprintln!("skip {}: {err}", path.display());
                    self.pending.remove(&path);
                    self.remove_from_queue(&path);
                }
            }
        }
        self.prefetch();
        if let Some(p) = self.current_path().cloned() {
            let already = self.shown.as_ref().is_some_and(|(sp, _)| *sp == p);
            if !already {
                if let Some(tex) = self.cache.get(&p) {
                    self.fit_aspect = Some(tex.aspect_ratio());
                    self.shown = Some((p, tex.clone()));
                    self.shown_at = Instant::now();
                    self.shown_since = Instant::now();
                }
            }
        }
    }

    /// With a server: asks for more cards when few are left ahead, unless it was just found unreachable.
    fn fetch_more(&mut self, ctx: &egui::Context) {
        let Some(remote) = self.remote.clone() else { return };
        let resting = self.net_error.as_ref().is_some_and(|(_, at)| at.elapsed() < RETRY);
        if self.fetching || resting || self.queue.len().saturating_sub(self.pos) >= AHEAD {
            return;
        }
        self.fetching = true;
        let (tx, ctx) = (self.net_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Net::Batch(remote.next(remote::BATCH)));
            ctx.request_repaint();
        });
    }

    fn on_net(&mut self, msg: Net) {
        match msg {
            Net::Batch(Ok(cards)) => {
                self.fetching = false;
                self.scanned = true;
                self.net_error = None;
                self.queue.extend(cards);
                // A window left running for weeks must not keep every card it has shown.
                if self.pos > BEHIND * 2 {
                    self.queue.drain(..BEHIND);
                    self.pos -= BEHIND;
                }
            }
            Net::Batch(Err(e)) => {
                self.fetching = false;
                self.scanned = true;
                self.net_error = Some((e, Instant::now()));
            }
            Net::Known(_, Ok(())) => {
                self.last_action = Some(Undo::Remote);
                self.toast = Some((format!("{} known", tr("已移到", "Moved to")), Instant::now(), true));
            }
            Net::Known(path, Err(e)) => {
                // The card was taken off the screen right away; bring it back since the server kept it.
                self.toast = Some((format!("{}: {e}", tr("移动失败", "Move failed")), Instant::now(), false));
                self.reinsert(path);
            }
            Net::Undone(Ok(path)) => {
                self.reinsert(path);
                self.toast = Some((tr("已放回去", "Moved back").to_string(), Instant::now(), false));
            }
            Net::Undone(Err(e)) => {
                self.toast = Some((format!("{}: {e}", tr("撤销失败", "Undo failed")), Instant::now(), false));
            }
        }
    }

    /// Runs a request to the server on its own thread and hands the answer to `on_net`.
    fn ask_server(&self, ctx: &egui::Context, call: impl FnOnce(&Remote) -> Net + Send + 'static) {
        let Some(remote) = self.remote.clone() else { return };
        let (tx, ctx) = (self.net_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(call(&remote));
            ctx.request_repaint();
        });
    }

    /// Removes every copy of the card (sentence cards appear several times in a round).
    fn remove_from_queue(&mut self, path: &PathBuf) {
        while let Some(i) = self.queue.iter().position(|p| p == path) {
            self.queue.remove(i);
            if i < self.pos {
                self.pos -= 1;
            }
        }
        // With a server, the end of the queue waits for the next cards instead of starting over.
        if self.pos >= self.queue.len() && self.remote.is_none() {
            self.pos = 0;
        }
    }

    /// How long the card on screen stays: sentences take longer to read, so they get twice as long.
    fn card_interval(&self) -> Duration {
        if self.current_path().is_some_and(|p| is_sentence(p)) { self.interval * 2 } else { self.interval }
    }

    fn step(&mut self, forward: bool) {
        let n = self.queue.len();
        if n == 0 {
            return;
        }
        if forward {
            if let Some((p, _)) = &self.shown {
                if self.history.last() != Some(p) {
                    self.history.push(p.clone());
                    if self.history.len() > 100 {
                        self.history.remove(0);
                    }
                }
            }
            self.pos += 1;
            if self.pos >= n && self.remote.is_some() {
                // The server's next cards are on their way (fetch_more asks well before this).
                self.pos = n;
            } else if self.pos >= n {
                self.pos = 0;
                self.queue = arrange(std::mem::take(&mut self.queue), self.settings.sentence_every);
                // A new round must not start with the card that ended the last one.
                if n > 1 && self.shown.as_ref().is_some_and(|(p, _)| *p == self.queue[0]) {
                    self.queue.swap(0, n - 1);
                }
            }
        } else {
            self.pos = (self.pos + n - 1) % n;
        }
        // Prevent the auto-advance timer from firing again before the new photo shows.
        self.shown_at = Instant::now();
        self.hold = None;
    }

    /// Goes back to the photo that was on screen just before this one.
    fn go_back(&mut self) {
        while let Some(p) = self.history.pop() {
            // A sentence card is in the queue several times: take the copy just before this card.
            let before = self.queue[..self.pos.min(self.queue.len())].iter().rposition(|q| *q == p);
            if let Some(i) = before.or_else(|| self.queue.iter().position(|q| *q == p)) {
                self.pos = i;
                self.shown_at = Instant::now();
                self.hold = None;
                return;
            }
        }
        self.step(false);
    }

    /// Where learned cards go: the `known` setting, or a `known` folder next to the first source folder.
    fn known_dir(&self) -> PathBuf {
        cards::known_dir(self.settings.known.as_deref(), &self.dirs)
    }

    /// "I know this one": moves the current card to the known folder (Undo stays available).
    fn mark_known(&mut self, ctx: &egui::Context) {
        let Some((path, _)) = self.shown.clone() else { return };
        if self.remote.is_some() {
            // Off the screen at once; the server moves it (on_net puts it back if that fails).
            self.cache.remove(&path);
            self.shown = None;
            self.remove_from_queue(&path);
            let id = path.clone();
            self.ask_server(ctx, move |r| Net::Known(id.clone(), r.known(&id)));
            return;
        }
        let dir = self.known_dir();
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast = Some((format!("{} {}: {e}", tr("无法创建", "Cannot create"), dir.display()), Instant::now(), false));
            return;
        }
        self.move_current_to(path, dir);
    }

    fn busy(&self) -> bool {
        self.pick.is_some()
    }

    fn undo(&mut self, ctx: &egui::Context) {
        let msg = match self.last_action.take() {
            Some(Undo::Remote) => {
                self.ask_server(ctx, |r| Net::Undone(r.undo()));
                self.toast = None;
                return;
            }
            Some(Undo::Moved { from, to }) => {
                let dest = if from.exists() { unique_path(&from) } else { from };
                match move_file(&to, &dest) {
                    Ok(()) => {
                        self.reinsert(dest);
                        tr("已放回去", "Moved back").to_string()
                    }
                    Err(e) => format!("{}: {e}", tr("撤销失败", "Undo failed")),
                }
            }
            None => return,
        };
        self.toast = Some((msg, Instant::now(), false));
    }

    /// Puts a restored photo right after the current one and shows it.
    fn reinsert(&mut self, path: PathBuf) {
        let at = (self.pos + 1).min(self.queue.len());
        self.queue.insert(at, path);
        self.step(true);
    }

    fn move_current_to(&mut self, photo: PathBuf, dir: PathBuf) {
        let Some(name) = photo.file_name() else { return };
        let mut dest = dir.join(name);
        if dest == photo {
            self.toast = Some((tr("已经在这个文件夹里", "Already in that folder").into(), Instant::now(), false));
            return;
        }
        if dest.exists() {
            dest = unique_path(&dest);
        }
        match move_file(&photo, &dest) {
            Ok(()) => {
                self.cache.remove(&photo);
                if self.shown.as_ref().is_some_and(|(p, _)| *p == photo) {
                    self.shown = None;
                }
                self.remove_from_queue(&photo);
                self.last_action = Some(Undo::Moved { from: photo, to: dest });
                let label = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string());
                self.toast = Some((format!("{} {label}", tr("已移到", "Moved to")), Instant::now(), true));
                self.settings.recent.retain(|p| *p != dir);
                self.settings.recent.insert(0, dir);
                self.settings.recent.truncate(8);
                settings::save(&self.settings);
            }
            Err(e) => self.toast = Some((format!("{}: {e}", tr("移动失败", "Move failed")), Instant::now(), false)),
        }
    }

    /// Opens the system folder picker without blocking the UI.
    fn start_pick(&mut self, ctx: &egui::Context, kind: Pick) {
        if self.pick.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let start = self.settings.recent.first().cloned().or_else(home_dir);
        let title = match kind {
            Pick::AddSource => tr("添加卡片文件夹", "Add a card folder"),
        };
        self.pick = Some((kind, rx));
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new().set_title(title);
            if let Some(dir) = start {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_folder());
            ctx.request_repaint();
        });
    }

    fn rescan(&mut self, ctx: &egui::Context) {
        self.scan_rx = loader::spawn_scan(ctx.clone(), self.dirs.clone());
        self.scanned = false;
    }

    fn save_dirs(&mut self, ctx: &egui::Context) {
        self.settings.dirs = self.dirs.clone();
        settings::save(&self.settings);
        self.rescan(ctx);
    }

    fn settings_viewport(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title(tr("词卡 设置", "GlanceCards settings"))
            .with_inner_size([560.0, 500.0])
            .with_min_inner_size([400.0, 380.0]);
        let dirs = self.dirs.clone();
        let server = self.remote.as_ref().map(|r| r.base().to_string());
        let mut secs = self.interval.as_secs();
        let mut every = self.settings.sentence_every;
        let mut fit = self.fit;
        let mut level = self.level;
        let wayland = is_wayland();
        let (mut close, mut add, mut remove) = (false, false, None::<usize>);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("glancecards-settings"), builder, |ctx, _| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(server) = &server {
                    ui.heading(tr("卡片服务器", "Card server"));
                    ui.label(tr("卡片来自这台服务器，“认识了”也记在那里（和手机共用）。", "Cards come from this server, and Got it is recorded there (shared with the phone)."));
                    ui.add_space(6.0);
                    ui.label(server);
                    ui.weak(tr("要改回读本机文件夹，删掉设置文件里的 server= 这一行。", "To read local folders again, remove the server= line from the settings file."));
                } else {
                    ui.heading(tr("卡片文件夹", "Card folders"));
                    ui.label(tr("从这些文件夹（包括里面的子文件夹）里随机抽取卡片。", "Cards are picked at random from these folders and everything inside them."));
                    ui.add_space(6.0);
                    for (i, d) in dirs.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui.add_enabled(dirs.len() > 1, egui::Button::new(tr("移除", "Remove"))).clicked() {
                                remove = Some(i);
                            }
                            ui.label(d.display().to_string());
                        });
                    }
                    ui.add_space(4.0);
                    if ui.button(tr("添加文件夹…", "Add folder...")).clicked() {
                        add = true;
                    }
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(tr("每隔", "Change card every"));
                    ui.add(egui::DragValue::new(&mut secs).range(1..=3600).suffix(tr(" 秒", " s")));
                    ui.label(tr("换一张卡片", ""));
                });
                ui.horizontal(|ui| {
                    ui.label(tr("句子卡和生词卡：每", "Sentence and own-word cards: one after every"));
                    ui.add(egui::DragValue::new(&mut every).range(0..=100));
                    ui.label(tr("张其他卡后出一张（0 = 不特别安排）", "other cards (0 = no special place)"));
                });
                ui.checkbox(&mut fit, tr("窗口贴合每张卡片的形状", "Fit the window to each card's shape"));
                ui.separator();
                ui.heading(tr("窗口位置", "Window position"));
                ui.radio_value(&mut level, Level::Top, tr("悬浮在其他窗口之上", "Float above other windows"));
                ui.radio_value(&mut level, Level::Normal, tr("普通窗口", "Ordinary window"));
                ui.add_enabled_ui(!wayland, |ui| {
                    ui.radio_value(&mut level, Level::Bottom, tr("沉在其他窗口之下（像桌面小部件）", "Below other windows (like a desktop widget)"));
                });
                if wayland {
                    ui.weak(tr("当前是 Wayland 桌面，不能沉在其他窗口之下。", "On Wayland the window cannot be put below other windows."));
                }
                ui.separator();
                if let Some(p) = settings::file_path() {
                    ui.weak(format!("{} {}", tr("设置保存在", "Saved in"), p.display()));
                }
                if ui.button(tr("关闭", "Close")).clicked() {
                    close = true;
                }
            });
            if ctx.input(|i| i.viewport().close_requested()) {
                close = true;
            }
        });
        if let Some(i) = remove {
            if i < self.dirs.len() && self.dirs.len() > 1 {
                self.dirs.remove(i);
                self.save_dirs(ctx);
            }
        }
        if add {
            self.start_pick(ctx, Pick::AddSource);
        }
        if secs != self.interval.as_secs() {
            self.interval = Duration::from_secs(secs.max(1));
            self.settings.interval = secs.max(1);
            settings::save(&self.settings);
        }
        if every != self.settings.sentence_every {
            // Takes effect when the next round is shuffled.
            self.settings.sentence_every = every;
            settings::save(&self.settings);
        }
        if fit != self.fit {
            self.fit = fit;
            self.settings.fit = fit;
            settings::save(&self.settings);
        }
        if level != self.level {
            self.level = level;
            self.settings.level = level;
            settings::save(&self.settings);
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(window_level(level)));
        }
        if close {
            self.settings_open = false;
        }
    }
}

/// Fills the window with the card background; the card gets the whole window (no frame, no margin).
fn paint_note(p: &egui::Painter, rect: Rect) -> Rect {
    p.rect_filled(rect, 0.0, CARD_BG);
    rect
}

/// Space taken around the card, for a window of this size: none.
fn note_pad(_size: egui::Vec2) -> f32 {
    0.0
}

fn resize_zone(rect: Rect, p: Pos2) -> Option<ResizeDirection> {
    let left = p.x - rect.min.x < EDGE;
    let right = rect.max.x - p.x < EDGE;
    let top = p.y - rect.min.y < EDGE;
    let bottom = rect.max.y - p.y < EDGE;
    use ResizeDirection::*;
    match (left, right, top, bottom) {
        (true, _, true, _) => Some(NorthWest),
        (_, true, true, _) => Some(NorthEast),
        (true, _, _, true) => Some(SouthWest),
        (_, true, _, true) => Some(SouthEast),
        (true, ..) => Some(West),
        (_, true, ..) => Some(East),
        (_, _, true, _) => Some(North),
        (_, _, _, true) => Some(South),
        _ => None,
    }
}

fn resize_cursor(d: ResizeDirection) -> CursorIcon {
    match d {
        ResizeDirection::North => CursorIcon::ResizeNorth,
        ResizeDirection::South => CursorIcon::ResizeSouth,
        ResizeDirection::East => CursorIcon::ResizeEast,
        ResizeDirection::West => CursorIcon::ResizeWest,
        ResizeDirection::NorthEast => CursorIcon::ResizeNorthEast,
        ResizeDirection::NorthWest => CursorIcon::ResizeNorthWest,
        ResizeDirection::SouthEast => CursorIcon::ResizeSouthEast,
        ResizeDirection::SouthWest => CursorIcon::ResizeSouthWest,
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Some X11 window managers drop the level requested at creation, so ask again once mapped.
        self.frames += 1;
        if self.level != Level::Normal && self.frames == 3 {
            ctx.send_viewport_cmd(ViewportCommand::WindowLevel(window_level(self.level)));
        }
        self.poll(&ctx);
        if let Some(a) = self.fit_aspect.take() {
            if self.fit {
                // Extremely wide or tall photos keep their letterbox instead of a sliver of a frame.
                self.fit_window(&ctx, a.clamp(0.5, 2.0));
            }
        }

        // Keyboard shortcuts.
        let (next, prev, del, pause) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::Delete),
                i.key_pressed(egui::Key::Space),
            )
        });
        if next {
            self.step(true);
        }
        if prev {
            self.go_back();
        }
        // Holding Delete repeats the key; one card per key press, not a run of them.
        if del && self.shown_since.elapsed() >= Duration::from_millis(400) {
            self.mark_known(&ctx);
        }
        if pause {
            self.paused = !self.paused;
        }

        // Auto-advance once the current photo has been on screen long enough.
        let showing_current = self.current_path().zip(self.shown.as_ref()).is_some_and(|(c, (s, _))| c == s);
        if self.hover_photo {
            // Hold the timer so the photo gets a full interval once the pointer leaves.
            self.shown_at = Instant::now();
        }
        // Stop waiting for the return from the dictionary after a while (the pointer may come back without
        // crossing the card, or the window may never notice it left); the card then gets a full interval.
        if self.hold.is_some_and(|t| t.elapsed() >= LOOKUP_HOLD) {
            self.hold = None;
            self.shown_at = Instant::now();
        }
        if !self.paused && self.hold.is_none() && !self.busy() && showing_current && self.shown_at.elapsed() >= self.card_interval() && self.queue.len() > 1 {
            self.step(true);
        }

        let rect = ui.max_rect();
        let painter = ui.painter().clone();
        let photo_area = paint_note(&painter, rect);
        let mut shown_rect: Option<Rect> = None;

        match &self.shown {
            Some((_, tex)) => {
                let size = tex.size_vec2();
                let scale = (photo_area.width() / size.x).min(photo_area.height() / size.y);
                let img_rect = Rect::from_center_size(photo_area.center(), size * scale);
                shown_rect = Some(img_rect);
                painter.image(
                    tex.id(),
                    img_rect,
                    Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            None => {
                let msg = if self.remote.is_some() && self.net_error.is_some() && self.current_path().is_none() {
                    tr("连不上卡片服务器，稍后自动重试", "Cannot reach the card server; trying again soon")
                } else if !self.scanned {
                    if self.remote.is_some() { tr("正在连接卡片服务器…", "Connecting to the card server...") } else { tr("正在扫描卡片…", "Scanning cards...") }
                } else if self.queue.is_empty() {
                    tr("没有找到卡片", "No cards found")
                } else {
                    tr("加载中…", "Loading...")
                };
                painter.text(
                    photo_area.center(),
                    Align2::CENTER_CENTER,
                    msg,
                    FontId::proportional(16.0),
                    Color32::from_rgb(110, 100, 85),
                );
            }
        }

        // Background: drag to move, right-click for menu.
        let bg = ui.interact(rect, Id::new("bg"), Sense::click_and_drag());
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        let zone = pointer.and_then(|p| resize_zone(rect, p));
        if let Some(dir) = zone {
            ctx.set_cursor_icon(resize_cursor(dir));
            if ctx.input(|i| i.pointer.primary_pressed()) {
                ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
            }
        } else if bg.drag_started_by(egui::PointerButton::Primary) {
            ctx.send_viewport_cmd(ViewportCommand::StartDrag);
        }
        if bg.double_clicked() {
            self.step(true);
        }
        bg.context_menu(|ui| {
            if ui.button(tr("下一张", "Next card")).clicked() {
                self.step(true);
                ui.close();
            }
            if ui.button(tr("上一张", "Previous card")).clicked() {
                self.go_back();
                ui.close();
            }
            if ui.checkbox(&mut self.fit, tr("窗口贴合卡片", "Fit window to card")).changed() && self.fit {
                self.fit_aspect = self.shown.as_ref().map(|(_, t)| t.aspect_ratio());
            }
            let label = if self.paused { tr("继续轮播", "Resume slideshow") } else { tr("暂停轮播", "Pause slideshow") };
            if ui.button(label).clicked() {
                self.paused = !self.paused;
                ui.close();
            }
            ui.separator();
            if ui.button(tr("设置…", "Settings...")).clicked() {
                self.settings_open = true;
                ui.close();
            }
            if ui.button(tr("退出", "Quit")).clicked() {
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        });

        // Hover overlay (only while the pointer is over the photo itself): file name and the Got it button.
        let overlay = shown_rect.unwrap_or(photo_area);
        let hovering = shown_rect.zip(pointer).is_some_and(|(r, p)| r.contains(p));
        if hovering && !self.hover_photo {
            self.hold = None; // back from the dictionary
        }
        self.hover_photo = hovering;
        // Ignore button clicks on a card that has only just appeared: the second click of a quick double
        // click would otherwise land on the next card's button.
        let settled = self.shown_since.elapsed() >= Duration::from_millis(400);
        if hovering && self.shown.is_some() {
            if let Some((path, _)) = &self.shown {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let galley = painter.layout_no_wrap(name, FontId::proportional(12.0), Color32::WHITE);
                let r = Rect::from_min_size(
                    overlay.left_bottom() + vec2(6.0, -galley.size().y - 8.0),
                    galley.size() + vec2(10.0, 6.0),
                );
                painter.rect_filled(r, 4.0, Color32::from_black_alpha(150));
                painter.galley(r.min + vec2(5.0, 3.0), galley, Color32::WHITE);
            }
            // "Previous photo" button: a round chevron at the left edge of the photo.
            let c = overlay.left_center() + vec2(26.0, 0.0);
            let prev_resp = ui.interact(Rect::from_center_size(c, vec2(38.0, 38.0)), Id::new("prev"), Sense::click());
            let prev_fill = if prev_resp.hovered() { Color32::from_black_alpha(190) } else { Color32::from_black_alpha(120) };
            painter.circle_filled(c, 18.0, prev_fill);
            let stroke = egui::Stroke::new(2.5, Color32::WHITE);
            painter.line_segment([c + vec2(3.5, -8.0), c + vec2(-3.5, 0.0)], stroke);
            painter.line_segment([c + vec2(-3.5, 0.0), c + vec2(3.5, 8.0)], stroke);
            if prev_resp.hovered() {
                ctx.set_cursor_icon(CursorIcon::PointingHand);
            }
            if prev_resp.clicked() {
                self.go_back();
            }
            let btn = Rect::from_min_size(overlay.right_top() + vec2(-84.0, 8.0), vec2(76.0, 28.0));
            let resp = ui.interact(btn, Id::new("known"), Sense::click());
            let fill = if resp.hovered() {
                Color32::from_rgb(50, 160, 80)
            } else {
                Color32::from_rgba_unmultiplied(40, 130, 65, 215)
            };
            painter.rect_filled(btn, 6.0, fill);
            painter.text(btn.center(), Align2::CENTER_CENTER, tr("认识了", "Got it"), FontId::proportional(14.0), Color32::WHITE);
            if resp.hovered() {
                ctx.set_cursor_icon(CursorIcon::PointingHand);
            }
            if resp.clicked() && settled {
                self.mark_known(&ctx);
            }
            // "Don't know" button: opens the word in the Cambridge English–Chinese dictionary.
            if let Some(word) = self.shown.as_ref().and_then(|(p, _)| card_word(p)) {
                let btn = Rect::from_min_size(overlay.left_top() + vec2(8.0, 8.0), vec2(76.0, 28.0));
                let resp = ui.interact(btn, Id::new("unknown"), Sense::click());
                let fill = if resp.hovered() {
                    Color32::from_rgb(215, 95, 45)
                } else {
                    Color32::from_rgba_unmultiplied(190, 80, 35, 215)
                };
                painter.rect_filled(btn, 6.0, fill);
                painter.text(btn.center(), Align2::CENTER_CENTER, tr("不认识", "Look up"), FontId::proportional(14.0), Color32::WHITE);
                if resp.hovered() {
                    ctx.set_cursor_icon(CursorIcon::PointingHand);
                }
                if resp.clicked() && settled {
                    self.hold = Some(Instant::now());
                    if let Err(e) = open_url(&cambridge_url(&word)) {
                        self.hold = None;
                        self.toast = Some((format!("{}: {e}", tr("打不开浏览器", "Could not open the browser")), Instant::now(), false));
                    }
                }
            }
        }

        // Toast with Undo.
        if let Some((msg, at, undoable)) = self.toast.clone() {
            if at.elapsed() > Duration::from_secs(TOAST_SECS) {
                self.toast = None;
            } else {
                let text_w = painter.layout_no_wrap(msg.clone(), FontId::proportional(13.0), Color32::WHITE).size().x;
                let w = (text_w + 28.0 + if undoable { 62.0 } else { 0.0 }).min(photo_area.width());
                let r = Rect::from_center_size(photo_area.center_bottom() + vec2(0.0, -24.0), vec2(w, 32.0));
                painter.rect_filled(r, 8.0, Color32::from_black_alpha(215));
                painter.text(
                    r.left_center() + vec2(12.0, 0.0),
                    Align2::LEFT_CENTER,
                    msg,
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
                if undoable {
                    let ub = Rect::from_min_size(r.right_top() + vec2(-58.0, 4.0), vec2(50.0, 24.0));
                    let resp = ui.interact(ub, Id::new("undo"), Sense::click());
                    let fill = if resp.hovered() { Color32::from_gray(90) } else { Color32::from_gray(60) };
                    painter.rect_filled(ub, 5.0, fill);
                    painter.text(ub.center(), Align2::CENTER_CENTER, tr("撤销", "Undo"), FontId::proportional(13.0), Color32::WHITE);
                    if resp.clicked() {
                        self.undo(&ctx);
                    }
                }
            }
        }

        self.settings_viewport(&ctx);

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// Opens a web page in the default browser without waiting for it.
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut cmd = {
        // rundll32 takes the URL as one argument, so "&" in it is not read by a shell.
        let mut c = std::process::Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(not(any(windows, target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(url);
        c
    };
    let mut child = cmd.spawn()?;
    // Reap the launcher when it exits so it does not linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

/// Wayland compositors (GNOME, KDE) have no "stay below other windows" level, so there the
/// frame opens as an ordinary window instead of trying to sit on the desktop.
fn is_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn parse_args(st: &Settings) -> Config {
    let mut dirs = Vec::new();
    let mut interval = st.interval;
    let mut level = None;
    let mut serve = None;
    let mut server = st.server.clone();
    let mut args = std::env::args().skip(1).peekable();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--server" => server = args.next(),
            "--serve" => {
                let addr = args.next_if(|v| v.contains(':')).unwrap_or_else(|| "127.0.0.1:8090".to_string());
                serve = Some(addr);
            }
            "--interval" | "-i" => {
                if let Some(v) = args.next().and_then(|v| v.parse().ok()) {
                    interval = v;
                }
            }
            "--normal" => level = Some(Level::Normal),
            "--top" => level = Some(Level::Top),
            "--bottom" => level = Some(Level::Bottom),
            "--help" | "-h" => {
                println!(
                    "glancecards (词卡) [--interval SECONDS] [--top | --normal | --bottom] [--serve [ADDR] | --server URL] [FOLDER...]\n\nDefault folder: ~/word-images/learning, default interval: 10s.\n--top floats above other windows, --normal is an ordinary window, --bottom sits below all windows like a desktop widget (X11 only; the default). These override the setting in the settings window.\n--serve runs the web page for phones instead of the window (default 127.0.0.1:8090; there is no sign-in, so keep it on 127.0.0.1 behind an HTTPS proxy such as tailscale serve).\n--server URL shows the cards of a glancecards --serve server (for example https://host:8443) instead of reading folders; Got it is sent there. The server= line in the settings file does the same."
                );
                std::process::exit(0);
            }
            _ => dirs.push(PathBuf::from(a)),
        }
    }
    if dirs.is_empty() {
        dirs = st.dirs.clone();
    }
    if dirs.is_empty() {
        dirs.push(home_dir().map(|h| h.join("word-images").join("learning")).unwrap_or_else(|| PathBuf::from(".")));
    }
    let mut level = level.unwrap_or(st.level);
    if level == Level::Bottom && is_wayland() {
        level = Level::Normal; // Wayland has no "below all windows"
    }
    Config { dirs, interval: Duration::from_secs(interval.max(1)), level, settings: st.clone(), serve, server }
}

fn main() -> eframe::Result {
    let st = settings::load();
    let cfg = parse_args(&st);
    if let Some(addr) = &cfg.serve {
        // The web page runs on its own, next to the window if that is open too.
        let mut st = cfg.settings.clone();
        st.interval = cfg.interval.as_secs();
        web::serve(addr, cfg.dirs.clone(), &st);
        return Ok(());
    }
    // Keep the lock until the app quits.
    let Some(_lock) = settings::single_instance() else {
        eprintln!("GlanceCards (词卡) is already running.");
        return Ok(());
    };
    let level = window_level(cfg.level);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("词卡 GlanceCards")
            .with_app_id("glancecards")
            .with_inner_size([300.0, 420.0])
            .with_min_inner_size([200.0, 160.0])
            .with_decorations(false)
            .with_taskbar(false)
            .with_window_level(level),
        ..Default::default()
    };
    let mut cfg = Some(cfg);
    eframe::run_native(
        "glancecards",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(&cc.egui_ctx, cfg.take().expect("single init"))))),
    )
}
