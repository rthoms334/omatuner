//! Lazy VOD/series detail loading. Owner: WP-06.
//! Details older than 7 days are refetched when the detail page opens; failures keep the
//! list-level data and surface a non-blocking toast.
//!
//! Details fixed by the implementation:
//! * Only Xtream items have details; for M3U items (and fresh details) the job is a no-op.
//! * A panel without data for the item (`NotFound`, empty series info) still stores an empty
//!   detail record, so the page does not re-ask on every visit for 7 days.
//! * Writes go through `submit_notify(DataScope::VodDetails)`. Details that bring a new TMDB
//!   id also rebuild the library: ids decide stacks (DESIGN §29.10).
//! * Episodes without a numeric id cannot be played (Xtream locators carry a number) and are
//!   skipped; an episode without a number is numbered by its position in its season.

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobKind, JobPhase, Progress, check, read};
use crate::error::{Error, Result};
use crate::model::{ItemKind, MovieId, SeriesId, Source, SourceKind, StreamLocator, Timestamp, XtreamStreamKind};
use crate::msg::DataScope;
use crate::sources::xtream::XtreamClient;
use crate::sources::xtream::types::{XtSeriesInfo, XtVodInfo};
use crate::sources::{identity, vod_match};
use crate::store::import::{ImportEpisode, ImportSeason, MovieDetails, SeriesDetails};

/// Details are re-fetched after this long.
pub(crate) const DETAILS_TTL_S: i64 = 7 * 86_400;

fn fresh(loaded_at: Option<Timestamp>, now: Timestamp) -> bool {
    fresh_within(loaded_at, now, DETAILS_TTL_S)
}

fn fresh_within(loaded_at: Option<Timestamp>, now: Timestamp, max_age_s: i64) -> bool {
    loaded_at.is_some_and(|t| now - t < max_age_s)
}

/// `None` for M3U sources, also those keeping the login of an earlier upgrade (DESIGN §27.5:
/// their movies and series are complete at import, the provider is never asked).
fn client(ctx: &JobContext, src: &Source) -> Option<XtreamClient> {
    let creds = src.xtream.clone().filter(|_| src.kind == SourceKind::Xtream)?;
    let cfg = crate::config::current(&ctx.config);
    let ua = src.settings.user_agent.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| cfg.player.user_agent.clone());
    Some(XtreamClient::new(ctx.http.clone(), creds, Some(ua)))
}

/// The provider id of an Xtream item (locator first, `provider_id` as fallback).
fn provider_id(stream: Option<&StreamLocator>, provider_id: Option<&str>) -> Option<i64> {
    match stream {
        Some(StreamLocator::Xtream { stream_id, .. }) => Some(*stream_id),
        _ => provider_id.and_then(|p| p.trim().parse().ok()),
    }
}

pub async fn load_movie(ctx: &JobContext, id: MovieId, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    let (movie, src, explicit) = read(ctx, move |c| {
        let m = crate::store::vod::movie(c, id)?;
        let s = crate::store::sources::get_source(c, m.source_id)?;
        let item = crate::model::ItemRef { source: m.source_id, kind: crate::model::ItemKind::Movie, key: m.key.clone() };
        let explicit = crate::store::live::item_explicit(c, &item)?;
        Ok((m, s, explicit))
    })
    .await?;
    // an explicit title is never named in the jobs overlay or the log (DESIGN §20.3)
    if !explicit {
        progress.set_subject(movie.display_name());
    }
    let now = crate::model::now();
    let Some(client) = client(ctx, &src) else { return Ok(()) };
    if fresh(movie.details_loaded_at, now) {
        return Ok(());
    }
    let Some(vod_id) = provider_id(Some(&movie.stream), movie.provider_id.as_deref()) else { return Ok(()) };
    progress.set(JobPhase::Connecting);
    let info = match client.vod_info(vod_id, cancel).await {
        Ok(info) => info,
        Err(Error::NotFound(_)) => XtVodInfo::default(),
        Err(e) => return Err(e),
    };
    check(cancel)?;
    progress.set(JobPhase::Finishing);
    let d = movie_details(&info);
    let restack = tmdb_changed(movie.tmdb_id.as_deref().and_then(vod_match::tmdb_id), d.tmdb_id.as_deref().and_then(vod_match::tmdb_id));
    ctx.store.writer().submit_notify(DataScope::VodDetails, move |c| crate::store::import::store_movie_details(c, id, &d, now)).recv().await?;
    if restack {
        (ctx.submit)(JobKind::RebuildLibrary);
    }
    Ok(())
}

/// A new TMDB id decides stacks (DESIGN §29.10): the library rebuilds when details bring one.
fn tmdb_changed(old: Option<u32>, new: Option<u32>) -> bool {
    new.is_some() && new != old
}

