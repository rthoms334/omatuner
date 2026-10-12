//! Playlist channel ↔ EPG channel matching. Owner: WP-05a. DESIGN §9.4.
//!
//! Automatic steps, first hit wins (only steps 1–4 are applied automatically; step 5 only
//! *suggests*):
//! 1. `tvg-id` == `xmltv_id` (exact), then case-insensitive.
//! 2. Xtream `epg_channel_id` against the same source's provider guide (same as 1, but tagged
//!    `XtreamEpgId`).
//! 3. iptv-org ids: strip `@Feed` (`CNN.us@SD` → `CNN.us`) on either side and retry.
//! 4. `normalize_name(tvg-name or name)` equals a normalized `display-name` — only if exactly one
//!    epg channel in that source has that norm (ambiguity → no match) and
//!    `config.epg.auto_name_match` is on.
//! 5. Fuzzy (Jaro-Winkler on norms, ≥ `config.epg.fuzzy_threshold`) → [`suggest`] for the
//!    "Assign EPG" picker; the user's choice is stored as `Manual`.
//!
//! Manual mappings are never touched by automatic matching.
//!
//! Tie-breaks: several EPG channels for one id (case-insensitive or feed-stripped) → the first
//! in index order (`channels` order passed to [`build_index`]). Step 2 applies to Xtream
//! channels (`xt:` item keys) when `provider_guide` is set; their `tvg_id` holds the panel's
//! `epg_channel_id`.
//!
//! Fuzzy prefilter: every distinct display-name norm is indexed by its padded character
//! trigrams (`^ab`, `abc`, …, `yz$`); [`suggest`] only scores norms that share at least a
//! quarter of the query's trigrams, best overlap first, at most 512 of them — never a full
//! Jaro-Winkler scan.

use std::collections::{HashMap, HashSet};

use crate::model::{ChannelId, EpgChannel, EpgChannelId, EpgMapping, EpgMatchMethod, EpgSourceId, EpgSuggestion, ItemKey, SourceId};
use crate::sources::identity::normalize_name;
use crate::store::epg::ChannelMatchRow;

/// Upper bound of Jaro-Winkler evaluations per [`suggest`] call.
const MAX_FUZZY_CANDIDATES: usize = 512;

/// Lookup structures for one EPG source.
#[derive(Debug, Default)]
pub struct EpgCandidateIndex {
    pub epg_source: Option<EpgSourceId>,
    by_id: HashMap<String, (EpgChannelId, String)>,
    by_id_lower: HashMap<String, Vec<(EpgChannelId, String)>>,
    by_id_feedless: HashMap<String, Vec<(EpgChannelId, String)>>,
    by_norm: HashMap<String, Vec<(EpgChannelId, String)>>,
    /// (norm, epg channel id, xmltv id, display name) for fuzzy scans.
    norms: Vec<(String, EpgChannelId, String, String)>,
    /// Padded trigram → indices into `norms` (fuzzy prefilter).
    grams: HashMap<u32, Vec<u32>>,
}

/// Build the index for `channels` of `epg_source`.
pub fn build_index(epg_source: EpgSourceId, channels: &[EpgChannel]) -> EpgCandidateIndex {
    let mut idx = EpgCandidateIndex { epg_source: Some(epg_source), ..Default::default() };
    for ch in channels {
        let id = ch.xmltv_id.trim();
        if id.is_empty() {
            continue;
        }
        let entry = (ch.id, id.to_owned());
        idx.by_id.entry(id.to_owned()).or_insert_with(|| entry.clone());
        idx.by_id_lower.entry(id.to_lowercase()).or_default().push(entry.clone());
        idx.by_id_feedless.entry(strip_feed(id).trim().to_lowercase()).or_default().push(entry.clone());

        let mut seen: HashSet<String> = HashSet::new();
        for name in &ch.display_names {
            let norm = normalize_name(name);
            if norm.is_empty() || !seen.insert(norm.clone()) {
                continue;
            }
            idx.by_norm.entry(norm.clone()).or_default().push(entry.clone());
            let pos = idx.norms.len() as u32;
            for g in trigrams(&norm) {
                let posting = idx.grams.entry(g).or_default();
                if posting.last() != Some(&pos) {
                    posting.push(pos);
                }
            }
            idx.norms.push((norm, ch.id, id.to_owned(), name.trim().to_owned()));
        }
    }
    idx
}

