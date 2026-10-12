//! The guide-channel catalogue (names, fuzzy indexes), moved out of the mapping screen: one
//! shared, lazily built `Arc<Catalog>` keyed by the guides' active generations and dropped
//! after 2 min unused ([`shared`]; the screen, the guide check and the agent share it).
//! OWNER: G-core.
//!
//! Memory: about 15–25 MB at 30k guide channels (norms, trigram postings), so it is never kept
//! twice and never kept idle.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::error::Result;
use crate::model::{EpgChannelId, EpgSourceId, EpgSuggestion, GuideTarget};
use crate::sources::epg_match::{EpgCandidateIndex, build_index, suggest};

/// A catalogue unused this long is dropped.
pub const IDLE_DROP: Duration = Duration::from_secs(120);

/// One guide channel of an enabled guide.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogEntry {
    pub id: EpgChannelId,
    pub source: EpgSourceId,
    pub xmltv_id: String,
    /// Its first display name (the xmltv id when it has none).
    pub name: String,
    /// The guide's name.
    pub source_name: String,
}

/// Every guide channel of the enabled guides, plus the fuzzy candidate indexes.
#[derive(Debug, Default)]
pub struct Catalog {
    /// The guides' active generations it was built from.
    pub generations: Vec<(EpgSourceId, i64)>,
    pub entries: Vec<CatalogEntry>,
    /// Filter haystacks of `entries` ("name xmltv-id guide").
    pub hay: Vec<String>,
    pub by_id: HashMap<EpgChannelId, usize>,
    /// One per guide, in guide priority order.
    pub indexes: Vec<EpgCandidateIndex>,
}

impl Catalog {
    /// Build it from the store (one pass per enabled guide).
    pub fn load(conn: &Connection) -> Result<Catalog> {
        let mut cat = Catalog { generations: generations(conn)?, ..Default::default() };
        for src in crate::store::sources::list_epg_sources(conn)?.into_iter().filter(|s| s.enabled) {
            let chans = crate::store::epg::epg_channels(conn, src.id)?;
            for ch in &chans {
                let name = ch.display_names.first().cloned().unwrap_or_else(|| ch.xmltv_id.clone());
                cat.by_id.insert(ch.id, cat.entries.len());
                cat.entries.push(CatalogEntry { id: ch.id, source: src.id, xmltv_id: ch.xmltv_id.clone(), name, source_name: src.name.clone() });
            }
            cat.indexes.push(build_index(src.id, &chans));
        }
        cat.hay = cat.entries.iter().map(|e| format!("{} {} {}", e.name, e.xmltv_id, e.source_name)).collect();
        Ok(cat)
    }

    /// The best `limit` fuzzy candidates for `name` across the guides (`except` one), best
    /// first.
    pub fn suggest(&self, name: &str, limit: usize, threshold: f64, except: Option<EpgSourceId>) -> Vec<EpgSuggestion> {
        let mut all: Vec<EpgSuggestion> = self
            .indexes
            .iter()
            .filter(|i| i.epg_source.is_some() && i.epg_source != except)
            .flat_map(|idx| suggest(idx, name, limit, threshold))
            .collect();
        all.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.epg_channel_id.cmp(&b.epg_channel_id)));
        all.truncate(limit);
        all
    }

    /// The guide channel of `epg_source` whose id is `xmltv_id` ignoring case, else ignoring
    /// its `@Feed` (a guide that renamed the id).
    pub fn find_id(&self, epg_source: EpgSourceId, xmltv_id: &str) -> Option<GuideTarget> {
        let idx = self.indexes.iter().find(|i| i.epg_source == Some(epg_source))?;
        let (id, _) = idx.find_id(xmltv_id)?;
        self.target(id)
    }

    /// A guide channel as a fix target.
    pub fn target(&self, id: EpgChannelId) -> Option<GuideTarget> {
        let e = &self.entries[*self.by_id.get(&id)?];
        Some(GuideTarget { epg_source: e.source, xmltv_id: e.xmltv_id.clone(), name: e.name.clone() })
    }

    /// The guide's name of a guide channel ("TV guide").
    pub fn guide_name(&self, source: EpgSourceId) -> Option<&str> {
        self.entries.iter().find(|e| e.source == source).map(|e| e.source_name.as_str())
    }
}

/// The active generation of every enabled guide (the catalogue's key).
pub fn generations(conn: &Connection) -> Result<Vec<(EpgSourceId, i64)>> {
    let mut stmt = conn.prepare_cached("SELECT id, active_gen FROM epg_source WHERE enabled = 1 ORDER BY id")?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

struct Slot {
    /// The database it was built from (one per process in the app; tests open several).
    db: String,
    cat: Arc<Catalog>,
    used: Instant,
}

static SHARED: Mutex<Option<Slot>> = Mutex::new(None);

fn slot() -> std::sync::MutexGuard<'static, Option<Slot>> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The shared catalogue for the guides as they are now: the cached one while the guides'
/// generations are unchanged, else a fresh build (off the UI thread: it takes 0.2–1 s at 30k
/// guide channels). A reaper thread drops it [`IDLE_DROP`] after its last use.
pub fn shared(conn: &Connection) -> Result<Arc<Catalog>> {
    let gens = generations(conn)?;
    let db = conn.path().unwrap_or_default().to_owned();
    if let Some(s) = slot().as_mut()
        && s.cat.generations == gens
        && s.db == db
    {
        s.used = Instant::now();
        return Ok(s.cat.clone());
    }
    let cat = Arc::new(Catalog::load(conn)?);
    let start_reaper = {
        let mut g = slot();
        let first = g.is_none();
        *g = Some(Slot { db, cat: cat.clone(), used: Instant::now() });
        first
    };
    if start_reaper {
        let spawned = std::thread::Builder::new().name("guide-catalog".into()).spawn(|| {
            loop {
                std::thread::sleep(IDLE_DROP / 4);
                let mut g = slot();
                if g.as_ref().is_none_or(|s| s.used.elapsed() >= IDLE_DROP) {
                    // dropped here, off the UI thread
                    *g = None;
                    return;
                }
            }
        });
        if spawned.is_err() {
            tracing::debug!("guide catalogue: no reaper thread; kept until replaced");
        }
    }
    Ok(cat)
}

/// Forget the shared catalogue now (a guide was removed or disabled).
pub fn invalidate() {
    let old = slot().take();
    drop(old);
}
