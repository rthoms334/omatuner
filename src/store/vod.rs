//! Movie / series / episode reads. Owner: WP-02.

use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::Result;
use crate::model::{
    Episode, EpisodeId, GroupId, ItemKey, ItemKind, ItemRef, Movie, MovieId, Season, Series, SeriesId, Timestamp, VodKind, VodSummary,
};
use crate::store::sql::{Json, or_not_found, u32_of};

/// Group ids of a movie/series (`link` = `movie_group`/`series_group`, `col` its item column)
/// in provider order, and whether any of them is PIN-locked.
fn item_groups(conn: &Connection, link: &str, col: &str, id: i64) -> Result<(Vec<GroupId>, bool)> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT l.group_id, COALESCE(o.locked, 0) FROM {link} l JOIN grp g ON g.id = l.group_id
         LEFT JOIN group_override o ON o.source_id = g.source_id AND o.kind = g.kind AND o.group_key = g.key
         WHERE l.{col} = ?1 ORDER BY g.provider_order, g.id"
    ))?;
    let rows = stmt.query_map([id], |r| Ok((r.get::<_, GroupId>(0)?, r.get::<_, bool>(1)?)))?;
    let mut ids = Vec::new();
    let mut locked = false;
    for row in rows {
        let (g, l) = row?;
        ids.push(g);
        locked |= l;
    }
    Ok((ids, locked))
}

fn movie_from_row(r: &Row<'_>) -> rusqlite::Result<Movie> {
    Ok(Movie {
        id: r.get(0)?,
        source_id: r.get(1)?,
        key: ItemKey(r.get(2)?),
        provider_id: r.get(3)?,
        name: r.get(4)?,
        clean_name: r.get(5)?,
        rename: r.get(22)?,
        year: r.get(6)?,
        group_ids: Vec::new(),
        poster: r.get(7)?,
        backdrop: r.get(8)?,
        rating: r.get::<_, Option<f64>>(9)?.map(|v| v as f32),
        plot: r.get(10)?,
        genre: r.get(11)?,
        cast: r.get(12)?,
        director: r.get(13)?,
        duration_s: r.get(14)?,
        release_date: r.get(15)?,
        tmdb_id: r.get(16)?,
        trailer: r.get(17)?,
        stream: r.get::<_, Json<_>>(18)?.0,
        added_at: r.get(19)?,
        provider_order: r.get(20)?,
        hidden: r.get(23)?,
        locked: r.get(24)?,
        details_loaded_at: r.get(21)?,
    })
}

/// `locked` is effective: the movie's own lock or a lock on any of its groups.
pub fn movie(conn: &Connection, id: MovieId) -> Result<Movie> {
    let r = conn
        .prepare_cached(
            "SELECT m.id, m.source_id, m.key, m.provider_id, m.name, m.clean_name, m.year, m.poster, m.backdrop, m.rating, m.plot,
                    m.genre, m.cast_, m.director, m.duration_s, m.release_date, m.tmdb_id, m.trailer, m.stream, m.added_at,
                    m.provider_order, m.details_at, o.rename, COALESCE(o.hidden, 0), COALESCE(o.locked, 0)
             FROM movie m LEFT JOIN item_override o ON o.source_id = m.source_id AND o.kind = 'movie' AND o.item_key = m.key
             WHERE m.id = ?1",
        )?
        .query_row([id], movie_from_row);
    let mut m = or_not_found(r, "movie", id)?;
    let (groups, locked) = item_groups(conn, "movie_group", "movie_id", id.0)?;
    m.group_ids = groups;
    m.locked |= locked;
    Ok(m)
}