/// Automatic match for one channel. `provider_guide` = this EPG source is the provider guide of
/// the channel's own playlist source (enables step 2 tagging).
pub fn match_channel(idx: &EpgCandidateIndex, row: &ChannelMatchRow, provider_guide: bool, name_match: bool) -> Option<EpgMapping> {
    let epg_source_id = idx.epg_source?;
    let mapping = |xmltv_id: &str, method: EpgMatchMethod| EpgMapping {
        channel: row.channel.clone(),
        epg_source_id,
        xmltv_id: xmltv_id.to_owned(),
        method,
        confidence: 1.0,
    };
    let xtream_guide = provider_guide && row.channel.key.as_str().starts_with("xt:");
    if let Some(tvg) = row.tvg_id.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        if let Some((_, xid)) = idx.by_id.get(tvg) {
            return Some(mapping(xid, if xtream_guide { EpgMatchMethod::XtreamEpgId } else { EpgMatchMethod::TvgIdExact }));
        }
        if let Some((_, xid)) = idx.by_id_lower.get(&tvg.to_lowercase()).and_then(|v| v.first()) {
            return Some(mapping(xid, if xtream_guide { EpgMatchMethod::XtreamEpgId } else { EpgMatchMethod::TvgIdCaseless }));
        }
        let feedless = strip_feed(tvg).trim().to_lowercase();
        if !feedless.is_empty()
            && let Some((_, xid)) = idx.by_id_feedless.get(&feedless).and_then(|v| v.first())
        {
            return Some(mapping(xid, EpgMatchMethod::FeedStripped));
        }
    }
    if name_match {
        let computed;
        let norm = if row.norm_name.is_empty() {
            computed = normalize_name(&row.name);
            &computed
        } else {
            &row.norm_name
        };
        if let Some([(_, xid)]) = idx.by_norm.get(norm.as_str()).map(Vec::as_slice) {
            return Some(mapping(xid, EpgMatchMethod::NormalizedName));
        }
    }
    None
}

/// Match all rows (parallel-safe; pure). Returns only matched rows.
pub fn match_all(idx: &EpgCandidateIndex, rows: &[ChannelMatchRow], provider_guide: bool, name_match: bool) -> Vec<EpgMapping> {
    rows.iter().filter_map(|r| match_channel(idx, r, provider_guide, name_match)).collect()
}

/// Top `limit` fuzzy candidates for `name` with score ≥ `threshold`, best first.
///
/// One entry per EPG channel (its best-scoring display name); ties are ordered by display name.
pub fn suggest(idx: &EpgCandidateIndex, name: &str, limit: usize, threshold: f64) -> Vec<EpgSuggestion> {
    let Some(epg_source_id) = idx.epg_source else { return Vec::new() };
    let norm = normalize_name(name);
    if norm.is_empty() || limit == 0 {
        return Vec::new();
    }
    let mut best: HashMap<EpgChannelId, (f64, usize)> = HashMap::new();
    for i in prefilter(idx, &norm) {
        let (cand, ch, _, _) = &idx.norms[i];
        let score = strsim::jaro_winkler(&norm, cand);
        if score < threshold {
            continue;
        }
        let slot = best.entry(*ch).or_insert((score, i));
        if score > slot.0 {
            *slot = (score, i);
        }
    }
    let mut out: Vec<EpgSuggestion> = best
        .into_values()
        .map(|(score, i)| {
            let (_, ch, xid, display) = &idx.norms[i];
            EpgSuggestion { epg_channel_id: *ch, epg_source_id, xmltv_id: xid.clone(), display_name: display.clone(), score: score as f32 }
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.display_name.cmp(&b.display_name)).then(a.epg_channel_id.cmp(&b.epg_channel_id)));
    out.truncate(limit);
    out
}

