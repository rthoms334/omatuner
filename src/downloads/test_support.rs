//! Catalogue copies shared by the Downloads UI and recovery regressions.
use crate::error::Result;
use crate::model::{ItemKey, ItemKind, ItemRef, SourceId};
use rusqlite::{Connection, params};

pub fn seed(c: &mut Connection) -> Result<()> {
    for (id, name, title, hash) in [(1, "TV", "Landman", 42), (2, "Missing", "EN ★ Landman", 42), (3, "TV 2", "Landman (US)", 99)] {
        c.execute(
            "INSERT OR IGNORE INTO source(id,name,kind,url,created_at,generation) VALUES (?1,?2,'m3u-url','http://127.0.0.1/list',0,1)",
            params![id, name],
        )?;
        c.execute(
            "INSERT INTO series(id,source_id,key,name,clean_name,norm_name,match_hash,provider_order,added_at,details_at,seen_gen)
            VALUES (?1,?1,'landman',?2,?2,?2,?3,0,0,1,1)",
            params![id, title, hash],
        )?;
    }
    for source in [2, 3] {
        c.execute("INSERT INTO title_link(source_id,kind,item_key,action,at,join_group) VALUES (?1,'series','landman','join',0,1)", [source])?;
    }
    for (source, season, episode) in [(1, 1, 10), (1, 2, 7), (1, 1, 2), (2, 2, 6), (3, 2, 7), (3, 1, 2)] {
        c.execute(
            "INSERT INTO episode(series_id,source_id,key,season,episode,title,duration_s,stream) VALUES (?1,?1,?2,?3,?4,'A spoiler',2700,?5)",
            params![
                source,
                format!("e{season}-{episode}"),
                season,
                episode,
                format!(r#"{{"t":"url","url":"http://127.0.0.1/{source}/{season}/{episode}.mp4"}}"#)
            ],
        )?;
    }
    Ok(())
}

pub fn job(c: &Connection, source: i64, season: u32, episode: u32) -> super::Job {
    super::planning::new_job(
        c,
        &crate::store::library::load(c).unwrap(),
        crate::shared::ParentalView { unlocked: true, hides_explicit: false, pin_active: false },
        &crate::config::Config::default(),
        &super::Target {
            item: ItemRef { source: SourceId(source), kind: ItemKind::Episode, key: ItemKey(format!("e{season}-{episode}")) },
            programme: None,
            minutes: None,
            any_copy: true,
        },
        false,
        100,
    )
    .unwrap()
}
