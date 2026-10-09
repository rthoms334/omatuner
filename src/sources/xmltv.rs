//! Streaming XMLTV parser (quick-xml 0.42). Owner: WP-05a. Spec: protocols.md §3.
//!
//! quick-xml 0.42 notes (verified by the protocols prototype):
//! * names are `&str` (`"programme"`), attributes via `normalized_value(XmlVersion::Implicit1_0)`;
//! * entities arrive as separate `Event::GeneralRef` → resolve `amp lt gt quot apos` + char refs
//!   yourself; NEVER enable `trim_text` (it eats spaces around entities); trim after joining;
//! * reuse one `Vec<u8>` buffer (`buf.clear()` per event).
//!
//! Measured parse: 415k programmes in 0.61 s; the DB insert dominates (see jobs::import_epg).
//!
//! Multi-language: the first `<title>` whose `lang` matches the preferred list wins, else the
//! first title. Programmes with unparsable start → skipped (counted); missing stop → filled
//! with the next programme's start on the same channel by the caller (store does not need it).

use std::io::BufRead;
use std::ops::ControlFlow;

use crate::error::Result;
use crate::model::{EpisodeNum, Timestamp};
use crate::store::epg::{ImportEpgChannel, ImportProgramme};

#[derive(Debug, Clone, PartialEq)]
pub enum XmltvItem {
    Channel(ImportEpgChannel),
    Programme(ImportProgramme),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XmltvStats {
    pub channels: u64,
    pub programmes: u64,
    pub skipped: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct XmltvOptions {
    /// ISO-639-1 codes in preference order for multi-language titles/descriptions.
    pub preferred_langs: Vec<String>,
    /// Skip programmes ending before this (saves DB work on huge archives).
    pub min_stop: Option<Timestamp>,
    /// Skip programmes starting after this (e.g. now + 14 days).
    pub max_start: Option<Timestamp>,
}

/// Parse `reader`, calling `sink` for each item; `sink` may return `Break` to cancel (then
/// `Err(Error::Cancelled)`). `progress(bytes)` is called every ~1 MiB of input.
pub fn parse<R: BufRead>(
    reader: R,
    opts: &XmltvOptions,
    sink: &mut dyn FnMut(XmltvItem) -> ControlFlow<()>,
    progress: &mut dyn FnMut(u64),
) -> Result<XmltvStats> {
    todo!("WP-05a")
}

/// `YYYYMMDDhhmmss ±hhmm` or any leading prefix (`YYYYMMDDhhmm`, `YYYYMMDD`), optional space,
/// zone `±hhmm` / `Z` / `UTC` / `GMT` / `BST` (+0100); no zone → UTC. Hand-written, no alloc.
pub fn parse_time(s: &str) -> Option<Timestamp> { todo!("WP-05a") }

/// `xmltv_ns` (`s.e.p`, zero-based, each optionally `x/y`) or `onscreen` (`S03E14`, `3x14`).
pub fn parse_episode_num(system: Option<&str>, value: &str) -> Option<EpisodeNum> { todo!("WP-05a") }
