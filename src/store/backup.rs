//! Export / import of all user state as one JSON document (UHF backup; DESIGN §19).
//! Owner: WP-02.
//!
//! Format: `tuner-backup.json`, `{"format":"tuner-backup","version":1,...}`. Catalogue rows
//! are NOT exported (they come back on refresh); user data carries fingerprints so it
//! re-attaches against the freshly imported catalogue. Credentials are included only when the
//! user ticks "Include passwords" (default off); the file is written 0600 either way.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::model::{
    FavouriteEntry, FavouriteList, GroupOverrideRow, HistoryEntry, ItemOverrideRow, Reminder, SourceColor, SourceKind,
    SourceSettings, Timestamp, WatchProgress,
};

pub const FORMAT: &str = "tuner-backup";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackupOptions {
    pub include_credentials: bool,
    pub include_history: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupSource {
    /// Original id; used only to re-map references inside this file.
    pub id: i64,
    pub name: String,
    pub kind: SourceKind,
    pub icon: String,
    pub color: SourceColor,
    pub url: Option<String>,
    pub path: Option<String>,
    pub xt_server: Option<String>,
    pub xt_username: Option<String>,
    pub xt_password: Option<String>,
    pub settings: SourceSettings,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupEpgSource {
    pub name: String,
    pub kind: String,
    pub url: Option<String>,
    pub path: Option<String>,
    pub priority: i32,
    pub offset_min: i32,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupManualMapping {
    pub source_id: i64,
    pub channel_key: String,
    /// Matched by name on import (EPG source ids differ between installs).
    pub epg_source_name: String,
    pub xmltv_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    pub format: String,
    pub version: u32,
    pub created_at: Timestamp,
    pub app_version: String,
    pub sources: Vec<BackupSource>,
    pub epg_sources: Vec<BackupEpgSource>,
    pub fav_lists: Vec<FavouriteList>,
    pub fav_items: Vec<FavouriteEntry>,
    pub item_overrides: Vec<ItemOverrideRow>,
    pub group_overrides: Vec<GroupOverrideRow>,
    pub progress: Vec<WatchProgress>,
    pub history: Vec<HistoryEntry>,
    pub reminders: Vec<Reminder>,
    pub manual_mappings: Vec<BackupManualMapping>,
    /// Raw `config.toml` text.
    pub config: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreMode {
    /// Add sources/lists that don't exist (matched by name+kind+url), merge user data.
    Merge,
    /// Wipe all sources and user data first.
    Replace,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RestoreReport {
    pub sources_added: u32,
    pub sources_matched: u32,
    pub user_rows: u32,
    /// Sources restored without credentials (user must re-enter them).
    pub needs_credentials: Vec<String>,
}

pub fn export(conn: &Connection, opts: BackupOptions, config_toml: Option<String>, now: Timestamp) -> Result<Backup> {
    todo!("WP-02")
}

/// Write pretty JSON with mode 0600.
pub fn write_file(path: &Path, b: &Backup) -> Result<()> { todo!("WP-02") }

/// Read + validate `format`/`version`.
pub fn read_file(path: &Path) -> Result<Backup> { todo!("WP-02") }

/// Restore into the DB in one transaction. The caller then schedules a refresh of every
/// restored source (user data re-attaches on that import).
pub fn restore(conn: &mut Connection, b: &Backup, mode: RestoreMode) -> Result<RestoreReport> { todo!("WP-02") }