/// `locked` is effective: the series' own lock or a lock on any of its groups.
pub fn series(conn: &Connection, id: SeriesId) -> Result<Series> {
    let r = conn
        .prepare_cached(
            "SELECT s.id, s.source_id, s.key, s.provider_id, s.name, s.clean_name, s.year, s.cover, s.backdrop, s.rating, s.plot,
                    s.genre, s.cast_, s.director, s.release_date, s.last_modified, s.added_at, s.provider_order, s.details_at,
                    o.rename, COALESCE(o.hidden, 0), COALESCE(o.locked, 0)
             FROM series s LEFT JOIN item_override o ON o.source_id = s.source_id AND o.kind = 'series' AND o.item_key = s.key
             WHERE s.id = ?1",
        )?
        .query_row([id], |r| {
            Ok(Series {
                id: r.get(0)?,
                source_id: r.get(1)?,
                key: ItemKey(r.get(2)?),
                provider_id: r.get(3)?,
                name: r.get(4)?,
                clean_name: r.get(5)?,
                rename: r.get(19)?,
                year: r.get(6)?,
                group_ids: Vec::new(),
                cover: r.get(7)?,
                backdrop: r.get(8)?,
                rating: r.get::<_, Option<f64>>(9)?.map(|v| v as f32),
                plot: r.get(10)?,
                genre: r.get(11)?,
                cast: r.get(12)?,
                director: r.get(13)?,
                release_date: r.get(14)?,
                last_modified: r.get(15)?,
                added_at: r.get(16)?,
                provider_order: r.get(17)?,
                hidden: r.get(20)?,
                locked: r.get(21)?,
                details_loaded_at: r.get(18)?,
            })
        });
    let mut s = or_not_found(r, "series", id)?;
    let (groups, locked) = item_groups(conn, "series_group", "series_id", id.0)?;
    s.group_ids = groups;
    s.locked |= locked;
    Ok(s)
}