/// Indices into `idx.norms` sharing at least a quarter of `norm`'s padded trigrams (at least
/// one), most shared first, capped at [`MAX_FUZZY_CANDIDATES`]. Strings at Jaro-Winkler ≥ 0.9
/// share far more than that, so this only drops hopeless candidates.
fn prefilter(idx: &EpgCandidateIndex, norm: &str) -> Vec<usize> {
    let mut shared: HashMap<u32, u32> = HashMap::new();
    let mut query: Vec<u32> = trigrams(norm).collect();
    query.sort_unstable();
    query.dedup();
    let min_shared = (query.len() / 4).max(1) as u32;
    for g in query {
        for &i in idx.grams.get(&g).map(Vec::as_slice).unwrap_or_default() {
            *shared.entry(i).or_default() += 1;
        }
    }
    let mut cands: Vec<(u32, u32)> = shared.into_iter().filter(|&(_, n)| n >= min_shared).collect();
    cands.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    cands.truncate(MAX_FUZZY_CANDIDATES);
    cands.into_iter().map(|(i, _)| i as usize).collect()
}

/// Whether two norms share at least one padded trigram (Guide fixes' "Looks wrong": a tvg-id
/// match whose guide channel shares none with the channel's name).
pub fn share_trigram(a: &str, b: &str) -> bool {
    let mut ga: Vec<u32> = trigrams(a).collect();
    ga.sort_unstable();
    trigrams(b).any(|g| ga.binary_search(&g).is_ok())
}

/// Padded byte trigrams of an (ASCII) norm packed into a `u32`: `"abc"` → `^ab abc bc$`.
fn trigrams(norm: &str) -> impl Iterator<Item = u32> + '_ {
    let b = norm.as_bytes();
    let at = move |i: usize| -> u32 {
        // Virtual string: '^' + norm + '$'.
        if i == 0 {
            u32::from(b'^')
        } else if i <= b.len() {
            u32::from(b[i - 1])
        } else {
            u32::from(b'$')
        }
    };
    let n = b.len() + 2;
    (0..n.saturating_sub(2)).map(move |i| (at(i) << 16) | (at(i + 1) << 8) | at(i + 2))
}

/// `CNN.us@SD` → `CNN.us`.
pub fn strip_feed(id: &str) -> &str {
    id.split_once('@').map(|(a, _)| a).unwrap_or(id)
}

impl EpgCandidateIndex {
    /// The guide channel whose id equals `xmltv_id` ignoring case, else ignoring the `@Feed`
    /// suffix (Guide fixes: a guide that renamed `bbcfour.uk` to `BBCFour.uk`): `(id, xmltv id)`.
    pub fn find_id(&self, xmltv_id: &str) -> Option<(EpgChannelId, String)> {
        let id = xmltv_id.trim();
        if id.is_empty() {
            return None;
        }
        if let Some(hit) = self.by_id.get(id) {
            return Some(hit.clone());
        }
        if let Some(hit) = self.by_id_lower.get(&id.to_lowercase()).and_then(|v| v.first()) {
            return Some(hit.clone());
        }
        let feedless = strip_feed(id).trim().to_lowercase();
        (!feedless.is_empty()).then(|| self.by_id_feedless.get(&feedless).and_then(|v| v.first()).cloned()).flatten()
    }
}

// ------------------------------------------------------------------ Guide fixes rules (DESIGN §31)

/// A playlist channel as the Guide fixes rules see it ("+1/+2 follow their base", "Copies
/// share a guide"): the derivation after a rematch (`store::guide_fix::derive_rule_rows`) and
/// the issue list (`guide_fix::issues`) use the same two rules.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleChannel {
    pub id: ChannelId,
    pub source: SourceId,
    pub key: ItemKey,
    /// The provider's name (the "+N" marker is read from it, then from `tvg_name`).
    pub name: String,
    pub tvg_name: Option<String>,
    /// `channel.norm_name` (`identity::normalize_name`).
    pub norm: String,
    /// The copies key (`channel.family`, else `norm_name`): the `store::copies` family (B5).
    pub family: String,
    pub tvg_id: Option<String>,
    /// May lend its guide to a copy: supported, not hidden by the user, playlist enabled.
    pub usable: bool,
    pub resolved: Option<Resolved>,
}

