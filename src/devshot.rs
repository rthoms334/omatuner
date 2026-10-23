//! Developer-only scripting and capture for the README's demo video (DESIGN §23.1). Not part of
//! the product: no CLI flag, nothing in `--help`; `scripts/demo-video.sh` drives it.
//!
//! ```text
//! TUNER_SCREENSHOT=out.png            capture the window to this PNG, then exit
//! TUNER_SCREENSHOT_SIZE=1600x1000     float + resize + centre our window first (Hyprland IPC)
//! TUNER_SCREENSHOT_SCRIPT="view:movies;key:Down;key:Space;wait:1"
//! TUNER_SCREENSHOT_AFTER=2            seconds after the last step (default 2)
//! TUNER_SCREENSHOT_MARKS=marks.txt    where `mark:` steps append "<label> <unix µs>" lines
//! ```
//! The script starts once the first library snapshot is in and the window has the requested
//! size; steps run [`STEP_GAP`] apart. Steps: `wait:<secs>`, `key:<Ctrl+Shift+Name>` (egui key
//! names: `Down`, `Enter`, `Space`, `Escape`, `F1`, `A`…; a plain printable key also types its
//! character, as a keyboard does), `text:<chars>`, `view:<home|live|
//! guide|movies|series|settings|search>`, `play:<source>:<item-key>` (both as the CLI does),
//! `move:<x>,<y>` / `click:<x>,<y>` (logical px), `size:<w>x<h>` and `categories:<source id>`
//! ("Choose what to show" of that playlist, which only a context menu opens) and `mark:<label>`
//! (appends the label and the wall-clock time to `TUNER_SCREENSHOT_MARKS`: `scripts/demo-video.sh`
//! cuts its recordings there). A `;` inside `text:` is written `\;`.
//!
//! The PNG is scaled to the window's logical size, so a capture looks the same on any monitor
//! scale. Captures are only stable with a fixed theme: see `theme::THEME_DIR_ENV`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