fn text(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned)
}

fn rating(r: Option<f64>) -> Option<f32> {
    r.filter(|r| r.is_finite() && *r > 0.0).map(|r| r.min(10.0) as f32)
}

fn secs(v: Option<i64>) -> Option<u32> {
    v.filter(|v| *v > 0).and_then(|v| u32::try_from(v).ok())
}

fn movie_details(info: &XtVodInfo) -> MovieDetails {
    let i = &info.info;
    MovieDetails {
        plot: text(&i.plot),
        genre: text(&i.genre),
        cast: text(&i.cast),
        director: text(&i.director),
        duration_s: secs(i.duration_secs),
        release_date: text(&i.releasedate),
        tmdb_id: text(&i.tmdb_id),
        trailer: text(&i.youtube_trailer),
        backdrop: i.backdrop_path.iter().find_map(|b| text(&Some(b.clone()))),
        rating: rating(i.rating),
        poster: text(&i.movie_image),
        container_ext: text(&info.movie_data.container_extension),
    }
}

pub async fn load_series(ctx: &JobContext, id: SeriesId, progress: &Progress, cancel: &CancellationToken) -> Result<()> {
    fetch_series(ctx, id, DETAILS_TTL_S, Some(progress), cancel).await.map(drop)
}

/// New episodes' fetch of a tracked copy (DESIGN §44 §5): [`load_series`]'s parsing and
/// `store_series_details`, with the caller's freshness (`max_age_s`; 0 = always fetch) and no
/// progress of its own (the check's job reports). True when it asked the provider; M3U series
/// (complete at import) are never fetched.
pub async fn load_series_checked(ctx: &JobContext, id: SeriesId, max_age_s: i64, cancel: &CancellationToken) -> Result<bool> {
    fetch_series(ctx, id, max_age_s, None, cancel).await
}

async fn fetch_series(ctx: &JobContext, id: SeriesId, max_age_s: i64, progress: Option<&Progress>, cancel: &CancellationToken) -> Result<bool> {
    let (series, src, old_tmdb, explicit) = read(ctx, move |c| {
        let s = crate::store::vod::series(c, id)?;
        let src = crate::store::sources::get_source(c, s.source_id)?;
        let tmdb: Option<i64> = c.query_row("SELECT tmdb_id FROM series WHERE id = ?1", [id], |r| r.get(0))?;
        let item = crate::model::ItemRef { source: s.source_id, kind: crate::model::ItemKind::Series, key: s.key.clone() };
        let explicit = crate::store::live::item_explicit(c, &item)?;
        Ok((s, src, tmdb.and_then(|t| u32::try_from(t).ok()), explicit))
    })
    .await?;
    // an explicit title is never named in the jobs overlay or the log (DESIGN §20.3)
    if let Some(p) = progress
        && !explicit
    {
        p.set_subject(series.display_name());
    }
    let now = crate::model::now();
    let Some(client) = client(ctx, &src) else { return Ok(false) };
    if fresh_within(series.details_loaded_at, now, max_age_s) {
        return Ok(false);
    }
    let Some(series_id) = provider_id(None, series.provider_id.as_deref()).or_else(|| series.key.0.strip_prefix("xt:series:")?.parse().ok()) else {
        return Ok(false);
    };
    if let Some(p) = progress {
        p.set(JobPhase::Connecting);
    }
    let info = client.series_info(series_id, cancel).await?;
    check(cancel)?;
    if let Some(p) = progress {
        p.set(JobPhase::Finishing);
    }
    let d = series_details(&info);
    let restack = tmdb_changed(old_tmdb, d.tmdb_id);
    ctx.store.writer().submit_notify(DataScope::VodDetails, move |c| crate::store::import::store_series_details(c, id, &d, now)).recv().await?;
    if restack {
        (ctx.submit)(JobKind::RebuildLibrary);
    }
    Ok(true)
}

