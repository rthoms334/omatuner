//! Transient `systemd-run --user` timers that fire reminders while Tuner is closed (B6,
//! `reminders::arming` has the policy). OWNER: remind.
//!
//! * A unit is `tuner-reminder-<id>.timer` + `.service`, described "Tuner reminder", running
//!   `tuner --reminder <id>` at `OnCalendar=@<epoch>` (1 s accuracy). Nothing else: no title,
//!   channel or ref (`systemctl --user list-timers` is readable by anything in the session).
//! * Transient units live in the user manager's runtime state: nothing is written in `$HOME`,
//!   and they are gone after a logout or reboot (the next Tuner start re-arms them).
//! * Only the default profile arms (a `--profile-dir` test profile never does: its units would
//!   fire the default profile's reminder ids), and only when `systemd-run` exists. Without it
//!   reminders fire only while Tuner runs, as in v1.
//! * [`ArmSync`] keeps the units in step with a plan: it learns what is armed once (units of an
//!   earlier run are re-armed, their times unknown), then only arms and disarms the
//!   differences. A failing `systemd-run` turns arming off for the session (logged once).

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::model::{ReminderId, Timestamp};
use crate::reminders::arming::{self, UNIT_DESCRIPTION};

/// The longest a `systemctl` / `systemd-run` call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// Variables a unit needs to open a window in this session (set when we have them; none of
/// them names anything).
const SESSION_VARS: [&str; 5] = ["WAYLAND_DISPLAY", "DISPLAY", "HYPRLAND_INSTANCE_SIGNATURE", "XDG_SESSION_TYPE", "XDG_CURRENT_DESKTOP"];

/// Where reminder units are made (systemd in production, a fake in tests).
pub trait Armer: Send + Sync + 'static {
    /// The reminder ids that have a unit now (blocking).
    fn armed(&self) -> Result<Vec<ReminderId>>;
    /// (Re)arm `id` to elapse at `at` (blocking).
    fn arm(&self, id: ReminderId, at: Timestamp) -> Result<()>;
    /// Remove `id`'s unit; a unit that doesn't exist is fine (blocking).
    fn disarm(&self, id: ReminderId) -> Result<()>;
}

/// What arming did last (Settings ▸ Reminders "Fire while Tuner is closed").
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ArmStatus {
    /// Not asked to arm (setting off, another profile, the pass not enabled).
    #[default]
    Off,
    /// This many units are armed.
    Armed(usize),
    /// systemd refused; reminders fire only while Tuner runs.
    Unavailable(String),
}

/// Keeps the units in step with [`arming::plan`].
pub struct ArmSync {
    armer: Arc<dyn Armer>,
    /// What this process armed and for when; `None` until the first sync looked.
    known: Option<HashMap<ReminderId, Timestamp>>,
    status: ArmStatus,
}

impl ArmSync {
    pub fn new(armer: Arc<dyn Armer>) -> Self {
        Self { armer, known: None, status: ArmStatus::Off }
    }

    pub fn status(&self) -> &ArmStatus {
        &self.status
    }

    /// Make the units match `want` (ids with their elapse times). Blocking: call it on the
    /// blocking pool. After a failure it does nothing more this session.
    pub fn sync(&mut self, want: &[(ReminderId, Timestamp)]) {
        if matches!(self.status, ArmStatus::Unavailable(_)) {
            return;
        }
        if let Err(e) = self.try_sync(want) {
            tracing::warn!("reminders won't fire while Tuner is closed: {e}");
            self.status = ArmStatus::Unavailable(e.to_string());
        }
    }

    /// Remove every unit this profile armed (the setting was turned off).
    pub fn clear(&mut self) {
        if !matches!(self.status, ArmStatus::Unavailable(_)) {
            self.sync(&[]);
            if !matches!(self.status, ArmStatus::Unavailable(_)) {
                self.status = ArmStatus::Off;
            }
        }
    }

