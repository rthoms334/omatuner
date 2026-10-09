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
        let script = if cfg.script_fallbacks { resolve_script_fallbacks() } else { Vec::new() };
        let extra = cfg.extra_fallbacks.iter().map(|p| FontFace { path: p.clone(), index: 0 });
        let fallbacks = dedup_fallbacks(regular.as_ref(), script.into_iter().chain(extra));
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
    pub fn install(&self, ctx: &egui::Context) { ctx.set_fonts(self.definitions()) }

    /// egui font definitions: `Proportional`/`Monospace` = [primary, script fallbacks…, egui
    /// defaults]; `Name("medium")`/`Name("bold")` = [face, primary, fallbacks…, defaults].
    fn definitions(&self) -> FontDefinitions {
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
        let defaults_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();
        // [face, primary, script fallbacks…, egui defaults]. Without a primary font the egui
        // defaults come BEFORE the script fallbacks, so Latin isn't drawn by a CJK collection.
        let chain = |face: Option<&str>, defaults: &[String]| -> Vec<String> {
            let mut v: Vec<String> = Vec::new();
            if let Some(f) = face {
                v.push(f.to_owned());
            }
            if primary {
                if face != Some("primary") {
                    v.push("primary".into());
                }
                v.extend(fb_names.iter().cloned());
                v.extend(defaults.iter().filter(|d| !v.contains(d)).cloned().collect::<Vec<_>>());
            } else {
                v.extend(defaults.iter().cloned());
                v.extend(fb_names.iter().cloned());
            }
            v
        };
        let primary_face = primary.then_some("primary");
        defs.families.insert(FontFamily::Proportional, chain(primary_face, &defaults_prop));
        defs.families.insert(FontFamily::Monospace, chain(primary_face, &defaults_mono));
        let medium_face = if medium { Some("primary-medium") } else { primary_face };
        let bold_face = if bold { Some("primary-bold") } else { primary_face };
        defs.families.insert(FontFamily::Name(FAMILY_MEDIUM.into()), chain(medium_face, &defaults_prop));
        defs.families.insert(FontFamily::Name(FAMILY_BOLD.into()), chain(bold_face, &defaults_prop));
        defs
    }
}

/// One Regular-style face per script in [`FALLBACK_LANGS`] (see module docs).
pub fn resolve_script_fallbacks() -> Vec<FontFace> {
    let picks = FALLBACK_LANGS.iter().filter_map(|lang| {
        let out = std::process::Command::new("fc-list").args([&format!(":lang={lang}:weight=80:slant=0"), "file", "index", "family"]).output().ok()?;
        pick_fallback(lang, &String::from_utf8_lossy(&out.stdout))
    });
    dedup_fallbacks(None, picks)
}

/// Best face for `lang` from `fc-list … file index family` output (lines
/// `/path/f.ttc: Family,Alias:index=0`): Sans over Serif, Noto (broad, consistent metrics)
/// over others, the language's own CJK region variant, proportional over Mono. Ties go to the
/// shortest family name (`Noto Sans Arabic` over `Noto Sans Arabic UI`), then path and index,
/// so the choice doesn't depend on fc-list's output order.
fn pick_fallback(lang: &str, fc_list: &str) -> Option<FontFace> {
    let region = match lang {
        "ja" => Some("JP"),
        "zh-cn" => Some("SC"),
        "ko" => Some("KR"),
        _ => None,
    };
    fc_list
        .lines()
        .filter_map(|line| {
            let (file, rest) = line.split_once(": ")?;
            let mut fields = rest.split(':');
            let family = fields.next().unwrap_or("").split(',').next().unwrap_or("").trim();
            let index: u32 = fields.find_map(|f| f.strip_prefix("index=")).and_then(|i| i.trim().parse().ok()).unwrap_or(0);
            let has = |w: &str| family.split_whitespace().any(|t| t == w);
            let score = 2 * i32::from(has("Sans")) - 2 * i32::from(has("Serif")) + i32::from(family.starts_with("Noto"))
                + region.map_or(0, |r| i32::from(has(r)))
                - i32::from(has("Mono"));
            Some((score, family.len(), FontFace { path: PathBuf::from(file), index }))
        })
        .min_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)).then_with(|| a.2.path.cmp(&b.2.path)).then(a.2.index.cmp(&b.2.index)))
        .map(|(_, _, face)| face)
}