/// Pause between two script steps (lets a screen react to the previous key).
pub const STEP_GAP: Duration = Duration::from_millis(350);
/// After the library and the window size are ready, before the first step.
const SETTLE: Duration = Duration::from_millis(600);
/// Give up waiting for the requested window size after this long (capture anyway).
const SIZE_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Wait(Duration),
    Key(egui::Key, egui::Modifiers),
    Text(String),
    View(String),
    Play(String),
    Move(egui::Pos2),
    Click(egui::Pos2),
    Size(i32, i32),
    Categories(i64),
    Mark(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShotConfig {
    pub path: PathBuf,
    pub size: Option<(i32, i32)>,
    pub after: Duration,
    pub steps: Vec<Step>,
    pub marks: Option<PathBuf>,
}

impl ShotConfig {
    /// From the process environment; `Ok(None)` when `TUNER_SCREENSHOT` is unset.
    pub fn from_env() -> Result<Option<Self>, String> {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>, String> {
        let Some(path) = get("TUNER_SCREENSHOT").filter(|p| !p.trim().is_empty()) else { return Ok(None) };
        let size = get("TUNER_SCREENSHOT_SIZE").filter(|s| !s.trim().is_empty()).map(|s| parse_size(&s)).transpose()?;
        let after = match get("TUNER_SCREENSHOT_AFTER").filter(|s| !s.trim().is_empty()) {
            Some(s) => secs(&s)?,
            None => Duration::from_secs(2),
        };
        let steps = parse_script(&get("TUNER_SCREENSHOT_SCRIPT").unwrap_or_default())?;
        let marks = get("TUNER_SCREENSHOT_MARKS").filter(|m| !m.trim().is_empty()).map(PathBuf::from);
        Ok(Some(Self { path: PathBuf::from(path), size, after, steps, marks }))
    }
}

fn secs(s: &str) -> Result<Duration, String> {
    s.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v >= 0.0).map(Duration::from_secs_f32).ok_or_else(|| format!("bad seconds '{s}'"))
}

fn parse_size(s: &str) -> Result<(i32, i32), String> {
    let (w, h) = s.trim().split_once('x').ok_or_else(|| format!("bad size '{s}' (want WxH)"))?;
    match (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
        (Ok(w), Ok(h)) if w >= 200 && h >= 150 => Ok((w, h)),
        _ => Err(format!("bad size '{s}' (want WxH, at least 200x150)")),
    }
}

fn parse_pos(s: &str) -> Result<egui::Pos2, String> {
    let (x, y) = s.split_once(',').ok_or_else(|| format!("bad position '{s}' (want x,y)"))?;
    match (x.trim().parse::<f32>(), y.trim().parse::<f32>()) {
        (Ok(x), Ok(y)) => Ok(egui::pos2(x, y)),
        _ => Err(format!("bad position '{s}' (want x,y)")),
    }
}

fn parse_key(s: &str) -> Result<(egui::Key, egui::Modifiers), String> {
    let mut mods = egui::Modifiers::NONE;
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    // a lone "+" key ("key:+" or "key:Ctrl++")
    let (name, prefix) = match parts.as_slice() {
        [.., "", ""] => ("+", &parts[..parts.len() - 2]),
        [prefix @ .., last] => (*last, prefix),
        [] => return Err("empty key".into()),
    };
    for m in prefix {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => {
                mods.ctrl = true;
                mods.command = true;
            }
            "shift" => mods.shift = true,
            "alt" => mods.alt = true,
            other => return Err(format!("unknown modifier '{other}' in key:{s}")),
        }
    }
    let title: String = name.chars().take(1).flat_map(char::to_uppercase).chain(name.chars().skip(1).flat_map(char::to_lowercase)).collect();
    let key = egui::Key::from_name(name).or_else(|| egui::Key::from_name(&title)).ok_or_else(|| format!("unknown key '{name}'"))?;
    Ok((key, mods))
}

/// The character a plain (or Shift+) printable key types along with its `Key` event, as
/// egui-winit reports it (`Text`): the app binds Space and letters to either event.
fn typed(key: egui::Key, mods: egui::Modifiers) -> Option<char> {
    if mods.ctrl || mods.alt || mods.command {
        return None;
    }
    let name = key.name();
    let mut chars = name.chars();
    match (key, chars.next(), chars.next()) {
        (egui::Key::Space, ..) => Some(' '),
        (_, Some(c), None) if c.is_ascii_alphabetic() || (c.is_ascii_digit() && !mods.shift) => {
            Some(if mods.shift { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() })
        }
        _ => None,
    }
}

/// `step;step;…` (see the module docs).
pub fn parse_script(s: &str) -> Result<Vec<Step>, String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&';') => cur.push(chars.next().unwrap_or(';')),
            ';' => parts.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    let mut steps = Vec::new();
    for part in parts {
        let part = part.trim_start();
        if part.trim().is_empty() {
            continue;
        }
        let (verb, arg) = part.split_once(':').ok_or_else(|| format!("bad step '{part}' (want verb:argument)"))?;
        let step = match verb.trim() {
            "wait" => Step::Wait(secs(arg)?),
            "key" => {
                let (k, m) = parse_key(arg)?;
                Step::Key(k, m)
            }
            "text" => Step::Text(arg.to_owned()),
            "view" => Step::View(arg.trim().to_owned()),
            "play" => Step::Play(arg.trim().to_owned()),
            "move" => Step::Move(parse_pos(arg)?),
            "click" => Step::Click(parse_pos(arg)?),
            "size" => {
                let (w, h) = parse_size(arg)?;
                Step::Size(w, h)
            }
            "categories" => Step::Categories(arg.trim().parse().map_err(|_| format!("bad playlist id '{arg}'"))?),
            "mark" => Step::Mark(arg.trim().to_owned()),
            other => return Err(format!("unknown step '{other}'")),
        };
        steps.push(step);
    }
    Ok(steps)
}

