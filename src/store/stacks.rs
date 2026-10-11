//! Stacks of copies of one title in "All playlists" (DESIGN §29.11–§29.12, P2), built inside
//! the library build ([`build`], from the stored match columns) and read by the Movies / Series
//! grids, detail pages and playback:
//!
//! * [`Stacks`]: per item (index into `VodIndex::movies` / `series`) its stack and its
//!   [`CopyMeta`] (language tag, dub, version tags, origin, TMDB id, pin); per stack its
//!   members in copy order and its **top** copy (the card's poster, title and metadata).
//! * Only items in the snapshot stack — hidden items (their own flag, every category hidden,
//!   the category filter) and explicit content while parental control hides it
//!   (`LoadOptions::visibility`, DESIGN §20.3) are not in it — and only from playlists in All
//!   (`in_all`). Locked copies stay (drawn disabled; the PIN on pick). A copy the user split
//!   off stands alone. [`Stacks::retain`] drops copies from built stacks (the relock re-filter).
//! * Copy order: language rank (the user's audio languages first, then untagged / multi-audio
//!   copies, then subtitled markets, then other languages), then CAM / TS after the clean
//!   copies of that language, then playlist order, then the playlist's own order (the vectors
//!   are in playlist order). The top is the first **unlocked** copy; with every copy locked,
//!   the first.
//! * An untagged copy in a category whose tagged titles are ≥ 80 % one market (≥ 10 of them)
//!   takes that market as its language (`CopyMeta::inferred`): it never passes as the user's
//!   language just because the provider left the prefix off.
//! * [`play_order`] / [`fallback`]: which copy plays (pin > latest progress > troubled copies
//!   last > language > CAM / TS last > playlist order or best quality > mkv / mp4), and the one
//!   tried once when the first fails before its first frame. [`stack_marks`]: what a stack
//!   card shows of the copies' favourites, watched marks, progress and locks.
//!
//! Inside one playlist nothing is combined: the grids collapse lists only for All (and with
//! `interface.same_title = combine`, `ui::stacks::collapse`); [`Stacks::elsewhere`] gives "+1
//! also in TV".

use std::collections::HashMap;
use std::sync::Arc;

use crate::model::{GroupId, Quality, SourceId, Timestamp, VodSummary};
use crate::sources::vod_match::{self, ClusterItem, Dub, VersionTags};

/// A stack (index into the stacks of one kind).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StackId(pub u32);

const NONE: u32 = u32::MAX;

/// What the library build reads per copy from the match columns (stacking input).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CopyRaw {
    /// `match_hash` (`None`: no key stacking).
    pub hash: Option<i64>,
    /// `match_lang`.
    pub lang: Option<Arc<str>>,
    /// `match_tags`.
    pub tags: VersionTags,
    pub origin: Option<[u8; 2]>,
    pub tmdb: Option<u32>,
    /// The user split this copy off its stack.
    pub split: bool,
    /// The user's "Always play from" copy.
    pub pinned: bool,
    /// "Same title as…" (P3): the user's join group; copies of one group stack together,
    /// with the stacks they are in.
    pub join: Option<i64>,
}

/// Per-copy facts for labels and ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CopyMeta {
    /// Index into [`Stacks::langs`] (0 = no tag).
    lang: u16,
    /// The tag comes from the copy's category, not its name.
    pub inferred: bool,
    pub dub: Dub,
    pub tags: VersionTags,
    pub origin: Option<[u8; 2]>,
    pub tmdb: Option<u32>,
    /// [`vod_match::lang_rank`] for the user's audio languages (lower first).
    pub lang_rank: u8,
    pub pinned: bool,
    pub split: bool,
    /// The user combined this copy with another title ("Same title as…").
    pub joined: bool,
}

/// The stacks of one kind (movies or series). `Default` = no stacks.
#[derive(Debug, Default, Clone)]
pub struct Stacks {
    meta: Vec<CopyMeta>,
    /// Language codes (`EN`, `AR-SUBS`); index 0 = none.
    langs: Vec<Arc<str>>,
    /// Item → stack (`NONE` = alone).
    of: Vec<u32>,
    /// Stack → `flat[start..end]`.
    spans: Vec<(u32, u32)>,
    /// Members in copy order, stack after stack.
    flat: Vec<u32>,
    tops: Vec<u32>,
    /// Playlists in playlist order (as the items first name them).
    playlists: Vec<SourceId>,
}

impl Stacks {
    /// Number of stacks (titles with two or more copies).
    pub fn len(&self) -> usize {
        self.spans.len()
    }
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The stack of `item`, if it has other copies.
    pub fn stack_of(&self, item: u32) -> Option<StackId> {
        self.of.get(item as usize).copied().filter(|s| *s != NONE).map(StackId)
    }

    /// Members in copy order (module docs); the top is [`Stacks::top`], not always the first.
    pub fn members(&self, s: StackId) -> &[u32] {
        self.spans.get(s.0 as usize).map_or(&[], |&(a, b)| &self.flat[a as usize..b as usize])
    }

