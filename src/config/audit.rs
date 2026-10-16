//! Redacted preference-change logging. Only schema keys, booleans, numbers and explicitly
//! enumerated values may reach the log. Text and dynamic maps never supply names or values.
use super::Config;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub fn key_input(action: crate::keymap::Action) -> &'static str {
    use crate::keymap::Action;
    match action {
        Action::Right => "key →",
        Action::Increase => "key Shift+→",
        Action::Decrease => "key Shift+←",
        Action::Activate => "key Enter",
        Action::FilterInput(' ') => "key Space",
        Action::LipSyncEarlier => "key [",
        Action::LipSyncLater => "key ]",
        _ => "key",
    }
}

pub struct Edit {
    pub input: &'static str,
    pub apply: Box<dyn FnOnce(&mut Config)>,
}

pub fn edit(input: &'static str, apply: impl FnOnce(&mut Config) + 'static) -> Edit {
    Edit { input, apply: Box::new(apply) }
}

const QUIET: Duration = Duration::from_millis(500);

struct Change<T> {
    old: T,
    new: T,
    input: &'static str,
    at: Instant,
}

impl<T> Change<T> {
    fn update(&mut self, new: T, input: &'static str, at: Instant) {
        self.new = new;
        self.input = input;
        self.at = at;
    }
}

#[derive(Default)]
pub struct Pending {
    settings: Mutex<BTreeMap<String, Change<serde_json::Value>>>,
    audio: Mutex<BTreeMap<String, Change<i32>>>,
}

impl Pending {
    pub fn record(&self, old: &Config, new: &Config, input: &'static str) {
        self.record_at(old, new, input, Instant::now());
    }

    fn record_at(&self, old: &Config, new: &Config, input: &'static str, now: Instant) {
        self.flush_ready(now);
        let mut pending = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        for (path, before, after) in changes(old, new) {
            pending.entry(path).and_modify(|change| change.update(after.clone(), input, now)).or_insert(Change {
                old: before,
                new: after,
                input,
                at: now,
            });
        }
        let mut pending = self.audio.lock().unwrap_or_else(|e| e.into_inner());
        for name in old.player.lip_sync.keys().chain(new.player.lip_sync.keys()) {
            let before = old.player.lip_sync.get(name).map_or(0, super::LipSync::offset_ms);
            let after = new.player.lip_sync.get(name).map_or(0, super::LipSync::offset_ms);
            if before != after {
                pending.entry(name.clone()).and_modify(|change| change.update(after, input, now)).or_insert(Change {
                    old: before,
                    new: after,
                    input,
                    at: now,
                });
            }
        }
    }

    /// Emit settings that have been quiet for 500 ms; return the next pending deadline.
    pub fn flush_ready(&self, now: Instant) -> Option<Duration> {
        self.flush_at(Some(now))
    }

    /// Exit (including signals and update restarts) emits the final values immediately.
    pub fn flush(&self) {
        self.flush_at(None);
    }

    fn flush_at(&self, now: Option<Instant>) -> Option<Duration> {
        let mut wait: Option<Duration> = None;
        let mut ready = |at: Instant| {
            if let Some(left) = now.and_then(|now| (at + QUIET).checked_duration_since(now)).filter(|left| !left.is_zero()) {
                wait = Some(wait.map_or(left, |wait| wait.min(left)));
                false
            } else {
                true
            }
        };
        let mut lines = Vec::new();
        self.settings.lock().unwrap_or_else(|e| e.into_inner()).retain(|path, change| {
            if !ready(change.at) {
                return true;
            }
            if change.old != change.new {
                change_line(path, &change.old, &change.new, change.input, &mut lines);
            }
            false
        });
        self.audio.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, change| {
            if !ready(change.at) {
                return true;
            }
            if change.old != change.new {
                let (old, new, input) = (change.old, change.new, change.input);
                lines.push(format!("settings: audio delay for the current output {old:+} ms → {new:+} ms ({input})"));
            }
            false
        });
        for line in lines {
            tracing::info!("{line}");
        }
        wait
    }
}

