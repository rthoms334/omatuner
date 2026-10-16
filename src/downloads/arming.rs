//! Recording timers reuse the reminder unit lifecycle (DESIGN §43 §3, DESIGN §36.2).
//! A unit contains only its numeric id. Transient timers are re-armed at login and on edits.
use crate::error::Result;
use crate::model::{ReminderId, Timestamp};
use crate::platform::systemd::{ArmSync, Armer, SystemdArmer, call};
use std::sync::Arc;

pub struct RecordingArmer(SystemdArmer);
impl RecordingArmer {
    pub fn for_profile(paths: &crate::paths::AppPaths) -> Option<Self> {
        SystemdArmer::for_profile(paths).map(Self)
    }
    fn name(id: ReminderId) -> String {
        format!("tuner-record-{}", id.0)
    }
}
impl Armer for RecordingArmer {
    fn armed(&self) -> Result<Vec<ReminderId>> {
        let out = call("systemctl", &["--user", "list-units", "--all", "--plain", "--no-legend", "--type=timer", "tuner-record-*.timer"])?;
        Ok(out
            .lines()
            .filter_map(|s| s.split_whitespace().next()?.strip_prefix("tuner-record-")?.strip_suffix(".timer")?.parse().ok().map(ReminderId))
            .collect())
    }
    fn arm(&self, id: ReminderId, at: Timestamp) -> Result<()> {
        self.disarm(id)?;
        let mut args = self.0.run_args(id, at.max(crate::model::now() + 2));
        for arg in &mut args {
            if arg.starts_with("--unit=") {
                *arg = format!("--unit={}", Self::name(id));
            } else if arg.starts_with("--description=") {
                *arg = "--description=Tuner recording".into();
            } else if arg == "--reminder" {
                *arg = "--record".into();
            }
        }
        call("systemd-run", &args.iter().map(String::as_str).collect::<Vec<_>>()).map(drop)
    }
    fn disarm(&self, id: ReminderId) -> Result<()> {
        let name = Self::name(id);
        let _ = call("systemctl", &["--user", "stop", &format!("{name}.timer")]);
        let _ = call("systemctl", &["--user", "reset-failed", &format!("{name}.timer"), &format!("{name}.service")]);
        Ok(())
    }
}
pub fn plan(c: &rusqlite::Connection, _now: Timestamp) -> Result<Vec<(ReminderId, Timestamp)>> {
    Ok(crate::store::downloads::jobs(c)?
        .iter()
        .filter(|j| j.kind == super::Kind::Live && j.state == super::State::Scheduled)
        .filter_map(|j| j.start().map(|at| (ReminderId(j.id.0), at)))
        .collect())
}
pub fn rearm(paths: &crate::paths::AppPaths) -> Result<()> {
    let Some(armer) = RecordingArmer::for_profile(paths) else { return Ok(()) };
    if !paths.db_file().exists() {
        return Ok(());
    }
    let store = crate::store::Store::open(&paths.db_file(), None)?;
    let want = plan(&store.reader()?, crate::model::now())?;
    store.close(std::time::Duration::from_secs(3));
    let mut sync = ArmSync::new(Arc::new(armer));
    sync.sync(&want);
    if let crate::platform::systemd::ArmStatus::Unavailable(_) = sync.status() {
        return Err(crate::error::Error::other("Recording timers could not be armed"));
    }
    Ok(())
}
