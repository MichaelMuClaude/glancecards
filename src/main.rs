//! A small, resizable desktop card window for learning words: shows random word cards (pictures)
//! from a folder and lets you move the ones you already know to another folder with one click.

// A release build on Windows is a window app: no console window next to it.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod loader;
mod settings;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align2, Color32, CursorIcon, FontId, Id, Pos2, Rect, ResizeDirection, Sense, TextureHandle,
    TextureOptions, ViewportCommand, vec2,
};
use rand::seq::SliceRandom;

use loader::Loaded;
use settings::{Level, Settings, home_dir};

/// Background behind the card: the same cream as the strip under each picture, so a card that does not
/// fill the window (no "fit to card") blends in instead of showing bars.
const CARD_BG: Color32 = Color32::from_rgb(250, 247, 240);
const EDGE: f32 = 7.0;
const TOAST_SECS: u64 = 8;

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
    /// Set by "Look up": the card stays until the pointer comes back over it, or Next is used.
    hold: bool,
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
}

impl App {
    fn new(ctx: &egui::Context, cfg: Config) -> Self {
        ZH.store(install_cjk_font(ctx), Ordering::Relaxed);
        let scan_rx = loader::spawn_scan(ctx.clone(), cfg.dirs.clone());
        let (job_tx, res_rx) = loader::spawn_workers(ctx.clone());
        Self {
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
            hold: false,
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

    /// Removes every copy of the card (sentence cards appear several times in a round).
    fn remove_from_queue(&mut self, path: &PathBuf) {
        while let Some(i) = self.queue.iter().position(|p| p == path) {
            self.queue.remove(i);
            if i < self.pos {
                self.pos -= 1;
            }
        }
        if self.pos >= self.queue.len() {
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
            if self.pos >= n {
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
        self.hold = false;
    }

    /// Goes back to the photo that was on screen just before this one.
    fn go_back(&mut self) {
        while let Some(p) = self.history.pop() {
            // A sentence card is in the queue several times: take the copy just before this card.
            let before = self.queue[..self.pos.min(self.queue.len())].iter().rposition(|q| *q == p);
            if let Some(i) = before.or_else(|| self.queue.iter().position(|q| *q == p)) {
                self.pos = i;
                self.shown_at = Instant::now();
                self.hold = false;
                return;
            }
        }
        self.step(false);
    }

    /// Where learned cards go: the `known` setting, or a `known` folder next to the first source folder.
    fn known_dir(&self) -> PathBuf {
        self.settings.known.clone().unwrap_or_else(|| {
            let first = self.dirs.first().cloned().unwrap_or_else(|| PathBuf::from("."));
            first.parent().map(|p| p.join("known")).unwrap_or_else(|| first.join("known"))
        })
    }

    /// "I know this one": moves the current card to the known folder (Undo stays available).
    fn mark_known(&mut self) {
        let Some((path, _)) = self.shown.clone() else { return };
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

    fn undo(&mut self) {
        let msg = match self.last_action.take() {
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
        let mut secs = self.interval.as_secs();
        let mut every = self.settings.sentence_every;
        let mut fit = self.fit;
        let mut level = self.level;
        let wayland = is_wayland();
        let (mut close, mut add, mut remove) = (false, false, None::<usize>);
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("glancecards-settings"), builder, |ctx, _| {
            egui::CentralPanel::default().show(ctx, |ui| {
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
            self.mark_known();
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
        if !self.paused && !self.hold && !self.busy() && showing_current && self.shown_at.elapsed() >= self.card_interval() && self.queue.len() > 1 {
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
                let msg = if !self.scanned {
                    tr("正在扫描卡片…", "Scanning cards...")
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
            self.hold = false; // back from the dictionary
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
                self.mark_known();
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
                    self.hold = true;
                    if let Err(e) = open_url(&cambridge_url(&word)) {
                        self.hold = false;
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
                        self.undo();
                    }
                }
            }
        }

        self.settings_viewport(&ctx);

        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn unique_path(p: &Path) -> PathBuf {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let dir = p.parent().unwrap_or(Path::new("."));
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|c| !c.exists())
        .expect("an unused name exists")
}

/// Sentence cards are named "sentence_<id>_<key-phrase>.png".
fn is_sentence(p: &Path) -> bool {
    p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("sentence_"))
}

/// Cards you are learning right now, shown more often: sentence cards and the words you looked up
/// yourself ("mine_<word>.png").
fn is_featured(p: &Path) -> bool {
    is_sentence(p) || p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("mine_"))
}

/// Shuffles the cards for a new round and spreads the featured cards (sentences and your own words)
/// through it: one after every `every` other cards, going round them again and again, so a handful come
/// back often instead of once in thousands of cards. `every` = 0 shuffles them in like any other card.
fn arrange(cards: Vec<PathBuf>, every: usize) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let (mut sentences, mut words): (Vec<_>, Vec<_>) =
        cards.into_iter().filter(|p| seen.insert(p.clone())).partition(|p| is_featured(p));
    let rng = &mut rand::rng();
    if every == 0 || sentences.is_empty() || words.is_empty() {
        words.append(&mut sentences);
        words.shuffle(rng);
        return words;
    }
    sentences.shuffle(rng);
    words.shuffle(rng);
    let n = words.len();
    let slots = (n / every).max(sentences.len());
    let mut out = Vec::with_capacity(n + slots);
    let mut placed = 0;
    for (i, w) in words.into_iter().enumerate() {
        out.push(w);
        while placed < slots && (placed + 1) * n / slots <= i + 1 {
            out.push(sentences[placed % sentences.len()].clone());
            placed += 1;
        }
    }
    out
}

/// The word a card shows, taken from its file name: "apple.png", "wa_according-to.jpg",
/// or "apple (2).png" after a name clash in the known folder. A sentence card gives its key phrase
/// ("sentence_005_figure-out.png" is "figure out"); one of your own words drops its prefix
/// ("mine_verge-collection.png" is "verge collection").
fn card_word(p: &Path) -> Option<String> {
    let stem = p.file_stem()?.to_string_lossy();
    let own = stem.strip_prefix("mine_");
    if let Some(rest) = own.or_else(|| stem.strip_prefix("sentence_")) {
        let phrase = if own.is_some() { rest } else { rest.split_once('_').map_or(rest, |(_, k)| k) };
        let phrase = phrase.split(" (").next().unwrap_or(phrase).replace('-', " ");
        let phrase = phrase.trim();
        return (!phrase.is_empty()).then(|| phrase.to_string());
    }
    let stem = stem.strip_prefix("wa_").unwrap_or(&stem);
    let stem = match stem.rfind(" (") {
        Some(i) if stem.ends_with(')') => &stem[..i],
        _ => stem,
    };
    let word = stem.trim();
    (!word.is_empty()).then(|| word.to_string())
}

/// Cambridge's direct search jumps to the entry, or to its spelling suggestions when there is none.
fn cambridge_url(word: &str) -> String {
    let q: String = word
        .bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            b' ' => "+".to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("https://dictionary.cambridge.org/search/direct/?datasetsearch=english-chinese-simplified&q={q}")
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

/// Renames, or copies and deletes when the folders are on different drives.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    if let Err(e) = std::fs::remove_file(from) {
        // Leave one copy, not two: the card stays where it was.
        let _ = std::fs::remove_file(to);
        return Err(e);
    }
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
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
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
                    "glancecards (词卡) [--interval SECONDS] [--top | --normal | --bottom] [FOLDER...]\n\nDefault folder: ~/word-images/learning, default interval: 10s.\n--top floats above other windows, --normal is an ordinary window, --bottom sits below all windows like a desktop widget (X11 only; the default). These override the setting in the settings window."
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
    Config { dirs, interval: Duration::from_secs(interval.max(1)), level, settings: st.clone() }
}

fn main() -> eframe::Result {
    let st = settings::load();
    let cfg = parse_args(&st);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_from_file_name() {
        assert_eq!(card_word(Path::new("/x/apple.png")).as_deref(), Some("apple"));
        assert_eq!(card_word(Path::new("/x/wa_according-to.jpg")).as_deref(), Some("according-to"));
        assert_eq!(card_word(Path::new("/x/apple (2).png")).as_deref(), Some("apple"));
        assert_eq!(card_word(Path::new("/x/wa_.jpg")), None);
        assert_eq!(card_word(Path::new("/x/sentence_005_figure-out.png")).as_deref(), Some("figure out"));
        assert_eq!(card_word(Path::new("/x/sentence_002_take-a-break (1).png")).as_deref(), Some("take a break"));
        assert_eq!(card_word(Path::new("/x/mine_verge-collection.png")).as_deref(), Some("verge collection"));
        assert_eq!(card_word(Path::new("/x/mine_tradie.jpg")).as_deref(), Some("tradie"));
        assert!(is_featured(Path::new("/x/mine_tradie.png")) && !is_sentence(Path::new("/x/mine_tradie.png")));
    }

    #[test]
    fn sentences_are_spread_through_the_round() {
        let words: Vec<PathBuf> = (0..100).map(|i| PathBuf::from(format!("w{i}.png"))).collect();
        let sentences: Vec<PathBuf> = (1..=3).map(|i| PathBuf::from(format!("sentence_00{i}_x.png"))).collect();
        let all: Vec<PathBuf> = words.iter().chain(&sentences).cloned().collect();
        let q = arrange(all.clone(), 5);
        assert_eq!(q.len(), 120);
        assert_eq!(q.iter().filter(|p| !is_sentence(p)).count(), 100);
        // Never two sentences in a row, and every sentence comes back several times.
        assert!(q.windows(2).all(|w| !(is_sentence(&w[0]) && is_sentence(&w[1]))));
        assert!(sentences.iter().all(|s| q.iter().filter(|p| *p == s).count() >= 6));
        // A round that already holds repeats is rebuilt from one copy of each card.
        assert_eq!(arrange(q, 5).len(), 120);
        // 0 turns it off; fewer words than `every` still shows each sentence once.
        assert_eq!(arrange(all, 0).len(), 103);
        assert_eq!(arrange(vec![words[0].clone()].into_iter().chain(sentences.clone()).collect(), 5).len(), 4);
    }

    #[test]
    fn cambridge_query_is_encoded() {
        assert!(cambridge_url("according-to").ends_with("&q=according-to"));
        assert!(cambridge_url("ice cream").ends_with("&q=ice+cream"));
        assert!(cambridge_url("o'clock&x").ends_with("&q=o%27clock%26x"));
        assert!(cambridge_url("café").ends_with("&q=caf%C3%A9"));
    }

    #[test]
    fn move_file_moves_once() {
        let dir = std::env::temp_dir().join(format!("glancecards-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.png"), dir.join("b.png"));
        std::fs::write(&a, b"x").unwrap();
        move_file(&a, &b).unwrap();
        assert!(!a.exists() && b.exists());
        assert_eq!(unique_path(&b), dir.join("b (1).png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
