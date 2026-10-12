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
    pub fn ensure_cast_dirs(&self) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        for d in [self.cast_live_dir(), self.cast_vod_dir(), self.cast_trace_dir()] {
            std::fs::create_dir_all(&d).map_err(|e| Error::io(&d, e))?;
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).map_err(|e| Error::io(&d, e))?;
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
