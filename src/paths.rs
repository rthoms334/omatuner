//! XDG paths. Implemented in the skeleton (small, shared by everyone).
//!
//! | what      | path                                   |
//! |-----------|----------------------------------------|
//! | config    | `~/.config/tuner/config.toml`          |
//! | database  | `~/.local/share/tuner/tuner.db` (0600) |
//! | cache     | `~/.cache/tuner/{img,epg,playlists}/` (epg, playlists: 0700) |
//! | logs      | `~/.local/state/tuner/tuner.log`       |
//! | runtime   | `$XDG_RUNTIME_DIR/tuner/` (IPC socket) |
//! | casting   | `$XDG_RUNTIME_DIR/tuner/cast/` (live HLS, tmpfs), `~/.cache/tuner/cast/` (VOD HLS), `~/.local/state/tuner/cast-trace/` (dev) — all 0700 |
//! | omarchy   | `~/.local/state/omarchy/current/`      |

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub home: PathBuf,
}

impl AppPaths {
    /// Resolve from the environment (XDG vars honoured via `directories`).
    pub fn from_env() -> Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", crate::APP_ID).ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
        let base = directories::BaseDirs::new().ok_or_else(|| Error::Config("cannot determine home directory".into()))?;
        let home = base.home_dir().to_path_buf();
        let state_dir = dirs.state_dir().map(Path::to_path_buf).unwrap_or_else(|| home.join(".local/state").join(crate::APP_ID));
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join(crate::APP_ID);
        Ok(Self {
            config_dir: dirs.config_dir().to_path_buf(),
            data_dir: dirs.data_dir().to_path_buf(),
            cache_dir: dirs.cache_dir().to_path_buf(),
            state_dir,
            runtime_dir,
            home,
        })
    }

    /// App-owned paths rooted in `root` (tests, `--profile-dir`). `home` stays the real home
    /// (the Omarchy theme is system state); tests that need a fake home overwrite the field.
    pub fn rooted(root: &Path) -> Self {
        let home = directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf()).unwrap_or_else(|| root.join("home"));
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            cache_dir: root.join("cache"),
            state_dir: root.join("state"),
            runtime_dir: root.join("run"),
            home,
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    /// Private media, numbered by download id (DESIGN §43 §4).
    pub fn downloads(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("tuner.db")
    }
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("tuner.log")
    }
    pub fn image_cache(&self) -> PathBuf {
        self.cache_dir.join("img")
    }
    pub fn epg_cache(&self) -> PathBuf {
        self.cache_dir.join("epg")
    }
    pub fn playlist_cache(&self) -> PathBuf {
        self.cache_dir.join("playlists")
    }
    pub fn ipc_socket(&self) -> PathBuf {
        self.runtime_dir.join("tuner.sock")
    }

    /// Live casting sessions' HLS output, one directory per session (tmpfs; DESIGN §40 §8.4).
    pub fn cast_live_dir(&self) -> PathBuf {
        self.runtime_dir.join("cast")
    }
    /// Catch-up and VOD casting sessions' HLS output (on disk: a film doesn't fit in tmpfs).
    pub fn cast_vod_dir(&self) -> PathBuf {
        self.cache_dir.join("cast")
    }
    /// `TUNER_CAST_TRACE=1` transcripts (dev only, redacted; DESIGN §40 §7.3).
    pub fn cast_trace_dir(&self) -> PathBuf {
        self.state_dir.join("cast-trace")
    }

    /// Create the casting directories (0700). The cast service calls it when it starts; it
    /// also sweeps them (sessions left by a crash; the trace keeps its last 5 files).
    ///
    /// Without `XDG_RUNTIME_DIR` the runtime directory is `/tmp/tuner`, where another local user
    /// could have made it first, with a `cast` symlink into this user's files that the sweep
    /// would empty. So nothing is created, chmodded or swept unless the runtime directory is a
    /// real directory of this user that nobody else can enter, and each casting directory a real
    /// directory of this user (not a symlink); otherwise `Error::Config`, and casting is off.
    pub fn ensure_cast_dirs(&self) -> Result<()> {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let run = &self.runtime_dir;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(run).map_err(|e| Error::io(run, e))?;
        own_dir(run, true)?;
        for d in [self.cast_live_dir(), self.cast_vod_dir(), self.cast_trace_dir()] {
            std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
            own_dir(&d, false)?;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(&d, e))?;
        }
        // no session runs before the service: whatever is here a crash left (the relay's
        // `<session>/p<n>` directories); entries are never followed through a symlink
        for root in [self.cast_live_dir(), self.cast_vod_dir()] {
            for entry in std::fs::read_dir(&root).map_err(|e| Error::io(&root, e))?.flatten() {
                let gone = match entry.file_type() {
                    Ok(t) if t.is_dir() => std::fs::remove_dir_all(entry.path()),
                    _ => std::fs::remove_file(entry.path()),
                };
                if let Err(e) = gone {
                    tracing::debug!("cast: can't sweep a leftover session: {e}");
                }
            }
        }
        // the dev trace keeps its newest 5 (`cast-<unix time>-<hex>.jsonl` sort by age)
        let trace = self.cast_trace_dir();
        let mut files: Vec<_> = std::fs::read_dir(&trace)
            .map_err(|e| Error::io(&trace, e))?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("cast-") && n.ends_with(".jsonl")))
            .collect();
        files.sort();
        for old in files.iter().rev().skip(5) {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }

    /// `~/.local/state/omarchy/current` — watched (non-recursively) for theme changes.
    pub fn omarchy_current(&self) -> PathBuf {
        self.home.join(".local/state/omarchy/current")
    }
    pub fn omarchy_theme_dir(&self) -> PathBuf {
        self.omarchy_current().join("theme")
    }
    /// `~/.config/omarchy` — user `shell.toml` override lives here.
    pub fn omarchy_config(&self) -> PathBuf {
        self.home.join(".config/omarchy")
    }
    pub fn fontconfig_dir(&self) -> PathBuf {
        self.home.join(".config/fontconfig")
    }

    /// Create every app-owned directory. Data/state/runtime dirs and the playlist/EPG caches get
    /// mode 0700 because they hold credentials (DB; `get.php`/`xmltv.php` downloads carry
    /// user/password in every URL) and the IPC socket. Images are public data.
    pub fn ensure(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in [&self.config_dir, &self.cache_dir, &self.image_cache()] {
            std::fs::create_dir_all(d).map_err(|e| Error::io(d, e))?;
        }
        for d in [&self.data_dir, &self.state_dir, &self.runtime_dir, &self.epg_cache(), &self.playlist_cache()] {
            std::fs::create_dir_all(d).map_err(|e| Error::io(d, e))?;
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(d, e))?;
        }
        Ok(())
    }
}