/// What the app does for the driver this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Run as a CLI / IPC request would (`view:` / `play:`).
    View(String),
    Play(String),
    /// Float + resize + centre our window (Hyprland).
    Size(i32, i32),
    /// Open "Choose what to show" of this playlist.
    Categories(i64),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Waiting for the library and the window size.
    Settling {
        since: Option<Instant>,
    },
    Running {
        next: usize,
        at: Instant,
    },
    /// Last step done: capture at this instant.
    Capture {
        at: Instant,
    },
    Requested,
    Done,
}

/// The capture driver (one per process, owned by the app).
pub struct Driver {
    cfg: ShotConfig,
    phase: Phase,
    started: Instant,
    sized: bool,
    /// Viewport commands for [`Driver::frame`]: sent from `raw_input_hook` (before the pass
    /// begins) egui would drop them.
    viewport: Vec<egui::ViewportCommand>,
}

impl Driver {
    pub fn new(cfg: ShotConfig) -> Self {
        Self { cfg, phase: Phase::Settling { since: None }, started: Instant::now(), sized: false, viewport: Vec::new() }
    }

    /// The window size to ask for at the first frame.
    pub fn initial_size(&mut self) -> Option<(i32, i32)> {
        if std::mem::replace(&mut self.sized, true) { None } else { self.cfg.size }
    }

    /// Called first in `raw_input_hook`: the window counts as focused while the script runs.
    /// `scripts/demo-video.sh` records on an output the user doesn't look at, where the window
    /// never gets the keyboard; egui would otherwise ignore typing into a text field
    /// (`Response::has_focus` wants a focused window) and the app would act as if out of sight.
    pub fn hold_focus(&self, raw: &mut egui::RawInput) {
        if self.phase == Phase::Done {
            return;
        }
        raw.focused = true;
        raw.events.retain(|e| !matches!(e, egui::Event::WindowFocused(false)));
        if let Some(v) = raw.viewports.get_mut(&raw.viewport_id) {
            v.focused = Some(true);
        }
    }

