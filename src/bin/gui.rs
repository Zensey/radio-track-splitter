//! Waveform editor for adjusting split points of a radio recording, then
//! exporting the tracks with ffmpeg (lossless -c copy).
//!
//! Starts from the neural auto-detection; then fix the cut points by hand:
//! drag markers, type exact times, nudge, snap to the quietest moment, and
//! listen to each cut before exporting.
//!
//! Mouse (waveform): click = move playhead, drag marker = move split, drag
//! empty area = pan, double-click = add split, right-click marker = delete,
//! wheel = zoom (shift+wheel pans). Overview bar: click/drag to jump.
//! Keys: space play/stop, left/right nudge (shift 1s, ctrl 0.01s), up/down
//! select split, delete, A add at playhead, P listen to the cut.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use radio_track_splitter::editor::{fmt_dur, fmt_time, parse_time, Editor};
use radio_track_splitter::envelope::Envelope;
use radio_track_splitter::{audio, detect, pipeline};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc};
use std::time::Instant;

const BG: Color32 = Color32::from_rgb(0x20, 0x21, 0x24);
const BG_ALT: Color32 = Color32::from_rgb(0x26, 0x28, 0x2d);
const WAVE: Color32 = Color32::from_rgb(0x6f, 0xa8, 0xdc);
const WAVE_DIM: Color32 = Color32::from_rgb(0x4a, 0x6f, 0x96);
const MARK: Color32 = Color32::from_rgb(0xff, 0x52, 0x52);
const MARK_SEL: Color32 = Color32::from_rgb(0xff, 0xb3, 0x00);
const HEAD: Color32 = Color32::from_rgb(0x4c, 0xaf, 0x50);
const TEXT: Color32 = Color32::from_rgb(0x9a, 0xa0, 0xa6);
const RULER_H: f32 = 22.0;
const APP_NAME: &str = "Radio Track Splitter";
const ABOUT_SCALE: f32 = 1.5;
// Shown as links in the About dialog; a link with an empty URL is left out.
const PROJECT_URL: &str = "";
const SUPPORT_URL: &str = "https://sites.google.com/view/anton-litvinov/donate";

const OVERVIEW_H: f32 = 58.0;
const HIT_TOLERANCE: f32 = 7.0;

// ---------- persistence ----------

#[derive(Serialize, Deserialize, Default, Clone)]
struct Saved {
    marks: Vec<f64>,
    out_dir: String,
}

fn state_path_in(dir_name: &str) -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join(dir_name).join("gui_state.json")
}

fn state_path() -> PathBuf {
    state_path_in("radio-track-splitter")
}

