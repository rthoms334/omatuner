//! File dialogs through the XDG portal (`rfd` xdg-portal backend → xdg-desktop-portal-gtk,
//! auto-floated by Omarchy, follows prefer-dark/light). Owner: WP-08.
//! Async on the runtime; result delivered as `PlatformEvent::FilePicked`.
//!
//! rfd's portal calls block until the user answers, so the dialog runs on the blocking pool
//! (never on the async workers or the UI thread). Without a portal rfd falls back to zenity
//! when installed; otherwise the result is `None` (same as cancel).

use super::PickPurpose;
use crate::msg::{AppEvent, EventBus};
use crate::platform::PlatformEvent;

/// What one purpose's dialog looks like.
#[derive(Debug, Clone, PartialEq)]
struct DialogSpec {
    title: &'static str,
    /// (name, extensions without dot)
    filters: Vec<(&'static str, &'static [&'static str])>,
    /// `Some(default file name)` = save dialog.
    save_as: Option<String>,
}

/// Open a picker for `purpose` (filters: `.m3u/.m3u8` for playlists, `.xml/.gz/.xz` for EPG,
/// `.json` for backups; save dialog for exports with a dated default name).
pub fn pick(rt: &tokio::runtime::Handle, bus: EventBus, purpose: PickPurpose) {
    let spec = spec(purpose, &today());
    rt.spawn_blocking(move || {
        let path = run(&spec);
        tracing::debug!("file picker {purpose:?}: {}", if path.is_some() { "picked" } else { "cancelled" });
        bus.send(AppEvent::Platform(PlatformEvent::FilePicked { purpose, path }));
    });
}

fn run(spec: &DialogSpec) -> Option<std::path::PathBuf> {
    let mut d = rfd::FileDialog::new().set_title(spec.title);
    for (name, exts) in &spec.filters {
        d = d.add_filter(*name, exts);
    }
    match &spec.save_as {
        Some(name) => d.set_file_name(name).save_file(),
        None => d.pick_file(),
    }
}

/// Local date `YYYY-MM-DD` for default export names.
fn today() -> String { jiff::Zoned::now().strftime("%Y-%m-%d").to_string() }

fn spec(purpose: PickPurpose, date: &str) -> DialogSpec {
    const ALL: (&str, &[&str]) = ("All files", &["*"]);
    match purpose {
        PickPurpose::OpenPlaylist => DialogSpec {
            title: "Open playlist",
            filters: vec![("M3U playlists", &["m3u", "m3u8"]), ALL],
            save_as: None,
        },
        PickPurpose::OpenEpg => DialogSpec {
            title: "Open TV guide (XMLTV)",
            filters: vec![("XMLTV guides", &["xml", "gz", "xz"]), ALL],
            save_as: None,
        },
        PickPurpose::ImportBackup => DialogSpec {
            title: "Restore Tuner backup",
            filters: vec![("Tuner backups", &["json"]), ALL],
            save_as: None,
        },
        PickPurpose::ExportBackup => DialogSpec {
            title: "Save Tuner backup",
            filters: vec![("Tuner backups", &["json"])],
            save_as: Some(format!("tuner-backup-{date}.json")),
        },
        PickPurpose::ExportLog => DialogSpec {
            title: "Save log",
            filters: vec![("Text files", &["txt", "log"])],
            save_as: Some(format!("tuner-log-{date}.txt")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_per_purpose() {
        let p = spec(PickPurpose::OpenPlaylist, "2026-10-10");
        assert_eq!(p.filters[0].1, &["m3u", "m3u8"]);
        assert_eq!(p.save_as, None);
        assert_eq!(spec(PickPurpose::OpenEpg, "x").filters[0].1, &["xml", "gz", "xz"]);
        assert_eq!(spec(PickPurpose::ImportBackup, "x").filters[0].1, &["json"]);
        assert_eq!(spec(PickPurpose::ExportBackup, "2026-10-10").save_as.as_deref(), Some("tuner-backup-2026-10-10.json"));
        assert_eq!(spec(PickPurpose::ExportLog, "2026-10-10").save_as.as_deref(), Some("tuner-log-2026-10-10.txt"));
    }

    #[test]
    fn today_is_an_iso_date() {
        let d = today();
        assert_eq!(d.len(), 10);
        assert!(d.chars().enumerate().all(|(i, c)| if i == 4 || i == 7 { c == '-' } else { c.is_ascii_digit() }), "{d}");
    }
}
