//! Downloaded titles use the library's stacks, independently of the browser's copy display
//! preference (DESIGN §43 §2.2). No catalogue reads in the paint path.
use std::collections::{BTreeMap, HashMap};

use crate::downloads::{Job, Kind, State};
use crate::model::{DownloadId, ItemKey, ItemKind, ItemRef, VodKind, VodSummary};
use crate::store::library::{Library, VodIdentity};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum TitleKey {
    Item(ItemRef),
    Recording(DownloadId),
}

pub(crate) fn title_item(job: &Job) -> Option<ItemRef> {
    if job.kind == Kind::Episode {
        Some(ItemRef { source: job.source_id?, kind: ItemKind::Series, key: ItemKey(job.fp.get("download_series_key")?.as_str()?.into()) })
    } else if job.kind == Kind::Movie {
        job.item()
    } else {
        None
    }
}

pub(crate) fn coordinates(job: &Job) -> (i64, i64) {
    (job.fp["download_season"].as_i64().unwrap_or(0), job.fp["download_episode"].as_i64().unwrap_or(0))
}

pub(crate) fn downloading(job: &Job) -> bool {
    matches!(job.state, State::Running | State::Finishing | State::Waiting | State::Paused | State::Scheduled)
}

#[derive(Clone)]
pub(crate) struct Episode {
    pub job: Job,
    pub others: Vec<Job>,
    pub watched: bool,
}

#[derive(Clone)]
pub(crate) struct Title {
    pub key: TitleKey,
    pub name: String,
    pub poster: Option<String>,
    pub kind: Option<VodKind>,
    pub series: bool,
    pub jobs: Vec<Job>,
}

impl Title {
    pub fn episodes(&self) -> Vec<Episode> {
        let mut episodes: BTreeMap<_, Vec<Job>> = BTreeMap::new();
        for job in &self.jobs {
            episodes.entry(coordinates(job)).or_default().push(job.clone());
        }
        episodes
            .into_values()
            .map(|mut jobs| {
                jobs.sort_by_key(|j| std::cmp::Reverse((j.state == State::Done, j.state.playable(), j.done_at.unwrap_or(j.added_at), j.id.0)));
                let watched = jobs.iter().any(|j| j.watched_at.is_some());
                let job = jobs.remove(0);
                Episode { job, others: jobs, watched }
            })
            .collect()
    }

    pub fn newest(&self) -> i64 {
        self.jobs.iter().filter(|j| j.state.playable()).map(|j| j.done_at.unwrap_or(j.added_at)).max().unwrap_or(0)
    }

    pub fn saved(&self) -> bool {
        self.jobs.iter().any(|j| j.state.playable())
    }

    pub fn summary(&self) -> String {
        // Size includes duplicate files until the user deletes them; counts are episodes.
        let bytes: u64 = self.jobs.iter().filter(|j| j.state.playable()).map(|j| j.bytes_done).sum();
        let size = format!("{:.1} GB", bytes as f64 / 1e9);
        let episodes = self.episodes();
        if self.series {
            let saved: Vec<_> = episodes.iter().filter(|e| e.job.state.playable()).collect();
            let unwatched = saved.iter().filter(|e| !e.watched).count();
            let mut text = format!("{} episodes · {size}", saved.len());
            if unwatched > 0 {
                text.push_str(&format!(" · {unwatched} unwatched"));
            }
            text
        } else if episodes.iter().any(|e| e.watched) {
            format!("{size} · ✓ Watched")
        } else {
            size
        }
    }
}

#[derive(Default)]
pub(crate) struct Titles {
    identity: Option<VodIdentity>,
    hide: bool,
    copies: HashMap<ItemRef, VodSummary>,
}

impl Titles {
    fn refresh(&mut self, lib: &Library, hide: bool) {
        let identity = lib.vod_identity();
        if self.identity.as_ref() == Some(&identity) && self.hide == hide {
            return;
        }
        self.identity = Some(identity);
        self.hide = hide;
        self.copies.clear();
        for movies in [false, true] {
            let all = crate::ui::stacks::all(lib, movies);
            let stacks = lib.vod.stacks(movies);
            let visible = |i: u32| {
                let item = crate::ui::stacks::item_ref(&all[i as usize]);
                !lib.conceals(&item) && (!hide || !lib.explicit.contains(&item))
            };
            for (i, copy) in all.iter().enumerate() {
                if !visible(i as u32) {
                    continue;
                }
                let top = stacks.stack_of(i as u32).map_or(i as u32, |s| {
                    let top = stacks.top(s);
                    if visible(top) { top } else { stacks.members(s).iter().copied().find(|i| visible(*i)).unwrap_or(i as u32) }
                });
                self.copies.insert(crate::ui::stacks::item_ref(copy), all[top as usize].clone());
            }
        }
    }

    pub fn key(&self, item: &ItemRef) -> TitleKey {
        TitleKey::Item(self.copies.get(item).map_or_else(|| item.clone(), crate::ui::stacks::item_ref))
    }

    pub fn list(&mut self, lib: &Library, hide: bool, jobs: &[Job]) -> Vec<Title> {
        self.refresh(lib, hide);
        let mut titles: Vec<Title> = Vec::new();
        let mut indices = HashMap::new();
        for job in jobs.iter().filter(|j| j.state != State::Cancelled && super::visible(lib, hide, j)) {
            let item = title_item(job);
            let summary = item.as_ref().and_then(|i| self.copies.get(i));
            let key = item.as_ref().map_or(TitleKey::Recording(job.id), |i| self.key(i));
            let index = *indices.entry(key.clone()).or_insert_with(|| {
                titles.push(Title {
                    key,
                    name: summary.map_or_else(|| job.title.clone(), |v| v.name.to_string()),
                    poster: summary.and_then(|v| v.poster.as_deref().map(str::to_owned)),
                    kind: summary.map(|v| v.kind),
                    series: job.kind == Kind::Episode,
                    jobs: Vec::new(),
                });
                titles.len() - 1
            });
            titles[index].jobs.push(job.clone());
        }
        titles.sort_by_key(|t| std::cmp::Reverse(t.newest()));
        titles
    }
}