/// `dir` itself (not followed) is a directory owned by this user; `private`: with no group or
/// other permissions either.
fn own_dir(dir: &Path, private: bool) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(dir).map_err(|e| Error::io(dir, e))?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let me = unsafe { libc::geteuid() };
    if !m.file_type().is_dir() || m.uid() != me || (private && m.mode() & 0o077 != 0) {
        return Err(Error::Config(format!("{} isn't a private directory of this user", dir.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Startup sweep (DESIGN §40 §13): a crash's session directories go, the trace keeps its
    /// newest 5, and nothing outside the casting directories is touched through a symlink.
    #[test]
    fn the_cast_sweep_clears_leftovers_and_keeps_five_traces() {
        let tmp = tempfile::tempdir().unwrap();
        let p = AppPaths::rooted(tmp.path());
        p.ensure_cast_dirs().unwrap();
        let outside = tmp.path().join("keep");
        std::fs::create_dir_all(outside.join("inner")).unwrap();
        std::fs::create_dir_all(p.cast_live_dir().join("ab12/p1")).unwrap();
        std::fs::write(p.cast_vod_dir().join("stray.ts"), b"x").unwrap();
        std::os::unix::fs::symlink(&outside, p.cast_vod_dir().join("link")).unwrap();
        for t in 0..7 {
            std::fs::write(p.cast_trace_dir().join(format!("cast-170000000{t}-00{t}a.jsonl")), b"{}").unwrap();
        }
        std::fs::write(p.cast_trace_dir().join("notes.txt"), b"mine").unwrap();

        p.ensure_cast_dirs().unwrap();
        let names = |d: PathBuf| {
            let mut v: Vec<_> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
            v.sort();
            v
        };
        assert!(names(p.cast_live_dir()).is_empty());
        assert!(names(p.cast_vod_dir()).is_empty());
        assert!(outside.join("inner").is_dir(), "a symlink is removed, not followed");
        let traces = names(p.cast_trace_dir());
        assert_eq!(traces.len(), 6, "{traces:?}");
        assert_eq!(traces.first().map(String::as_str), Some("cast-1700000002-002a.jsonl"));
        assert!(traces.contains(&"notes.txt".to_owned()));
    }

    /// A runtime directory others can enter (the `/tmp` fallback made by another user), or a
    /// casting directory that is a symlink, is refused before anything is chmodded or swept.
    #[test]
    fn a_foreign_runtime_dir_or_a_symlinked_cast_dir_is_never_swept() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = AppPaths::rooted(tmp.path());
        let victim = tmp.path().join("documents");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("thesis.odt"), b"mine").unwrap();
        let mode = || std::fs::metadata(&victim).unwrap().permissions().mode();
        let before = mode();
        std::fs::create_dir_all(&p.runtime_dir).unwrap();
        std::fs::set_permissions(&p.runtime_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::os::unix::fs::symlink(&victim, p.cast_live_dir()).unwrap();
        assert!(matches!(p.ensure_cast_dirs(), Err(Error::Config(_))));
        std::fs::set_permissions(&p.runtime_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(p.ensure_cast_dirs(), Err(Error::Config(_))), "the symlinked cast dir");
        assert!(victim.join("thesis.odt").is_file());
        assert_eq!(mode(), before, "not chmodded through the symlink");
    }
}
