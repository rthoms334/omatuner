//! Classification (pure over the snapshot and the store's facts; < 100 ms for 79k channels;
//! DESIGN §31 §4.3). OWNER: G-core.
//!
//! The universe is the library snapshot: explicit channels are absent while hidden, hidden and
//! category-filtered channels unless `include_hidden`, unsupported ones always. Each channel
//! gets at most one row, the first kind that applies and isn't dismissed (`guide_ok`):
//!
//! | Kind | When | Proposal |
//! |---|---|---|
//! | Gone from guide | a manual row whose guide channel left the guide | the same id ignoring case / feed, else the stored name (fuzzy) |
//! | +1 without shift | "+N" name, own shift unset, effective shift 0, resolves to its base's guide channel — or no guide channel and its base has one | shift N·60 (and the base's guide channel); none when bases disagree |
//! | Looks wrong | an automatic tvg-id row whose guide channel shares no trigram with the name | none ("It's right" dismisses) |
//! | Guide ended | resolved, nothing now or later | another guide that lists it (fuzzy), else none |
//! | Copy has guide | unmapped, the same channel in another playlist (B5 family) has one | that guide channel |
//! | Suggested | unmapped, a cached suggestion ≥ the fuzzy threshold | the top suggestion |
//! | No guide | unmapped, none of the above | none |

use std::collections::HashMap;

use super::SourceCoverage;
use super::catalog::Catalog;
use crate::model::{ChannelId, EpgChannelId, EpgMatchMethod, FixChange, GuideTarget, IssueKind, ItemKind, ItemRef, SourceId};
use crate::sources::epg_match::{RuleIndex, Sibling, share_trigram};
use crate::store::guide_fix::GuideFacts;
use crate::store::library::Library;

/// One row of the issue list.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueRow {
    pub channel: ChannelId,
    pub kind: IssueKind,
    /// The proposed fix (`None`: search by hand).
    pub proposal: Option<FixChange>,
    /// 0–1; ✓ = 1.0 (rule-derived).
    pub confidence: f32,
    /// A fixed phrase ("+1 of E4", "same channel in TV"), never a URL.
    pub why: String,
}