fn load_state() -> HashMap<String, Saved> {
    // Fall back to the folder used before the project was renamed.
    std::fs::read_to_string(state_path())
        .or_else(|_| std::fs::read_to_string(state_path_in("split_radio")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(state: &HashMap<String, Saved>) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(state) {
        let _ = std::fs::write(path, json);
    }
}

// ---------- playback (ffplay) ----------

struct Player {
    child: Option<Child>,
    from: f64,
    started: Instant,
}

impl Player {
    fn new() -> Self {
        Self { child: None, from: 0.0, started: Instant::now() }
    }

    fn is_playing(&mut self) -> bool {
        match self.child.as_mut().map(|c| c.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.child = None;
                false
            }
            None => false,
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn play(&mut self, input: &Path, start: f64, duration: Option<f64>) -> std::io::Result<()> {
        self.stop();
        let mut cmd = audio::quiet_command("ffplay");
        cmd.args(["-nodisp", "-autoexit", "-loglevel", "quiet", "-ss", &format!("{start:.3}")]);
        if let Some(d) = duration {
            cmd.args(["-t", &format!("{d:.3}")]);
        }
        cmd.arg(input).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        self.child = Some(cmd.spawn()?);
        self.from = start;
        self.started = Instant::now();
        Ok(())
    }

    /// Estimated playback position (ffplay takes ~0.25s to start producing sound).
    fn position(&self) -> f64 {
        self.from + (self.started.elapsed().as_secs_f64() - 0.25).max(0.0)
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------- background jobs ----------

struct Loaded {
    input: PathBuf,
    duration: f64,
    samples: Arc<Vec<i16>>,
    env: Envelope,
}

enum Msg {
    Status(String),
    Loaded(Result<Loaded, String>),
    Detected(Result<Vec<f64>, String>),
    Exported(Result<(usize, PathBuf), String>),
}

// ---------- app ----------

struct App {
    ed: Editor,
    input: Option<PathBuf>,
    samples: Option<Arc<Vec<i16>>>,
    env: Option<Arc<Envelope>>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    busy: Option<String>,
    status: String,
    status_is_error: bool,
    sens: String,
    minlen: String,
    out_dir: String,
    time_text: String,
    time_focus: bool,
    drag: Option<usize>,
    player: Player,
    has_ffplay: bool,
    state: HashMap<String, Saved>,
    dirty: bool,
    overview: Option<(usize, Vec<(f32, f32)>)>,
    about_open: bool,
    /// Counts openings of the About dialog; each one is a fresh window (see `centered_dialog`).
    about_serial: u32,
    /// Where the waveform was drawn last frame; the About dialog centres on it.
    signal_rect: Rect,
    /// The title currently set on the window (see `title_update`).
    title: String,
}

/// Window title: the program name, then the open recording's file name (no folder).
fn window_title(input: Option<&Path>) -> String {
    match input.and_then(|path| path.file_name()) {
        Some(name) => format!("{APP_NAME} - {}", name.to_string_lossy()),
        None => APP_NAME.to_string(),
    }
}

/// The new window title if it differs from `current` (which is then updated), else `None`,
/// so the title is only sent to the window when the open file actually changes.
fn title_update(current: &mut String, input: Option<&Path>) -> Option<String> {
    let title = window_title(input);
    (title != *current).then(|| {
        current.clone_from(&title);
        title
    })
}

/// A dialog that appears centred on `center` and can then be dragged by its title bar
/// (which `.anchor()` / `.fixed_pos()` would prevent).
///
/// `serial` is part of the window's identity: use a new number for every opening. egui
/// remembers a window's position under its id, and a window that already has a remembered
/// position ignores `default_pos` (and, in this egui version, `current_pos` too), so
/// reusing the id would bring the dialog back wherever it was last left instead of
/// centred again.
fn centered_dialog<'a>(title: impl Into<egui::WidgetText>, serial: u32, center: Pos2) -> egui::Window<'a> {
    egui::Window::new(title)
        .id(egui::Id::new(("dialog", serial)))
        .collapsible(false)
        .resizable(false)
        .pivot(Align2::CENTER_CENTER)
        .default_pos(center)
}

/// egui's own link opening is not compiled into this build, so hand the URL to
/// Explorer (which opens the default browser). No shell is involved.
fn open_url(url: &str) {
    let _ = Command::new("explorer").arg(url).spawn();
}

impl App {
    fn new(initial: Option<PathBuf>, ctx: &egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let has_ffplay = audio::quiet_command("ffplay")
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let out_dir = audio::default_output_dir();
        let mut app = Self {
            ed: Editor::new(0.0),
            input: None,
            samples: None,
            env: None,
            tx,
            rx,
            busy: None,
            status: "Open a recording to begin.".into(),
            status_is_error: false,
            sens: "1.5".into(),
            minlen: "120".into(),
            out_dir: out_dir.display().to_string(),
            time_text: String::new(),
            time_focus: false,
            drag: None,
            player: Player::new(),
            has_ffplay,
            state: load_state(),
            dirty: false,
            overview: None,
            about_open: false,
            about_serial: 0,
            signal_rect: Rect::ZERO,
            title: APP_NAME.to_string(), // what run_native starts the window with
        };
        if let Some(path) = initial {
            app.open(path, ctx);
        }
        app
    }

    fn set_status(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.status_is_error = error;
    }

    fn spawn(&self, ctx: &egui::Context, job: impl FnOnce(&mpsc::Sender<Msg>, &egui::Context) + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || job(&tx, &ctx));
    }

    // ----- loading / detection -----

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Open recording")
            .add_filter("Audio", &["mp3", "wav", "m4a", "aac", "flac", "ogg", "mp4"]);
        if let Some(music) = std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Music")) {
            dialog = dialog.set_directory(music);
        }
        if let Some(path) = dialog.pick_file() {
            self.open(path, ctx);
        }
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.busy.is_some() {
            return;
        }
        for tool in ["ffmpeg", "ffprobe"] {
            if let Err(e) = audio::check_tool(tool) {
                self.set_status(e.to_string(), true);
                return;
            }
        }
        self.player.stop();
        self.busy = Some("Decoding audio...".into());
        let path = path.canonicalize().unwrap_or(path);
        self.spawn(ctx, move |tx, ctx| {
            let result = (|| -> anyhow::Result<Loaded> {
                let duration = audio::probe_duration(&path)?;
                let tmp = tempfile::tempdir()?;
                let wav = tmp.path().join("audio.wav");
                audio::decode_to_wav(&path, &wav)?;
                let samples = audio::read_wav(&wav)?;
                let env = Envelope::from_samples(&samples);
                Ok(Loaded { input: path.clone(), duration, samples: Arc::new(samples), env })
            })();
            let _ = tx.send(Msg::Loaded(result.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn on_loaded(&mut self, l: Loaded, ctx: &egui::Context) {
        self.ed = Editor::new(l.duration);
        self.samples = Some(l.samples);
        self.env = Some(Arc::new(l.env));
        self.overview = None;
        let key = l.input.display().to_string();
        self.input = Some(l.input);
        self.busy = None;
        if let Some(saved) = self.state.get(&key).cloned() {
            if !saved.out_dir.is_empty() {
                self.out_dir = saved.out_dir;
            }
            self.ed.set_marks(saved.marks);
            self.set_status(
                format!("Loaded {} saved split points. Press Auto-detect to start over.", self.ed.marks.len()),
                false,
            );
            if let Some(i) = self.ed.sel {
                self.ed.center_on(self.ed.marks[i], Some(30.0));
            }
        } else {
            self.auto_detect(ctx, false);
        }
    }

    fn auto_detect(&mut self, ctx: &egui::Context, ask: bool) {
        let (Some(input), Some(samples)) = (self.input.clone(), self.samples.clone()) else { return };
        if self.busy.is_some() {
            return;
        }
        let (Ok(sens), Ok(minlen)) = (self.sens.trim().parse::<f64>(), self.minlen.trim().parse::<f64>()) else {
            self.set_status("Sensitivity and min track length must be numbers.", true);
            return;
        };
        if ask
            && !self.ed.marks.is_empty()
            && rfd::MessageDialog::new()
                .set_title("Auto-detect")
                .set_description("Replace the current split points with auto-detected ones?")
                .set_buttons(rfd::MessageButtons::YesNo)
                .show()
                != rfd::MessageDialogResult::Yes
        {
            return;
        }
        self.busy = Some("Detecting track changes (first run computes the neural embeddings)...".into());
        self.spawn(ctx, move |tx, ctx| {
            let progress = |done: usize, total: usize| {
                let _ = tx.send(Msg::Status(format!("Computing neural embeddings {done}/{total}...")));
                ctx.request_repaint();
            };
            let result = (|| -> anyhow::Result<Vec<f64>> {
                let (emb, _) = pipeline::load_embeddings(&input, &samples, None, &progress)?;
                let mut points: Vec<f64> =
                    pipeline::find_candidates(&emb, &samples, &pipeline::DEFAULT_SCALES, sens, minlen, 3.0)
                        .iter()
                        .map(|c| c.snapped)
                        .collect();
                points.sort_by(|a, b| a.partial_cmp(b).unwrap());
                Ok(points)
            })();
            let _ = tx.send(Msg::Detected(result.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn export(&mut self, ctx: &egui::Context) {
        let Some(input) = self.input.clone() else { return };
        if self.busy.is_some() {
            return;
        }
        let out = PathBuf::from(&self.out_dir);
        let prefix = audio::default_prefix(&input);
        let ext = audio::output_extension(&input);
        let dot_ext = format!(".{}", ext.to_lowercase());
        let existing = std::fs::read_dir(&out)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.starts_with(&prefix) && n.to_lowercase().ends_with(&dot_ext)
                    })
                    .count()
            })
            .unwrap_or(0);
        if existing > 0
            && rfd::MessageDialog::new()
                .set_title("Export")
                .set_description(format!(
                    "{} already contains {existing} files named {prefix}*.{ext}.\n\
                     Files with the same names are overwritten and any extra old ones stay.\n\nContinue?",
                    out.display()
                ))
                .set_buttons(rfd::MessageButtons::YesNo)
                .show()
                != rfd::MessageDialogResult::Yes
        {
            return;
        }
        self.save_now();
        let (marks, duration) = (self.ed.marks.clone(), self.ed.duration);
        self.busy = Some("Exporting...".into());
        self.spawn(ctx, move |tx, ctx| {
            let result = audio::extract_segments(&input, &marks, duration, &out, &prefix, &mut |i, n, path, _, _| {
                let name = path.file_name().unwrap().to_string_lossy().to_string();
                let _ = tx.send(Msg::Status(format!("Writing {name} ({i}/{n})")));
                ctx.request_repaint();
            });
            let result = result.map(|_| (marks.len() + 1, out.clone())).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Msg::Exported(result));
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Status(text) => self.set_status(text, false),
                Msg::Loaded(Ok(l)) => self.on_loaded(l, ctx),
                Msg::Detected(Ok(points)) => {
                    self.busy = None;
                    self.set_status(
                        format!("Auto-detected {} split points -> {} tracks.", points.len(), points.len() + 1),
                        false,
                    );
                    self.ed.set_marks(points);
                    if let Some(i) = self.ed.sel {
                        self.ed.center_on(self.ed.marks[i], Some(30.0));
                    }
                    self.dirty = true;
                }
                Msg::Exported(Ok((n, out))) => {
                    self.busy = None;
                    self.set_status(format!("Exported {n} tracks to {}", out.display()), false);
                }
                Msg::Loaded(Err(e)) | Msg::Detected(Err(e)) | Msg::Exported(Err(e)) => {
                    self.busy = None;
                    self.set_status(format!("Error: {e}"), true);
                }
            }
        }
    }

    // ----- editing actions -----

    fn save_now(&mut self) {
        if let Some(input) = &self.input {
            self.state.insert(
                input.display().to_string(),
                Saved {
                    marks: self.ed.marks.iter().map(|m| (m * 1000.0).round() / 1000.0).collect(),
                    out_dir: self.out_dir.clone(),
                },
            );
            save_state(&self.state);
        }
        self.dirty = false;
    }

    fn nudge(&mut self, delta: f64) {
        if self.ed.nudge(delta) {
            self.dirty = true;
        } else {
            self.set_status("Select a split point first.", false);
        }
    }

    fn snap_selected(&mut self) {
        let (Some(i), Some(samples)) = (self.ed.sel, &self.samples) else { return };
        let t = detect::snap_to_energy_minimum(samples, 16000, self.ed.marks[i], 1.5);
        self.ed.move_marker(i, t);
        self.dirty = true;
    }

    fn add_marker(&mut self, t: f64) {
        if self.input.is_some() && self.ed.add_marker(t) {
            self.dirty = true;
        }
    }

    fn delete_selected(&mut self) {
        if self.ed.sel.is_some() {
            self.ed.delete_selected();
            self.dirty = true;
        }
    }

    fn play(&mut self, start: f64, duration: Option<f64>) {
        let Some(input) = self.input.clone() else { return };
        if !self.has_ffplay {
            self.set_status("ffplay not found on PATH; playback unavailable.", true);
            return;
        }
        if let Err(e) = self.player.play(&input, start, duration) {
            self.set_status(format!("Could not start ffplay: {e}"), true);
        }
    }

    fn listen_to_cut(&mut self) {
        match self.ed.sel {
            Some(i) => self.play((self.ed.marks[i] - 3.0).max(0.0), Some(6.0)),
            None => self.set_status("Select a split point first.", false),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() || self.input.is_none() {
            return;
        }
        use egui::Key;
        let (shift, ctrl) = ctx.input(|i| (i.modifiers.shift, i.modifiers.ctrl));
        let step = if shift { 1.0 } else if ctrl { 0.01 } else { 0.1 };
        let pressed = |k: Key| ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, k));
        let pressed_any = |k: Key| {
            ctx.input_mut(|i| {
                i.consume_key(egui::Modifiers::NONE, k)
                    || i.consume_key(egui::Modifiers::SHIFT, k)
                    || i.consume_key(egui::Modifiers::CTRL, k)
            })
        };
        if pressed(Key::Space) {
            if self.player.is_playing() {
                self.player.stop();
            } else {
                self.play(self.ed.playhead, None);
            }
        }
        if pressed_any(Key::ArrowLeft) {
            self.nudge(-step);
        }
        if pressed_any(Key::ArrowRight) {
            self.nudge(step);
        }
        if pressed(Key::ArrowUp) {
            self.ed.select_relative(-1);
        }
        if pressed(Key::ArrowDown) {
            self.ed.select_relative(1);
        }
        if pressed(Key::Delete) {
            self.delete_selected();
        }
        if pressed(Key::A) {
            self.add_marker(self.ed.playhead);
        }
        if pressed(Key::P) {
            self.listen_to_cut();
        }
    }

    // ----- drawing -----

    fn toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if ui.button("Open...").clicked() {
                self.open_dialog(ctx);
            }
            ui.separator();
            if ui.button("Auto-detect").clicked() {
                self.auto_detect(ctx, true);
            }
            ui.label("sensitivity");
            ui.add(egui::TextEdit::singleline(&mut self.sens).desired_width(40.0));
            ui.label("min track (s)");
            ui.add(egui::TextEdit::singleline(&mut self.minlen).desired_width(40.0));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("About").clicked() {
                    if !self.about_open {
                        self.about_serial += 1; // a fresh window, centred again
                    }
                    self.about_open = true;
                }
                ui.separator();
                if ui.button("Export tracks").clicked() {
                    self.export(ctx);
                }
                if ui.button("Browse...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().set_title("Output folder").pick_folder() {
                        self.out_dir = dir.display().to_string();
                        self.dirty = true;
                    }
                }
                ui.add(egui::TextEdit::singleline(&mut self.out_dir).desired_width(320.0));
                ui.label("Output folder");
            });
        });
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.about_open = false;
            return;
        }
        // Scale the whole dialog (title bar included, which egui draws with the
        // app-wide heading font) by swapping in a bigger style while it is shown.
        let normal = ctx.global_style();
        let mut big = (*normal).clone();
        for font in big.text_styles.values_mut() {
            font.size *= ABOUT_SCALE;
        }
        big.spacing.item_spacing *= ABOUT_SCALE;
        big.spacing.button_padding *= ABOUT_SCALE;
        big.spacing.interact_size *= ABOUT_SCALE;
        big.spacing.icon_width *= ABOUT_SCALE;
        big.spacing.icon_width_inner *= ABOUT_SCALE;
        big.spacing.window_margin = egui::Margin::same((8.0 * ABOUT_SCALE) as i8);
        ctx.set_global_style(big);

        // Centre on the waveform (the whole window until it has been drawn once).
        let center = if self.signal_rect.width() > 0.0 { self.signal_rect.center() } else { ctx.content_rect().center() };
        centered_dialog(format!("About {APP_NAME}"), self.about_serial, center)
            .open(&mut self.about_open)
            .show(ctx, |ui| {
                // Wide enough for the title bar and the text without wrapping.
                ui.set_min_width(300.0 * ABOUT_SCALE);
                ui.vertical_centered(|ui| {
                    ui.heading(APP_NAME);
                    ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                    ui.add_space(6.0);
                    ui.label("Splits long radio recordings into separate tracks.");
                    ui.add_space(8.0);
                    for (label, url) in [("Project page", PROJECT_URL), ("Author support page", SUPPORT_URL)] {
                        if !url.is_empty() && ui.link(label).on_hover_text(url).clicked() {
                            open_url(url);
                        }
                    }
                });
            });
        ctx.set_global_style(normal);
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal_top(|ui| {
            // split list
            ui.vertical(|ui| {
                ui.set_width(430.0);
                ui.monospace(" #    Split time     Track before   Track after");
                egui::ScrollArea::vertical().max_height(150.0).auto_shrink([false, false]).show(ui, |ui| {
                    let bounds = self.ed.bounds();
                    for i in 0..self.ed.marks.len() {
                        let text = format!(
                            "{:>2}    {:<13}  {:<13}  {}",
                            i + 1,
                            fmt_time(self.ed.marks[i]),
                            fmt_dur(bounds[i + 1] - bounds[i]),
                            fmt_dur(bounds[i + 2] - bounds[i + 1])
                        );
                        let resp = ui.selectable_label(self.ed.sel == Some(i), egui::RichText::new(text).monospace());
                        if resp.clicked() {
                            self.ed.sel = Some(i);
                            self.ed.center_on(self.ed.marks[i], None);
                        }
                        if self.ed.sel == Some(i) && self.drag.is_some() {
                            resp.scroll_to_me(None);
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label("Selected split");
                    if !self.time_focus {
                        self.time_text = self.ed.sel.map(|i| fmt_time(self.ed.marks[i])).unwrap_or_default();
                    }
                    let resp = ui.add(egui::TextEdit::singleline(&mut self.time_text).desired_width(90.0));
                    self.time_focus = resp.has_focus();
                    let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Set").clicked() || enter {
                        match (parse_time(&self.time_text), self.ed.sel) {
                            (Some(t), Some(i)) => {
                                self.ed.move_marker(i, t);
                                self.dirty = true;
                            }
                            (None, _) => self.set_status("Could not parse time. Use m:ss.mmm or seconds.", true),
                            _ => {}
                        }
                        self.time_focus = false;
                    }
                    ui.weak("(m:ss.mmm or seconds)");
                });
                ui.horizontal(|ui| {
                    ui.label("Nudge");
                    for (label, d) in [("-1s", -1.0), ("-0.1", -0.1), ("-0.01", -0.01), ("+0.01", 0.01), ("+0.1", 0.1), ("+1s", 1.0)] {
                        if ui.button(label).clicked() {
                            self.nudge(d);
                        }
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Snap to quiet").clicked() {
                        self.snap_selected();
                    }
                    if ui.button("Add at playhead").clicked() {
                        self.add_marker(self.ed.playhead);
                    }
                    if ui.button("Delete").clicked() {
                        self.delete_selected();
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Play from playhead").clicked() {
                        self.play(self.ed.playhead, None);
                    }
                    if ui.button("Listen to cut (+-3s)").clicked() {
                        self.listen_to_cut();
                    }
                    if ui.button("Stop").clicked() {
                        self.player.stop();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Zoom");
                    if ui.button("-").clicked() {
                        self.ed.zoom(1.6, None);
                    }
                    if ui.button("+").clicked() {
                        self.ed.zoom(1.0 / 1.6, None);
                    }
                    if ui.button("Whole file").clicked() {
                        self.ed.zoom_all();
                    }
                    if ui.button("Around split (10s)").clicked() {
                        if let Some(i) = self.ed.sel {
                            self.ed.center_on(self.ed.marks[i], Some(10.0));
                        }
                    }
                });
                ui.add_space(2.0);
                ui.weak("space play/stop | arrows nudge (shift 1s, ctrl 0.01s) | up/down select | del | A add | P listen");
            });
        });
        let _ = ctx;
    }

    fn waveform(&mut self, ui: &mut egui::Ui, size: Vec2) {
        let (resp, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let rect = resp.rect;
        self.signal_rect = rect;
        painter.rect_filled(rect, 0.0, BG);
        let Some(env) = self.env.clone() else {
            painter.text(rect.center(), Align2::CENTER_CENTER, "Open a recording to begin", FontId::proportional(16.0), TEXT);
            return;
        };
        let width = rect.width() as f64;
        let x_of = |ed: &Editor, t: f64| rect.left() + ((t - ed.view_start) / ed.view_span * width) as f32;
        let t_of = |ed: &Editor, x: f32| ed.view_start + (x - rect.left()) as f64 / width * ed.view_span;

        // --- interaction ---
        let press = ui.input(|i| i.pointer.press_origin());
        if resp.drag_started() {
            self.drag = press.and_then(|p| self.hit_marker(p.x, |t| x_of(&self.ed, t), HIT_TOLERANCE));
            if let Some(i) = self.drag {
                self.ed.sel = Some(i);
            }
        }
        if resp.dragged() {
            match (self.drag, resp.interact_pointer_pos()) {
                (Some(i), Some(p)) => {
                    let t = t_of(&self.ed, p.x);
                    self.ed.move_marker(i, t);
                    self.dirty = true;
                }
                (None, _) => self.ed.pan(-(resp.drag_delta().x as f64) / width * self.ed.view_span),
                _ => {}
            }
        }
        if resp.drag_stopped() {
            self.drag = None;
        }
        if let Some(p) = resp.interact_pointer_pos() {
            let over = self.hit_marker(p.x, |t| x_of(&self.ed, t), HIT_TOLERANCE);
            if resp.clicked() {
                match over {
                    Some(i) => self.ed.sel = Some(i),
                    None => self.ed.playhead = t_of(&self.ed, p.x).clamp(0.0, self.ed.duration),
                }
            }
            if resp.double_clicked() && over.is_none() {
                let t = t_of(&self.ed, p.x);
                self.add_marker(t);
            }
            if resp.secondary_clicked() {
                if let Some(i) = self.hit_marker(p.x, |t| x_of(&self.ed, t), 10.0) {
                    self.ed.delete(i);
                    self.dirty = true;
                }
            }
        }
        if resp.hovered() {
            let (scroll, shift) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.shift));
            if let Some(p) = resp.hover_pos() {
                if shift {
                    let amount = (scroll.x + scroll.y) as f64;
                    self.ed.pan(-amount / 50.0 * 0.15 * self.ed.view_span);
                } else if scroll.y != 0.0 {
                    let factor = 0.8f64.powf(scroll.y as f64 / 50.0);
                    let anchor = t_of(&self.ed, p.x);
                    self.ed.zoom(factor, Some(anchor));
                }
            }
        }

        // --- drawing ---
        let ed = &self.ed;
        let (t0, t1) = (ed.view_start, ed.view_start + ed.view_span);
        let bounds = ed.bounds();
        for k in 0..bounds.len() - 1 {
            let (a, b) = (bounds[k], bounds[k + 1]);
            if b < t0 || a > t1 {
                continue;
            }
            let x0 = x_of(ed, a).max(rect.left());
            let x1 = x_of(ed, b).min(rect.right());
            let strip = Rect::from_min_max(Pos2::new(x0, rect.top() + RULER_H), Pos2::new(x1, rect.bottom()));
            painter.rect_filled(strip, 0.0, if k % 2 == 1 { BG_ALT } else { BG });
            if x1 - x0 > 90.0 {
                painter.text(
                    Pos2::new(x0 + 6.0, rect.bottom() - 4.0),
                    Align2::LEFT_BOTTOM,
                    format!("Track {}  ({})", k + 1, fmt_dur(b - a)),
                    FontId::proportional(12.0),
                    TEXT,
                );
            }
        }

        let area_h = rect.height() - RULER_H;
        let mid = rect.top() + RULER_H + area_h / 2.0;
        let half = area_h / 2.0 - 2.0;
        let gain = half / env.gain_ref;
        let columns = env.columns(t0, t1, rect.width().max(1.0) as usize);
        let wave_shapes: Vec<egui::Shape> = columns
            .iter()
            .enumerate()
            .map(|(k, &(lo, hi))| {
                let x = rect.left() + k as f32 + 0.5;
                let top = mid - (hi * gain).min(half);
                let bottom = mid - (lo * gain).max(-half);
                egui::Shape::line_segment([Pos2::new(x, top), Pos2::new(x, bottom.max(top + 1.0))], Stroke::new(1.0, WAVE))
            })
            .collect();
        painter.extend(wave_shapes);

        draw_ruler(&painter, rect, ed, |t| x_of(ed, t));

        for (i, &m) in ed.marks.iter().enumerate() {
            if m < t0 || m > t1 {
                continue;
            }
            let x = x_of(ed, m);
            let selected = ed.sel == Some(i);
            let color = if selected { MARK_SEL } else { MARK };
            painter.line_segment(
                [Pos2::new(x, rect.top() + RULER_H), Pos2::new(x, rect.bottom())],
                Stroke::new(if selected { 3.0 } else { 2.0 }, color),
            );
            let tag = Rect::from_center_size(Pos2::new(x, rect.top() + RULER_H + 8.0), Vec2::new(22.0, 16.0));
            painter.rect_filled(tag, 2.0, color);
            painter.text(tag.center(), Align2::CENTER_CENTER, (i + 1).to_string(), FontId::proportional(12.0), Color32::BLACK);
        }

        if ed.playhead >= t0 && ed.playhead <= t1 {
            let x = x_of(ed, ed.playhead);
            painter.line_segment(
                [Pos2::new(x, rect.top() + RULER_H), Pos2::new(x, rect.bottom())],
                Stroke::new(2.0, HEAD),
            );
        }
    }

    fn hit_marker(&self, px: f32, x_of: impl Fn(f64) -> f32, tolerance: f32) -> Option<usize> {
        self.ed
            .marks
            .iter()
            .enumerate()
            .map(|(i, &m)| (i, (x_of(m) - px).abs()))
            .filter(|&(_, d)| d <= tolerance)
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap())
            .map(|(i, _)| i)
    }

    fn overview_bar(&mut self, ui: &mut egui::Ui, width: f32) {
        let (resp, painter) = ui.allocate_painter(Vec2::new(width, OVERVIEW_H), Sense::click_and_drag());
        let rect = resp.rect;
        painter.rect_filled(rect, 0.0, BG);
        let Some(env) = self.env.clone() else { return };
        let duration = self.ed.duration;
        if duration <= 0.0 {
            return;
        }
        if resp.clicked() || resp.dragged() {
            if let Some(p) = resp.interact_pointer_pos() {
                let t = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64 * duration;
                self.ed.center_on(t, None);
            }
        }

        let w = rect.width().max(1.0) as usize;
        if self.overview.as_ref().map(|(cw, _)| *cw) != Some(w) {
            self.overview = Some((w, env.columns(0.0, duration, w)));
        }
        let columns = &self.overview.as_ref().unwrap().1;
        let mid = rect.center().y;
        let half = rect.height() / 2.0 - 3.0;
        let gain = half / env.gain_ref;
        let shapes: Vec<egui::Shape> = columns
            .iter()
            .enumerate()
            .map(|(k, &(lo, hi))| {
                let x = rect.left() + k as f32 + 0.5;
                let top = mid - (hi * gain).min(half);
                let bottom = mid - (lo * gain).max(-half);
                egui::Shape::line_segment([Pos2::new(x, top), Pos2::new(x, bottom.max(top + 1.0))], Stroke::new(1.0, WAVE_DIM))
            })
            .collect();
        painter.extend(shapes);

        let x_of = |t: f64| rect.left() + (t / duration) as f32 * rect.width();
        for (i, &m) in self.ed.marks.iter().enumerate() {
            let x = x_of(m);
            painter.line_segment(
                [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                Stroke::new(2.0, if self.ed.sel == Some(i) { MARK_SEL } else { MARK }),
            );
        }
        let view = Rect::from_min_max(
            Pos2::new(x_of(self.ed.view_start), rect.top() + 1.0),
            Pos2::new(x_of(self.ed.view_start + self.ed.view_span).max(x_of(self.ed.view_start) + 3.0), rect.bottom() - 1.0),
        );
        painter.rect_stroke(view, 0.0, Stroke::new(2.0, Color32::WHITE), egui::StrokeKind::Inside);
        let x = x_of(self.ed.playhead);
        painter.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())], Stroke::new(2.0, HEAD));
    }
}