/// The guide channel a channel resolves to (`channel_epg`) and the row that won.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub epg_channel: EpgChannelId,
    pub epg_source: EpgSourceId,
    pub xmltv_id: String,
    pub method: EpgMatchMethod,
    /// The winning row's time shift (a rule's), minutes.
    pub shift_min: Option<i32>,
}

/// What "+1/+2 follow their base" says about one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sibling {
    /// Not a "+N" name.
    NotShifted,
    /// A "+N" name whose base isn't in the playlist or has no guide channel.
    NoBase { minutes: i32 },
    /// Its bases resolve to different guide channels: a person picks one.
    Ambiguous { minutes: i32, bases: usize },
    /// It has a guide channel of its own (another than the base's), or the base's guide channel
    /// is itself a "+N" entry: nothing to derive.
    Own,
    /// Follows `base` (an index into the rule channels) `minutes` later. `already`: it resolves
    /// to the base's guide channel already (a provider reusing the base's tvg-id), only the
    /// shift is missing.
    Follows { base: usize, minutes: i32, already: bool },
}

/// Lookups over every playlist channel for the two rules.
#[derive(Debug)]
pub struct RuleIndex<'a> {
    pub chans: &'a [RuleChannel],
    /// The "+N" channels: index → (base norm, minutes).
    markers: HashMap<u32, (String, i32)>,
    /// The channels some "+N" channel's base norm names (only those: the index is per frame
    /// of a 79k-channel classification).
    by_norm: HashMap<(SourceId, String), Vec<u32>>,
    /// Built on the first [`Self::copy_of`].
    by_family: std::cell::OnceCell<HashMap<&'a str, Vec<u32>>>,
}

impl<'a> RuleIndex<'a> {
    /// `chans` in playlist order, then provider order (the copy rule takes the first copy).
    pub fn new(chans: &'a [RuleChannel]) -> Self {
        let mut markers = HashMap::new();
        let mut by_norm: HashMap<(SourceId, String), Vec<u32>> = HashMap::new();
        for (i, c) in chans.iter().enumerate() {
            let m = crate::textmatch::timeshift::marker(&c.name).or_else(|| c.tvg_name.as_deref().and_then(crate::textmatch::timeshift::marker));
            if let Some((base, minutes)) = m {
                let norm = normalize_name(&base);
                by_norm.entry((c.source, norm.clone())).or_default();
                markers.insert(i as u32, (norm, minutes));
            }
        }
        if !by_norm.is_empty() {
            let mut probe = (SourceId(0), String::new());
            for (i, c) in chans.iter().enumerate() {
                probe.0 = c.source;
                probe.1.clear();
                probe.1.push_str(&c.norm);
                if let Some(v) = by_norm.get_mut(&probe) {
                    v.push(i as u32);
                }
            }
        }
        Self { chans, markers, by_norm, by_family: std::cell::OnceCell::new() }
    }

    /// Rule "+1/+2 follow their base" for channel `i`: its bases are the channels of its
    /// playlist named like it without the marker. `marked(guide channel)` = that guide channel's
    /// display names carry a "+N" marker.
    pub fn sibling(&self, i: usize, marked: &mut dyn FnMut(EpgChannelId) -> bool) -> Sibling {
        let Some((norm, minutes)) = self.markers.get(&(i as u32)) else { return Sibling::NotShifted };
        let minutes = *minutes;
        let c = &self.chans[i];
        let bases = self.by_norm.get(&(c.source, norm.clone())).map(Vec::as_slice).unwrap_or_default();
        let mut targets: Vec<(EpgChannelId, usize)> = Vec::new();
        for &b in bases {
            let b = b as usize;
            if b == i {
                continue;
            }
            if let Some(r) = &self.chans[b].resolved
                && !targets.iter().any(|(t, _)| *t == r.epg_channel)
            {
                targets.push((r.epg_channel, b));
            }
        }
        match targets.as_slice() {
            [] => Sibling::NoBase { minutes },
            [(target, base)] => {
                if marked(*target) {
                    return Sibling::Own;
                }
                match &c.resolved {
                    None => Sibling::Follows { base: *base, minutes, already: false },
                    Some(r) if r.epg_channel == *target => Sibling::Follows { base: *base, minutes, already: true },
                    Some(_) => Sibling::Own,
                }
            }
            many => Sibling::Ambiguous { minutes, bases: many.len() },
        }
    }