/// Keep the first face per FILE (one collection serves every language it covers: ~19 MB of
/// CJK once, not three times) and drop the primary font's file.
fn dedup_fallbacks(primary: Option<&FontFace>, faces: impl IntoIterator<Item = FontFace>) -> Vec<FontFace> {
    let mut out: Vec<FontFace> = Vec::new();
    for f in faces {
        if primary.is_some_and(|p| p.path == f.path) || out.iter().any(|o| o.path == f.path) {
            continue;
        }
        out.push(f);
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const CJK: &str = "\
/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc: Noto Sans Mono CJK TC:index=8
/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc: Noto Sans Mono CJK JP:index=5
/usr/share/fonts/noto-cjk/NotoSerifCJK-Regular.ttc: Noto Serif CJK JP:index=0
/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc: Noto Sans CJK KR:index=1
/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc: Noto Sans CJK JP:index=0
/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc: Noto Sans CJK SC:index=2
";
    const HE: &str = "\
/usr/share/fonts/liberation/LiberationSans-Regular.ttf: Liberation Sans:index=0
/usr/share/fonts/noto/NotoRashiHebrew-Regular.ttf: Noto Rashi Hebrew:index=0
/usr/share/fonts/liberation/LiberationMono-Regular.ttf: Liberation Mono:index=0
/usr/share/fonts/noto/NotoSerifHebrew-Regular.ttf: Noto Serif Hebrew:index=0
/usr/share/fonts/noto/NotoSansHebrew-Regular.ttf: Noto Sans Hebrew,Noto Sans Hebrew Regular:index=0
";
    const AR: &str = "\
/usr/share/fonts/noto/NotoNaskhArabicUI-Regular.ttf: Noto Naskh Arabic UI:index=0
/usr/share/fonts/noto/NotoSansArabicUI-Regular.ttf: Noto Sans Arabic UI:index=0
/usr/share/fonts/noto/NotoKufiArabic-Regular.ttf: Noto Kufi Arabic:index=0
/usr/share/fonts/noto/NotoSansArabic-Regular.ttf: Noto Sans Arabic:index=0
";

    fn face(p: &str, index: u32) -> FontFace { FontFace { path: PathBuf::from(p), index } }

    #[test]
    fn picks_regular_sans_per_script() {
        let cjk = "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc";
        assert_eq!(pick_fallback("ja", CJK), Some(face(cjk, 0)));
        assert_eq!(pick_fallback("zh-cn", CJK), Some(face(cjk, 2)));
        assert_eq!(pick_fallback("ko", CJK), Some(face(cjk, 1)));
        assert_eq!(pick_fallback("he", HE), Some(face("/usr/share/fonts/noto/NotoSansHebrew-Regular.ttf", 0)));
        assert_eq!(pick_fallback("ar", AR), Some(face("/usr/share/fonts/noto/NotoSansArabic-Regular.ttf", 0)));
        assert_eq!(pick_fallback("th", ""), None);
        assert_eq!(pick_fallback("th", "garbage line\n"), None);
    }

    #[test]
    fn pick_is_independent_of_fc_list_order() {
        for (lang, text) in [("he", HE), ("ar", AR), ("ja", CJK)] {
            let reversed: String = text.lines().rev().map(|l| format!("{l}\n")).collect();
            assert_eq!(pick_fallback(lang, &reversed), pick_fallback(lang, text), "{lang}");
        }
    }

    #[test]
    fn fallbacks_deduplicated_by_file() {
        let primary = face("/f/JetBrainsMono.ttf", 0);
        let faces = [face("/f/cjk.ttc", 0), face("/f/cjk.ttc", 2), face("/f/ar.ttf", 0), face("/f/JetBrainsMono.ttf", 0), face("/f/cjk.ttc", 1), face("/f/ar.ttf", 0)];
        assert_eq!(dedup_fallbacks(Some(&primary), faces.clone()), vec![face("/f/cjk.ttc", 0), face("/f/ar.ttf", 0)]);
        assert_eq!(dedup_fallbacks(None, faces).len(), 3);
    }

    /// A real font file in a temp dir (egui's bundled Hack), so tests need no system fonts.
    fn temp_font(dir: &Path, name: &str) -> FontFace {
        let defs = FontDefinitions::default();
        let bytes = defs.font_data.get("Hack").expect("egui default font").font.to_vec();
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write temp font");
        FontFace { path, index: 0 }
    }

    #[test]
    fn families_chain_primary_fallbacks_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let set = FontSet {
            regular: Some(temp_font(dir.path(), "regular.ttf")),
            medium: None,
            bold: Some(temp_font(dir.path(), "bold.ttf")),
            fallbacks: vec![temp_font(dir.path(), "cjk.ttf"), temp_font(dir.path(), "ar.ttf")],
        };
        let loaded = set.load();
        assert!(loaded.regular.is_some() && loaded.bold.is_some() && loaded.medium.is_none());
        let defs = loaded.definitions();
        let fam = |f: FontFamily| defs.families[&f].clone();
        let prop = fam(FontFamily::Proportional);
        assert_eq!(&prop[..3], ["primary", "fallback-0", "fallback-1"]);
        assert!(prop.contains(&"NotoEmoji-Regular".to_owned()), "egui emoji fonts stay in the chain: {prop:?}");
        assert_eq!(&fam(FontFamily::Monospace)[..3], ["primary", "fallback-0", "fallback-1"]);
        assert_eq!(&fam(FontFamily::Name(FAMILY_BOLD.into()))[..3], ["primary-bold", "primary", "fallback-0"]);
        assert_eq!(&fam(FontFamily::Name(FAMILY_MEDIUM.into()))[..2], ["primary", "fallback-0"], "no Medium face → Regular");
    }

    #[test]
    fn without_primary_egui_defaults_come_first() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = FontSet { fallbacks: vec![temp_font(dir.path(), "cjk.ttf")], ..Default::default() }.load();
        let defs = loaded.definitions();
        assert_eq!(defs.families[&FontFamily::Proportional].last().map(String::as_str), Some("fallback-0"));
        assert_eq!(defs.families[&FontFamily::Monospace][0], "Hack");
        assert!(!defs.families[&FontFamily::Name(FAMILY_BOLD.into())].is_empty());
    }

    /// Startup font work (resolve + file reads) runs on a helper thread; the UI thread only
    /// installs bytes already in memory (no I/O: the files may even be gone by then).
    #[test]
    fn font_work_runs_off_the_ui_thread() {
        fn assert_send<T: Send + 'static>() {}
        assert_send::<FontSet>();
        assert_send::<LoadedFonts>();
        let dir = tempfile::tempdir().unwrap();
        let set = FontSet { regular: Some(temp_font(dir.path(), "offthread.ttf")), ..Default::default() };
        let ui_thread = std::thread::current().id();
        let worker = std::thread::spawn(move || {
            assert_ne!(std::thread::current().id(), ui_thread);
            set.load()
        });
        let loaded = worker.join().unwrap();
        std::fs::remove_file(dir.path().join("offthread.ttf")).unwrap();
        let ctx = egui::Context::default();
        loaded.install(&ctx);
        let mut out = ctx.run_ui(Default::default(), |ui| {
            ui.label("installed");
        });
        assert!(!out.textures_delta.set.is_empty(), "font atlas built from in-memory bytes");
        out.textures_delta.clear();
    }

    #[test]
    fn missing_font_files_are_skipped() {
        let set = FontSet { regular: Some(face("/nonexistent/x.ttf", 0)), fallbacks: vec![face("/nonexistent/y.ttf", 0)], ..Default::default() };
        let loaded = set.load();
        assert!(loaded.regular.is_none() && loaded.fallbacks.is_empty());
    }

    /// Real fontconfig (read-only). Skipped where fontconfig isn't installed; on Omarchy this
    /// resolves JetBrainsMono Nerd Font + Regular-weight Noto CJK/Arabic/Hebrew, each file once.
    #[test]
    fn resolves_system_fonts_each_file_once() {
        if std::process::Command::new("fc-match").arg("--version").output().is_err() {
            eprintln!("fc-match not installed; skipping");
            return;
        }
        let set = FontSet::resolve(&FontConfig::default());
        let Some(regular) = set.regular.as_ref() else {
            eprintln!("fc-match found no monospace font; skipping");
            return;
        };
        let mut paths: Vec<&PathBuf> = set.fallbacks.iter().map(|f| &f.path).collect();
        let n = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), n, "each fallback file once: {:?}", set.fallbacks);
        assert!(!paths.contains(&&regular.path));
        for f in &set.fallbacks {
            let name = f.path.to_string_lossy().to_lowercase();
            assert!(!name.contains("thin") && !name.contains("light") && !name.contains("bold"), "Regular weight only: {}", f.path.display());
        }
        eprintln!("primary {} / bold {:?} / medium {:?}", regular.path.display(), set.bold, set.medium);
        for f in &set.fallbacks {
            eprintln!("fallback {}#{}", f.path.display(), f.index);
        }
    }
}