    /// The copy a stack card shows.
    pub fn top(&self, s: StackId) -> u32 {
        self.tops[s.0 as usize]
    }

    /// Cards All saves by stacking (copies in stacks minus stacks): All's card count is its
    /// copies' count minus this (every stacked copy is in All).
    pub fn saved(&self) -> usize {
        self.flat.len() - self.spans.len()
    }

    /// Every stack's id.
    pub fn ids(&self) -> impl Iterator<Item = StackId> {
        (0..self.spans.len() as u32).map(StackId)
    }

    /// The copies of `item`'s title: its stack's members, or `[item]`.
    pub fn copies(&self, item: u32) -> Vec<u32> {
        match self.stack_of(item) {
            Some(s) => self.members(s).to_vec(),
            None => vec![item],
        }
    }

    pub fn meta(&self, item: u32) -> CopyMeta {
        self.meta.get(item as usize).copied().unwrap_or_default()
    }

    /// The copy's language / market code (`EN`, `AR-SUBS`, `IN-TL`), inferred ones included.
    pub fn lang(&self, item: u32) -> Option<&str> {
        let m = self.meta.get(item as usize)?;
        (m.lang != 0).then(|| &*self.langs[m.lang as usize])
    }

    /// The language badge of a copy: `EN`, `FR · sub`, `AR?` (inferred from its category),
    /// `multi`; `None` when nothing is known.
    pub fn lang_label(&self, item: u32) -> Option<String> {
        let m = self.meta(item);
        let code = self.lang(item).map(|c| if m.inferred { format!("{c}?") } else { c.to_owned() });
        match (code, m.dub.label()) {
            (Some(c), Some(d)) => Some(format!("{c} · {d}")),
            (Some(c), None) => Some(c),
            (None, Some(d)) => Some(d.to_owned()),
            (None, None) => None,
        }
    }

    /// These stacks for the items left after a removal (explicit content hidden on relock):
    /// `map[old]` is the item's new index (`None`: removed), `items` the new vector. Members
    /// keep their copy order; a stack left with one copy dissolves; the top is again the first
    /// unlocked member. Removing items never joins titles, so this equals a rebuild's stacks
    /// up to the rare TMDB-run split a removed copy decided. O(items).
    pub fn retain(self, map: &[Option<u32>], items: &[VodSummary]) -> Stacks {
        let Stacks { meta, langs, flat, spans, playlists, .. } = self;
        let mut new_meta = vec![CopyMeta::default(); items.len()];
        for (old, m) in meta.into_iter().enumerate() {
            if let Some(Some(new)) = map.get(old)
                && let Some(slot) = new_meta.get_mut(*new as usize)
            {
                *slot = m;
            }
        }
        let mut of = vec![NONE; items.len()];
        let (mut new_flat, mut new_spans, mut tops) = (Vec::with_capacity(flat.len()), Vec::new(), Vec::new());
        for &(a, b) in &spans {
            let start = new_flat.len();
            new_flat.extend(flat[a as usize..b as usize].iter().filter_map(|i| map.get(*i as usize).copied().flatten()));
            if new_flat.len() - start < 2 {
                new_flat.truncate(start);
                continue;
            }
            let members = &new_flat[start..];
            let s = new_spans.len() as u32;
            members.iter().for_each(|&i| of[i as usize] = s);
            tops.push(members.iter().copied().find(|&i| !items[i as usize].locked).unwrap_or(members[0]));
            new_spans.push((start as u32, new_flat.len() as u32));
        }
        Stacks { meta: new_meta, langs, of, spans: new_spans, flat: new_flat, tops, playlists }
    }

    /// The other playlists `item`'s title is in ("+1 also in TV" inside one playlist), in
    /// playlist order.
    pub fn elsewhere(&self, item: u32, items: &[VodSummary]) -> Vec<SourceId> {
        let Some(own) = items.get(item as usize).map(|v| v.source_id) else { return Vec::new() };
        let mut out: Vec<SourceId> = Vec::new();
        for &m in self.stack_of(item).map_or(&[][..], |s| self.members(s)) {
            let src = items[m as usize].source_id;
            if src != own && !out.contains(&src) {
                out.push(src);
            }
        }
        out
    }

    /// The place of a playlist in the playlist order (Settings ▸ Playlists).
    pub fn playlist_rank(&self, source: SourceId) -> u32 {
        self.playlists.iter().position(|s| *s == source).map_or(u32::MAX, |p| p as u32)
    }

    /// The facts [`play_order`] needs of a copy that the snapshot knows (language, playlist,
    /// quality, CAM, lock, pin); the caller fills in progress, trouble and container.
    pub fn facts(&self, item: u32, items: &[VodSummary]) -> CopyFacts {
        let m = self.meta(item);
        CopyFacts {
            item,
            playlist_rank: items.get(item as usize).map_or(u32::MAX, |v| self.playlist_rank(v.source_id)),
            lang_rank: m.lang_rank,
            quality: m.tags.quality(),
            cam: m.tags.is_cam(),
            locked: items.get(item as usize).is_some_and(|v| v.locked),
            pinned: m.pinned,
            last_played: None,
            trouble: false,
            container_ok: None,
        }
    }
}