    /// Rule "Copies share a guide" for channel `i` (unresolved): the first copy (`store::copies`
    /// family, another playlist, usable, no tvg-id veto) that resolves.
    pub fn copy_of(&self, i: usize) -> Option<usize> {
        let c = &self.chans[i];
        let own_tvg = c.tvg_id.as_deref().map(str::trim).filter(|t| !t.is_empty());
        let by_family = self.by_family.get_or_init(|| {
            let mut m: HashMap<&str, Vec<u32>> = HashMap::new();
            for (i, c) in self.chans.iter().enumerate().filter(|(_, c)| !c.family.is_empty()) {
                m.entry(c.family.as_str()).or_default().push(i as u32);
            }
            m
        });
        by_family.get(c.family.as_str())?.iter().map(|&j| j as usize).find(|&j| {
            let o = &self.chans[j];
            o.source != c.source
                && o.usable
                && o.resolved.is_some()
                && match (own_tvg, o.tvg_id.as_deref().map(str::trim).filter(|t| !t.is_empty())) {
                    (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                    _ => true,
                }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};

    fn ch(id: i64, xmltv: &str, names: &[&str]) -> EpgChannel {
        EpgChannel {
            id: EpgChannelId(id),
            epg_source_id: EpgSourceId(7),
            xmltv_id: xmltv.into(),
            display_names: names.iter().map(|s| s.to_string()).collect(),
            icon: None,
        }
    }

    #[test]
    fn trigram_packing() {
        let g: Vec<u32> = trigrams("ab").collect();
        assert_eq!(g, vec![0x5E_61_62, 0x61_62_24]);
        assert_eq!(trigrams("a").count(), 1);
        assert_eq!(trigrams("").count(), 0);
        assert_eq!(trigrams("bbcone").count(), 6);
    }

    #[test]
    fn prefilter_does_not_scan_everything() {
        let mut chans: Vec<EpgChannel> = (0..5_000).map(|i| ch(i, &format!("c{i}.tv"), &[&format!("Generic Channel {i}")])).collect();
        chans.push(ch(9_000, "bbc1.uk", &["BBC One"]));
        chans.push(ch(9_001, "bbc2.uk", &["BBC Two"]));
        let idx = build_index(EpgSourceId(7), &chans);
        assert_eq!(idx.norms.len(), 5_002);
        let c = prefilter(&idx, &normalize_name("BBC One London"));
        assert!(!c.is_empty() && c.len() < 10, "only the bbc* names share enough trigrams: {}", c.len());
        assert!(prefilter(&idx, &normalize_name("Generic Channel 1")).len() <= MAX_FUZZY_CANDIDATES);
        assert!(prefilter(&idx, "zzzzqqqq").is_empty());
    }

    #[test]
    fn feed_stripping_on_either_side() {
        let idx = build_index(EpgSourceId(7), &[ch(1, "CNN.us@SD", &["CNN"]), ch(2, "BBCNews.uk", &["BBC News"])]);
        let row = |tvg: &str| ChannelMatchRow {
            channel: ItemRef { source: SourceId(1), kind: ItemKind::Channel, key: ItemKey("m3u:x".into()) },
            tvg_id: Some(tvg.into()),
            name: "zzz".into(),
            norm_name: "zzz".into(),
        };
        let m = match_channel(&idx, &row("cnn.us"), false, false).unwrap();
        assert_eq!((m.xmltv_id.as_str(), m.method), ("CNN.us@SD", EpgMatchMethod::FeedStripped));
        let m = match_channel(&idx, &row("BBCNews.uk@HD"), false, false).unwrap();
        assert_eq!((m.xmltv_id.as_str(), m.method), ("BBCNews.uk", EpgMatchMethod::FeedStripped));
        assert!(match_channel(&idx, &row("@HD"), false, false).is_none());
    }
}