/// Future schema fields get a safe fallback automatically. User-controlled objects are
/// atomic: their keys (mpv options, output names, paths…) must never become log labels.
fn changes(old: &Config, new: &Config) -> Vec<(String, serde_json::Value, serde_json::Value)> {
    let old = serde_json::to_value(old).expect("config is serializable");
    let new = serde_json::to_value(new).expect("config is serializable");
    let mut out = Vec::new();
    for (section, fields) in new.as_object().unwrap() {
        for (field, value) in fields.as_object().unwrap() {
            let before = &old[section][field];
            if before == value || (section == "player" && field == "lip_sync") {
                continue;
            }
            let path = format!("{section}.{field}");
            if matches!(path.as_str(), "player.multiview" | "follow.news_flash") {
                for (key, next) in value.as_object().unwrap() {
                    let previous = &before[key];
                    if previous != next {
                        out.push((format!("{path}.{key}"), previous.clone(), next.clone()));
                    }
                }
            } else {
                out.push((path, before.clone(), value.clone()));
            }
        }
    }
    out
}

fn change_line(path: &str, before: &serde_json::Value, value: &serde_json::Value, input: &str, out: &mut Vec<String>) {
    let name = match path {
        "general.start_screen" => "start screen".to_owned(),
        "player.hwdec" => "hardware decoding".to_owned(),
        "parental.pin_hash" | "parental.pin_salt" => "parental PIN".to_owned(),
        _ => path.replace(['.', '_'], " "),
    };
    let show = |v: &serde_json::Value| -> Option<String> {
        match v {
            serde_json::Value::Bool(b) => Some(if *b { "On" } else { "Off" }.to_owned()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) => enum_value(path, s).map(str::to_owned),
            _ => None,
        }
    };
    let line = match (show(before), show(value)) {
        (Some(a), Some(b)) => format!("settings: {name} {a} → {b} ({input})"),
        _ => format!("settings: {name} changed ({input})"),
    };
    if !out.contains(&line) {
        out.push(line);
    }
}

fn enum_value<'a>(path: &str, value: &'a str) -> Option<&'a str> {
    if path == "general.start_screen" {
        return match value {
            "home" => Some("Home"),
            "live" => Some("Live TV"),
            "guide" => Some("Guide"),
            "last" => Some("Last used"),
            _ => None,
        };
    }
    // Even fields represented as strings are checked against known values. Arbitrary hand
    // edited strings cannot leak through a nominally enum-like setting such as hwdec.
    let allowed: &[&str] = match path {
        "player.hwdec" => &["auto", "auto-safe", "no", "vaapi", "nvdec", "vulkan"],
        "general.live_format" => &["auto", "ts", "m3u8"],
        "interface.sidebar" => &["auto", "expanded", "rail"],
        "interface.density" => &["comfortable", "compact"],
        "interface.live_opens_on" => &["all-channels", "favourites"],
        "player.sub_auto" => &["off", "forced", "always"],
        "player.popout_size" => &["small", "medium"],
        "player.multiview.layout" => &["auto", "side-by-side", "big-small", "pip"],
        "player.popout_corner" | "player.multiview.pip_corner" => &["bottom-right", "bottom-left", "top-right", "top-left"],
        "interface.live_sort" | "interface.movie_sort" | "interface.series_sort" => {
            &["provider", "number", "name-asc", "name-desc", "recently-added", "year", "rating"]
        }
        "interface.playlist_on_cards" => &["auto", "always", "never"],
        "interface.same_title" => &["combine", "show-each"],
        "interface.copy_order" => &["playlist-order", "best-quality"],
        "integration.idle_inhibit" => &["while-playing", "fullscreen", "never"],
        "agents.play_in" | "reminders.open_in" => &["pop-out", "full-window", "where-i-am"],
        "cast.convert" => &["auto", "never"],
        "cast.interlaced" => &["auto", "as-is"],
        _ => &[],
    };
    allowed.contains(&value).then_some(value)
}

#[cfg(test)]
pub(crate) fn capture(f: impl FnOnce()) -> String {
    use std::io::Write;
    use std::sync::Arc;
    #[derive(Clone)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let sink = Sink(bytes.clone());
    let subscriber =
        tracing_subscriber::fmt().without_time().with_ansi(false).with_max_level(tracing::Level::INFO).with_writer(move || sink.clone()).finish();
    tracing::subscriber::with_default(subscriber, f);
    String::from_utf8(bytes.lock().unwrap().clone()).unwrap()
}

