//! Fonts: fontconfig resolution, script fallbacks, egui installation, RTL display helper.
//! Owner: WP-01. DESIGN §17.
//!
//! * Primary = `fc-match -f '%{file}' <family>` (Omarchy: alias `monospace` → JetBrainsMono
//!   Nerd Font). Bold = `<family>:bold`; Medium = `<family>:medium` if it resolves to a
//!   different file than Regular, else Regular. Nerd glyphs (icons) come from the primary.
//! * Fallbacks (critique gap 1): for each script language in [`FALLBACK_LANGS`],
//!   `fc-list ':lang=<l>:weight=80:slant=0' file index family` (weight 80 = Regular — matching
//!   "Regular" in the style list picked `NotoSansCJK-Light.ttc`/`…Arabic…-Thin`), preferring
//!   Sans families; de-duplicated BY PATH (one CJK collection covers ja/zh/ko: ~19 MB once
//!   instead of three times). `.ttc` collections use the face index.
//! * Threading: [`FontSet::resolve`] (subprocesses, ~145 ms for 12 languages) and
//!   [`FontSet::load`] (file reads) run OFF the UI thread — at startup on a helper thread
//!   joined in the eframe creation callback, on reload in the theme watcher. The UI thread
//!   only calls [`LoadedFonts::install`]. Font bytes are cached per (path, index) for the
//!   process lifetime, so reinstalls don't re-read 19 MB files.
//! * Families installed into egui: `Proportional` and `Monospace` = [primary, fallbacks…,
//!   egui defaults]; `Name("medium")`, `Name("bold")` = [face, fallbacks…].
//! * egui has no shaping and no bidi: [`display_text`] reshapes Arabic (presentation forms via
//!   `ar-reshaper`) and reorders RTL runs visually (`unicode-bidi`) for single-line labels.
//!   Indic conjuncts remain unshaped (documented limitation).
//! * Reload on `ThemeEvent::FontsChanged(Arc<LoadedFonts>)` (fonts.conf changed).

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use eframe::egui::{self, FontData, FontDefinitions, FontFamily};

use crate::config::FontConfig;
use crate::theme::egui_style::{FAMILY_BOLD, FAMILY_MEDIUM};

/// Script languages probed for fallbacks (fontconfig `lang=` codes).
pub const FALLBACK_LANGS: &[&str] = &["ja", "zh-cn", "ko", "ar", "he", "th", "hi", "bn", "ta", "ka", "hy", "am"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FontFace {
    pub path: PathBuf,
    /// Face index inside a collection (.ttc).
    pub index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FontSet {
    pub regular: Option<FontFace>,
    pub medium: Option<FontFace>,
    pub bold: Option<FontFace>,
    pub fallbacks: Vec<FontFace>,
}

fn fc_match(pattern: &str) -> Option<FontFace> {
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}|%{index}", pattern]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let (file, idx) = s.trim().split_once('|')?;
    if file.is_empty() {
        return None;
    }
    Some(FontFace { path: PathBuf::from(file), index: idx.parse().unwrap_or(0) })
}

impl FontSet {
    /// Resolve via fontconfig. Blocking (spawns fc-match/fc-list); ~20–80 ms total.
    pub fn resolve(cfg: &FontConfig) -> Self {
        let regular = fc_match(&cfg.family);
        let bold = fc_match(&format!("{}:bold", cfg.family)).filter(|b| Some(b) != regular.as_ref());
        let medium = fc_match(&format!("{}:medium", cfg.family)).filter(|m| Some(m) != regular.as_ref());
        let mut fallbacks = if cfg.script_fallbacks { resolve_script_fallbacks() } else { Vec::new() };
        for p in &cfg.extra_fallbacks {
            fallbacks.push(FontFace { path: p.clone(), index: 0 });
        }
        fallbacks.dedup();
        Self { regular, medium, bold, fallbacks }
    }

    /// Read the font files (cached per path+index). Blocking I/O: never on the UI thread.
    pub fn load(&self) -> LoadedFonts {
        let get = |f: &FontFace| -> Option<Arc<FontData>> {
            let key = (f.path.clone(), f.index);
            if let Some(d) = FONT_CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
                return Some(d.clone());
            }
            match std::fs::read(&f.path) {
                Ok(bytes) => {
                    let mut fd = FontData::from_owned(bytes);
                    fd.index = f.index;
                    let fd = Arc::new(fd);
                    FONT_CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, fd.clone());
                    Some(fd)
                }
                Err(e) => {
                    tracing::warn!("font {}: {e}", f.path.display());
                    None
                }
            }
        };
        LoadedFonts {
            regular: self.regular.as_ref().and_then(get),
            medium: self.medium.as_ref().and_then(get),
            bold: self.bold.as_ref().and_then(get),
            fallbacks: self.fallbacks.iter().filter_map(get).collect(),
        }
    }

    /// Convenience for tests/tools: load + install (blocking).
    pub fn install(&self, ctx: &egui::Context) { self.load().install(ctx) }
}

