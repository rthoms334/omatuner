//! When each named agent last asked Tuner something (Settings ▸ AI agents: "Last used by
//! Claude, 2 h ago"). The MCP handler notes every request; at most one write per name a minute
//! reaches the database (meta `agent.used.<name>` = unix seconds). An agent Tuner can't name
//! ("your agent") isn't recorded.

use std::collections::HashMap;
use std::sync::Mutex;

use rusqlite::{Connection, params};

use crate::error::Result;
use crate::model::Timestamp;
use crate::store::Store;

const PREFIX: &str = "agent.used.";
/// Seconds between two writes for one name.
const EVERY_S: Timestamp = 60;
/// Names kept (the newest): the name comes from the client, so the list can't grow for ever.
const KEEP: i64 = 20;

pub(crate) fn write(c: &Connection, name: &str, at: Timestamp) -> Result<()> {
    c.execute(
        "INSERT INTO meta(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![format!("{PREFIX}{name}"), at.to_string()],
    )?;
    c.execute(
        "DELETE FROM meta WHERE substr(key, 1, ?1) = ?2 AND key NOT IN \
         (SELECT key FROM meta WHERE substr(key, 1, ?1) = ?2 ORDER BY CAST(value AS INTEGER) DESC, key LIMIT ?3)",
        params![PREFIX.len() as i64, PREFIX, KEEP],
    )?;
    Ok(())
}

/// Every recorded agent, newest first.
pub(crate) fn read(c: &Connection) -> Result<Vec<(String, Timestamp)>> {
    let mut stmt = c.prepare("SELECT key, value FROM meta WHERE substr(key, 1, ?1) = ?2")?;
    let rows = stmt.query_map(params![PREFIX.len() as i64, PREFIX], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out: Vec<(String, Timestamp)> =
        rows.filter_map(|r| r.ok()).filter_map(|(k, v)| Some((k.strip_prefix(PREFIX)?.to_owned(), v.trim().parse().ok()?))).collect();
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// The once-a-minute throttle in front of [`write`].
#[derive(Default)]
pub(crate) struct Usage {
    last: Mutex<HashMap<String, Timestamp>>,
}

impl Usage {
    /// `name` (a sanitised `AgentName`) asked at `now`: recorded unless it was within a minute.
    pub(crate) fn note(&self, store: &Store, name: &str, now: Timestamp) {
        {
            let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
            if last.get(name).is_some_and(|t| (now - t).abs() < EVERY_S) {
                return;
            }
            last.insert(name.to_owned(), now);
        }
        let name = name.to_owned();
        // fire and forget: the reply never waits for it
        let _ = store.writer().submit(move |c| write(c, &name, now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_is_written_once_a_minute_per_name_and_read_newest_first() {
        let store = Store::open_temp("agent-usage", None).unwrap();
        let usage = Usage::default();
        usage.note(&store, "Claude", 1_000);
        usage.note(&store, "Claude", 1_030); // throttled
        usage.note(&store, "Codex", 1_010);
        store.writer().blocking(|_| Ok(())).unwrap();
        let c = store.reader().unwrap();
        assert_eq!(read(&c).unwrap(), [("Codex".to_owned(), 1_010), ("Claude".to_owned(), 1_000)]);
        usage.note(&store, "Claude", 1_061);
        store.writer().blocking(|_| Ok(())).unwrap();
        assert_eq!(read(&c).unwrap()[0], ("Claude".to_owned(), 1_061));
        // other meta keys aren't agents
        store.writer().blocking(|c| Ok(c.execute("INSERT INTO meta(key, value) VALUES ('agent_x', '5')", [])?)).unwrap();
        assert_eq!(read(&c).unwrap().len(), 2);
        // made-up names can't pile up: the newest 20 stay
        store.writer().blocking(|c| (0..30).try_for_each(|i| write(c, &format!("bot{i}"), 2_000 + i))).unwrap();
        let kept = read(&c).unwrap();
        assert_eq!((kept.len(), kept[0].0.as_str(), kept[19].0.as_str()), (20, "bot29", "bot10"));
    }
}