/// Seasons ordered by number (synthesized from episodes when the provider sent none).
pub fn seasons(conn: &Connection, id: SeriesId) -> Result<Vec<Season>> {
    let mut stmt = conn.prepare_cached(
        "SELECT series_id, number, name, cover, air_date, overview, episode_count FROM season WHERE series_id = ?1 ORDER BY number",
    )?;
    let out: Vec<Season> = stmt
        .query_map([id], |r| {
            Ok(Season {
                series_id: r.get(0)?,
                number: r.get(1)?,
                name: r.get(2)?,
                cover: r.get(3)?,
                air_date: r.get(4)?,
                overview: r.get(5)?,
                episode_count: u32_of(r.get(6)?),
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    if !out.is_empty() {
        return Ok(out);
    }
    let mut stmt = conn.prepare_cached("SELECT season, count(*) FROM episode WHERE series_id = ?1 GROUP BY season ORDER BY season")?;
    let rows = stmt.query_map([id], |r| {
        Ok(Season {
            series_id: id,
            number: r.get(0)?,
            name: None,
            cover: None,
            air_date: None,
            overview: None,
            episode_count: u32_of(r.get(1)?),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

const EPISODE_COLS: &str =
    "id, series_id, source_id, key, provider_id, season, episode, title, plot, duration_s, still, air_date, rating, stream";

fn episode_from_row(r: &Row<'_>) -> rusqlite::Result<Episode> {
    Ok(Episode {
        id: r.get(0)?,
        series_id: r.get(1)?,
        source_id: r.get(2)?,
        key: ItemKey(r.get(3)?),
        provider_id: r.get(4)?,
        season: r.get(5)?,
        episode: r.get(6)?,
        title: r.get(7)?,
        plot: r.get(8)?,
        duration_s: r.get(9)?,
        still: r.get(10)?,
        air_date: r.get(11)?,
        rating: r.get::<_, Option<f64>>(12)?.map(|v| v as f32),
        stream: r.get::<_, Json<_>>(13)?.0,
    })
}

/// Episodes of a series, optionally one season, ordered (season, episode).
pub fn episodes(conn: &Connection, id: SeriesId, season: Option<u32>) -> Result<Vec<Episode>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {EPISODE_COLS} FROM episode WHERE series_id = ?1 AND (?2 IS NULL OR season = ?2) ORDER BY season, episode, id"
    ))?;
    let rows = stmt.query_map(params![id, season], episode_from_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn episode(conn: &Connection, id: EpisodeId) -> Result<Episode> {
    let r = conn.prepare_cached(&format!("SELECT {EPISODE_COLS} FROM episode WHERE id = ?1"))?.query_row([id], episode_from_row);
    or_not_found(r, "episode", id)
}

/// The episode to open the series page at (UHF users: "open at the current episode"):
/// the most recently progressed unfinished episode, else the first episode after the last
/// finished one, else S1E1.
pub fn next_episode(conn: &Connection, id: SeriesId) -> Result<Option<Episode>> {
    let eps = episodes(conn, id, None)?;
    if eps.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT p.item_key, p.finished, p.updated_at FROM watch_progress p JOIN episode e ON e.source_id = p.source_id AND e.key = p.item_key
         WHERE p.kind = 'episode' AND e.series_id = ?1",
    )?;
    let progress: Vec<(ItemKey, bool, Timestamp)> =
        stmt.query_map([id], |r| Ok((ItemKey(r.get(0)?), r.get(1)?, r.get(2)?)))?.collect::<rusqlite::Result<_>>()?;
    let latest = |finished: bool| progress.iter().filter(|p| p.1 == finished).max_by_key(|p| p.2).map(|p| &p.0);
    let pos = |key: &ItemKey| eps.iter().position(|e| &e.key == key);
    if let Some(i) = latest(false).and_then(pos) {
        return Ok(Some(eps[i].clone()));
    }
    if let Some(i) = latest(true).and_then(pos)
        && i + 1 < eps.len()
    {
        return Ok(Some(eps[i + 1].clone()));
    }
    // S1E1: specials (season 0) only when there is nothing else
    Ok(eps.iter().find(|e| e.season >= 1).or(eps.first()).cloned())
}

/// Most recently added (Home "Recently added").
///
/// Visible items of enabled sources only (hidden items and items whose groups are all hidden
/// are left out); `locked` is effective (item or any group).
pub fn recently_added(conn: &Connection, movies: bool, limit: usize) -> Result<Vec<VodSummary>> {
    let (table, link, col, kind, poster) =
        if movies { ("movie", "movie_group", "movie_id", "movie", "poster") } else { ("series", "series_group", "series_id", "series", "cover") };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT x.id, x.source_id, x.key, COALESCE(o.rename, x.clean_name), x.year, x.{poster}, x.rating, x.added_at,
                x.provider_order, COALESCE(o.locked, 0)
         FROM {table} x JOIN source s ON s.id = x.source_id AND s.enabled = 1
         LEFT JOIN item_override o ON o.source_id = x.source_id AND o.kind = '{kind}' AND o.item_key = x.key
         WHERE COALESCE(o.hidden, 0) = 0
           AND NOT (EXISTS (SELECT 1 FROM {link} l WHERE l.{col} = x.id)
                    AND NOT EXISTS (SELECT 1 FROM {link} l JOIN grp g ON g.id = l.group_id
                                    LEFT JOIN group_override go ON go.source_id = g.source_id AND go.kind = g.kind AND go.group_key = g.key
                                    WHERE l.{col} = x.id AND COALESCE(go.hidden, 0) = 0))
         ORDER BY x.added_at IS NULL, x.added_at DESC, x.id DESC LIMIT ?1"
    ))?;
    let rows: Vec<(i64, VodSummary)> = stmt
        .query_map([limit as i64], |r| {
            let id: i64 = r.get(0)?;
            Ok((
                id,
                VodSummary {
                    kind: if movies { VodKind::Movie(MovieId(id)) } else { VodKind::Series(SeriesId(id)) },
                    source_id: r.get(1)?,
                    key: ItemKey(r.get(2)?),
                    name: Arc::from(r.get::<_, String>(3)?),
                    year: r.get(4)?,
                    poster: r.get::<_, Option<String>>(5)?.map(Arc::from),
                    rating: r.get::<_, Option<f64>>(6)?.map(|v| v as f32),
                    group_ids: Vec::new(),
                    added_at: r.get(7)?,
                    provider_order: r.get(8)?,
                    locked: r.get(9)?,
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for (id, mut v) in rows {
        let (groups, locked) = item_groups(conn, link, col, id)?;
        v.group_ids = groups;
        v.locked |= locked;
        out.push(v);
    }
    Ok(out)
}

fn resolve_id(conn: &Connection, table: &str, item: &ItemRef) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(&format!("SELECT id FROM {table} WHERE source_id = ?1 AND key = ?2"))?
        .query_row(params![item.source, item.key.as_str()], |r| r.get(0))
        .optional()?)
}

/// Resolve stable refs (Continue Watching rows) to current ids.
pub fn resolve_movie(conn: &Connection, item: &ItemRef) -> Result<Option<MovieId>> {
    if item.kind != ItemKind::Movie {
        return Ok(None);
    }
    Ok(resolve_id(conn, "movie", item)?.map(MovieId))
}
pub fn resolve_episode(conn: &Connection, item: &ItemRef) -> Result<Option<EpisodeId>> {
    if item.kind != ItemKind::Episode {
        return Ok(None);
    }
    Ok(resolve_id(conn, "episode", item)?.map(EpisodeId))
}