impl IssueRow {
    /// A one-key fix (✓ or ≥ 95 %).
    pub fn one_key(&self) -> bool {
        self.proposal.is_some() && self.confidence >= 0.95
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct IssueReport {
    pub coverage: Vec<SourceCoverage>,
    pub rows: Vec<IssueRow>,
}

impl IssueReport {
    /// Rows per kind (the chips), in `IssueKind::ALL` order.
    pub fn kind_counts(&self, source: Option<SourceId>, lib: &Library) -> [usize; 7] {
        let mut out = [0; 7];
        for r in self.rows.iter().filter(|r| source.is_none_or(|s| lib.live.get(r.channel).is_some_and(|c| c.source_id == s))) {
            out[r.kind as usize] += 1;
        }
        out
    }

    /// "X to fix · Y one-key" of a playlist.
    pub fn to_fix(&self, source: SourceId, lib: &Library) -> (usize, usize) {
        let mine = self.rows.iter().filter(|r| lib.live.get(r.channel).is_some_and(|c| c.source_id == source));
        mine.fold((0, 0), |(n, k), r| (n + 1, k + usize::from(r.one_key())))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClassifyOpts {
    pub include_hidden: bool,
}

/// The phrase for a guide that has nothing now or later and no other guide lists the channel.
pub const WHY_ENDED: &str = "the guide has nothing now or later · refresh the guide";
/// "Guide ended" rows that look for another guide per classification (a whole guide that
/// expired makes thousands; refreshing it is the fix, so the rest carry [`WHY_ENDED`]).
pub const ENDED_LOOKUPS: usize = 500;

/// Classify every shown channel of `lib` (see the module table). `catalog` finds proposals for
/// "Gone from guide" and "Guide ended"; without it those rows carry none.
pub fn classify(lib: &Library, facts: &GuideFacts, catalog: Option<&Catalog>, opts: ClassifyOpts) -> IssueReport {
    let idx = RuleIndex::new(&facts.chans);
    let threshold = facts.opts.fuzzy_threshold;
    let source_name = |s: SourceId| lib.sources.iter().find(|m| m.id == s).map_or("another playlist", |m| m.name.as_str());
    let guide_name = |g: EpgChannelId| facts.guide_names.get(&g).and_then(|v| v.first()).map(|(n, _)| n.as_str());
    let mut marked_cache: HashMap<EpgChannelId, bool> = HashMap::new();
    let mut marked = |g: EpgChannelId| -> bool {
        *marked_cache
            .entry(g)
            .or_insert_with(|| facts.guide_names.get(&g).is_some_and(|v| v.iter().any(|(n, _)| crate::textmatch::timeshift::marker(n).is_some())))
    };
    // the copy rule over SHOWN channels only (B5, as Live "Also on" lists them), built on the
    // first unmapped channel
    let shown_family: std::cell::OnceCell<HashMap<u64, Vec<ChannelId>>> = std::cell::OnceCell::new();
    let shown_family = || {
        shown_family.get_or_init(|| {
            let mut m: HashMap<u64, Vec<ChannelId>> = HashMap::new();
            for c in lib.live.channels.iter().filter(|c| !c.unsupported && !lib.live.hidden.contains(&c.id) && c.epg_channel_id.is_some()) {
                m.entry(c.family).or_default().push(c.id);
            }
            m
        })
    };

    let mut rows = Vec::new();
    let mut ended_lookups = 0usize;
    for c in lib.live.channels.iter() {
        if c.unsupported || (!opts.include_hidden && lib.live.hidden.contains(&c.id)) {
            continue;
        }
        let Some(&i) = facts.index.get(&c.id) else { continue };
        let i = i as usize;
        let fc = &facts.chans[i];
        let extra = facts.extras.get(&c.id);
        let dismissed = |k: IssueKind| extra.is_some_and(|e| e.guide_ok & k.bit() != 0);
        let change = |map: Option<Option<GuideTarget>>, shift: Option<i32>| FixChange {
            channel: Some(ItemRef { source: c.source_id, kind: ItemKind::Channel, key: c.key.clone() }),
            map,
            shift_min: shift.map(Some),
            ..Default::default()
        };
        let row = |kind, proposal, confidence: f32, why: String| IssueRow { channel: c.id, kind, proposal, confidence, why };
        let mut found: Option<IssueRow> = None;

        // Gone from guide: a manual row pointing at nothing
        if let Some(m) = extra.and_then(|e| e.manual.as_ref()).filter(|m| !m.live)
            && !dismissed(IssueKind::GoneFromGuide)
        {
            let by_id = catalog.and_then(|cat| cat.find_id(m.epg_source, &m.xmltv_id));
            let (proposal, conf, why) = match by_id {
                Some(t) => (Some(change(Some(Some(t)), None)), 1.0, "renamed in the guide".to_owned()),
                None => {
                    let by_name = m
                        .epg_name
                        .as_deref()
                        .filter(|n| !n.is_empty())
                        .and_then(|n| catalog.and_then(|cat| cat.suggest(n, 1, threshold, None).into_iter().next()))
                        .and_then(|s| catalog.and_then(|cat| cat.target(s.epg_channel_id)).map(|t| (t, s.score)));
                    match by_name {
                        Some((t, score)) => (Some(change(Some(Some(t)), None)), score, "found by its name in the guide".to_owned()),
                        None => (None, 0.0, "gone from the guide".to_owned()),
                    }
                }
            };
            found = Some(row(IssueKind::GoneFromGuide, proposal, conf, why));
        }

        // +1 without shift
        if found.is_none() && !dismissed(IssueKind::PlusNoShift) && extra.and_then(|e| e.shift_min).is_none() && c.tvg_shift_min == 0 {
            match idx.sibling(i, &mut marked) {
                Sibling::Follows { base, minutes, already } if let Some(target) = facts.chans[base].resolved.as_ref() => {
                    let base_name = guide_name(target.epg_channel).unwrap_or(&facts.chans[base].name).to_owned();
                    let why = format!("+{} of {base_name}", minutes / 60);
                    let map = (!already).then(|| {
                        let name = guide_name(target.epg_channel).unwrap_or_default().to_owned();
                        Some(GuideTarget { epg_source: target.epg_source, xmltv_id: target.xmltv_id.clone(), name })
                    });
                    found = Some(row(IssueKind::PlusNoShift, Some(change(map, Some(minutes))), 1.0, why));
                }
                Sibling::Ambiguous { bases, .. } if c.epg_channel_id.is_none() => {
                    found = Some(row(IssueKind::PlusNoShift, None, 0.0, format!("{bases} bases, pick one")));
                }
                _ => {}
            }
        }

        if let Some(epg) = c.epg_channel_id {
            // Looks wrong: a tvg-id match whose guide channel shares nothing with the name
            let tvg_match = fc.resolved.as_ref().is_some_and(|r| {
                matches!(
                    r.method,
                    EpgMatchMethod::TvgIdExact | EpgMatchMethod::TvgIdCaseless | EpgMatchMethod::XtreamEpgId | EpgMatchMethod::FeedStripped
                )
            });
            if found.is_none() && tvg_match && !dismissed(IssueKind::LooksWrong) && !fc.norm.is_empty() {
                let names = facts.guide_names.get(&epg).map(Vec::as_slice).unwrap_or_default();
                if !names.is_empty() && names.iter().all(|(_, norm)| !share_trigram(&fc.norm, norm)) {
                    found = Some(row(IssueKind::LooksWrong, None, 0.0, "the guide's name doesn't match".into()));
                }
            }
            // Guide ended
            if found.is_none() && !facts.airing.contains(&epg) && !dismissed(IssueKind::GuideEnded) {
                let own = fc.resolved.as_ref().map(|r| r.epg_source);
                ended_lookups += 1;
                let other = catalog.filter(|_| ended_lookups <= ENDED_LOOKUPS).and_then(|cat| {
                    let s = cat.suggest(&c.name, 1, threshold, own).into_iter().next()?;
                    Some((cat.target(s.epg_channel_id)?, s.score, cat.guide_name(s.epg_source_id).unwrap_or("another guide").to_owned()))
                });
                found = Some(match other {
                    Some((t, score, g)) => row(IssueKind::GuideEnded, Some(change(Some(Some(t)), None)), score, format!("listed in {g}")),
                    None => row(IssueKind::GuideEnded, None, 0.0, WHY_ENDED.into()),
                });
            }
        } else if found.is_none() {
            // Copy has guide: the same channel, shown, in another playlist (no tvg-id veto)
            let own_tvg = fc.tvg_id.as_deref().map(str::trim);
            let copy = (!dismissed(IssueKind::CopyHasGuide))
                .then(|| {
                    shown_family().get(&c.family)?.iter().filter_map(|id| lib.live.get(*id)).find(|o| {
                        o.source_id != c.source_id
                            && match (own_tvg, facts.channel(o.id).and_then(|f| f.tvg_id.as_deref()).map(str::trim)) {
                                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                                _ => true,
                            }
                    })
                })
                .flatten();
            if let Some(o) = copy
                && let Some(r) = facts.channel(o.id).and_then(|f| f.resolved.as_ref())
            {
                let name = guide_name(r.epg_channel).unwrap_or_default().to_owned();
                let t = GuideTarget { epg_source: r.epg_source, xmltv_id: r.xmltv_id.clone(), name };
                found = Some(row(
                    IssueKind::CopyHasGuide,
                    Some(change(Some(Some(t)), r.shift_min)),
                    1.0,
                    format!("same channel in {}", source_name(o.source_id)),
                ));
            }
            // Suggested / No guide
            if found.is_none() {
                let best = facts.suggestions.get(&c.id).and_then(|v| v.first()).filter(|s| f64::from(s.score) >= threshold);
                found = match best {
                    Some(s) if !dismissed(IssueKind::Suggested) => {
                        let t = GuideTarget { epg_source: s.epg_source_id, xmltv_id: s.xmltv_id.clone(), name: s.display_name.clone() };
                        Some(row(IssueKind::Suggested, Some(change(Some(Some(t)), None)), s.score, "similar name in the guide".into()))
                    }
                    Some(_) => None,
                    None => (!dismissed(IssueKind::NoGuide)).then(|| row(IssueKind::NoGuide, None, 0.0, "not in any guide".into())),
                };
            }
        }
        rows.extend(found);
    }
    IssueReport { coverage: super::coverage(lib), rows }
}