    fn try_sync(&mut self, want: &[(ReminderId, Timestamp)]) -> Result<()> {
        if self.known.is_none() {
            // an earlier run's units: times unknown → re-armed below when still wanted
            let found = self.armer.armed()?;
            self.known = Some(found.into_iter().map(|id| (id, Timestamp::MIN)).collect());
        }
        let Some(known) = self.known.as_mut() else { return Ok(()) };
        let wanted: HashMap<ReminderId, Timestamp> = want.iter().copied().collect();
        let stale: Vec<ReminderId> = known.iter().filter(|(id, at)| wanted.get(id) != Some(at)).map(|(id, _)| *id).collect();
        for id in stale {
            self.armer.disarm(id)?;
            known.remove(&id);
        }
        for (&id, &at) in &wanted {
            if let std::collections::hash_map::Entry::Vacant(slot) = known.entry(id) {
                self.armer.arm(id, at)?;
                slot.insert(at);
            }
        }
        self.status = ArmStatus::Armed(known.len());
        Ok(())
    }
}

/// The real units.
#[derive(Debug, Clone)]
pub struct SystemdArmer {
    /// `--setenv` pairs (see [`SESSION_VARS`]).
    env: Vec<(String, String)>,
}

impl SystemdArmer {
    /// The armer for `paths`' profile: `None` for a `--profile-dir` profile or without
    /// `systemd-run`.
    pub fn for_profile(paths: &crate::paths::AppPaths) -> Option<Self> {
        let default = crate::paths::AppPaths::from_env().ok()?;
        if default.data_dir != paths.data_dir || !on_path("systemd-run") {
            return None;
        }
        let env = SESSION_VARS.iter().filter_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()).map(|v| ((*k).to_owned(), v))).collect();
        Some(Self { env })
    }

    /// `systemd-run`'s arguments for reminder `id` at `at`.
    pub fn run_args(&self, id: ReminderId, at: Timestamp) -> Vec<String> {
        let mut args = vec![
            "--user".to_owned(),
            "--quiet".to_owned(),
            "--collect".to_owned(),
            format!("--unit={}", arming::unit_name(id)),
            format!("--description={UNIT_DESCRIPTION}"),
            format!("--on-calendar=@{at}"),
            "--timer-property=AccuracySec=1s".to_owned(),
        ];
        args.extend(self.env.iter().map(|(k, v)| format!("--setenv={k}={v}")));
        args.push("--".to_owned());
        args.extend(arming::command(id));
        args
    }
}

impl Armer for SystemdArmer {
    fn armed(&self) -> Result<Vec<ReminderId>> {
        let pattern = format!("{}*.timer", arming::UNIT_PREFIX);
        let out = call("systemctl", &["--user", "list-units", "--all", "--plain", "--no-legend", "--type=timer", &pattern])?;
        Ok(out.lines().filter_map(|l| l.split_whitespace().next()).filter_map(arming::unit_id).collect())
    }

    fn arm(&self, id: ReminderId, at: Timestamp) -> Result<()> {
        // a unit left over (failed service, timer of another time) must go first
        self.disarm(id)?;
        let args = self.run_args(id, at);
        call("systemd-run", &args.iter().map(String::as_str).collect::<Vec<_>>()).map(drop)
    }

    fn disarm(&self, id: ReminderId) -> Result<()> {
        let name = arming::unit_name(id);
        // stopping a unit that isn't loaded fails; that is the state we want
        let _ = call("systemctl", &["--user", "stop", &format!("{name}.timer")]);
        let _ = call("systemctl", &["--user", "reset-failed", &format!("{name}.timer"), &format!("{name}.service")]);
        Ok(())
    }
}

/// `prog` is an executable on `PATH`.
fn on_path(prog: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(prog).is_file()))
}

