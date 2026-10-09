//! Theme engine & fonts (WP-01): palette cascade against golden `omarchy-theme-color --all`
//! output, shell.toml cascade, Liftoff tokens, live reload in a temp home that replays
//! `omarchy-theme-set`'s file sequence, fonts. Never touches the real `~/.local/state/omarchy`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui::Color32;
use tuner::msg::{AppEvent, EventBus};
use tuner::paths::AppPaths;
use tuner::theme::colors::{Mode, Palette, parse_color, rgb};
use tuner::theme::tokens::{OSD_EFFECTIVE_BG, contrast_ratio};
use tuner::theme::watch::ThemeWatcher;
use tuner::theme::{Interaction, ShellConfig, SpaceToken, Theme, ThemeEvent, TypeToken, with_alpha};

fn fixture(rel: &str) -> PathBuf { PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/theme").join(rel) }

fn read(p: &Path) -> String { std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())) }

fn palette_of(name: &str) -> Palette {
    let dir = fixture(name);
    Palette::parse(&read(&dir.join("colors.toml")), dir.join("light.mode").exists()).unwrap()
}

fn theme_of(name: &str, ui_scale: f32) -> Theme {
    let dir = fixture(name);
    let shell = std::fs::read_to_string(dir.join("shell.toml")).unwrap_or_default();
    Theme::from_parts(name.into(), palette_of(name), ShellConfig::parse(&shell, None).unwrap(), 0.0, ui_scale)
}