type FontCache = Mutex<HashMap<(PathBuf, u32), Arc<FontData>>>;
static FONT_CACHE: LazyLock<FontCache> = LazyLock::new(Mutex::default);

/// Font data ready to install (bytes already in memory).
#[derive(Clone, Default)]
pub struct LoadedFonts {
    pub regular: Option<Arc<FontData>>,
    pub medium: Option<Arc<FontData>>,
    pub bold: Option<Arc<FontData>>,
    pub fallbacks: Vec<Arc<FontData>>,
}

impl std::fmt::Debug for LoadedFonts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LoadedFonts {{ regular: {}, fallbacks: {} }}", self.regular.is_some(), self.fallbacks.len())
    }
}

impl LoadedFonts {
    /// Install into egui (replaces all font definitions). Cheap: no I/O (UI thread).
    pub fn install(&self, ctx: &egui::Context) {
        let mut defs = FontDefinitions::default();
        let primary = if let Some(d) = &self.regular {
            defs.font_data.insert("primary".into(), d.clone());
            true
        } else {
            false
        };
        let medium = if let Some(d) = &self.medium {
            defs.font_data.insert("primary-medium".into(), d.clone());
            true
        } else {
            false
        };
        let bold = if let Some(d) = &self.bold {
            defs.font_data.insert("primary-bold".into(), d.clone());
            true
        } else {
            false
        };
        let mut fb_names = Vec::new();
        for (i, d) in self.fallbacks.iter().enumerate() {
            let n = format!("fallback-{i}");
            defs.font_data.insert(n.clone(), d.clone());
            fb_names.push(n);
        }
        let defaults_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
        let chain = |first: Option<&str>| -> Vec<String> {
            let mut v: Vec<String> = Vec::new();
            if let Some(f) = first {
                v.push(f.to_owned());
            }
            if primary && first != Some("primary") {
                v.push("primary".into());
            }
            v.extend(fb_names.iter().cloned());
            v.extend(defaults_prop.iter().cloned());
            v
        };
        let base = chain(primary.then_some("primary"));
        defs.families.insert(FontFamily::Proportional, base.clone());
        defs.families.insert(FontFamily::Monospace, base.clone());
        defs.families.insert(FontFamily::Name(FAMILY_MEDIUM.into()), chain(Some(if medium { "primary-medium" } else if primary { "primary" } else { "Ubuntu-Light" })));
        defs.families.insert(FontFamily::Name(FAMILY_BOLD.into()), chain(Some(if bold { "primary-bold" } else if primary { "primary" } else { "Ubuntu-Light" })));
        ctx.set_fonts(defs);
    }
}

/// One Regular-style face per script in [`FALLBACK_LANGS`] (see module docs).
pub fn resolve_script_fallbacks() -> Vec<FontFace> {
    let mut out: Vec<FontFace> = Vec::new();
    for lang in FALLBACK_LANGS {
        let Ok(o) = std::process::Command::new("fc-list").args([&format!(":lang={lang}:weight=80:slant=0"), "file", "index", "family"]).output()
        else {
            continue;
        };
        let text = String::from_utf8_lossy(&o.stdout);
        // lines: "/path/f.ttc: Family,Alias:index=0"
        let mut best: Option<(i32, FontFace)> = None;
        for line in text.lines() {
            let Some((file, rest)) = line.split_once(": ") else { continue };
            let index = rest.split(':').find_map(|f| f.strip_prefix("index=")).and_then(|i| i.parse().ok()).unwrap_or(0);
            let family = rest.split(':').next().unwrap_or("");
            let score = if family.contains("Sans") { 2 } else { 1 } + if family.contains("Mono") { 1 } else { 0 };
            if best.as_ref().map(|(b, _)| score > *b).unwrap_or(true) {
                best = Some((score, FontFace { path: PathBuf::from(file), index }));
            }
        }
        if let Some((_, f)) = best {
            // dedupe by PATH: one collection serves every language it covers
            if !out.iter().any(|o| o.path == f.path) {
                out.push(f);
            }
        }
    }
    out
}

/// Prepare a single-line label for egui: Arabic reshaping + visual bidi reordering when the
/// string contains RTL characters; borrowed unchanged otherwise (fast path: ASCII check).
pub fn display_text(s: &str) -> Cow<'_, str> {
    if s.is_ascii() {
        return Cow::Borrowed(s);
    }
    display_text_slow(s)
}

fn display_text_slow(s: &str) -> Cow<'_, str> {
    let has_rtl = s.chars().any(|c| matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF));
    if !has_rtl {
        return Cow::Borrowed(s);
    }
    let shaped = ar_reshaper::reshape_line(s);
    let info = unicode_bidi::BidiInfo::new(&shaped, None);
    let mut out = String::with_capacity(shaped.len());
    for para in &info.paragraphs {
        out.push_str(&info.reorder_line(para, para.range.clone()));
    }
    Cow::Owned(out)
}