/// Run `prog args…` with a timeout; its stdout, or an error with its stderr's first line.
fn call(prog: &str, args: &[&str]) -> Result<String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::other(format!("{prog}: {e}")))?;
    let end = Instant::now() + CALL_TIMEOUT;
    loop {
        match child.try_wait().map_err(Error::other)? {
            Some(_) => break,
            None if Instant::now() >= end => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::other(format!("{prog} timed out")));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    let out = child.wait_with_output().map_err(Error::other)?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        Err(Error::other(format!("{prog}: {}", err.lines().next().unwrap_or("failed"))))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records arm / disarm calls; `fail` makes every arm fail.
    #[derive(Default)]
    pub(crate) struct FakeArmer {
        pub units: Mutex<HashMap<ReminderId, Timestamp>>,
        pub calls: Mutex<Vec<String>>,
        pub fail: bool,
    }

    impl Armer for FakeArmer {
        fn armed(&self) -> Result<Vec<ReminderId>> {
            let mut ids: Vec<_> = self.units.lock().unwrap().keys().copied().collect();
            ids.sort();
            Ok(ids)
        }
        fn arm(&self, id: ReminderId, at: Timestamp) -> Result<()> {
            if self.fail {
                return Err(Error::other("Failed to connect to bus"));
            }
            self.calls.lock().unwrap().push(format!("arm {} @{at}", id.0));
            self.units.lock().unwrap().insert(id, at);
            Ok(())
        }
        fn disarm(&self, id: ReminderId) -> Result<()> {
            self.calls.lock().unwrap().push(format!("disarm {}", id.0));
            self.units.lock().unwrap().remove(&id);
            Ok(())
        }
    }

    /// Only differences are applied; units of an earlier run are re-armed once (their times
    /// are unknown) and dropped when no longer wanted.
    #[test]
    fn sync_applies_only_the_differences() {
        let fake = Arc::new(FakeArmer::default());
        fake.units.lock().unwrap().extend([(ReminderId(1), 500), (ReminderId(9), 900)]); // a previous run's
        let mut sync = ArmSync::new(fake.clone());
        sync.sync(&[(ReminderId(1), 500), (ReminderId(2), 600)]);
        let mut calls = std::mem::take(&mut *fake.calls.lock().unwrap());
        calls.sort();
        assert_eq!(calls, ["arm 1 @500", "arm 2 @600", "disarm 1", "disarm 9"]);
        assert_eq!(sync.status(), &ArmStatus::Armed(2));
        // a moved reminder is re-armed, an unchanged one left alone, a gone one disarmed
        sync.sync(&[(ReminderId(2), 660), (ReminderId(3), 700)]);
        let mut calls = std::mem::take(&mut *fake.calls.lock().unwrap());
        calls.sort();
        assert_eq!(calls, ["arm 2 @660", "arm 3 @700", "disarm 1", "disarm 2"]);
        sync.sync(&[(ReminderId(2), 660), (ReminderId(3), 700)]);
        assert!(fake.calls.lock().unwrap().is_empty(), "nothing changed");
        sync.clear();
        assert!(fake.units.lock().unwrap().is_empty());
        assert_eq!(sync.status(), &ArmStatus::Off);
    }

    /// No systemd (or a refusing one): arming stops for the session, the scheduler goes on.
    #[test]
    fn a_failing_systemd_turns_arming_off_once() {
        let fake = Arc::new(FakeArmer { fail: true, ..Default::default() });
        let mut sync = ArmSync::new(fake.clone());
        sync.sync(&[(ReminderId(1), 500)]);
        assert!(matches!(sync.status(), ArmStatus::Unavailable(e) if e.contains("bus")));
        sync.sync(&[(ReminderId(1), 500)]);
        assert!(fake.calls.lock().unwrap().is_empty());
    }

    /// The unit names nothing but the id: no title, channel or ref in its name, description
    /// or command line; it runs `tuner --reminder <id>` once at the given second.
    #[test]
    fn units_carry_only_the_id() {
        let armer = SystemdArmer { env: vec![("WAYLAND_DISPLAY".into(), "wayland-1".into())] };
        let args = armer.run_args(ReminderId(7), 1_791_900_000);
        assert_eq!(
            args,
            [
                "--user",
                "--quiet",
                "--collect",
                "--unit=tuner-reminder-7",
                "--description=Tuner reminder",
                "--on-calendar=@1791900000",
                "--timer-property=AccuracySec=1s",
                "--setenv=WAYLAND_DISPLAY=wayland-1",
                "--",
                "tuner",
                "--reminder",
                "7"
            ]
        );
    }

    /// A test profile never arms units (they would fire the default profile's ids).
    #[test]
    fn only_the_default_profile_arms() {
        let dir = std::env::temp_dir().join("tuner-systemd-profile");
        assert!(SystemdArmer::for_profile(&crate::paths::AppPaths::rooted(&dir)).is_none());
    }
}