fn fixture_themes() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(fixture(""))
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("colors.toml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------- palette

/// Every fixture theme parses, and every palette field equals what `omarchy-theme-color
/// --file <colors.toml> --all` printed for it (golden `palette.expected.tsv`, generated on an
/// Omarchy 4 machine; regenerate after copying new themes in, per fixture directory:
/// `omarchy-theme-color --file colors.toml --all > palette.expected.tsv`, which only reads).
#[test]
fn all_fixture_themes_match_omarchy_theme_color() {
    let themes = fixture_themes();
    assert!(themes.len() >= 25, "expected every /usr/share/omarchy theme plus synthetic ones, got {themes:?}");
    for name in &themes {
        let golden: HashMap<String, String> = read(&fixture(name).join("palette.expected.tsv"))
            .lines()
            .filter_map(|l| l.split_once('\t').map(|(k, v)| (k.to_owned(), v.to_owned())))
            .collect();
        let p = palette_of(name);
        // A colour the theme leaves undefined is empty in the script's output; we fall back to
        // the foreground instead (documented deviation in `theme::colors`).
        let want = |k: &str| match golden[k].as_str() {
            "" => p.foreground,
            v => parse_color(v).unwrap_or_else(|| panic!("{name}: golden {k} = {v:?}")),
        };
        let fields: [(&str, Color32); 26] = [
            ("selection", p.selection),
            ("selection_foreground", p.selection_foreground),
            ("muted", p.muted),
            ("background", p.background),
            ("dark_background", p.dark_background),
            ("darker_background", p.darker_background),
            ("lighter_background", p.lighter_background),
            ("foreground", p.foreground),
            ("dark_foreground", p.dark_foreground),
            ("light_foreground", p.light_foreground),
            ("bright_foreground", p.bright_foreground),
            ("red", p.red),
            ("yellow", p.yellow),
            ("orange", p.orange),
            ("green", p.green),
            ("cyan", p.cyan),
            ("blue", p.blue),
            ("magenta", p.magenta),
            ("brown", p.brown),
            ("bright_red", p.bright_red),
            ("bright_yellow", p.bright_yellow),
            ("bright_green", p.bright_green),
            ("bright_cyan", p.bright_cyan),
            ("bright_blue", p.bright_blue),
            ("bright_magenta", p.bright_magenta),
            ("red", p.urgent),
        ];
        for (k, got) in fields {
            assert_eq!(got, want(k), "{name}: {k}");
        }
        // accent: the theme's accent, else color4 (Color.qml)
        let accent_key = if golden.contains_key("accent") { "accent" } else { "color4" };
        assert_eq!(p.accent, want(accent_key), "{name}: accent");
        let mode = if golden["mode"] == "light" { Mode::Light } else { Mode::Dark };
        assert_eq!(p.mode, mode, "{name}: mode");
    }
}

#[test]
fn ansi_only_and_legacy_names_cascade() {
    let p = palette_of("ansi-only");
    assert_eq!(p.background, rgb(0x15, 0x16, 0x1e), "background ← color0");
    assert_eq!(p.foreground, rgb(0xa9, 0xb1, 0xd6), "foreground ← color7");
    assert_eq!(p.accent, rgb(0x7a, 0xa2, 0xf7), "accent ← color4");
    assert_eq!(p.bright_foreground, rgb(0xc0, 0xca, 0xf5), "bright_foreground ← color15");
    assert_eq!(p.selection, rgb(0x33, 0x46, 0x7c), "selection ← selection_background");
    assert_eq!(p.muted, rgb(0x41, 0x48, 0x68), "muted ← color8");
    assert_eq!(p.orange, p.yellow, "orange ← yellow");
    assert_eq!(p.brown, rgb(0x70, 0x58, 0x34), "brown = mix(orange, black, 50%)");
    assert_eq!(p.urgent, rgb(0xf7, 0x76, 0x8e), "urgent ← color1");

    let p = palette_of("legacy-short");
    assert_eq!(p.background, rgb(0x2e, 0x34, 0x40), "bg");
    assert_eq!(p.dark_background, rgb(0x27, 0x2c, 0x36), "dark_bg");
    assert_eq!(p.darker_background, rgb(0x17, 0x1a, 0x20), "derived darker");
    assert_eq!(p.magenta, rgb(0xb4, 0x8e, 0xad), "magenta ← purple");
    assert_eq!(p.bright_red, rgb(0xcc, 0x81, 0x88), "bright = mix(x, white, 20%)");
    assert_eq!(p.mode, Mode::Dark);
}

/// A sparse theme: undefined base colours fall back to the foreground, while the derived
/// `bright_*`/`brown` mix an undefined colour as black — exactly like the script's awk mixer.
#[test]
fn undefined_colours_follow_the_script() {
    let p = palette_of("light-mode-file");
    assert_eq!((p.yellow, p.green, p.blue), (p.foreground, p.foreground, p.foreground));
    assert_eq!(p.accent, rgb(0x00, 0x5f, 0x87));
    assert_eq!(p.bright_yellow, rgb(0x33, 0x33, 0x33));
    assert_eq!(p.brown, rgb(0, 0, 0));
    assert_eq!(p.bright_red, rgb(0xbf, 0x33, 0x33));
}

#[test]
fn light_detection() {
    assert_eq!(palette_of("catppuccin-latte").mode, Mode::Light, "mode key");
    assert_eq!(palette_of("latte-no-mode").mode, Mode::Light, "luminance");
    assert_eq!(palette_of("light-mode-file").mode, Mode::Light, "light.mode file");
    assert_eq!(palette_of("flexoki-light").mode, Mode::Light);
    assert_eq!(palette_of("white").mode, Mode::Light);
    assert_eq!(palette_of("liftoff").mode, Mode::Dark);
    assert_eq!(palette_of("vantablack").mode, Mode::Dark);
}

// ---------------------------------------------------------------- shell.toml & tokens

#[test]
fn liftoff_tokens_match_omarchy_tables() {
    let t = theme_of("liftoff", 1.0);
    let fonts: Vec<f32> = [TypeToken::Caption, TypeToken::BodySmall, TypeToken::Body, TypeToken::Subtitle, TypeToken::Title, TypeToken::Heading, TypeToken::Display, TypeToken::DisplayLarge]
        .iter()
        .map(|tok| t.font(*tok))
        .collect();
    assert_eq!(fonts, [10.0, 11.0, 12.0, 13.0, 14.0, 16.0, 24.0, 28.0]);
    assert_eq!((t.font(TypeToken::IconSmall), t.font(TypeToken::Icon), t.font(TypeToken::IconLarge)), (11.0, 14.0, 18.0));
    use SpaceToken::*;
    let table = [
        (Hairline, 1.0), (Xxs, 2.0), (Xs, 3.0), (Sm, 4.0), (Md, 6.0), (Lg, 8.0), (Xl, 10.0), (Xxl, 12.0), (Xxxl, 14.0), (Huge, 18.0),
        (ControlGap, 8.0), (ControlPaddingX, 10.0), (ControlPaddingY, 6.0), (InputPaddingY, 7.0), (ControlHeight, 28.0), (PopupRowHeight, 28.0),
        (RowGap, 8.0), (RowPaddingX, 12.0), (LabelGap, 4.0), (PanelGap, 14.0), (PanelPadding, 18.0), (PopupPadding, 14.0),
    ];
    for (tok, px) in table {
        assert_eq!(t.space(tok), px, "{tok:?}");
    }
    assert_eq!(t.rem(), 1.0);

    let (k, p) = (&t.tokens, &t.palette);
    let fg = rgb(0xEC, 0xE5, 0xD8);
    let accent = rgb(0xF3, 0xB7, 0x7B);
    assert_eq!((p.foreground, p.accent), (fg, accent));
    assert_eq!(k.card_bg, p.background);
    assert_eq!(k.card_border, fg, "menu.border → hyprland.active-border-foreground");
    assert_eq!(k.popup_border, accent, "popups.border → hyprland.active-border");
    assert_eq!(k.card_border_width, 2.0);
    assert_eq!(k.list_cursor_bg, with_alpha(fg, 0.08));
    assert_eq!(k.list_cursor_text, accent);
    assert_eq!(k.list_cursor_border, with_alpha(fg, 0.25));
    assert_eq!(k.scrim, with_alpha(p.background, 0.5));
    assert_eq!(k.fill(Interaction::Idle), Color32::TRANSPARENT);
    assert_eq!(k.fill(Interaction::Field), with_alpha(fg, 0.04));
    assert_eq!(k.fill(Interaction::Hover), with_alpha(fg, 0.08));
    assert_eq!(k.fill(Interaction::Focus), with_alpha(fg, 0.08));
    assert_eq!(k.fill(Interaction::Selected), with_alpha(fg, 0.18));
    assert_eq!(k.fill(Interaction::Pressed), with_alpha(fg, 0.22));
    assert_eq!(k.fill_text_selection, with_alpha(fg, 0.35));
    assert_eq!(k.border(Interaction::Field), Some((1.0, with_alpha(fg, 0.4))));
    assert_eq!(k.border(Interaction::Hover), Some((1.0, with_alpha(fg, 0.25))));
    assert_eq!(k.border(Interaction::Selected), None);
    assert_eq!(k.text_secondary, with_alpha(fg, 0.52));
    assert_eq!(k.separator, with_alpha(fg, 0.12));
    assert_eq!(k.osd_accent, accent, "liftoff amber is readable on the OSD scrim");
}

#[test]
fn hackerman_gradient_border_resolves_to_first_stop() {
    let t = theme_of("hackerman", 1.0);
    let first = rgb(0x26, 0xa2, 0x69);
    assert_eq!(t.shell.color("hyprland", "active-border", &t.palette), Some(Color32::from_rgba_unmultiplied(0x26, 0xa2, 0x69, 0xee)));
    assert_eq!(t.tokens.popup_border, first, "gradient first stop, alpha replaced by popups.border-alpha");
    assert_eq!(t.tokens.card_border, first, "reference → gradient");
    assert_eq!(t.tokens.list_cursor_text, rgb(0x82, 0xFB, 0x9C));
}

#[test]
fn osd_accent_falls_back_on_low_contrast_accents() {
    let near_white = rgb(0xf2, 0xf2, 0xf2);
    for name in fixture_themes() {
        let t = theme_of(&name, 1.0);
        let readable = contrast_ratio(t.palette.accent, OSD_EFFECTIVE_BG) >= 3.0;
        assert_eq!(t.tokens.osd_accent, if readable { t.palette.accent } else { near_white }, "{name}");
    }
    assert_eq!(theme_of("white", 1.0).tokens.osd_accent, near_white, "#6e6e6e accent");
    assert_eq!(theme_of("catppuccin-latte", 1.0).tokens.osd_accent, near_white, "dark blue accent");
    assert_eq!(theme_of("tokyo-night", 1.0).tokens.osd_accent, rgb(0x7a, 0xa2, 0xf7));
}

#[test]
fn light_theme_states_are_foreground_tints() {
    let t = theme_of("catppuccin-latte", 1.0);
    let fg = rgb(0x4c, 0x4f, 0x69);
    assert!(t.is_light());
    assert_eq!(t.tokens.fill_hover, with_alpha(fg, 0.08));
    assert_eq!(t.tokens.scrim, with_alpha(rgb(0xef, 0xf1, 0xf5), 0.5));
}

#[test]
fn text_size_override_and_ui_scale() {
    let pal = palette_of("liftoff");
    let theme_shell = read(&fixture("liftoff/shell.toml"));
    let shell = ShellConfig::parse(&theme_shell, Some("[font]\nbase-size = 16\n")).unwrap();
    let t = Theme::from_parts("x".into(), pal.clone(), shell, 0.0, 1.0);
    assert_eq!(t.font(TypeToken::Body), 16.0);
    assert_eq!(t.font(TypeToken::Heading), 21.0);
    assert_eq!(t.space(SpaceToken::ControlHeight), 37.0, "spacing scales with font");
    assert!((t.rem() - 16.0 / 12.0).abs() < 1e-6);

    let shell = ShellConfig::parse("[font]\nbase-size = 16\n[spacing]\nscale-with-font = false\nscale = 2\npanel-padding = 5\nhairline = 9\n[font]\ntitle = 30\n", None).unwrap();
    let t = Theme::from_parts("x".into(), pal.clone(), shell, 0.0, 1.0);
    assert_eq!(t.space(SpaceToken::ControlHeight), 56.0);
    assert_eq!(t.space(SpaceToken::PanelPadding), 5.0, "pinned token");
    assert_eq!(t.space(SpaceToken::Hairline), 2.0, "hairline is never pinned");
    assert_eq!(t.font(TypeToken::Title), 30.0, "pinned font token");

    let t = Theme::from_parts("x".into(), pal, ShellConfig::parse(&theme_shell, None).unwrap(), 0.0, 1.5);
    assert_eq!(t.font(TypeToken::Body), 18.0);
    assert_eq!(t.space(SpaceToken::ControlHeight), 42.0);
    assert_eq!(t.rem(), 1.5);
}

// ---------------------------------------------------------------- Theme::try_load cascade

/// A fake home with `~/.local/state/omarchy/current/theme` = a copy of fixture `name`.
fn fake_home(name: &str) -> (tempfile::TempDir, AppPaths) {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = AppPaths::rooted(&dir.path().join("app"));
    paths.home = dir.path().join("home");
    let current = paths.omarchy_current();
    std::fs::create_dir_all(&current).unwrap();
    copy_dir(&fixture(name), &paths.omarchy_theme_dir());
    std::fs::write(current.join("theme.name"), format!("{name}\n")).unwrap();
    (dir, paths)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        if e.path().is_file() {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[test]
fn try_load_cascade_section_file_then_user_override() {
    let (_dir, paths) = fake_home("tokyo-night");
    std::fs::create_dir_all(paths.omarchy_config()).unwrap();
    std::fs::write(paths.omarchy_config().join("shell.toml"), "[lock]\nborder-alpha = 0.3\n[font]\nbase-size = 14\n").unwrap();
    let t = Theme::try_load(&paths, 1.0).unwrap();
    assert_eq!(t.name, "tokyo-night");
    let (s, p) = (&t.shell, &t.palette);
    // shell.lock.toml replaced the template's [lock] section …
    assert_eq!(s.color("lock", "placeholder", p), Some(rgb(0xa9, 0xb1, 0xd6)));
    assert_eq!(s.color("lock", "background", p), None, "keys missing from the section file are gone");
    assert_eq!(s.color("lock", "border", p), Some(rgb(0xa9, 0xb1, 0xd6)));
    // … and the user override was merged last, key by key
    assert_eq!(s.f32("lock", "border-alpha"), Some(0.3));
    assert_eq!(s.color("lock", "text", p), Some(rgb(0xa9, 0xb1, 0xd6)));
    assert_eq!(t.font(TypeToken::Body), 14.0);
    assert_eq!(s.color("menu", "selected-text", p), Some(rgb(0x7a, 0xa2, 0xf7)), "rest of the theme file intact");
}

#[test]
fn try_load_errors_without_colors_and_load_falls_back() {
    let (_dir, paths) = fake_home("liftoff");
    std::fs::remove_dir_all(paths.omarchy_theme_dir()).unwrap();
    assert!(Theme::try_load(&paths, 1.0).is_err());
    let t = Theme::load(&paths, 1.0);
    assert_eq!(t.name, "fallback");
    assert_eq!(t.palette, Palette::fallback());
    let t = Theme::load(&paths, 2.0);
    assert_eq!(t.font(TypeToken::Body), 24.0, "ui_scale applies to the fallback too");
}

#[test]
fn try_load_without_shell_toml_uses_color_qml_defaults() {
    let (_dir, paths) = fake_home("ansi-only");
    let t = Theme::try_load(&paths, 1.0).unwrap();
    let fg = t.palette.foreground;
    assert_eq!(t.tokens.card_border, fg);
    assert_eq!(t.tokens.popup_border, t.palette.accent);
    assert_eq!(t.tokens.list_cursor_bg, with_alpha(fg, 0.08));
    assert_eq!(t.tokens.list_cursor_border, with_alpha(fg, 0.0));
    assert_eq!(t.font(TypeToken::Heading), 16.0);
}

// ---------------------------------------------------------------- live reload

struct Events {
    rx: crossbeam_channel::Receiver<AppEvent>,
}

impl Events {
    /// Theme events received until `deadline` (other events ignored).
    fn collect_until(&self, deadline: Instant) -> Vec<ThemeEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.rx.recv_deadline(deadline) {
            if let AppEvent::Theme(t) = ev {
                out.push(t);
            }
        }
        out
    }

    fn first_within(&self, d: Duration) -> Option<(ThemeEvent, Duration)> {
        let start = Instant::now();
        let deadline = start + d;
        while let Ok(ev) = self.rx.recv_deadline(deadline) {
            if let AppEvent::Theme(t) = ev {
                return Some((t, start.elapsed()));
            }
        }
        None
    }
}

fn start_watcher(paths: &AppPaths) -> (ThemeWatcher, Events) {
    let (bus, rx) = EventBus::new(None);
    let w = ThemeWatcher::start(paths.clone(), 1.0, bus).expect("watcher");
    (w, Events { rx })
}

fn changed(ev: &ThemeEvent) -> Option<&Arc<Theme>> {
    match ev {
        ThemeEvent::Changed(t) => Some(t),
        ThemeEvent::FontsChanged(_) => None,
    }
}

/// `omarchy-theme-set <name>`: rebuild `next-theme`, `rm -rf theme && mv next-theme theme`,
/// then `echo name > theme.name` (the last write).
fn omarchy_theme_set(paths: &AppPaths, name: &str) {
    let current = paths.omarchy_current();
    let next = current.join("next-theme");
    let _ = std::fs::remove_dir_all(&next);
    copy_dir(&fixture(name), &next);
    std::fs::remove_dir_all(current.join("theme")).unwrap();
    std::fs::rename(&next, current.join("theme")).unwrap();
    std::fs::write(current.join("theme.name"), format!("{name}\n")).unwrap();
}

#[test]
fn theme_set_sequence_yields_exactly_one_changed() {
    let (_dir, paths) = fake_home("liftoff");
    let (_w, events) = start_watcher(&paths);
    omarchy_theme_set(&paths, "catppuccin-latte");
    let (ev, after) = events.first_within(Duration::from_millis(500)).expect("Changed within 500 ms");
    let t = changed(&ev).expect("a Changed event");
    assert!(after <= Duration::from_millis(500), "{after:?}");
    assert_eq!(t.name, "catppuccin-latte");
    assert_eq!(t.palette, palette_of("catppuccin-latte"));
    assert_ne!(t.palette, Palette::fallback());
    let more = events.collect_until(Instant::now() + Duration::from_millis(800));
    assert!(more.is_empty(), "exactly one event, got {} more", more.len());

    // and back again
    omarchy_theme_set(&paths, "liftoff");
    let all = events.collect_until(Instant::now() + Duration::from_millis(1000));
    assert_eq!(all.len(), 1);
    assert_eq!(changed(&all[0]).unwrap().palette, palette_of("liftoff"));
}

/// The reload races the switch: `theme.name` is written while `theme/` is missing (the reload
/// fails and is retried) and the new directory only lands 400 ms later. Still exactly one
/// Changed, never the fallback palette.
#[test]
fn raced_switch_retries_and_never_flashes_fallback() {
    let (_dir, paths) = fake_home("liftoff");
    let (_w, events) = start_watcher(&paths);
    let current = paths.omarchy_current();
    copy_dir(&fixture("hackerman"), &current.join("next-theme"));
    std::fs::remove_dir_all(current.join("theme")).unwrap();
    std::fs::write(current.join("theme.name"), "hackerman\n").unwrap();
    std::thread::sleep(Duration::from_millis(400));
    std::fs::rename(current.join("next-theme"), current.join("theme")).unwrap();
    let all = events.collect_until(Instant::now() + Duration::from_millis(1500));
    assert_eq!(all.len(), 1, "{all:?}");
    let t = changed(&all[0]).unwrap();
    assert_eq!(t.name, "hackerman");
    assert_eq!(t.palette, palette_of("hackerman"));
}

#[test]
fn user_override_change_reloads() {
    let (_dir, paths) = fake_home("liftoff");
    std::fs::create_dir_all(paths.omarchy_config()).unwrap();
    let (_w, events) = start_watcher(&paths);
    std::fs::write(paths.omarchy_config().join("shell.toml"), "[font]\nbase-size = 16\n").unwrap();
    let (ev, _) = events.first_within(Duration::from_millis(1000)).expect("Changed");
    assert_eq!(changed(&ev).unwrap().font(TypeToken::Body), 16.0);
    std::fs::remove_file(paths.omarchy_config().join("shell.toml")).unwrap();
    let (ev, _) = events.first_within(Duration::from_millis(1000)).expect("Changed after delete");
    assert_eq!(changed(&ev).unwrap().font(TypeToken::Body), 12.0);
}

#[test]
fn ignores_staging_and_background_changes() {
    let (_dir, paths) = fake_home("liftoff");
    let (_w, events) = start_watcher(&paths);
    let current = paths.omarchy_current();
    copy_dir(&fixture("white"), &current.join("next-theme"));
    std::fs::write(current.join("background"), "x").unwrap();
    std::fs::remove_dir_all(current.join("next-theme")).unwrap();
    assert!(events.collect_until(Instant::now() + Duration::from_millis(600)).is_empty());
}

#[test]
fn fonts_conf_change_sends_loaded_fonts() {
    let (_dir, paths) = fake_home("liftoff");
    // keep the reload light: no script fallbacks (fontconfig itself is read-only here)
    std::fs::create_dir_all(&paths.config_dir).unwrap();
    std::fs::write(paths.config_file(), "[fonts]\nscript_fallbacks = false\n").unwrap();
    std::fs::create_dir_all(paths.fontconfig_dir()).unwrap();
    let (_w, events) = start_watcher(&paths);
    std::fs::write(paths.fontconfig_dir().join("fonts.conf"), "<fontconfig/>\n").unwrap();
    let (ev, _) = events.first_within(Duration::from_secs(5)).expect("FontsChanged");
    let ThemeEvent::FontsChanged(f) = ev else { panic!("expected FontsChanged, got {ev:?}") };
    assert!(f.fallbacks.is_empty());
}

/// No `current/` directory when the watcher starts (not on Omarchy yet): it polls
/// `theme.name` every 5 s and picks the theme up once it appears.
#[test]
fn polls_when_the_directory_cannot_be_watched() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = AppPaths::rooted(&dir.path().join("app"));
    paths.home = dir.path().join("home");
    let (_w, events) = start_watcher(&paths);
    std::thread::sleep(Duration::from_millis(100));
    let current = paths.omarchy_current();
    std::fs::create_dir_all(&current).unwrap();
    copy_dir(&fixture("nord"), &paths.omarchy_theme_dir());
    std::fs::write(current.join("theme.name"), "nord\n").unwrap();
    let (ev, _) = events.first_within(Duration::from_secs(8)).expect("Changed via polling");
    assert_eq!(changed(&ev).unwrap().palette, palette_of("nord"));
}

#[test]
fn dropping_the_watcher_stops_events() {
    let (_dir, paths) = fake_home("liftoff");
    let (w, events) = start_watcher(&paths);
    drop(w);
    omarchy_theme_set(&paths, "nord");
    assert!(events.collect_until(Instant::now() + Duration::from_millis(500)).is_empty());
}

// ---------------------------------------------------------------- fonts

#[test]
fn display_text_reorders_arabic_and_latin() {
    use tuner::fonts::display_text;
    assert!(matches!(display_text("BBC One HD"), std::borrow::Cow::Borrowed(_)), "ASCII fast path");
    assert!(matches!(display_text("Café Ünïcode"), std::borrow::Cow::Borrowed(_)), "LTR non-ASCII untouched");

    let out = display_text("قناة BBC");
    let s = out.as_ref();
    assert!(s.contains("BBC"), "Latin run kept in logical order: {s:?}");
    // RTL paragraph: the Latin run is drawn first (leftmost), the Arabic after it, reversed
    assert!(s.starts_with("BBC"), "{s:?}");
    let arabic: Vec<char> = s.chars().filter(|c| !c.is_ascii()).collect();
    assert!(arabic.iter().all(|c| ('\u{FE70}'..='\u{FEFF}').contains(c)), "presentation forms: {arabic:?}");
    // logical first letter ق (initial form U+FED7) is now the last Arabic glyph
    assert_eq!(arabic.last(), Some(&'\u{FED7}'), "{arabic:?}");

    let he = display_text("ערוץ 12");
    assert!(he.as_ref().starts_with("12"), "{he:?}");
    assert!(he.as_ref().ends_with('ע'), "{he:?}");
}