/// Mixed into the key of a copy split off its stack: it never joins the copies it was split
/// from, but copies split off the same title join each other.
const SPLIT_SALT: i64 = 0x5b1d_c0de_7a1e_0001;

/// Market dominance thresholds of a category (untagged copies take its market).
const DOMINANT_MIN: u32 = 10;
const DOMINANT_SHARE: f32 = 0.8;

/// Build the stacks of one kind. `items` are the snapshot's visible items in display order
/// (playlist order first), `raw` their match data (same length), `in_all` says whether a
/// playlist is in "All playlists" for this kind, `prefs` the user's audio languages
/// ([`vod_match::norm_lang`] codes, most preferred first).
pub fn build(items: &[VodSummary], raw: Vec<CopyRaw>, in_all: impl Fn(SourceId) -> bool, prefs: &[String]) -> Stacks {
    debug_assert_eq!(items.len(), raw.len());
    let n = items.len();
    // languages: interned codes, dub markers apart
    let mut langs: Vec<Arc<str>> = vec![Arc::from("")];
    let mut lang_ids: HashMap<Arc<str>, u16> = HashMap::new();
    let mut intern = |code: &str| -> u16 {
        if let Some(&id) = lang_ids.get(code) {
            return id;
        }
        let id = u16::try_from(langs.len()).unwrap_or(0);
        let a: Arc<str> = Arc::from(code);
        langs.push(a.clone());
        lang_ids.insert(a, id);
        id
    };
    let mut meta: Vec<CopyMeta> = Vec::with_capacity(n);
    // the library build shares one `Arc` per distinct text: look it up by address first
    let mut by_ptr: HashMap<usize, (u16, Dub)> = HashMap::new();
    let mut by_text: HashMap<Arc<str>, (u16, Dub)> = HashMap::new();
    for r in &raw {
        let (lang, dub) = match &r.lang {
            Some(t) => match by_ptr.get(&(Arc::as_ptr(t) as *const u8 as usize)) {
                Some(v) => *v,
                None => {
                    let v = *by_text.entry(t.clone()).or_insert_with(|| {
                        let (code, dub) = vod_match::split_lang_text(t);
                        (code.map_or(0, &mut intern), dub)
                    });
                    if by_ptr.len() < 4096 {
                        by_ptr.insert(Arc::as_ptr(t) as *const u8 as usize, v);
                    }
                    v
                }
            },
            None => (0, Dub::None),
        };
        meta.push(CopyMeta {
            lang,
            dub,
            tags: r.tags,
            origin: r.origin,
            tmdb: r.tmdb,
            pinned: r.pinned,
            split: r.split,
            joined: r.join.is_some(),
            ..Default::default()
        });
    }
    infer_from_categories(items, &mut meta);
    // one rank per (language, dub): few distinct pairs
    let mut ranks: HashMap<(u16, Dub), u8> = HashMap::new();
    for m in &mut meta {
        m.lang_rank = *ranks.entry((m.lang, m.dub)).or_insert_with(|| {
            let code = (m.lang != 0).then(|| &*langs[m.lang as usize]);
            vod_match::lang_rank(code, m.dub, prefs)
        });
    }
    // clustering: only copies in All; a copy the user split off ("Not the same title") leaves
    // its stack for good — copies split off one title stack together as the other title
    let cluster_items: Vec<ClusterItem> = items
        .iter()
        .zip(&raw)
        .map(|(v, r)| match (in_all(v.source_id), r.split) {
            (true, false) => ClusterItem { hash: r.hash, tmdb: r.tmdb, origin: r.origin },
            (true, true) => ClusterItem { hash: r.hash.map(|h| h ^ SPLIT_SALT), tmdb: None, origin: r.origin },
            (false, _) => ClusterItem { hash: None, tmdb: None, origin: None },
        })
        .collect();
    // "Same title as…": (item, group) of the joined copies in All
    let joined: Vec<(u32, i64)> =
        items.iter().zip(&raw).enumerate().filter_map(|(i, (v, r))| r.join.filter(|_| in_all(v.source_id)).map(|g| (i as u32, g))).collect();
    drop(raw);
    let vod_match::Clusters { flat, spans } = vod_match::cluster_flat(&cluster_items);
    drop(cluster_items);
    let (mut flat, spans) = apply_joins(flat, spans, &joined);
    let mut playlists: Vec<SourceId> = Vec::new();
    for v in items {
        if playlists.last() != Some(&v.source_id) && !playlists.contains(&v.source_id) {
            playlists.push(v.source_id);
        }
    }
    let rank_of = |s: SourceId| playlists.iter().position(|p| *p == s).unwrap_or(usize::MAX) as u32;
    let mut of = vec![NONE; n];
    let mut tops = Vec::with_capacity(spans.len());
    for (s, &(a, b)) in spans.iter().enumerate() {
        let members = &mut flat[a as usize..b as usize];
        members.sort_unstable_by_key(|&i| {
            let m = &meta[i as usize];
            (m.lang_rank, m.tags.is_cam(), rank_of(items[i as usize].source_id), i)
        });
        tops.push(members.iter().copied().find(|&i| !items[i as usize].locked).unwrap_or(members[0]));
        for &i in members.iter() {
            of[i as usize] = s as u32;
        }
    }
    Stacks { meta, langs, of, spans, flat, tops, playlists }
}