fn series_details(info: &XtSeriesInfo) -> SeriesDetails {
    let seasons = info
        .seasons
        .iter()
        .filter_map(|s| {
            let number = u32::try_from(s.season_number?).ok()?;
            Some(ImportSeason {
                number,
                name: text(&s.name),
                cover: text(&s.cover).or_else(|| text(&s.cover_big)),
                air_date: text(&s.air_date),
                overview: text(&s.overview),
                episode_count: s.episode_count.and_then(|n| u32::try_from(n).ok()).unwrap_or(0),
            })
        })
        .collect();
    let mut episodes = Vec::new();
    for (season_key, eps) in &info.episodes {
        for (i, e) in eps.iter().enumerate() {
            let Some(ep_id) = e.id.as_deref().and_then(|s| s.trim().parse::<i64>().ok()).filter(|n| *n > 0) else { continue };
            let season = e.season.or(Some(*season_key)).and_then(|s| u32::try_from(s).ok()).unwrap_or(0);
            let episode = e.episode_num.and_then(|n| u32::try_from(n).ok()).unwrap_or_else(|| u32::try_from(i + 1).unwrap_or(u32::MAX));
            episodes.push(ImportEpisode {
                key: identity::xtream_key(ItemKind::Episode, ep_id),
                provider_id: Some(ep_id.to_string()),
                season,
                episode,
                title: text(&e.title).unwrap_or_else(|| format!("Episode {episode}")),
                plot: text(&e.info.plot),
                duration_s: secs(e.info.duration_secs),
                still: text(&e.info.movie_image),
                air_date: text(&e.info.releasedate),
                rating: rating(e.info.rating),
                stream: StreamLocator::Xtream { kind: XtreamStreamKind::Series, stream_id: ep_id, ext: text(&e.container_extension) },
            });
        }
    }
    SeriesDetails {
        seasons,
        episodes,
        plot: text(&info.info.plot),
        backdrop: info.info.backdrop_path.iter().find_map(|b| text(&Some(b.clone()))),
        tmdb_id: crate::sources::xtream::types::tmdb_of(&info.info.tmdb, &info.info.tmdb_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_window() {
        assert!(!fresh(None, 100));
        assert!(fresh(Some(100), 100 + DETAILS_TTL_S - 1));
        assert!(!fresh(Some(100), 100 + DETAILS_TTL_S));
    }

    #[test]
    fn series_info_converts_to_episodes() {
        let info: XtSeriesInfo = serde_json::from_str(
            r#"{"seasons":[{"season_number":1,"name":"Season 1","episode_count":"2","cover":"","cover_big":"http://i/c.jpg"}],
                "info":{"plot":"Plot","backdrop_path":["http://i/b.jpg"]},
                "episodes":{"1":[{"id":"9001","episode_num":"1","title":"Pilot","container_extension":"mp4","season":"1",
                                  "info":{"movie_image":"http://i/e1.jpg","plot":"First","duration_secs":2520}},
                                 {"id":9002,"title":"","container_extension":"mkv","info":[]},
                                 {"id":"x","title":"no numeric id"}]}}"#,
        )
        .unwrap();
        let d = series_details(&info);
        assert_eq!(d.plot.as_deref(), Some("Plot"));
        assert_eq!(d.tmdb_id, None);
        let with_id: XtSeriesInfo = serde_json::from_str(r#"{"info":{"tmdb":"1871"}}"#).unwrap();
        assert_eq!(series_details(&with_id).tmdb_id, Some(1871));
        assert!(tmdb_changed(None, Some(1871)) && tmdb_changed(Some(1), Some(1871)));
        assert!(!tmdb_changed(Some(1871), Some(1871)) && !tmdb_changed(Some(1871), None), "no new id: no rebuild");
        assert_eq!(d.backdrop.as_deref(), Some("http://i/b.jpg"));
        assert_eq!(d.seasons.len(), 1);
        assert_eq!(d.seasons[0].cover.as_deref(), Some("http://i/c.jpg"));
        assert_eq!(d.seasons[0].episode_count, 2);
        assert_eq!(d.episodes.len(), 2);
        let e = &d.episodes[0];
        assert_eq!((e.key.0.as_str(), e.season, e.episode, e.title.as_str()), ("xt:ep:9001", 1, 1, "Pilot"));
        assert_eq!(e.duration_s, Some(2520));
        assert_eq!(e.stream, StreamLocator::Xtream { kind: XtreamStreamKind::Series, stream_id: 9001, ext: Some("mp4".into()) });
        let e2 = &d.episodes[1];
        assert_eq!((e2.season, e2.episode, e2.title.as_str()), (1, 2, "Episode 2"));
    }

    #[test]
    fn vod_info_converts_to_details() {
        let info: XtVodInfo = serde_json::from_str(
            r#"{"info":{"movie_image":"http://i/m.jpg","tmdb_id":"603","backdrop_path":["","http://i/b2.jpg"],"youtube_trailer":"abc",
                        "genre":"Action","plot":"A hacker.","rating":"8.7","duration_secs":"8160","releasedate":"1999-03-31"},
                "movie_data":{"stream_id":5001,"container_extension":"mkv"}}"#,
        )
        .unwrap();
        let d = movie_details(&info);
        assert_eq!(d.duration_s, Some(8160));
        assert_eq!(d.backdrop.as_deref(), Some("http://i/b2.jpg"));
        assert_eq!(d.container_ext.as_deref(), Some("mkv"));
        assert_eq!(d.rating, Some(8.7));
        assert_eq!(d.trailer.as_deref(), Some("abc"));
        assert_eq!(movie_details(&XtVodInfo::default()), MovieDetails::default());
    }
}
