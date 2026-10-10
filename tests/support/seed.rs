//! Raw-SQL seeders (skeleton-implemented, frozen): realistic catalogue/EPG rows without going
//! through WP-02's import functions. Use on a writer connection
//! (`store.writer().blocking(|c| seed::seed_channels(c, src, 5000))`) or a plain test
//! connection with migrations applied. Every helper runs in one transaction.

use rusqlite::{Connection, params};
use tuner::error::Result;
use tuner::model::{ChannelId, EpgSourceId, MovieId, SeriesId, SourceId, Timestamp};

/// An M3U-URL source named `name`; returns its id.
pub fn seed_source(c: &mut Connection, name: &str) -> Result<SourceId> {
    c.execute("INSERT INTO source(name, kind, url, created_at, generation) VALUES (?1, 'm3u-url', 'http://127.0.0.1/playlist.m3u', 0, 1)", [name])?;
    Ok(SourceId(c.last_insert_rowid()))
}

/// `n` channels "Channel {i}" (number i+1, tvg-id `ch{i}.tv`) in group "All"
/// (`channel:all`), 10 % of them also in "News". Returns ids in provider order.
pub fn seed_channels(c: &mut Connection, source: SourceId, n: usize) -> Result<Vec<ChannelId>> {
    let tx = c.transaction()?;
    let mut ids = Vec::with_capacity(n);
    {
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:all','All',0,?2,1)",
            params![source, n as i64],
        )?;
        let all = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'channel','channel:news','News',1,?2,1)",
            params![source, (n / 10) as i64],
        )?;
        let news = tx.last_insert_rowid();
        let mut ch = tx.prepare(
            "INSERT INTO channel(source_id, key, name, clean_name, norm_name, number, tvg_id, stream, url_key, provider_order, added_at, seen_gen)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
        )?;
        let mut cg = tx.prepare("INSERT INTO channel_group(group_id, channel_id, position, is_primary) VALUES (?1, ?2, ?3, ?4)")?;
        for i in 0..n {
            let name = format!("Channel {i}");
            let url = format!("http://127.0.0.1/live/{i}.ts");
            ch.execute(params![
                source,
                format!("m3u:seed{i:011}"),
                name,
                format!("channel{i}"),
                (i + 1) as i64,
                format!("ch{i}.tv"),
                format!(r#"{{"t":"url","url":"{url}"}}"#),
                url,
                i as i64,
                1_700_000_000 + i as i64,
            ])?;
            let id = tx.last_insert_rowid();
            cg.execute(params![all, id, i as i64, 1])?;
            if i % 10 == 0 {
                cg.execute(params![news, id, (i / 10) as i64, 0])?;
            }
            ids.push(ChannelId(id));
        }
    }
    tx.commit()?;
    Ok(ids)
}

/// One URL EPG source (active generation 1) with an epg channel per playlist channel
/// (`ch{i}.tv`, mapped via `channel_epg`) and 30-minute programmes from `start` for `hours`;
/// FTS rows included. Returns the EPG source id.
pub fn seed_epg(c: &mut Connection, channels: &[ChannelId], start: Timestamp, hours: u32) -> Result<EpgSourceId> {
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO epg_source(name, kind, url, active_gen, n_channels) VALUES ('Seed EPG','url','http://127.0.0.1/epg.xml',1,?1)",
        [channels.len() as i64],
    )?;
    let src = tx.last_insert_rowid();
    {
        let mut ec = tx.prepare("INSERT INTO epg_channel(epg_source_id, xmltv_id, seen_gen) VALUES (?1, ?2, 1)")?;
        let mut dn = tx.prepare("INSERT INTO epg_display_name(epg_channel_id, name, norm) VALUES (?1, ?2, ?3)")?;
        let mut map = tx.prepare("INSERT INTO channel_epg(channel_id, epg_channel_id) VALUES (?1, ?2)")?;
        let mut pr = tx.prepare(
            "INSERT INTO programme(epg_source_id, epg_channel_id, gen, start, stop, title, sub_title) VALUES (?1, ?2, 1, ?3, ?4, ?5, NULL)",
        )?;
        let mut fts = tx.prepare("INSERT INTO programme_fts(rowid, title, sub_title) VALUES (?1, ?2, NULL)")?;
        for (i, ch) in channels.iter().enumerate() {
            ec.execute(params![src, format!("ch{i}.tv")])?;
            let eid = tx.last_insert_rowid();
            dn.execute(params![eid, format!("Channel {i}"), format!("channel{i}")])?;
            map.execute(params![ch.0, eid])?;
            for slot in 0..(hours * 2) as i64 {
                let s = start + slot * 1800;
                let title = format!("Show {} on {i}", slot % 17);
                pr.execute(params![src, eid, s, s + 1800, title])?;
                fts.execute(params![tx.last_insert_rowid(), title])?;
            }
        }
    }
    tx.commit()?;
    Ok(EpgSourceId(src))
}