fn draw_ruler(painter: &egui::Painter, rect: Rect, ed: &Editor, x_of: impl Fn(f64) -> f32) {
    let px_per_s = rect.width() as f64 / ed.view_span;
    const STEPS: [f64; 18] = [
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0,
    ];
    let step = STEPS.iter().copied().find(|s| s * px_per_s >= 90.0).unwrap_or(1800.0);
    let mut t = (ed.view_start / step).ceil() * step;
    while t <= ed.view_start + ed.view_span {
        let x = x_of(t);
        painter.line_segment([Pos2::new(x, rect.top() + RULER_H - 6.0), Pos2::new(x, rect.top() + RULER_H)], Stroke::new(1.0, TEXT));
        let label = if step < 1.0 { fmt_time(t) } else { fmt_dur(t) };
        painter.text(Pos2::new(x + 3.0, rect.top() + 2.0), Align2::LEFT_TOP, label, FontId::proportional(12.0), TEXT);
        t += step;
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll(ctx);
        self.handle_keys(ctx);

        if self.player.is_playing() {
            self.ed.playhead = self.player.position().clamp(0.0, self.ed.duration);
            let (t0, t1) = (self.ed.view_start, self.ed.view_start + self.ed.view_span);
            if self.ed.playhead < t0 || self.ed.playhead > t1 {
                self.ed.center_on(self.ed.playhead, None);
            }
            ctx.request_repaint();
        }
        if self.busy.is_some() {
            ctx.request_repaint();
        }
    }

    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        let ctx = &ctx;
        if let Some(title) = title_update(&mut self.title, self.input.as_deref()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));
        }
        egui::Panel::top("toolbar").show(root, |ui| self.toolbar(ui, ctx));
        egui::Panel::bottom("status").show(root, |ui| {
            ui.horizontal(|ui| {
                if let Some(msg) = &self.busy {
                    ui.spinner();
                    let text = if self.status.is_empty() { msg.clone() } else { self.status.clone() };
                    ui.label(text);
                } else {
                    let color = if self.status_is_error { Color32::from_rgb(0xff, 0x6b, 0x6b) } else { ui.visuals().text_color() };
                    ui.colored_label(color, if self.input.is_some() && !self.status_is_error {
                        format!("{}   |   {} split points -> {} tracks", self.status, self.ed.marks.len(), self.ed.marks.len() + 1)
                    } else {
                        self.status.clone()
                    });
                }
            });
        });
        egui::Panel::bottom("controls").resizable(false).exact_size(190.0).show(root, |ui| {
            ui.add_space(4.0);
            self.controls(ui, ctx);
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(root, |ui| {
            let avail = ui.available_size();
            self.waveform(ui, Vec2::new(avail.x, (avail.y - OVERVIEW_H - 6.0).max(80.0)));
            ui.add_space(4.0);
            self.overview_bar(ui, avail.x);
        });
        self.about_window(ctx);

        if self.dirty && self.drag.is_none() {
            self.save_now();
        }
    }
}