    /// Called from `raw_input_hook` before key resolution: injects the script's input into
    /// `raw`, saves a delivered screenshot (then closes the window) and returns the app-level
    /// commands that are due. `ready` = the first library snapshot is in; `window` = the logical
    /// window size.
    pub fn hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput, ready: bool, window: egui::Vec2) -> Vec<Command> {
        let now = Instant::now();
        let mut out = Vec::new();
        if let Some(image) = raw.events.iter().find_map(|e| match e {
            egui::Event::Screenshot { image, .. } => Some(image.clone()),
            _ => None,
        }) {
            raw.events.retain(|e| !matches!(e, egui::Event::Screenshot { .. }));
            if self.phase == Phase::Requested {
                let ppp = ctx.pixels_per_point();
                match save_png(&image, ppp, &self.cfg.path) {
                    Ok((w, h)) => tracing::info!("screenshot: {} ({w}x{h})", self.cfg.path.display()),
                    Err(e) => tracing::error!("screenshot {}: {e}", self.cfg.path.display()),
                }
                self.phase = Phase::Done;
                self.viewport.push(egui::ViewportCommand::Close);
                return out;
            }
        }
        let before = self.phase;
        match self.phase {
            Phase::Settling { since } => {
                let sized = match self.cfg.size {
                    Some((w, h)) => (window.x - w as f32).abs() <= 1.5 && (window.y - h as f32).abs() <= 1.5,
                    None => true,
                };
                let sized = sized || now.duration_since(self.started) > SIZE_WAIT;
                self.phase = match (ready && sized, since) {
                    (false, _) => Phase::Settling { since: None },
                    (true, Some(s)) if now.duration_since(s) >= SETTLE => Phase::Running { next: 0, at: now },
                    (true, s) => Phase::Settling { since: Some(s.unwrap_or(now)) },
                };
            }
            Phase::Running { next, at } if now >= at => match self.cfg.steps.get(next) {
                None => self.phase = Phase::Capture { at: now + self.cfg.after },
                Some(step) => {
                    let mut wait = STEP_GAP;
                    match step {
                        Step::Wait(d) => wait = *d,
                        Step::Key(key, modifiers) => {
                            let ev = |pressed| egui::Event::Key { key: *key, physical_key: None, pressed, repeat: false, modifiers: *modifiers };
                            raw.events.push(ev(true));
                            // like a keyboard: a printable key also types its character
                            raw.events.extend(typed(*key, *modifiers).map(|c| egui::Event::Text(c.to_string())));
                            raw.events.push(ev(false));
                        }
                        Step::Text(t) => raw.events.extend(t.chars().map(|c| egui::Event::Text(c.to_string()))),
                        Step::View(v) => out.push(Command::View(v.clone())),
                        Step::Play(p) => out.push(Command::Play(p.clone())),
                        Step::Move(p) => raw.events.push(egui::Event::PointerMoved(*p)),
                        Step::Click(p) => {
                            raw.events.push(egui::Event::PointerMoved(*p));
                            for pressed in [true, false] {
                                raw.events.push(egui::Event::PointerButton {
                                    pos: *p,
                                    button: egui::PointerButton::Primary,
                                    pressed,
                                    modifiers: egui::Modifiers::NONE,
                                });
                            }
                        }
                        Step::Size(w, h) => out.push(Command::Size(*w, *h)),
                        Step::Categories(id) => out.push(Command::Categories(*id)),
                        Step::Mark(label) => {
                            wait = Duration::ZERO;
                            self.mark(label);
                        }
                    }
                    self.phase = Phase::Running { next: next + 1, at: now + wait };
                }
            },
            Phase::Capture { at } if now >= at => {
                self.viewport.push(egui::ViewportCommand::Screenshot(egui::UserData::default()));
                self.phase = Phase::Requested;
            }
            _ => {}
        }
        if std::mem::discriminant(&before) != std::mem::discriminant(&self.phase) {
            tracing::debug!("screenshot: {:?} (window {window:?}, ready {ready})", self.phase);
        }
        out
    }

    /// `mark:` — append "<label> <unix µs>" to the marks file (no file: the log only).
    fn mark(&self, label: &str) {
        let us = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_micros());
        tracing::info!("screenshot mark: {label} {us}");
        let Some(path) = &self.cfg.marks else { return };
        let line = format!("{label} {us}\n");
        let written =
            std::fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
        if let Err(e) = written {
            tracing::error!("screenshot marks {}: {e}", path.display());
        }
    }

    /// Called during the frame's pass (`ui`): sends the pending viewport commands and keeps
    /// frames coming while the script runs.
    pub fn frame(&mut self, ctx: &egui::Context) {
        for cmd in self.viewport.drain(..) {
            ctx.send_viewport_cmd(cmd);
        }
        if self.phase != Phase::Done {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
    }
}

