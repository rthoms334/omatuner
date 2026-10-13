//! Settings ▸ About & logs ▸ Updates (DESIGN §41.7): where Tuner's updates stand and their two
//! switches.
//!
//! ```text
//! UPDATES
//! Tuner 0.2.5 · up to date · checked 12 min ago
//! ↻ Check now
//! Automatically install updates                                      [ On  ]
//! Install updates without asking for my password                     [ Off ]
//! Updates are Tuner's stable releases on GitHub, signed with its release key …
//! ```
//! A newer version adds "Install Tuner 0.2.6 now" (or, once installed, "Restart now"); a Tuner
//! that only tells (not pacman's package, no helper, no pkexec, no release key) says why instead
//! of the switches. The passwordless switch doesn't edit the config itself: it asks the updater
//! to have the helper write (or remove) the polkit rule, which asks for the password, and the
//! config follows once that worked (`Updater::take_config_change`, in the app's frame); while
//! it waits a note says so, and a failure stays under it until the next try.

use super::*;
use crate::update::{Mode, Phase, SwitchOp, UpdateStatus};

/// `Act::Feature(Feature::Updates, n)`.
const ACT_CHECK: u16 = 0;
const ACT_INSTALL: u16 = 1;
const ACT_RESTART: u16 = 2;

static AUTO_INSTALL: CfgToggle =
    CfgToggle { label: "Automatically install updates", get: |c| c.updates.auto_install, set: |c, v| c.updates.auto_install = v };

/// Shown from the config; switching it goes through [`passwordless`] (the screen's `adjust`
/// sends it there), never straight into the config.
pub(super) static PASSWORDLESS: CfgToggle =
    CfgToggle { label: "Install updates without asking for my password", get: |c| c.updates.passwordless, set: |_, _| {} };

/// "Tuner 0.2.5 · up to date · checked 12 min ago".
fn status_line(st: &UpdateStatus, now: crate::model::Timestamp) -> String {
    let me = crate::update::VERSION;
    let mut s = match &st.phase {
        _ if st.inert => format!("Tuner {me} · update checks are off here"),
        Phase::Idle => format!("Tuner {me} · not checked yet"),
        Phase::UpToDate => format!("Tuner {me} · up to date"),
        Phase::Available(r) => format!("Tuner {} is available (you have {me})", r.version),
        Phase::Downloading(r) => format!("Downloading Tuner {}…", r.version),
        Phase::Ready { release, declined: false, .. } => {
            format!(
                "Tuner {} is downloaded and verified: it installs when you're back at Tuner with nothing playing (it asks for your password)",
                release.version
            )
        }
        Phase::Ready { release, .. } => format!("Tuner {} is downloaded and verified, ready to install", release.version),
        Phase::Installing(r) => format!("Installing Tuner {}…", r.version),
        Phase::Installed { version } => format!("Tuner {version} is installed: it restarts when idle (you're running {me})"),
    };
    if st.checking {
        s.push_str(" · checking…");
    } else if let Some(at) = st.last_check {
        s.push_str(&format!(" · checked {}", super::super::live::age_label(now, at)));
    }
    s
}

/// The Updates section of About & logs.
pub(super) fn rows(_s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    let st = ctx.services.updater.status();
    v.push(Row::Section("Updates".into()));
    v.push(Row::Note(status_line(&st, ctx.now), false));
    if let Some(p) = &st.problem {
        v.push(Row::Note(p.clone(), true));
    }
    let act = |n: u16, icon: &'static str, label: String| Row::Act { act: Act::Feature(Feature::Updates, n), icon, label, danger: false };
    if !st.inert {
        v.push(act(ACT_CHECK, glyphs::REFRESH, "Check now".into()));
    }
    let managed = matches!(st.mode, Some(Mode::Managed { .. }));
    match &st.phase {
        Phase::Available(r) if managed && r.package.is_some() => v.push(act(ACT_INSTALL, glyphs::ADD, format!("Install Tuner {} now", r.version))),
        Phase::Ready { release, .. } => v.push(act(ACT_INSTALL, glyphs::ADD, format!("Install Tuner {} now", release.version))),
        Phase::Installed { version } => v.push(act(ACT_RESTART, glyphs::REFRESH, format!("Restart now into Tuner {version}"))),
        _ => {}
    }
    match &st.mode {
        Some(Mode::Managed { .. }) => {
            v.push(Row::Toggle(&AUTO_INSTALL));
            v.push(Row::Toggle(&PASSWORDLESS));
            match &st.switch {
                Some(SwitchOp::Working(true)) => v.push(Row::Note("Waiting for your password to allow installs without it…".into(), false)),
                Some(SwitchOp::Working(false)) => v.push(Row::Note("Waiting for your password to ask for it again…".into(), false)),
                Some(SwitchOp::Failed(why)) => v.push(Row::Note(format!("The switch didn't change: {why}"), true)),
                None => {}
            }
        }
        Some(Mode::TellOnly(why)) => v.push(Row::Note(why.text(), false)),
        None => {}
    }
    v.push(Row::Note(
        "Updates are Tuner's stable releases on GitHub. Tuner installs a download only when it is signed with Tuner's release key, is the omatuner package for this computer and is newer than yours; installing asks for your password unless you allow it without (only for these updates). It checks at start and every 6 hours.".into(),
        false,
    ));
}

/// The passwordless switch moved (Enter, a click, Shift+←/→).
pub(super) fn passwordless(ctx: &mut UiCtx<'_>, on: bool) {
    if on != ctx.config.updates.passwordless {
        ctx.services.updater.set_passwordless(on);
    }
}

/// Its `Act::Feature(Feature::Updates, n)` buttons.
pub(super) fn act(_s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    match n {
        ACT_CHECK => ctx.services.updater.check_now(),
        ACT_INSTALL => ctx.services.updater.install_now(),
        ACT_RESTART => ctx.services.updater.restart_now(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{Asset, Package, Release, Why};

    fn release(v: &str) -> Release {
        let asset = Asset { name: String::new(), url: String::new(), size: 0 };
        Release {
            version: v.into(),
            page: String::new(),
            package: Some(Package { version: format!("{v}-1"), file: asset.clone(), signature: asset }),
        }
    }

    /// What the section says in each phase, and when it was checked.
    #[test]
    fn the_status_line_says_where_the_update_stands() {
        let me = crate::update::VERSION;
        let st = |phase| UpdateStatus { phase, last_check: Some(1_000), ..Default::default() };
        assert_eq!(status_line(&st(Phase::UpToDate), 1_000 + 720), format!("Tuner {me} · up to date · checked 12 min ago"));
        assert!(status_line(&st(Phase::Available(release("9.0.0"))), 1_000).starts_with(&format!("Tuner 9.0.0 is available (you have {me})")));
        let ready = Phase::Ready { release: release("9.0.0"), path: "/x".into(), declined: false };
        assert!(status_line(&st(ready), 1_000).contains("asks for your password"));
        assert!(status_line(&st(Phase::Installed { version: "9.0.0".into() }), 1_000).contains("restarts when idle"));
        let checking = UpdateStatus { checking: true, ..st(Phase::Idle) };
        assert_eq!(status_line(&checking, 1_000), format!("Tuner {me} · not checked yet · checking…"));
        let inert = UpdateStatus { inert: true, ..Default::default() };
        assert_eq!(status_line(&inert, 0), format!("Tuner {me} · update checks are off here"));
        assert!(Why::NotPackaged("/home/u/.cargo/bin/tuner".into()).text().contains("/home/u/.cargo/bin/tuner"));
    }
}