/// `n` movies in group "Movies" (`movie:movies`). Returns ids.
pub fn seed_movies(c: &mut Connection, source: SourceId, n: usize) -> Result<Vec<MovieId>> {
    let tx = c.transaction()?;
    let mut ids = Vec::with_capacity(n);
    {
        tx.execute(
            "INSERT INTO grp(source_id, kind, key, name, provider_order, item_count, seen_gen) VALUES (?1,'movie','movie:movies','Movies',0,?2,1)",
            params![source, n as i64],
        )?;
        let g = tx.last_insert_rowid();
        let mut m = tx.prepare(
            "INSERT INTO movie(source_id, key, name, clean_name, norm_name, year, rating, stream, provider_order, added_at, seen_gen)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1)",
        )?;
        let mut mg = tx.prepare("INSERT INTO movie_group(group_id, movie_id, position) VALUES (?1, ?2, ?3)")?;
        for i in 0..n {
            m.execute(params![
                source,
                format!("m3u:movie{i:010}"),
                format!("Movie {i}"),
                format!("movie{i}"),
                1980 + (i % 45) as i64,
                (i % 10) as f64,
                format!(r#"{{"t":"url","url":"http://127.0.0.1/movie/{i}.mp4"}}"#),
                i as i64,
                1_700_000_000 + i as i64,
            ])?;
            let id = tx.last_insert_rowid();
            mg.execute(params![g, id, i as i64])?;
            ids.push(MovieId(id));
        }
    }
    tx.commit()?;
    Ok(ids)
}

/// `n` series with `seasons` × `episodes` episodes each (details already "loaded").
pub fn seed_series_with_episodes(c: &mut Connection, source: SourceId, n: usize, seasons: u32, episodes: u32) -> Result<Vec<SeriesId>> {
    let tx = c.transaction()?;
    let mut ids = Vec::with_capacity(n);
    {
        let mut s = tx.prepare(
            "INSERT INTO series(source_id, key, name, clean_name, norm_name, provider_order, added_at, details_at, seen_gen)
             VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?6, 1)",
        )?;
        let mut se = tx.prepare("INSERT INTO season(series_id, number, episode_count) VALUES (?1, ?2, ?3)")?;
        let mut ep = tx.prepare(
            "INSERT INTO episode(series_id, source_id, key, season, episode, title, duration_s, stream) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2700, ?7)",
        )?;
        for i in 0..n {
            s.execute(params![source, format!("m3u:series{i:09}"), format!("Series {i}"), format!("series{i}"), i as i64, 1_700_000_000 + i as i64])?;
            let sid = tx.last_insert_rowid();
            for sn in 1..=seasons {
                se.execute(params![sid, sn, episodes])?;
                for e in 1..=episodes {
                    ep.execute(params![
                        sid,
                        source,
                        format!("m3u:ep{i}-{sn}-{e}"),
                        sn,
                        e,
                        format!("Episode {e}"),
                        format!(r#"{{"t":"url","url":"http://127.0.0.1/series/{i}/{sn}/{e}.mp4"}}"#),
                    ])?;
                }
            }
            ids.push(SeriesId(sid));
        }
    }
    tx.commit()?;
    Ok(ids)
}