/// Write `image` (physical px) as a PNG at its logical size (`/ ppp`). Returns the size written.
fn save_png(image: &egui::ColorImage, ppp: f32, path: &std::path::Path) -> Result<(u32, u32), String> {
    let [w, h] = image.size;
    let mut rgba = Vec::with_capacity(w * h * 4);
    for c in &image.pixels {
        rgba.extend_from_slice(&[c.r(), c.g(), c.b(), 255]);
    }
    let mut img = image::RgbaImage::from_raw(w as u32, h as u32, rgba).ok_or("bad screenshot buffer")?;
    if ppp > 0.0 && (ppp - 1.0).abs() > 0.01 {
        let (lw, lh) = (((w as f32) / ppp).round() as u32, ((h as f32) / ppp).round() as u32);
        img = image::imageops::resize(&img, lw.max(1), lh.max(1), image::imageops::FilterType::Lanczos3);
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // opaque RGB at the best compression: these go into git
    let rgb = image::DynamicImage::ImageRgba8(img).into_rgb8();
    let file = std::io::BufWriter::new(std::fs::File::create(path).map_err(|e| e.to_string())?);
    let png =
        image::codecs::png::PngEncoder::new_with_quality(file, image::codecs::png::CompressionType::Best, image::codecs::png::FilterType::Adaptive);
    rgb.write_with_encoder(png).map_err(|e| e.to_string())?;
    Ok(rgb.dimensions())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn env_turns_capture_on_with_defaults() {
        assert_eq!(ShotConfig::from_vars(vars(&[])), Ok(None));
        assert_eq!(ShotConfig::from_vars(vars(&[("TUNER_SCREENSHOT", " ")])), Ok(None));
        let cfg = ShotConfig::from_vars(vars(&[("TUNER_SCREENSHOT", "/tmp/a.png"), ("TUNER_SCREENSHOT_SIZE", "1600x1000")])).unwrap().unwrap();
        assert_eq!(
            (cfg.path, cfg.size, cfg.after, cfg.steps, cfg.marks),
            (PathBuf::from("/tmp/a.png"), Some((1600, 1000)), Duration::from_secs(2), vec![], None)
        );
        for (k, v) in [
            ("TUNER_SCREENSHOT_SIZE", "big"),
            ("TUNER_SCREENSHOT_SIZE", "10x10"),
            ("TUNER_SCREENSHOT_AFTER", "-1"),
            ("TUNER_SCREENSHOT_SCRIPT", "jump:1"),
        ] {
            assert!(ShotConfig::from_vars(vars(&[("TUNER_SCREENSHOT", "a.png"), (k, v)])).is_err(), "{k}={v}");
        }
    }

    #[test]
    fn script_steps() {
        use egui::{Key, Modifiers};
        let steps = parse_script("view:movies; key:Down;key:Ctrl+K;key:shift+tab;text:news\\; weather;wait:1.5;play:2:xt:live:7;click:10,20;size:950x1000;key:Ctrl++;categories:3;mark:in").unwrap();
        assert_eq!(
            steps,
            vec![
                Step::View("movies".into()),
                Step::Key(Key::ArrowDown, Modifiers::NONE),
                Step::Key(Key::K, Modifiers::CTRL | Modifiers::COMMAND),
                Step::Key(Key::Tab, Modifiers::SHIFT),
                Step::Text("news; weather".into()),
                Step::Wait(Duration::from_millis(1500)),
                Step::Play("2:xt:live:7".into()),
                Step::Click(egui::pos2(10.0, 20.0)),
                Step::Size(950, 1000),
                Step::Key(Key::Plus, Modifiers::CTRL | Modifiers::COMMAND),
                Step::Categories(3),
                Step::Mark("in".into()),
            ]
        );
        assert!(parse_script("key:Hyper+A").is_err());
        assert!(parse_script("key:NoSuchKey").is_err());
        assert!(parse_script("move:1").is_err());
        assert_eq!(parse_script(""), Ok(vec![]));
    }

    #[test]
    fn printable_keys_also_type_their_character() {
        use egui::{Key, Modifiers};
        assert_eq!(typed(Key::Space, Modifiers::NONE), Some(' '));
        assert_eq!(typed(Key::O, Modifiers::NONE), Some('o'));
        assert_eq!(typed(Key::G, Modifiers::SHIFT), Some('G'));
        assert_eq!(typed(Key::Num3, Modifiers::NONE), Some('3'));
        for (k, m) in [(Key::K, Modifiers::CTRL), (Key::Enter, Modifiers::NONE), (Key::F1, Modifiers::NONE), (Key::Num1, Modifiers::SHIFT)] {
            assert_eq!(typed(k, m), None, "{k:?} {m:?}");
        }
    }
}