fn main() -> eframe::Result<()> {
    let initial = std::env::args_os().nth(1).map(PathBuf::from);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1280.0, 780.0]).with_title(APP_NAME),
        ..Default::default()
    };
    eframe::run_native(APP_NAME, options, Box::new(move |cc| Ok(Box::new(App::new(initial, &cc.egui_ctx)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One real egui frame with the app's dialog builder; returns the window's rectangle.
    fn frame(ctx: &egui::Context, center: Pos2, serial: u32, time: f64, events: Vec<egui::Event>) -> Rect {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
            time: Some(time),
            events,
            ..Default::default()
        };
        let mut rect = Rect::NOTHING;
        let output = ctx.run_ui(input, |ui| {
            let shown = centered_dialog("About", serial, center).show(ui.ctx(), |ui| {
                ui.set_min_width(300.0);
                ui.label("Radio Track Splitter");
            });
            if let Some(r) = shown {
                rect = r.response.rect;
            }
        });
        // Nothing renders here; egui insists the frame's texture uploads are dropped explicitly.
        output.drop_without_applying_deltas();
        rect
    }

    /// Frames with no input (the first one only measures the window).
    fn settle(ctx: &egui::Context, center: Pos2, serial: u32) -> Rect {
        let mut rect = Rect::NOTHING;
        for i in 0..3 {
            rect = frame(ctx, center, serial, i as f64 * 0.02, vec![]);
        }
        rect
    }

    fn assert_centered_on(rect: Rect, center: Pos2) {
        let off = (rect.center() - center).abs();
        assert!(off.x < 1.5 && off.y < 1.5, "window centre {:?}, wanted {center:?}", rect.center());
    }

    #[test]
    fn dialog_opens_centered_on_the_requested_point() {
        let ctx = egui::Context::default();
        let center = Pos2::new(640.0, 300.0);
        assert_centered_on(settle(&ctx, center, 1), center);
    }

    #[test]
    fn dialog_can_be_dragged_by_its_title_bar_and_then_stays_put() {
        use egui::{Event, Modifiers, PointerButton};
        let ctx = egui::Context::default();
        let center = Pos2::new(640.0, 300.0);
        let rect = settle(&ctx, center, 1);

        // Grab the title bar (near the top of the window) and pull the window by (+80, +50).
        let grab = Pos2::new(rect.center().x, rect.top() + 12.0);
        let by = Vec2::new(80.0, 50.0);
        let button = |pressed| Event::PointerButton { pos: grab, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        let mut t = 1.0;
        let mut step = |events: Vec<Event>| {
            t += 0.02;
            frame(&ctx, center, 1, t, events)
        };
        step(vec![Event::PointerMoved(grab)]);
        step(vec![button(true)]);
        step(vec![Event::PointerMoved(grab + by * 0.5)]);
        step(vec![Event::PointerMoved(grab + by)]);
        step(vec![Event::PointerButton { pos: grab + by, button: PointerButton::Primary, pressed: false, modifiers: Modifiers::NONE }]);
        let moved = step(vec![]);

        let shift = moved.center() - rect.center();
        assert!((shift - by).length() < 2.0, "dragged window moved by {shift:?}, expected {by:?}");

        // Later frames (the signal pane moving, the app redrawing) must not pull it back.
        let later = frame(&ctx, Pos2::new(300.0, 200.0), 1, 2.0, vec![]);
        assert_centered_on(later, moved.center());
    }

    #[test]
    fn a_new_opening_is_centered_on_the_current_point() {
        let ctx = egui::Context::default();
        let first = Pos2::new(640.0, 300.0);
        assert_centered_on(settle(&ctx, first, 1), first);

        // Opened again (new serial) over a signal pane that is now elsewhere.
        let second = Pos2::new(400.0, 200.0);
        assert_centered_on(settle(&ctx, second, 2), second);
    }
}

#[cfg(test)]
mod title_tests {
    use super::*;

    #[test]
    fn title_is_the_program_name_and_the_file_name_without_its_folder() {
        assert_eq!(window_title(None), "Radio Track Splitter");
        assert_eq!(
            window_title(Some(Path::new(r"C:\Users\anton\Music\Morning Show 2026.mp3"))),
            "Radio Track Splitter - Morning Show 2026.mp3"
        );
        // The extension stays; only the folder is dropped.
        assert_eq!(window_title(Some(Path::new("clip.m4a"))), "Radio Track Splitter - clip.m4a");
        // A path with no file name (e.g. a drive root) falls back to the plain name.
        assert_eq!(window_title(Some(Path::new(r"C:\"))), "Radio Track Splitter");
    }

    #[test]
    fn the_title_is_only_sent_when_it_changes() {
        let mut current = APP_NAME.to_string();
        assert_eq!(title_update(&mut current, None), None, "nothing open: already correct");

        let a = Path::new(r"D:\rec\a.mp3");
        assert_eq!(title_update(&mut current, Some(a)).as_deref(), Some("Radio Track Splitter - a.mp3"));
        assert_eq!(title_update(&mut current, Some(a)), None, "same file: no repeat");

        let b = Path::new(r"D:\rec\b.flac");
        assert_eq!(title_update(&mut current, Some(b)).as_deref(), Some("Radio Track Splitter - b.flac"));
        assert_eq!(current, "Radio Track Splitter - b.flac");
    }
}