/// Per-playlist preferences live in the store. Never derive a log label from the source's
/// name, URL, credentials or any user-controlled map key.
pub(crate) fn source_changes(old: &crate::model::Source, new: &crate::model::Source, input: &str) {
    for (changed, name) in [
        (old.name != new.name, "playlist name"),
        (old.icon != new.icon, "playlist icon"),
        (old.color != new.color, "playlist colour"),
        (old.url != new.url || old.path != new.path, "playlist location"),
        (old.xtream != new.xtream, "playlist credentials"),
    ] {
        if changed {
            tracing::info!("settings: {name} changed ({input})");
        }
    }
    if old.enabled != new.enabled {
        let (old, new) = (old.enabled, new.enabled);
        tracing::info!("settings: playlist enabled {old} → {new} ({input})");
    }
    let old = serde_json::to_value(&old.settings).expect("source settings serialize");
    let new = serde_json::to_value(&new.settings).expect("source settings serialize");
    for (field, value) in new.as_object().unwrap() {
        if &old[field] != value {
            let name = field.replace('_', " ");
            // Optional text, category maps and credentials are never formatted.
            match (&old[field], value) {
                (serde_json::Value::Bool(a), serde_json::Value::Bool(b)) => tracing::info!("settings: playlist {name} {a} → {b} ({input})"),
                (serde_json::Value::Number(a), serde_json::Value::Number(b)) => tracing::info!("settings: playlist {name} {a} → {b} ({input})"),
                _ => tracing::info!("settings: playlist {name} changed ({input})"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_settings_batch_independently_after_500_ms_and_flush_on_exit() {
        let pending = Pending::default();
        let start = Instant::now();
        let old = Config::default();
        let mut first = old.clone();
        first.player.cache_secs += 1;
        first.general.start_screen = super::super::StartScreen::Live;
        first.player.multiview.layout = super::super::MultiViewLayout::Pip;
        let mut last = first.clone();
        last.player.cache_secs += 20;
        last.general.start_screen = super::super::StartScreen::Guide;
        let presses = capture(|| {
            pending.record_at(&old, &first, "key →", start);
            pending.record_at(&first, &last, "key →", start + Duration::from_millis(400));
            pending.flush_ready(start + Duration::from_millis(499));
        });
        assert!(presses.is_empty(), "{presses}");
        let layout = capture(|| {
            pending.flush_ready(start + Duration::from_millis(500));
        });
        assert_eq!(layout.lines().count(), 1, "{layout}");
        assert!(layout.contains("player multiview layout auto → pip"), "{layout}");
        assert!(
            capture(|| {
                pending.flush_ready(start + Duration::from_millis(899));
            })
            .is_empty()
        );
        let settings = capture(|| {
            pending.flush_ready(start + Duration::from_millis(900));
        });
        assert_eq!(settings.lines().count(), 2, "{settings}");
        assert!(settings.contains("start screen Home → Guide (key →)"), "{settings}");
        assert!(settings.contains(&format!("player cache secs {} → {}", old.player.cache_secs, last.player.cache_secs)), "{settings}");
        assert!(capture(|| pending.flush()).is_empty());
        let exit = capture(|| {
            pending.record_at(&last, &first, "key", start + Duration::from_millis(950));
            pending.flush();
        });
        assert_eq!(exit.lines().count(), 2, "{exit}");
        let no_change = capture(|| {
            pending.record_at(&old, &first, "key", start);
            pending.record_at(&first, &old, "key", start + Duration::from_millis(100));
            pending.flush();
        });
        assert!(no_change.is_empty(), "{no_change}");
    }

    #[test]
    fn redacts_text_maps_devices_and_pin_even_on_restore() {
        let old = Config::default();
        let mut new = old.clone();
        new.general.start_screen = super::super::StartScreen::Live;
        new.player.user_agent = "secret URL username password".into();
        new.player.mpv_options.insert("private-title".into(), "private-device".into());
        new.parental.pin_hash = Some("private-pin".into());
        new.parental.pin_salt = Some("private-salt".into());
        let log = capture(|| {
            let pending = Pending::default();
            pending.record(&old, &new, "restore");
            pending.flush();
        });
        assert!(log.contains("INFO"));
        assert!(log.contains("settings: start screen Home → Live TV (restore)"));
        assert!(log.contains("player user agent changed (restore)"));
        assert_eq!(log.matches("parental PIN changed").count(), 1);
        for secret in ["secret URL", "private-title", "private-device", "private-pin", "private-salt"] {
            assert!(!log.contains(secret), "{log}");
        }
        assert!(capture(|| Pending::default().record(&new, &new, "click")).is_empty());
    }
}