/// Per category, the language its tagged titles are mostly for (the same ≥ 80 % of ≥ 10 rule
/// as the stacks' inference): `market[i]` is item `i`'s `vod_match::market_lang`, `tagged[i]`
/// says it has a tag at all (a multi-audio or regional tag counts against every language).
/// "Also hide titles in other languages" gives an untagged title its category's market
/// (DESIGN §28.12).
pub fn dominant_market(items: &[VodSummary], market: &[Option<&'static str>], tagged: &[bool]) -> HashMap<GroupId, &'static str> {
    // per category: tagged titles, and their count per market
    type Counts = (u32, Vec<(&'static str, u32)>);
    let mut counts: HashMap<GroupId, Counts> = HashMap::new();
    for ((v, m), t) in items.iter().zip(market).zip(tagged) {
        if !*t {
            continue;
        }
        for g in &v.group_ids {
            let (total, c) = counts.entry(*g).or_default();
            *total += 1;
            if let Some(l) = m {
                match c.iter_mut().find(|(x, _)| x == l) {
                    Some((_, n)) => *n += 1,
                    None => c.push((l, 1)),
                }
            }
        }
    }
    counts
        .into_iter()
        .filter_map(|(g, (total, c))| {
            let &(lang, top) = c.iter().max_by_key(|(l, n)| (*n, std::cmp::Reverse(*l)))?;
            (total >= DOMINANT_MIN && top as f32 >= DOMINANT_SHARE * total as f32).then_some((g, lang))
        })
        .collect()
}

/// The user's joins ("Same title as…", P3) on top of the clusters: the copies of one join
/// group, and every copy of the stacks they are in, become one stack. Clusters no join
/// touches keep their place; merged ones follow. O(joined + flat) — nothing when there are
/// no joins.
fn apply_joins(flat: Vec<u32>, spans: Vec<(u32, u32)>, joined: &[(u32, i64)]) -> (Vec<u32>, Vec<(u32, u32)>) {
    if joined.is_empty() {
        return (flat, spans);
    }
    // a unit is a cluster (its index) or a copy standing alone (`spans.len() + item`)
    let wanted: std::collections::HashSet<u32> = joined.iter().map(|(i, _)| *i).collect();
    let mut cluster_of: HashMap<u32, u64> = HashMap::new();
    for (c, &(a, b)) in spans.iter().enumerate() {
        for &i in &flat[a as usize..b as usize] {
            if wanted.contains(&i) {
                cluster_of.insert(i, c as u64);
            }
        }
    }
    let unit = |i: u32| cluster_of.get(&i).copied().unwrap_or(spans.len() as u64 + u64::from(i));
    let mut parent: HashMap<u64, u64> = HashMap::new();
    fn root(parent: &mut HashMap<u64, u64>, mut u: u64) -> u64 {
        while let Some(&p) = parent.get(&u).filter(|p| **p != u) {
            u = p;
        }
        u
    }
    let mut first_of: HashMap<i64, u64> = HashMap::new();
    for &(i, g) in joined {
        let u = unit(i);
        parent.entry(u).or_insert(u);
        match first_of.get(&g) {
            None => {
                first_of.insert(g, u);
            }
            Some(&f) => {
                let (a, b) = (root(&mut parent, f), root(&mut parent, u));
                if a != b {
                    parent.insert(b, a.min(b));
                    parent.insert(a, a.min(b));
                }
            }
        }
    }
    // the units of each merged component (two or more units)
    let mut comps: HashMap<u64, Vec<u64>> = HashMap::new();
    let units: Vec<u64> = parent.keys().copied().collect();
    for u in units {
        let r = root(&mut parent, u);
        comps.entry(r).or_default().push(u);
    }
    comps.retain(|_, v| v.len() > 1);
    if comps.is_empty() {
        return (flat, spans);
    }
    let merged: std::collections::HashSet<u64> = comps.values().flatten().copied().collect();
    let (mut out_flat, mut out_spans) = (Vec::with_capacity(flat.len() + joined.len()), Vec::with_capacity(spans.len()));
    for (c, &(a, b)) in spans.iter().enumerate() {
        if !merged.contains(&(c as u64)) {
            let start = out_flat.len() as u32;
            out_flat.extend_from_slice(&flat[a as usize..b as usize]);
            out_spans.push((start, out_flat.len() as u32));
        }
    }
    let mut roots: Vec<u64> = comps.keys().copied().collect();
    roots.sort_unstable();
    for r in roots {
        let start = out_flat.len() as u32;
        let mut units = comps.remove(&r).unwrap_or_default();
        units.sort_unstable();
        for u in units {
            match usize::try_from(u).ok().filter(|u| *u < spans.len()) {
                Some(c) => out_flat.extend_from_slice(&flat[spans[c].0 as usize..spans[c].1 as usize]),
                None => out_flat.push((u - spans.len() as u64) as u32),
            }
        }
        out_spans.push((start, out_flat.len() as u32));
    }
    (out_flat, out_spans)
}

/// Untagged copies in a category whose tagged copies are mostly one market take that market
/// (module docs). One pass over the memberships.
fn infer_from_categories(items: &[VodSummary], meta: &mut [CopyMeta]) {
    // dense category index → (language, count) pairs (a category holds few languages)
    let mut dense: HashMap<GroupId, u32> = HashMap::new();
    let mut counts: Vec<Vec<(u16, u32)>> = Vec::new();
    let mut untagged = false;
    let mut last: Option<(GroupId, u32)> = None;
    for (v, m) in items.iter().zip(meta.iter()) {
        if m.lang == 0 {
            untagged = true;
            continue;
        }
        for g in &v.group_ids {
            // neighbours mostly share their category: skip the map then
            let d = match last {
                Some((lg, ld)) if lg == *g => ld,
                _ => *dense.entry(*g).or_insert_with(|| {
                    counts.push(Vec::new());
                    counts.len() as u32 - 1
                }),
            };
            last = Some((*g, d));
            let c = &mut counts[d as usize];
            match c.iter_mut().find(|(l, _)| *l == m.lang) {
                Some((_, n)) => *n += 1,
                None => c.push((m.lang, 1)),
            }
        }
    }
    if !untagged {
        return;
    }
    let dominant: Vec<Option<u16>> = counts
        .iter()
        .map(|c| {
            let total: u32 = c.iter().map(|(_, n)| n).sum();
            let &(lang, top) = c.iter().max_by_key(|(l, n)| (*n, std::cmp::Reverse(*l)))?;
            (total >= DOMINANT_MIN && top as f32 >= DOMINANT_SHARE * total as f32).then_some(lang)
        })
        .collect();
    if dominant.iter().all(Option::is_none) {
        return;
    }
    for (v, m) in items.iter().zip(meta.iter_mut()) {
        if m.lang == 0
            && let Some(lang) = v.group_ids.iter().find_map(|g| dense.get(g).and_then(|d| dominant[*d as usize]))
        {
            m.lang = lang;
            m.inferred = true;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Which copy plays

/// What decides which copy of a title plays first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CopyFacts {
    /// Index of the copy (`VodIndex::movies` / `series`).
    pub item: u32,
    /// Its playlist's place in the playlist order ([`Stacks::playlist_rank`]); copies of one
    /// playlist tie here.
    pub playlist_rank: u32,
    pub lang_rank: u8,
    pub quality: Option<Quality>,
    /// CAM / TS copy.
    pub cam: bool,
    pub locked: bool,
    /// "Always play from" this copy.
    pub pinned: bool,
    /// When the user last played this copy (latest progress).
    pub last_played: Option<Timestamp>,
    /// Failed recently, its account is at the connection limit, or expired.
    pub trouble: bool,
    /// mkv / mp4 (`Some(true)`), another container (`Some(false)`), unknown (`None`).
    pub container_ok: Option<bool>,
}

/// The order copies are tried in (the first plays): a pinned copy, then the latest progress,
/// troubled copies last, then the audio language, CAM / TS after the clean copies of that
/// language, then the playlist order — or the best quality first with
/// [`CopyOrder::BestQuality`](crate::config::CopyOrder) — then mkv / mp4 before other
/// containers. Locked copies are left out unless the session is `unlocked`.
pub fn play_order(copies: &[CopyFacts], unlocked: bool, order: crate::config::CopyOrder) -> Vec<u32> {
    let mut v: Vec<&CopyFacts> = copies.iter().filter(|c| unlocked || !c.locked).collect();
    let best = order == crate::config::CopyOrder::BestQuality;
    v.sort_by_key(|c| {
        (
            !c.pinned,
            std::cmp::Reverse(c.last_played.unwrap_or(i64::MIN)),
            c.trouble,
            c.lang_rank,
            // CAM / TS last within the language, whichever playlist has the clean copy
            c.cam,
            if best { std::cmp::Reverse(c.quality.map_or(0, |q| q.code())) } else { std::cmp::Reverse(0) },
            c.playlist_rank,
            match c.container_ok {
                Some(true) => 0u8,
                None => 1,
                Some(false) => 2,
            },
            c.item,
        )
    });
    v.into_iter().map(|c| c.item).collect()
}

/// The copy to try when `failed` couldn't play before its first frame: the next one in
/// `order` (tried once; the caller doesn't chain fallbacks).
pub fn fallback(order: &[u32], failed: u32) -> Option<u32> {
    let at = order.iter().position(|c| *c == failed)?;
    order.get(at + 1).copied()
}

/// What a stack card shows of its copies (DESIGN §29.12): ★ when any copy is a favourite, ✓
/// when any is watched, progress from the most recently played copy, and the lock only when
/// every copy is locked (a locked copy otherwise stays in the stack, drawn disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StackMarks {
    pub favourite: bool,
    pub watched: bool,
    /// The copy whose progress the card shows (latest played).
    pub resume: Option<u32>,
    pub locked: bool,
}

pub fn stack_marks(
    members: &[u32],
    favourite: impl Fn(u32) -> bool,
    watched: impl Fn(u32) -> bool,
    played_at: impl Fn(u32) -> Option<Timestamp>,
    locked: impl Fn(u32) -> bool,
) -> StackMarks {
    StackMarks {
        favourite: members.iter().any(|m| favourite(*m)),
        watched: members.iter().any(|m| watched(*m)),
        resume: members.iter().filter_map(|m| Some((played_at(*m)?, *m))).max_by_key(|(at, m)| (*at, std::cmp::Reverse(*m))).map(|(_, m)| m),
        locked: !members.is_empty() && members.iter().all(|m| locked(*m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CopyOrder;
    use crate::model::{ItemKey, MovieId, VodKind};
    use crate::sources::vod_match::PrefixCodes;

    /// A copy: (source, provider name, group, locked).
    fn copies(rows: &[(i64, &str, i64, bool)]) -> (Vec<VodSummary>, Vec<CopyRaw>) {
        let codes = PrefixCodes::default();
        rows.iter()
            .enumerate()
            .map(|(i, (src, name, group, locked))| {
                let m = vod_match::parse(name, None, &codes);
                let v = VodSummary {
                    kind: VodKind::Movie(MovieId(i as i64)),
                    source_id: SourceId(*src),
                    key: ItemKey(format!("k{i}")),
                    name: Arc::from(*name),
                    year: m.year,
                    poster: None,
                    rating: None,
                    group_ids: vec![GroupId(*group)],
                    added_at: None,
                    provider_order: i as i32,
                    locked: *locked,
                };
                let raw = CopyRaw {
                    hash: m.stack_hash(crate::model::ItemKind::Movie),
                    lang: m.lang_text().map(Arc::from),
                    tags: m.tags,
                    origin: m.origin,
                    ..Default::default()
                };
                (v, raw)
            })
            .unzip()
    }

    fn en() -> Vec<String> {
        vec!["en".to_owned()]
    }

    /// The top card is the first unlocked copy in the user's language; locked copies stay in
    /// the stack; an all-locked stack shows its first copy; with no copy in the user's language
    /// the playlist order decides (CAM last); other playlists' copies are listed for "+1".
    #[test]
    fn top_card_and_copy_order() {
        let (items, raw) = copies(&[
            (1, "FR ★ Dune - 2021", 1, false),                // 0
            (1, "EN ★ Dune - 2021", 2, true),                 // 1 English but locked
            (2, "EN - Dune (2021)", 3, false),                // 2 English, unlocked: top
            (2, "AR-SUBS - Dune (2021)", 4, false),           // 3
            (1, "EN-CAM - Heat (1995)", 2, false),            // 4 CAM
            (2, "DE - Heat (1995)", 5, false),                // 5
            (1, "EN ★ Alien - 1979", 2, true),                // 6 all locked
            (2, "EN - Alien (1979)", 3, true),                // 7
            (1, "EN-CAM - Brave the Dark (2025)", 2, false),  // 8 CAM in the first playlist
            (2, "EN - Brave the Dark (2025) [4K]", 3, false), // 9
        ]);
        let s = build(&items, raw, |_| true, &en());
        assert_eq!(s.len(), 4);
        assert_eq!(s.saved(), 10 - 4, "ten copies, four cards");
        let dune = s.stack_of(0).unwrap();
        assert_eq!(s.members(dune), &[1, 2, 3, 0], "English first, then subtitled, then French");
        assert_eq!(s.top(dune), 2, "the first unlocked English copy");
        let heat = s.stack_of(4).unwrap();
        assert_eq!(s.members(heat), &[4, 5], "the English CAM copy still ranks by language first");
        assert_eq!(s.top(s.stack_of(6).unwrap()), 6, "every copy locked: the first");
        assert_eq!(s.elsewhere(2, &items), vec![SourceId(1)]);
        assert_eq!(s.lang_label(3).as_deref(), Some("AR-SUBS"));
        let brave = s.stack_of(8).unwrap();
        assert_eq!((s.members(brave), s.top(brave)), (&[9, 8][..], 9), "a clean copy in a later playlist beats a CAM copy");
    }

    /// Removing copies (explicit content hidden on relock) re-points the stacks at the shorter
    /// vector: members keep their order, the top is re-chosen among the unlocked copies left,
    /// and a stack left with one copy dissolves.
    #[test]
    fn retain_after_removing_copies() {
        let (items, raw) = copies(&[
            (1, "FR ★ Dune - 2021", 1, false),      // 0
            (1, "EN ★ Dune - 2021", 2, true),       // 1 locked
            (2, "EN - Dune (2021)", 3, false),      // 2 top, removed
            (2, "AR-SUBS - Dune (2021)", 4, false), // 3
            (1, "EN ★ Heat - 1995", 2, false),      // 4
            (2, "DE - Heat (1995)", 5, false),      // 5 removed
        ]);
        let s = build(&items, raw, |_| true, &en());
        let map = [Some(0), Some(1), None, Some(2), Some(3), None];
        let kept: Vec<VodSummary> = items.iter().zip(&map).filter(|(_, m)| m.is_some()).map(|(v, _)| v.clone()).collect();
        let r = s.retain(&map, &kept);
        assert_eq!(r.len(), 1, "Heat is alone now");
        let dune = r.stack_of(0).unwrap();
        assert_eq!(r.members(dune), &[1, 2, 0], "English (locked), subtitled, French");
        assert_eq!(r.top(dune), 2, "the first unlocked copy left");
        assert_eq!((r.stack_of(3), r.saved()), (None, 2));
        assert_eq!(r.lang_label(2).as_deref(), Some("AR-SUBS"), "copy facts follow the copy");
    }

    /// Only copies of playlists in All stack; a copy the user split off stands alone, and copies
    /// split off one title stack together; hidden and hidden-explicit items never reach
    /// `build` (the library leaves them out).
    #[test]
    fn in_all_and_split() {
        let (items, mut raw) = copies(&[(1, "EN ★ Heat - 1995", 1, false), (2, "EN - Heat (1995)", 2, false), (3, "EN - Heat (1995)", 3, false)]);
        let s = build(&items, raw.clone(), |src| src != SourceId(3), &en());
        assert_eq!(s.members(s.stack_of(0).unwrap()), &[0, 1]);
        assert_eq!(s.stack_of(2), None, "playlist 3 is out of All");
        raw[1].split = true;
        let s = build(&items, raw.clone(), |_| true, &en());
        assert_eq!(s.members(s.stack_of(0).unwrap()), &[0, 2]);
        assert_eq!(s.stack_of(1), None, "split off");
        assert!(s.meta(1).split);
        // a second copy split off the same title joins the first: the namesake's own stack
        raw[2].split = true;
        let s = build(&items, raw, |_| true, &en());
        assert_eq!((s.stack_of(0), s.members(s.stack_of(1).unwrap())), (None, &[1, 2][..]));
    }

    /// "Same title as…" (P3): copies of one join group stack together with the stacks they
    /// are in — whatever their names, years or TMDB ids — and still order by language; a join
    /// with a copy out of All does nothing; a split copy can be joined elsewhere.
    #[test]
    fn joins_merge_titles_with_their_stacks() {
        let (items, mut raw) = copies(&[
            (1, "EN ★ Incendies - 2010", 1, false),          // 0
            (2, "EN - Incendies (2010)", 2, false),          // 1 stacks with 0 by key
            (2, "DE - Die Frau die singt (2010)", 3, false), // 2 another name: alone
            (1, "EN ★ Heat - 1995", 1, false),               // 3 alone
            (3, "EN - Heat 2 (1995)", 4, false),             // 4 out of All
        ]);
        raw[0].tmdb = Some(1);
        raw[2].tmdb = Some(2);
        let s = build(&items, raw.clone(), |src| src != SourceId(3), &en());
        assert_eq!((s.len(), s.stack_of(2)), (1, None), "different ids, different names: apart");
        // the user joins the German copy with Incendies' English copy in playlist 2
        raw[1].join = Some(7);
        raw[2].join = Some(7);
        // and Heat with a copy out of All (no effect)
        raw[3].join = Some(9);
        raw[4].join = Some(9);
        let s = build(&items, raw.clone(), |src| src != SourceId(3), &en());
        let st = s.stack_of(2).expect("joined");
        assert_eq!(s.members(st), &[0, 1, 2], "the whole stack, English first");
        assert!(s.meta(2).joined && !s.meta(0).joined);
        assert_eq!((s.stack_of(3), s.stack_of(4), s.len()), (None, None, 1));
        // a copy split off its stack can still be joined to another title
        raw[0].split = true;
        raw[0].join = Some(9);
        raw[3].join = Some(9);
        let s = build(&items, raw, |src| src != SourceId(3), &en());
        assert_eq!(s.members(s.stack_of(1).unwrap()), &[1, 2], "Incendies without the split copy");
        assert_eq!(s.members(s.stack_of(0).unwrap()), &[0, 3], "the split copy joined Heat");
    }

    /// An untagged copy in a category that is ≥ 80 % one market takes that market: it is not
    /// the English top card. In a mostly English category it counts as English.
    #[test]
    fn untagged_copies_take_their_categorys_market() {
        let mut rows: Vec<(i64, String, i64, bool)> = (0..12).map(|i| (2, format!("AR-SUBS - Film {i} ({})", 2000 + i), 9, false)).collect();
        rows.push((2, "NF - Ziam (2025)".into(), 9, false)); // 12: untagged, Arabic-subtitles shelf
        rows.push((1, "GR ★ Ziam - 2025".into(), 1, false)); // 13
        rows.extend((0..12).map(|i| (1, format!("EN ★ Show {i} - {}", 2000 + i), 7, false)));
        rows.push((1, "Leo (2023)".into(), 7, false)); // 26: untagged, English shelf
        rows.push((2, "FR - Leo (2023)".into(), 8, false)); // 27
        let rows: Vec<(i64, &str, i64, bool)> = rows.iter().map(|(a, b, c, d)| (*a, b.as_str(), *c, *d)).collect();
        let (items, raw) = copies(&rows);
        let s = build(&items, raw, |_| true, &en());
        assert_eq!(s.lang_label(12).as_deref(), Some("AR-SUBS?"));
        assert!(s.meta(12).inferred);
        assert_eq!(s.lang(26), Some("EN"));
        assert_eq!(s.top(s.stack_of(26).unwrap()), 26);
        // Ziam: neither copy is English; playlist order decides between equal ranks
        let ziam = s.stack_of(12).unwrap();
        assert_eq!(s.members(ziam), &[12, 13], "AR-SUBS (audio unknown) before Greek");
    }

    /// Which copy plays: pin > latest progress > trouble last > language > CAM last > playlist
    /// order (or best quality) > mkv/mp4; locked copies only when the session is unlocked; one
    /// fallback.
    #[test]
    fn play_order_rules() {
        // items 1 and 2 are in the first playlist, 0 and 3 in the second
        let c = |item: u32, lang_rank: u8| CopyFacts {
            item,
            playlist_rank: u32::from(item == 0 || item == 3),
            lang_rank,
            quality: None,
            cam: false,
            locked: false,
            pinned: false,
            last_played: None,
            trouble: false,
            container_ok: None,
        };
        let base = vec![c(0, 2), c(1, 0), c(2, 0), c(3, 1)];
        let order = |v: &[CopyFacts]| play_order(v, false, CopyOrder::PlaylistOrder);
        assert_eq!(order(&base), vec![1, 2, 3, 0], "language, then playlist order");
        let mut v = base.clone();
        v[2].quality = Some(Quality::Uhd);
        assert_eq!(play_order(&v, false, CopyOrder::BestQuality), vec![2, 1, 3, 0], "best quality within the language");
        v[1].container_ok = Some(false);
        v[2].quality = None;
        assert_eq!(order(&v), vec![2, 1, 3, 0], "mkv / mp4 before another container");
        let mut v = base.clone();
        v[1].cam = true;
        assert_eq!(order(&v), vec![2, 1, 3, 0], "CAM after an equal copy");
        let mut w = base.clone();
        (w[0].lang_rank, w[1].cam, w[2].cam) = (0, true, true);
        assert_eq!(order(&w), vec![0, 1, 2, 3], "a clean copy of a later playlist before CAM copies");
        v[2].trouble = true;
        assert_eq!(order(&v), vec![1, 3, 0, 2], "a failed / at-limit / expired copy goes last");
        v[0].last_played = Some(100);
        assert_eq!(order(&v)[0], 0, "the copy you were watching");
        v[3].pinned = true;
        assert_eq!(order(&v)[0], 3, "the pin beats everything");
        let mut v = base.clone();
        v[1].locked = true;
        assert_eq!(order(&v), vec![2, 3, 0], "locked copies are skipped");
        assert_eq!(play_order(&v, true, CopyOrder::PlaylistOrder), vec![1, 2, 3, 0], "unless the session is unlocked");
        assert_eq!(fallback(&[2, 3, 0], 2), Some(3));
        assert_eq!(fallback(&[2, 3, 0], 0), None);
        assert_eq!(fallback(&[2, 3, 0], 7), None);
    }

    /// A stack card: ★ / ✓ when any copy has it, progress from the latest played copy, the
    /// lock only when every copy is locked.
    #[test]
    fn stack_card_marks() {
        let m = stack_marks(&[4, 5, 6], |i| i == 6, |i| i == 5, |i| [None, Some(10), Some(30)][(i - 4) as usize], |i| i == 4);
        assert_eq!(m, StackMarks { favourite: true, watched: true, resume: Some(6), locked: false });
        let m = stack_marks(&[4, 5], |_| false, |_| false, |_| None, |_| true);
        assert_eq!(m, StackMarks { favourite: false, watched: false, resume: None, locked: true });
    }
}
