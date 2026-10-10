//! Onboarding wizard (DESIGN §13.10): one centred card, steps: Welcome (Add playlist /
//! Restore backup) → Type (Xtream / M3U URL / M3U file — pasted get.php URLs auto-detected) →
//! Details (name, icon, colour, credentials) → Test (ProbeSource: account info, warnings) →
//! Guide (only for M3U without `url-tvg`: XMLTV URL/file or Skip) → Import progress
//! (AddSource job phases) → Done. Details (DESIGN §13.10): "Open Live TV" is enabled as soon as
//! the channels phase finishes; Test failures route back to the offending field (auth) or offer
//! Retry / Add anyway (unreachable); the Xtream server field is normalized with
//! `xtream::urls::normalize_server`. The same card serves `Route::Onboarding(start)` for
//! Settings ▸ Add, playlist files and restored sources needing credentials. Key contexts:
//! `[Modal]` on choice steps, `[TextInput, Modal]` while a form field is focused. No sidebar,
//! status bar or mini-player while onboarding. Owner: WP-12.
//!
//! Implementation notes:
//! * **One job at a time.** A probe or an import is submitted at most once until its
//!   `JobEvent::Finished` arrives; repeated Enter / Retry / "Add anyway" while one runs are
//!   ignored. Two concurrent `AddSource` jobs for a new locator would both insert a source
//!   (the job only reuses a *never-refreshed* row it can already see), so the wizard never
//!   lets that happen. A retry after a failed import resubmits the same draft, which reuses
//!   the source row saved by the failed attempt.
//! * **Estimates.** An M3U probe downloads and counts the whole playlist, so its number is the
//!   exact channel count; an Xtream probe only counts live *categories* (the stream list can
//!   be 100 MB) — the copy says so.
//! * **Channels first.** The import refreshes live channels first and rebuilds the library
//!   before movies/series; the channels row turns done (and "Open Live TV" enables) as soon
//!   as the library contains channels of a source that had none when the import started.
//! * **Text fields** follow the dialogs' pattern: Enter is acted on after the fields were drawn
//!   (a character typed in the same frame is not lost); Tab/Shift+Tab move between fields,
//!   Esc leaves a field, Esc again goes back a step.

use std::collections::HashSet;
use std::path::PathBuf;

use eframe::egui::{self, RichText};

use super::home::kit::{self, Loader};
use crate::jobs::{JobEvent, JobKind, JobOutcome, JobPhase, ProbeResult};
use crate::keymap::{Action, KeyContext};
use crate::model::{
    AccountInfo, AccountStatus, EpgSourceKind, JobId, Source, SourceColor, SourceDraft, SourceId, SourceKind, SourceSettings, XtreamCredentials,
};
use crate::msg::{AppEvent, DataScope};
use crate::platform::{PickPurpose, PlatformEvent};
use crate::store::Ticket;
use crate::theme::{SpaceToken, Theme, TypeToken};
use crate::ui::widgets::button::{ButtonStyle, button};
use crate::ui::widgets::card::{CardKind, card_frame};
use crate::ui::widgets::progress::{busy_bar, progress_bar};
use crate::ui::widgets::text_field::text_field;
use crate::ui::widgets::{RowState, glyphs, rows};
use crate::ui::{NavRequest, OnboardingStart, Route, Screen, ScreenId, SettingsPage, UiCtx};

/// Card width (pt × rem).
const CARD_W: f32 = 560.0;

pub(crate) const COLORS: [(SourceColor, &str); 10] = [
    (SourceColor::Accent, "Accent"),
    (SourceColor::Red, "Red"),
    (SourceColor::Yellow, "Yellow"),
    (SourceColor::Orange, "Orange"),
    (SourceColor::Green, "Green"),
    (SourceColor::Cyan, "Cyan"),
    (SourceColor::Blue, "Blue"),
    (SourceColor::Magenta, "Magenta"),
    (SourceColor::Brown, "Brown"),
    (SourceColor::Muted, "Muted"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Step {
    #[default]
    Welcome,
    Type,
    Details,
    Test,
    Guide,
    Importing,
    Done,
    Restoring,
}

/// Form rows of the Details and Guide steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field {
    Name,
    Icon,
    Color,
    Url,
    UseXtream,
    Server,
    Username,
    Password,
    Path,
    ChooseFile,
    Continue,
    // Guide step
    GuideUrl,
    GuideFile,
    Skip,
}

impl Field {
    fn is_text(self) -> bool {
        matches!(self, Field::Name | Field::Url | Field::Server | Field::Username | Field::Password | Field::Path | Field::GuideUrl)
    }
}

/// Why a probe failed, and what the wizard does about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Failure {
    /// Back to Details with this field focused.
    Field(Field),
    /// Network / timeout / 5xx: Retry or Add anyway.
    Unreachable,
}

/// Map a probe error (redacted `Error` display) to the wizard's reaction.
pub(crate) fn classify(msg: &str, kind: SourceKind) -> Failure {
    let m = msg.to_ascii_lowercase();
    let locator = match kind {
        SourceKind::Xtream => Field::Server,
        SourceKind::M3uUrl => Field::Url,
        SourceKind::M3uFile => Field::Path,
    };
    let login = if kind == SourceKind::Xtream { Field::Username } else { locator };
    let http = m.strip_prefix("http ").and_then(|r| r.split_whitespace().next()).and_then(|s| s.parse::<u16>().ok());
    if m.starts_with("network error") || matches!(http, Some(s) if s >= 500 || s == 408 || s == 429) {
        Failure::Unreachable
    } else if m.contains("login rejected")
        || matches!(http, Some(401 | 403))
        || m.starts_with("account ")
        || m.contains("password")
        || m.contains("username")
    {
        // rejected login, unusable account, or a "enter the username/password" config error
        Failure::Field(login)
    } else {
        Failure::Field(locator)
    }
}

/// The user-facing sentence for a probe error.
fn failure_text(msg: &str) -> String {
    let m = msg.to_ascii_lowercase();
    if m.contains("login rejected") {
        "The provider rejected this username or password.".into()
    } else if let Some(status) = m.strip_prefix("account ") {
        format!("This account is {status}. Check it with your provider.")
    } else if m.starts_with("network error") {
        format!("Couldn't reach the server ({}).", msg.trim_start_matches("network error: "))
    } else if let Some(rest) = msg.strip_prefix("unexpected response: ") {
        let mut s = rest.to_owned();
        if let Some(c) = s.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        s
    } else if let Some(rest) = msg.strip_prefix("config error: ") {
        let mut s = rest.to_owned();
        if let Some(c) = s.get_mut(0..1) {
            c.make_ascii_uppercase();
        }
        s
    } else {
        msg.to_owned()
    }
}

/// Estimate copy: exact for M3U (the probe counted every entry), categories for Xtream.
pub(crate) fn estimate_text(r: &ProbeResult) -> Option<String> {
    let n = r.estimate_channels?;
    Some(match r.kind {
        SourceKind::Xtream => format!("{n} live TV categor{}", if n == 1 { "y" } else { "ies" }),
        _ => format!("{n} channel{} in the playlist", if n == 1 { "" } else { "s" }),
    })
}

/// "Active · expires 2027-01-01 · 1/2 connections · ts, m3u8" pieces (urgent when unusable).
pub(crate) fn account_lines(a: &AccountInfo) -> Vec<(String, bool)> {
    let mut v = Vec::new();
    let status = match a.status {
        AccountStatus::Active => "Active",
        AccountStatus::Expired => "Expired",
        AccountStatus::Banned => "Banned",
        AccountStatus::Disabled => "Disabled",
        AccountStatus::Other => "Unusual status",
        AccountStatus::Unknown => "Status unknown",
    };
    let bad = matches!(a.status, AccountStatus::Expired | AccountStatus::Banned | AccountStatus::Disabled);
    let mut first = if a.is_trial { format!("{status} (trial)") } else { status.to_owned() };
    if let Some(exp) = a.exp_date {
        if a.status == AccountStatus::Expired || exp < crate::model::now() {
            first = format!("Expired on {}", kit::date(exp));
            v.push((first, true));
        } else {
            v.push((format!("{first} · expires {}", kit::date(exp)), bad));
        }
    } else {
        v.push((first, bad));
    }
    match (a.active_cons, a.max_connections) {
        (Some(c), Some(m)) => v.push((format!("{c}/{m} connections in use"), false)),
        (None, Some(m)) => v.push((format!("Up to {m} connection{}", if m == 1 { "" } else { "s" }), false)),
        _ => {}
    }
    if !a.allowed_formats.is_empty() {
        v.push((format!("Formats: {}", a.allowed_formats.join(", ")), false));
    }
    if let Some(m) = a.message.as_deref().filter(|m| !m.trim().is_empty()) {
        v.push((m.trim().to_owned(), false));
    }
    v
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Form {
    pub kind: Option<SourceKind>,
    pub name: String,
    /// The user typed a name (a probe's suggestion doesn't replace it).
    pub name_edited: bool,
    pub icon: usize,
    pub color: usize,
    pub url: String,
    pub path: String,
    pub server: String,
    pub username: String,
    pub password: String,
}

impl Form {
    fn kind(&self) -> SourceKind {
        self.kind.unwrap_or(SourceKind::Xtream)
    }

    pub(crate) fn fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Name, Field::Icon, Field::Color];
        match self.kind() {
            SourceKind::Xtream => v.extend([Field::Server, Field::Username, Field::Password]),
            SourceKind::M3uUrl => {
                v.push(Field::Url);
                if crate::sources::m3u::detect_xtream_url(&self.url).is_some() {
                    v.push(Field::UseXtream);
                }
            }
            SourceKind::M3uFile => v.extend([Field::Path, Field::ChooseFile]),
        }
        v.push(Field::Continue);
        v
    }

    /// Switch an Xtream `get.php` URL to the Xtream login.
    fn use_xtream(&mut self) -> bool {
        let Some(c) = crate::sources::m3u::detect_xtream_url(&self.url) else { return false };
        self.kind = Some(SourceKind::Xtream);
        self.server = c.server;
        self.username = c.username;
        self.password = c.password.expose().clone();
        self.icon = icon_for(SourceKind::Xtream);
        true
    }

    /// Validate and build the draft; `Err((field, message))` points at the problem.
    pub(crate) fn draft(&self) -> Result<SourceDraft, (Field, String)> {
        let kind = self.kind();
        let mut d = SourceDraft {
            name: self.name.trim().to_owned(),
            kind,
            icon: glyphs::SOURCE_ICONS.get(self.icon).copied().unwrap_or(glyphs::PLAYLIST).to_owned(),
            color: COLORS.get(self.color).map(|c| c.0).unwrap_or_default(),
            url: None,
            path: None,
            xtream: None,
            settings: SourceSettings::default(),
        };
        match kind {
            SourceKind::Xtream => {
                let server = crate::sources::xtream::urls::normalize_server(&self.server)
                    .ok_or((Field::Server, "Enter the server address, e.g. panel.example.com:8080".to_owned()))?;
                if self.username.trim().is_empty() {
                    return Err((Field::Username, "Enter your username".into()));
                }
                if self.password.is_empty() {
                    return Err((Field::Password, "Enter your password".into()));
                }
                d.xtream = Some(XtreamCredentials {
                    server,
                    username: self.username.trim().to_owned(),
                    password: crate::redact::Secret::new(self.password.clone()),
                });
            }
            SourceKind::M3uUrl => {
                let url = self.url.trim();
                if url.is_empty() {
                    return Err((Field::Url, "Enter the playlist URL".into()));
                }
                d.url = Some(if url.contains("://") { url.to_owned() } else { format!("http://{url}") });
            }
            SourceKind::M3uFile => {
                let p = self.path.trim();
                if p.is_empty() {
                    return Err((Field::Path, "Choose a playlist file".into()));
                }
                d.path = Some(PathBuf::from(p));
            }
        }
        if d.name.is_empty() {
            d.name = self.default_name();
        }
        Ok(d)
    }

    /// Host name / file stem / a generic name.
    fn default_name(&self) -> String {
        let host = |s: &str| {
            let t = s.trim();
            let with = if t.contains("://") { t.to_owned() } else { format!("http://{t}") };
            url::Url::parse(&with).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_owned()))
        };
        match self.kind() {
            SourceKind::Xtream => host(&self.server),
            SourceKind::M3uUrl => host(&self.url),
            SourceKind::M3uFile => PathBuf::from(self.path.trim()).file_stem().map(|s| s.to_string_lossy().into_owned()),
        }
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My playlist".into())
    }
}

fn icon_for(kind: SourceKind) -> usize {
    let g = match kind {
        SourceKind::Xtream => glyphs::SOURCE_XTREAM,
        SourceKind::M3uUrl => glyphs::SOURCE_URL,
        SourceKind::M3uFile => glyphs::PLAYLIST,
    };
    glyphs::SOURCE_ICONS.iter().position(|x| *x == g).unwrap_or(0)
}

/// The import in progress (or finished).
#[derive(Default)]
struct Import {
    job: Option<JobId>,
    /// Sources that had channels when the import started.
    had_channels: HashSet<SourceId>,
    channels_done: bool,
    finished: bool,
    source: Option<SourceId>,
    error: Option<String>,
    /// The XMLTV source the Guide step asked for (inserted once).
    guide_ticket: Option<Ticket<crate::model::EpgSourceId>>,
    guide_requested: bool,
}

#[derive(Default)]
pub struct OnboardingScreen {
    start: OnboardingStart,
    step: Step,
    form: Form,
    /// Focused row of the current step.
    focus: usize,
    /// A text field owns the keyboard (`TextInput`).
    editing: bool,
    /// Enter/activate requested; acted on after the fields were drawn (see module docs).
    submit: bool,
    error: Option<(Field, String)>,
    probe: Option<JobId>,
    probe_result: Option<ProbeResult>,
    /// Probe failed with a network-ish error (Test step: Retry / Add anyway).
    unreachable: Option<String>,
    guide_url: String,
    guide_path: Option<PathBuf>,
    import: Import,
    restore: Option<JobId>,
    restore_report: Option<Result<crate::store::backup::RestoreReport, String>>,
    creds_loader: Option<Loader<Source>>,
    /// The file dialog this card opened (answers to other screens' dialogs are ignored).
    asked: Option<PickPurpose>,
    /// Done: the guides and how many channels each matches (re-read when the guide changes).
    guides: Option<super::settings::GuideList>,
    guides_loader: Option<Loader<super::settings::GuideList>>,
    /// Done offers "TV guide settings" (no guide, or it covers under half the channels).
    guide_action: bool,
}

impl OnboardingScreen {
    /// Rows of a choice step / fields of a form step.
    fn rows(&self) -> usize {
        match self.step {
            Step::Welcome => 2,
            Step::Type => 3,
            Step::Details => self.form.fields().len(),
            Step::Guide => self.guide_fields().len(),
            Step::Test => self.test_buttons().len(),
            Step::Importing => 2,
            Step::Done => 3 + usize::from(self.guide_action),
            Step::Restoring => 1,
        }
    }

    fn guide_fields(&self) -> Vec<Field> {
        vec![Field::GuideUrl, Field::GuideFile, Field::Continue, Field::Skip]
    }

    /// Test step buttons for the current state.
    fn test_buttons(&self) -> Vec<&'static str> {
        if self.unreachable.is_some() {
            vec!["Retry", "Add anyway", "Back"]
        } else if self.probe_result.is_some() {
            vec!["Continue", "Back"]
        } else {
            vec!["Back"]
        }
    }

    fn current_field(&self) -> Option<Field> {
        match self.step {
            Step::Details => self.form.fields().get(self.focus).copied(),
            Step::Guide => self.guide_fields().get(self.focus).copied(),
            _ => None,
        }
    }

    fn go(&mut self, step: Step) {
        self.step = step;
        self.focus = 0;
        self.editing = matches!(step, Step::Details | Step::Guide) && self.current_field().is_some_and(Field::is_text);
        self.submit = false;
    }

    fn focus_field(&mut self, f: Field) {
        if let Some(i) = self.form.fields().iter().position(|x| *x == f) {
            self.focus = i;
            self.editing = f.is_text();
        }
    }

    fn choose_kind(&mut self, kind: SourceKind) {
        if self.form.kind != Some(kind) {
            self.form.icon = icon_for(kind);
        }
        self.form.kind = Some(kind);
        self.error = None;
        self.go(Step::Details);
        // start at the first credential field (the name is optional)
        let first = match kind {
            SourceKind::Xtream => Field::Server,
            SourceKind::M3uUrl => Field::Url,
            SourceKind::M3uFile => Field::Path,
        };
        self.focus_field(first);
    }

    /// Back one step; false when there is nothing before (the app's Back then leaves).
    /// The Back / Cancel button under the card (`None` where Esc does nothing: first-run
    /// Welcome, a running import, the end of a restore).
    fn back_label(&self) -> Option<&'static str> {
        match self.step {
            Step::Welcome | Step::Importing | Step::Done | Step::Restoring => None,
            Step::Test if self.probe.is_some() => Some("Cancel"),
            _ if self.step == self.first_step() => Some("Cancel"),
            _ => Some("Back"),
        }
    }

    /// Where this card starts for its route.
    fn first_step(&self) -> Step {
        match self.start {
            OnboardingStart::Welcome => Step::Welcome,
            OnboardingStart::AddSource => Step::Type,
            OnboardingStart::PlaylistFile(_) | OnboardingStart::NeedsCredentials(_) => Step::Details,
        }
    }

    fn back(&mut self) -> bool {
        let first = self.first_step();
        let prev = match self.step {
            s if s == first => return false,
            Step::Welcome => return false,
            Step::Type => Step::Welcome,
            Step::Details => Step::Type,
            Step::Test => Step::Details,
            Step::Guide => Step::Test,
            // a running import keeps running (and the button says so); leaving is the app's
            Step::Importing | Step::Done | Step::Restoring => return false,
        };
        if self.step == Step::Test {
            // a late probe answer is ignored
            self.probe = None;
            self.probe_result = None;
            self.unreachable = None;
        }
        self.go(prev);
        true
    }

    /// Details ▸ Continue: validate and probe (at most one probe at a time).
    fn test(&mut self, ctx: &mut UiCtx<'_>) {
        if self.probe.is_some() {
            return;
        }
        match self.form.draft() {
            Err((f, msg)) => {
                self.error = Some((f, msg));
                self.focus_field(f);
            }
            Ok(d) => {
                self.error = None;
                self.probe_result = None;
                self.unreachable = None;
                self.probe = Some(ctx.services.jobs.submit(JobKind::ProbeSource(d)));
                self.go(Step::Test);
            }
        }
    }

    /// Test ▸ Continue: the Guide step for M3U without `url-tvg`, else import.
    fn after_test(&mut self, ctx: &mut UiCtx<'_>) {
        let needs_guide = self.form.kind() != SourceKind::Xtream
            && self.probe_result.as_ref().is_none_or(|r| r.epg_urls.is_empty() && r.kind != SourceKind::Xtream);
        if needs_guide {
            self.go(Step::Guide);
        } else {
            self.start_import(ctx);
        }
    }

    /// Submit `AddSource` exactly once per attempt (see module docs).
    fn start_import(&mut self, ctx: &mut UiCtx<'_>) {
        if self.import.job.is_some() {
            return;
        }
        let mut draft = match self.form.draft() {
            Ok(d) => d,
            Err((f, msg)) => {
                self.error = Some((f, msg));
                self.go(Step::Details);
                self.focus_field(f);
                return;
            }
        };
        // a pasted get.php URL the probe upgraded to Xtream
        if let Some(r) = &self.probe_result
            && r.kind == SourceKind::Xtream
            && draft.kind != SourceKind::Xtream
            && let Some(x) = r.xtream.clone()
        {
            draft.kind = SourceKind::Xtream;
            draft.xtream = Some(x);
            draft.url = None;
        }
        let had: HashSet<SourceId> = ctx.lib.live.channels.iter().map(|c| c.source_id).collect();
        self.import = Import { had_channels: had, guide_requested: self.import.guide_requested, ..Default::default() };
        self.import.job = Some(ctx.services.jobs.submit(JobKind::AddSource(draft)));
        self.request_guide(ctx);
        self.go(Step::Importing);
    }

    /// Insert the XMLTV source chosen in the Guide step (once) and refresh it.
    fn request_guide(&mut self, ctx: &mut UiCtx<'_>) {
        if self.import.guide_requested {
            return;
        }
        let kind = if let Some(p) = self.guide_path.clone() {
            EpgSourceKind::File { path: p }
        } else if !self.guide_url.trim().is_empty() {
            let u = self.guide_url.trim();
            EpgSourceKind::Url { url: crate::redact::Secret::new(if u.contains("://") { u.to_owned() } else { format!("http://{u}") }) }
        } else {
            return;
        };
        self.import.guide_requested = true;
        let name = format!("{} guide", self.form.draft().map(|d| d.name).unwrap_or_else(|_| "Playlist".into()));
        self.import.guide_ticket =
            Some(ctx.services.store.writer().submit_notify(DataScope::Epg, move |c| crate::store::sources::insert_epg_source(c, &name, &kind, 0)));
    }

    fn ask_file(&mut self, ctx: &UiCtx<'_>, purpose: PickPurpose) {
        self.asked = Some(purpose);
        kit::pick_file(ctx, purpose);
    }

    fn open_live(&self, ctx: &mut UiCtx<'_>) {
        ctx.navigate(NavRequest::Switch(Route::Live { group: None, channel: None }));
    }

    fn reset_for_another(&mut self) {
        let start = std::mem::take(&mut self.start);
        *self = Self::default();
        self.start = start;
        self.go(Step::Type);
    }

    /// Activate the focused row / button of the current step.
    fn activate(&mut self, ctx: &mut UiCtx<'_>) {
        match self.step {
            Step::Welcome => {
                if self.focus == 0 {
                    self.go(Step::Type);
                } else {
                    self.ask_file(ctx, PickPurpose::ImportBackup);
                }
            }
            Step::Type => self.choose_kind([SourceKind::Xtream, SourceKind::M3uUrl, SourceKind::M3uFile][self.focus.min(2)]),
            Step::Details => match self.current_field() {
                Some(Field::Icon) => self.form.icon = (self.form.icon + 1) % glyphs::SOURCE_ICONS.len(),
                Some(Field::Color) => self.form.color = (self.form.color + 1) % COLORS.len(),
                Some(Field::ChooseFile) => self.ask_file(ctx, PickPurpose::OpenPlaylist),
                Some(Field::UseXtream) => {
                    if self.form.use_xtream() {
                        self.focus_field(Field::Continue);
                    }
                }
                Some(Field::Continue) => self.test(ctx),
                Some(f) if f.is_text() => {
                    // Enter in a field: next field, the last one submits
                    let fields = self.form.fields();
                    let next = fields.iter().skip(self.focus + 1).position(|f| f.is_text()).map(|i| i + self.focus + 1);
                    match next {
                        Some(n) if self.editing => {
                            self.focus = n;
                            self.editing = true;
                        }
                        _ if self.editing => self.test(ctx),
                        _ => self.editing = true,
                    }
                }
                _ => {}
            },
            Step::Test => match self.test_buttons().get(self.focus).copied() {
                Some("Continue") | Some("Add anyway") => self.after_test(ctx),
                Some("Retry") => {
                    self.unreachable = None;
                    self.test(ctx);
                }
                Some("Back") => {
                    self.back();
                }
                _ => {}
            },
            Step::Guide => match self.current_field() {
                Some(Field::GuideFile) => self.ask_file(ctx, PickPurpose::OpenEpg),
                Some(Field::Skip) => {
                    self.guide_url.clear();
                    self.guide_path = None;
                    self.start_import(ctx);
                }
                Some(Field::GuideUrl) if !self.editing => self.editing = true,
                Some(Field::GuideUrl) | Some(Field::Continue) => self.start_import(ctx),
                _ => {}
            },
            Step::Importing => {
                if self.import.error.is_some() {
                    if self.focus == 0 {
                        // retry: the job reuses the row saved by the failed attempt
                        self.import.job = None;
                        self.import.error = None;
                        self.start_import(ctx);
                    } else {
                        self.go(Step::Details);
                    }
                } else if self.focus == 0 && self.import.channels_done {
                    self.open_live(ctx);
                }
            }
            Step::Done => match self.focus {
                0 => self.open_live(ctx),
                1 => self.reset_for_another(),
                3 => ctx.navigate(NavRequest::Switch(Route::Settings(SettingsPage::Epg))),
                _ => ctx.navigate(NavRequest::Switch(Route::Home)),
            },
            Step::Restoring => {
                if self.restore.is_none() {
                    self.open_live(ctx);
                }
            }
        }
    }

    fn step_title(&self) -> (&'static str, &'static str) {
        match self.step {
            Step::Welcome => ("Welcome to Tuner", "Add a playlist to start watching, or restore a backup."),
            Step::Type => ("Add a playlist", "How does your provider give you access?"),
            Step::Details => match self.form.kind() {
                SourceKind::Xtream => ("Xtream login", "Server, username and password from your provider."),
                SourceKind::M3uUrl => ("M3U playlist URL", "Paste the playlist link (a get.php link becomes an Xtream login)."),
                SourceKind::M3uFile => ("M3U playlist file", "Pick an .m3u or .m3u8 file on this computer."),
            },
            Step::Test => ("Checking your playlist", "Talking to the provider…"),
            Step::Guide => ("TV guide", "This playlist has no guide. Add an XMLTV guide, or skip — you can add one later in Settings ▸ EPG."),
            Step::Importing => ("Importing", "Channels come first; movies, series and the guide continue in the background."),
            Step::Done => ("All set", "Your playlist is ready."),
            Step::Restoring => ("Restoring the backup", "Your sources refresh after the restore."),
        }
    }

    fn poll_jobs(&mut self, ctx: &mut UiCtx<'_>) {
        // channels of the new source are in the library → "Open Live TV"
        if self.step == Step::Importing && !self.import.channels_done {
            let had = &self.import.had_channels;
            if let Some(c) = ctx.lib.live.channels.iter().find(|c| !had.contains(&c.source_id)) {
                self.import.channels_done = true;
                self.import.source = Some(c.source_id);
            }
        }
        if let Some(t) = &mut self.import.guide_ticket {
            match t.try_take() {
                Some(Ok(id)) => {
                    ctx.services.jobs.submit(JobKind::RefreshEpg { epg_source: id, force: true });
                    self.import.guide_ticket = None;
                }
                Some(Err(e)) => {
                    kit::error_toast(ctx, "Couldn't add the guide", &e);
                    self.import.guide_ticket = None;
                }
                None => ui_repaint(ctx),
            }
        }
        match kit::poll(&mut self.creds_loader) {
            Some(Ok(src)) => {
                self.form.kind = Some(src.kind);
                self.form.name = src.name.clone();
                self.form.name_edited = true;
                self.form.icon = glyphs::SOURCE_ICONS.iter().position(|g| *g == src.icon).unwrap_or_else(|| icon_for(src.kind));
                self.form.color = COLORS.iter().position(|c| c.0 == src.color).unwrap_or(0);
                self.form.url = src.url.as_ref().map(|u| u.expose().clone()).unwrap_or_default();
                self.form.path = src.path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                if let Some(x) = &src.xtream {
                    self.form.server = x.server.clone();
                    self.form.username = x.username.clone();
                }
                self.go(Step::Details);
                self.focus_field(if src.kind == SourceKind::Xtream { Field::Password } else { Field::Url });
            }
            Some(Err(e)) => kit::error_toast(ctx, "Couldn't read that playlist", &e),
            None => {}
        }
    }
}

fn ui_repaint(ctx: &UiCtx<'_>) {
    ctx.services.bus.request_repaint();
}

impl Screen for OnboardingScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Onboarding
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        // no field takes the keyboard while a restored source is still being read
        let field = self.editing && self.creds_loader.is_none() && self.current_field().is_some_and(Field::is_text);
        if field { vec![KeyContext::TextInput, KeyContext::Modal] } else { vec![KeyContext::Modal] }
    }

    fn allows_mini_player(&self) -> bool {
        false
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, route: &Route) {
        let Route::Onboarding(start) = route else { return };
        // keep a running import / restore when the card is shown again
        if matches!(self.step, Step::Importing | Step::Restoring) && (self.import.job.is_some() || self.restore.is_some()) && *start == self.start {
            return;
        }
        *self = Self { start: start.clone(), ..Default::default() };
        match start {
            OnboardingStart::Welcome => self.go(Step::Welcome),
            OnboardingStart::AddSource => self.go(Step::Type),
            OnboardingStart::PlaylistFile(p) => {
                self.form.path = p.display().to_string();
                self.form.name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                self.form.name_edited = true;
                self.form.kind = Some(SourceKind::M3uFile);
                self.form.icon = icon_for(SourceKind::M3uFile);
                self.go(Step::Details);
                self.focus_field(Field::Continue);
            }
            OnboardingStart::NeedsCredentials(id) => {
                let id = *id;
                self.go(Step::Details);
                self.creds_loader = Some(Loader::start(ctx, move |c| crate::store::sources::get_source(c, id)));
            }
        }
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        match ev {
            AppEvent::Job(JobEvent::Finished { id, outcome, .. }) if Some(*id) == self.probe => {
                self.probe = None;
                match outcome {
                    Ok(JobOutcome::Probe(r)) => {
                        if !self.form.name_edited
                            && let Some(n) = &r.suggested_name
                        {
                            self.form.name = n.clone();
                        }
                        self.probe_result = Some(r.clone());
                        self.unreachable = None;
                        self.focus = 0;
                    }
                    Ok(_) => {}
                    Err(msg) if msg == &crate::error::Error::Cancelled.to_string() => {}
                    Err(msg) => match classify(msg, self.form.kind()) {
                        Failure::Unreachable => {
                            self.unreachable = Some(failure_text(msg));
                            self.focus = 0;
                        }
                        Failure::Field(f) => {
                            self.error = Some((f, failure_text(msg)));
                            self.go(Step::Details);
                            self.focus_field(f);
                        }
                    },
                }
            }
            AppEvent::Job(JobEvent::Finished { id, outcome, .. }) if Some(*id) == self.import.job => {
                self.import.job = None;
                match outcome {
                    Ok(JobOutcome::SourceAdded(s)) => {
                        self.import.finished = true;
                        self.import.source = Some(*s);
                        self.import.channels_done = true;
                        if self.step == Step::Importing {
                            self.go(Step::Done);
                        }
                    }
                    Ok(_) => {}
                    Err(msg) => {
                        self.import.error = Some(failure_text(msg));
                        self.focus = 0;
                    }
                }
            }
            AppEvent::Job(JobEvent::Finished { id, outcome, .. }) if Some(*id) == self.restore => {
                self.restore = None;
                self.restore_report = Some(match outcome {
                    Ok(JobOutcome::Restored(r)) => Ok(r.clone()),
                    Ok(_) => Err("unexpected answer".into()),
                    Err(e) => Err(e.clone()),
                });
            }
            // a guide import finished (or mappings changed): the Done summary re-reads coverage
            // (a read already running may predate the change: drop it)
            AppEvent::Data(DataScope::Epg | DataScope::Live | DataScope::All) => {
                self.guides = None;
                self.guides_loader = None;
            }
            AppEvent::Platform(PlatformEvent::FilePicked { purpose, path }) if self.asked == Some(*purpose) => match (purpose, path) {
                (_, None) => self.asked = None,
                (PickPurpose::OpenPlaylist, Some(path)) if self.step == Step::Details => {
                    self.asked = None;
                    self.form.path = path.display().to_string();
                    if !self.form.name_edited {
                        self.form.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    }
                    self.focus_field(Field::Continue);
                }
                (PickPurpose::OpenEpg, Some(path)) if self.step == Step::Guide => {
                    self.asked = None;
                    self.guide_path = Some(path.clone());
                    self.guide_url.clear();
                    self.focus = self.guide_fields().iter().position(|f| *f == Field::Continue).unwrap_or(0);
                    self.editing = false;
                }
                (PickPurpose::ImportBackup, Some(path)) if self.step == Step::Welcome && self.restore.is_none() => {
                    use crate::store::backup::RestoreMode;
                    self.asked = None;
                    self.restore = Some(ctx.services.jobs.submit(JobKind::ImportBackup { path: path.clone(), mode: RestoreMode::Merge }));
                    self.restore_report = None;
                    self.go(Step::Restoring);
                }
                _ => {}
            },
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        let n = self.rows().max(1);
        let in_form = matches!(self.step, Step::Details | Step::Guide);
        match action {
            Action::Back if self.editing => {
                self.editing = false;
                true
            }
            Action::Back => self.back(),
            Action::Up | Action::FocusPrev => {
                self.focus = if action == Action::FocusPrev && self.focus == 0 { n - 1 } else { self.focus.saturating_sub(1) };
                self.editing = in_form && self.current_field().is_some_and(Field::is_text);
                true
            }
            Action::Down | Action::FocusNext => {
                self.focus = if action == Action::FocusNext && self.focus + 1 >= n { 0 } else { (self.focus + 1).min(n - 1) };
                self.editing = in_form && self.current_field().is_some_and(Field::is_text);
                true
            }
            Action::Left | Action::Right => {
                let fwd = action == Action::Right;
                match self.current_field() {
                    Some(Field::Icon) => {
                        let m = glyphs::SOURCE_ICONS.len();
                        self.form.icon = if fwd { (self.form.icon + 1) % m } else { (self.form.icon + m - 1) % m };
                    }
                    Some(Field::Color) => {
                        let m = COLORS.len();
                        self.form.color = if fwd { (self.form.color + 1) % m } else { (self.form.color + m - 1) % m };
                    }
                    _ if !in_form => {
                        // buttons laid out in a row
                        self.focus = if fwd { (self.focus + 1).min(n - 1) } else { self.focus.saturating_sub(1) };
                    }
                    _ => {}
                }
                true
            }
            Action::Activate | Action::ActivateAlt => {
                // acted on after the fields of this frame were drawn (see module docs)
                self.submit = true;
                ui_repaint(ctx);
                true
            }
            _ => false,
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        self.poll_jobs(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        let full = ui.available_rect_before_wrap();
        let width = (CARD_W * theme.rem()).min(full.width() - 2.0 * theme.space(SpaceToken::PanelPadding)).max(240.0);
        let top = full.top() + (full.height() * 0.08).min(80.0);
        let frame = card_frame(theme, CardKind::Menu);
        let chrome = frame.total_margin().sum().x;
        let card_rect = egui::Rect::from_min_size(egui::pos2(full.center().x - width / 2.0, top), egui::vec2(width, full.bottom() - top));
        let mut card_ui =
            ui.new_child(egui::UiBuilder::new().id_salt("onboarding-card").max_rect(card_rect).layout(egui::Layout::top_down(egui::Align::Min)));
        let mut clicked: Option<usize> = None;
        let mut back_clicked = false;
        frame.show(&mut card_ui, |ui| {
            ui.set_width(width - chrome);
            egui::ScrollArea::vertical().id_salt("onboarding").max_height(full.height() - 2.0 * theme.space(SpaceToken::PanelPadding)).show(
                ui,
                |ui| {
                    let (title, sub) = self.step_title();
                    ui.label(RichText::new(title).size(theme.font(TypeToken::Heading)).color(t.fg));
                    ui.add_space(theme.space(SpaceToken::Xs));
                    ui.add(egui::Label::new(RichText::new(sub).size(theme.font(TypeToken::Body)).color(t.text_secondary)).wrap());
                    ui.add_space(theme.space(SpaceToken::Xxl));
                    clicked = match self.step {
                        Step::Welcome => choice_rows(
                            ui,
                            theme,
                            self.focus,
                            &[
                                (glyphs::ADD, "Add a playlist", "Xtream login, M3U URL or M3U file"),
                                (glyphs::BACKUP, "Restore a backup", "A Tuner backup file (.json)"),
                            ],
                        ),
                        Step::Type => choice_rows(
                            ui,
                            theme,
                            self.focus,
                            &[
                                (glyphs::SOURCE_XTREAM, "Xtream login", "Server, username and password"),
                                (glyphs::SOURCE_URL, "M3U URL", "A playlist link (get.php links become Xtream logins)"),
                                (glyphs::FOLDER, "M3U file", "An .m3u / .m3u8 file on this computer"),
                            ],
                        ),
                        Step::Details => self.details_ui(ui, ctx),
                        Step::Test => self.test_ui(ui, ctx),
                        Step::Guide => self.guide_ui(ui, ctx),
                        Step::Importing => self.importing_ui(ui, ctx),
                        Step::Done => self.done_ui(ui, ctx),
                        Step::Restoring => self.restoring_ui(ui, ctx),
                    };
                    ui.add_space(theme.space(SpaceToken::Lg));
                    // the mouse twin of Esc: a step back, or out of the card where it was opened
                    // from Settings / a file / a restored playlist
                    ui.horizontal(|ui| {
                        if let Some(label) = self.back_label() {
                            back_clicked = button(ui, theme, Some(glyphs::PREVIOUS), label, ButtonStyle::default()).clicked();
                            ui.add_space(theme.space(SpaceToken::Lg));
                        }
                        let hint = if self.editing {
                            "Enter next · Tab next field · Esc leave the field"
                        } else {
                            "↑/↓ move · Enter choose · Esc back"
                        };
                        ui.label(RichText::new(hint).size(theme.font(TypeToken::BodySmall)).color(t.text_disabled));
                    });
                },
            );
        });
        ui.allocate_rect(full, egui::Sense::hover());
        if back_clicked && !self.back() {
            ctx.navigate(crate::ui::NavRequest::Back);
        }
        if let Some(i) = clicked {
            self.focus = i;
            self.editing = self.current_field().is_some_and(Field::is_text);
            if !self.editing {
                self.submit = true;
            }
        }
        if std::mem::take(&mut self.submit) {
            self.activate(ctx);
            ui.ctx().request_repaint();
        }
        if self.probe.is_some() || self.import.job.is_some() || self.restore.is_some() {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

/// Launcher-style choice rows; returns the clicked row.
fn choice_rows(ui: &mut egui::Ui, theme: &Theme, focus: usize, items: &[(&str, &str, &str)]) -> Option<usize> {
    let mut clicked = None;
    for (i, (icon, label, detail)) in items.iter().enumerate() {
        let state = if i == focus { RowState::Cursor } else { RowState::Idle };
        if rows::menu_row(ui, theme, Some(icon), label, Some(detail), true, state).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// A labelled text field row that follows the wizard's focus; returns true when clicked.
#[allow(clippy::too_many_arguments)]
fn field_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    label: &str,
    hint: Option<&str>,
    text: &mut String,
    placeholder: &str,
    password: bool,
    focused: bool,
    editing: bool,
    error: Option<&str>,
) -> (bool, bool) {
    let t = &theme.tokens;
    ui.label(RichText::new(label).size(theme.font(TypeToken::BodySmall)).color(if focused { t.accent } else { t.text_secondary }));
    let resp = text_field(ui, theme, text, placeholder, password);
    if focused && editing && !resp.has_focus() {
        resp.request_focus();
    } else if (!focused || !editing) && resp.has_focus() {
        ui.memory_mut(|m| m.surrender_focus(resp.id));
    }
    if let Some(h) = hint {
        ui.label(RichText::new(h).size(theme.font(TypeToken::BodySmall)).color(t.text_disabled));
    }
    if let Some(e) = error {
        ui.label(RichText::new(e).size(theme.font(TypeToken::BodySmall)).color(t.urgent));
    }
    ui.add_space(theme.space(SpaceToken::Md));
    (resp.clicked() || resp.gained_focus(), resp.changed())
}

/// `‹ value ›` picker row (icon, colour).
fn picker_row(ui: &mut egui::Ui, theme: &Theme, label: &str, value: &str, color: egui::Color32, focused: bool) -> bool {
    let t = &theme.tokens;
    let r = ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(if focused { t.accent } else { t.text_secondary }));
        ui.add_space(theme.space(SpaceToken::Lg));
        let b = button(ui, theme, None, &format!("‹  {value}  ›"), ButtonStyle { focused, bordered: true, ..Default::default() });
        // a swatch of the value's colour (icons: fg), as tall as the text
        let side = theme.font(TypeToken::Body).round();
        let sw = egui::Rect::from_min_size(
            egui::pos2(b.rect.right() + theme.space(SpaceToken::Lg), b.rect.center().y - side / 2.0),
            egui::vec2(side, side),
        );
        ui.allocate_rect(sw, egui::Sense::hover());
        ui.painter().rect_filled(sw, t.rounding, color);
        b.clicked()
    });
    ui.add_space(theme.space(SpaceToken::Md));
    r.inner
}

impl OnboardingScreen {
    fn details_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if self.creds_loader.is_some() {
            busy_bar(ui, theme, 3.0);
            return None;
        }
        let fields = self.form.fields();
        let focus = self.focus;
        let editing = self.editing;
        let err = |f: Field| self.error.as_ref().filter(|(ef, _)| *ef == f).map(|(_, m)| m.clone());
        let mut clicked = None;
        for (i, f) in fields.iter().enumerate() {
            let focused = i == focus;
            let e = err(*f);
            let hit = match f {
                Field::Name => {
                    let placeholder = self.form.default_name();
                    let (c, changed) = field_row(ui, theme, "Name", None, &mut self.form.name, &placeholder, false, focused, editing, e.as_deref());
                    if changed {
                        self.form.name_edited = true;
                    }
                    c
                }
                Field::Icon => {
                    let g = glyphs::SOURCE_ICONS.get(self.form.icon).copied().unwrap_or(glyphs::PLAYLIST);
                    picker_row(ui, theme, "Icon", g, t.fg, focused)
                }
                Field::Color => {
                    let (c, name) = COLORS.get(self.form.color).copied().unwrap_or(COLORS[0]);
                    let col = theme.source_color(c);
                    let g = glyphs::SOURCE_ICONS.get(self.form.icon).copied().unwrap_or(glyphs::PLAYLIST);
                    let r = picker_row(ui, theme, "Colour", name, col, focused);
                    // the preview is text: yellow on a light theme falls back to fg
                    let text = kit::legible(theme, col);
                    ui.label(
                        RichText::new(format!(
                            "{g}  {}",
                            if self.form.name.trim().is_empty() { self.form.default_name() } else { self.form.name.trim().to_owned() }
                        ))
                        .size(theme.font(TypeToken::Body))
                        .color(text),
                    );
                    ui.add_space(theme.space(SpaceToken::Md));
                    r
                }
                Field::Server => {
                    field_row(
                        ui,
                        theme,
                        "Server",
                        Some("http:// is added when missing; paths and queries are removed; the port is kept."),
                        &mut self.form.server,
                        "panel.example.com:8080",
                        false,
                        focused,
                        editing,
                        e.as_deref(),
                    )
                    .0
                }
                Field::Username => {
                    field_row(ui, theme, "Username", None, &mut self.form.username, "username", false, focused, editing, e.as_deref()).0
                }
                Field::Password => {
                    field_row(ui, theme, "Password", None, &mut self.form.password, "password", true, focused, editing, e.as_deref()).0
                }
                Field::Url => {
                    field_row(
                        ui,
                        theme,
                        "Playlist URL",
                        None,
                        &mut self.form.url,
                        "http://example.com/playlist.m3u",
                        false,
                        focused,
                        editing,
                        e.as_deref(),
                    )
                    .0
                }
                Field::UseXtream => {
                    ui.label(
                        RichText::new("This is an Xtream link: the Xtream login also brings movies, series and account info.")
                            .size(theme.font(TypeToken::BodySmall))
                            .color(t.text_secondary),
                    );
                    let r = button(
                        ui,
                        theme,
                        Some(glyphs::SOURCE_XTREAM),
                        "Use the Xtream login instead",
                        ButtonStyle { focused, bordered: true, ..Default::default() },
                    )
                    .clicked();
                    ui.add_space(theme.space(SpaceToken::Md));
                    r
                }
                Field::Path => {
                    field_row(ui, theme, "Playlist file", None, &mut self.form.path, "/home/you/playlist.m3u", false, focused, editing, e.as_deref())
                        .0
                }
                Field::ChooseFile => {
                    let r = button(ui, theme, Some(glyphs::FOLDER), "Choose file…", ButtonStyle { focused, bordered: true, ..Default::default() })
                        .clicked();
                    ui.add_space(theme.space(SpaceToken::Md));
                    r
                }
                Field::Continue => {
                    if let Some(m) = e {
                        ui.label(RichText::new(m).size(theme.font(TypeToken::BodySmall)).color(t.urgent));
                    }
                    ui.add_space(theme.space(SpaceToken::Md));
                    button(ui, theme, Some(glyphs::CHEVRON_RIGHT), "Continue", ButtonStyle { primary: true, focused, ..Default::default() }).clicked()
                }
                _ => false,
            };
            if hit {
                clicked = Some(i);
            }
        }
        clicked
    }

    fn test_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if self.probe.is_some() {
            let phase = ctx
                .services
                .jobs
                .active()
                .into_iter()
                .find(|j| Some(j.id) == self.probe)
                .map(|j| phase_text(&j.phase))
                .unwrap_or_else(|| "Connecting…".into());
            ui.label(RichText::new(phase).size(theme.font(TypeToken::Body)).color(t.text_secondary));
            ui.add_space(theme.space(SpaceToken::Md));
            busy_bar(ui, theme, (3.0 * theme.rem()).round());
            ui.add_space(theme.space(SpaceToken::Lg));
        } else if let Some(msg) = &self.unreachable {
            ui.label(RichText::new(format!("{}  {msg}", glyphs::OFFLINE)).size(theme.font(TypeToken::Body)).color(t.urgent));
            ui.label(
                RichText::new("You can retry, or add the playlist anyway and refresh it later.")
                    .size(theme.font(TypeToken::BodySmall))
                    .color(t.text_secondary),
            );
            ui.add_space(theme.space(SpaceToken::Lg));
        } else if let Some(r) = &self.probe_result {
            let kind = match r.kind {
                SourceKind::Xtream => "Xtream login",
                SourceKind::M3uUrl => "M3U playlist",
                SourceKind::M3uFile => "M3U file",
            };
            ui.label(RichText::new(format!("{}  Looks good — {kind}", glyphs::CHECK)).size(theme.font(TypeToken::Title)).color(t.success));
            if r.kind == SourceKind::Xtream && self.form.kind() != SourceKind::Xtream {
                ui.label(
                    RichText::new("This link is an Xtream login; it will be added as one (movies, series and account info included).")
                        .size(theme.font(TypeToken::BodySmall))
                        .color(t.text_secondary),
                );
            }
            if let Some(e) = estimate_text(r) {
                ui.label(RichText::new(e).size(theme.font(TypeToken::Body)).color(t.fg));
            }
            if let Some(a) = &r.account {
                for (line, bad) in account_lines(a) {
                    ui.label(RichText::new(line).size(theme.font(TypeToken::Body)).color(if bad { t.urgent } else { t.text_secondary }));
                }
            }
            if !r.epg_urls.is_empty() {
                ui.label(RichText::new("Includes a TV guide").size(theme.font(TypeToken::Body)).color(t.text_secondary));
            }
            ui.add_space(theme.space(SpaceToken::Lg));
        }
        button_row(ui, theme, &self.test_buttons(), self.focus)
    }

    fn guide_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let fields = self.guide_fields();
        let mut clicked = None;
        for (i, f) in fields.iter().enumerate() {
            let focused = i == self.focus;
            let hit = match f {
                Field::GuideUrl => {
                    let (c, changed) = field_row(
                        ui,
                        theme,
                        "XMLTV URL",
                        Some(".xml, .xml.gz or .xml.xz"),
                        &mut self.guide_url,
                        "http://example.com/guide.xml.gz",
                        false,
                        focused,
                        self.editing,
                        None,
                    );
                    if changed {
                        self.guide_path = None;
                    }
                    c
                }
                Field::GuideFile => {
                    if let Some(p) = &self.guide_path {
                        ui.label(RichText::new(format!("{}  {}", glyphs::FOLDER, p.display())).size(theme.font(TypeToken::Body)).color(t.fg));
                    }
                    let r = button(
                        ui,
                        theme,
                        Some(glyphs::FOLDER),
                        "Choose a guide file…",
                        ButtonStyle { focused, bordered: true, ..Default::default() },
                    )
                    .clicked();
                    ui.add_space(theme.space(SpaceToken::Lg));
                    r
                }
                Field::Continue => {
                    button(ui, theme, Some(glyphs::CHEVRON_RIGHT), "Continue", ButtonStyle { primary: true, focused, ..Default::default() }).clicked()
                }
                Field::Skip => {
                    ui.add_space(theme.space(SpaceToken::Xs));
                    button(ui, theme, None, "Skip", ButtonStyle { focused, ..Default::default() }).clicked()
                }
                _ => false,
            };
            if hit {
                clicked = Some(i);
            }
        }
        clicked
    }

    fn importing_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let status = ctx.services.jobs.active().into_iter().find(|j| Some(j.id) == self.import.job);
        let running = self.import.job.is_some();
        let xtream = self.form.kind() == SourceKind::Xtream || self.probe_result.as_ref().is_some_and(|r| r.kind == SourceKind::Xtream);
        let epg_running = ctx.services.jobs.active().iter().any(|j| matches!(j.kind, JobKind::RefreshEpg { .. }));
        // (label, done, active)
        let mut rows_: Vec<(&str, bool, bool)> = vec![("Channels", self.import.channels_done, running && !self.import.channels_done)];
        if xtream {
            rows_.push(("Movies & series", self.import.finished, running && self.import.channels_done));
        }
        let has_guide = xtream || self.import.guide_requested || self.probe_result.as_ref().is_some_and(|r| !r.epg_urls.is_empty());
        if has_guide {
            rows_.push(("TV guide", self.import.finished && !epg_running, epg_running));
        }
        for (label, done, active) in rows_ {
            ui.horizontal(|ui| {
                let (g, c) = if done {
                    (glyphs::CHECK, t.success)
                } else if active {
                    (glyphs::BUSY, t.accent)
                } else {
                    (glyphs::HISTORY, t.text_disabled)
                };
                ui.label(RichText::new(g).size(theme.font(TypeToken::Icon)).color(c));
                ui.label(RichText::new(label).size(theme.font(TypeToken::Body)).color(if done || active { t.fg } else { t.text_secondary }));
                if active && let Some(s) = &status {
                    ui.label(RichText::new(phase_text(&s.phase)).size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                }
                if label == "TV guide" && active {
                    ui.label(RichText::new("continues in the background").size(theme.font(TypeToken::BodySmall)).color(t.text_secondary));
                }
            });
            if active {
                match status.as_ref().and_then(|s| s.fraction) {
                    Some(f) if label != "TV guide" => progress_bar(ui, theme, f, (3.0 * theme.rem()).round()),
                    _ => busy_bar(ui, theme, (3.0 * theme.rem()).round()),
                };
            }
            ui.add_space(theme.space(SpaceToken::Md));
        }
        ui.add_space(theme.space(SpaceToken::Lg));
        if let Some(e) = &self.import.error {
            ui.label(RichText::new(format!("Couldn't import: {e}")).size(theme.font(TypeToken::Body)).color(t.urgent));
            ui.add_space(theme.space(SpaceToken::Md));
            return button_row(ui, theme, &["Retry", "Back to details"], self.focus);
        }
        let mut clicked = None;
        ui.add_enabled_ui(self.import.channels_done, |ui| {
            if button(ui, theme, Some(glyphs::LIVE_TV), "Open Live TV", ButtonStyle { primary: true, focused: self.focus == 0, ..Default::default() })
                .clicked()
            {
                clicked = Some(0);
            }
        });
        clicked
    }

    fn done_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if let Some(src) = self.import.source {
            let channels = ctx.lib.live.channels.iter().filter(|c| c.source_id == src).count();
            let movies = ctx.lib.vod.movies.iter().filter(|c| c.source_id == src).count();
            let series = ctx.lib.vod.series.iter().filter(|c| c.source_id == src).count();
            let mut parts = vec![format!("{channels} channels")];
            if movies > 0 {
                parts.push(format!("{movies} movies"));
            }
            if series > 0 {
                parts.push(format!("{series} series"));
            }
            ui.label(RichText::new(format!("{}  {}", glyphs::CHECK, parts.join(" · "))).size(theme.font(TypeToken::Title)).color(t.fg));
            ui.add_space(theme.space(SpaceToken::Md));
        }
        self.guide_summary(ui, ctx);
        ui.add_space(theme.space(SpaceToken::Lg));
        let mut labels = vec!["Open Live TV", "Add another playlist", "Go to Home"];
        if self.guide_action {
            labels.push("TV guide settings");
        }
        button_row(ui, theme, &labels, self.focus)
    }

    /// The guide part of the Done summary: per guide, how many of the playlist channels it
    /// matches, its channels and programmes — so a guide that imported fine but lists almost
    /// none of the channels (the iptv-org playlist's own guide: 2 channels) says so, with the
    /// way forward. Read off the UI thread; re-read when the guide changes.
    fn guide_summary(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if let Some(r) = kit::poll(&mut self.guides_loader) {
            match r {
                Ok(g) => self.guides = Some(g),
                Err(e) => {
                    // an empty summary until the next data change (not a read per frame)
                    tracing::debug!("guide summary: {e}");
                    self.guides = Some(Default::default());
                }
            }
        }
        if self.guides.is_none() && self.guides_loader.is_none() {
            self.guides_loader = Some(Loader::start(ctx, super::settings::load_guides));
        }
        let line = |ui: &mut egui::Ui, glyph: &str, text: String, color: egui::Color32| {
            ui.add(egui::Label::new(RichText::new(format!("{glyph}  {text}")).size(theme.font(TypeToken::Body)).color(color)).wrap());
        };
        let total = ctx.lib.live.channels.len();
        let covered = ctx.lib.live.channels.iter().filter(|c| c.epg_channel_id.is_some()).count();
        let importing = ctx.services.jobs.active().iter().any(|j| matches!(j.kind, JobKind::RefreshEpg { .. }));
        let guides: Vec<&crate::model::EpgSource> = self.guides.as_ref().map(|(g, _)| g.iter().filter(|g| g.enabled).collect()).unwrap_or_default();
        if importing {
            line(ui, glyphs::BUSY, "TV guide: still loading in the background".into(), t.text_secondary);
        } else if self.guides.is_some() && guides.is_empty() {
            line(ui, glyphs::WARNING, "No TV guide — add an XMLTV guide in Settings ▸ EPG".into(), t.warning);
        }
        if let Some((_, matches)) = &self.guides {
            for g in guides.iter().filter(|g| g.last_refresh.is_some()) {
                let text = super::settings::coverage_text(g, matches.get(&g.id).copied().unwrap_or(0), total);
                line(ui, glyphs::GUIDE, format!("{}: {}", g.name, text.replacen("Matches", "matches", 1)), t.fg);
            }
            for g in guides.iter().filter(|g| g.last_refresh.is_none() && !importing) {
                let why = g.last_error.as_deref().map(crate::redact::redact).unwrap_or_else(|| "not loaded yet".into());
                line(ui, glyphs::WARNING, format!("{}: {why}", g.name), t.warning);
            }
        }
        let poor = self.guides.is_some() && !importing && total > 0 && covered * 2 < total;
        // only once a guide has loaded: with none, there is nothing to map to
        if poor && guides.iter().any(|g| g.last_refresh.is_some()) {
            line(
                ui,
                glyphs::INFO,
                format!(
                    "Only {} of {} channels have a guide. Add a guide that lists them, or map them with Ctrl+E in Live TV.",
                    crate::ui::widgets::rows::grouped(covered),
                    crate::ui::widgets::rows::grouped(total)
                ),
                t.text_secondary,
            );
        }
        self.guide_action = poor;
        if !self.guide_action {
            self.focus = self.focus.min(2);
        }
    }

    fn restoring_ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) -> Option<usize> {
        let theme = ctx.theme;
        let t = &theme.tokens;
        match &self.restore_report {
            None => {
                busy_bar(ui, theme, (3.0 * theme.rem()).round());
                None
            }
            Some(Ok(r)) => {
                ui.label(
                    RichText::new(format!("{}  {}", glyphs::CHECK, restored_text(r.sources_added, r.sources_matched)))
                        .size(theme.font(TypeToken::Body))
                        .color(t.fg),
                );
                if !r.needs_credentials.is_empty() {
                    ui.label(
                        RichText::new(format!("Needs credentials: {} — enter them in Settings ▸ Playlists.", r.needs_credentials.join(", ")))
                            .size(theme.font(TypeToken::BodySmall))
                            .color(t.warning),
                    );
                }
                ui.add_space(theme.space(SpaceToken::Lg));
                button_row(ui, theme, &["Open Live TV"], 0)
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("Couldn't restore: {}", crate::redact::redact(e))).size(theme.font(TypeToken::Body)).color(t.urgent));
                ui.add_space(theme.space(SpaceToken::Lg));
                let r = button_row(ui, theme, &["Back"], 0);
                if r.is_some() {
                    self.restore_report = None;
                    self.go(Step::Welcome);
                    return None;
                }
                r
            }
        }
    }
}

/// "Restored 2 playlists (1 matched an existing one)".
fn restored_text(added: u32, matched: u32) -> String {
    let plural = |n: u32, one: &str, many: &str| if n == 1 { format!("{n} {one}") } else { format!("{n} {many}") };
    let mut s = format!("Restored {}", plural(added, "playlist", "playlists"));
    if matched > 0 {
        s.push_str(&format!(" ({} matched {})", matched, if matched == 1 { "an existing one" } else { "existing ones" }));
    }
    s
}

/// A row of buttons (focus ring on `focus`); returns the clicked index.
fn button_row(ui: &mut egui::Ui, theme: &Theme, labels: &[&str], focus: usize) -> Option<usize> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = theme.space(SpaceToken::Lg);
        for (i, l) in labels.iter().enumerate() {
            let style = ButtonStyle { primary: i == 0, bordered: true, focused: i == focus, ..Default::default() };
            if button(ui, theme, None, l, style).clicked() {
                clicked = Some(i);
            }
        }
    });
    clicked
}

/// "Downloading 12.4 MB", "Parsing 4 000 entries", "Writing 45 %".
pub(crate) fn phase_text(p: &JobPhase) -> String {
    match p {
        JobPhase::Queued => "Waiting…".into(),
        JobPhase::Connecting => "Connecting…".into(),
        JobPhase::Downloading { done, total } => match total {
            Some(t) if *t > 0 => format!("Downloading {:.1} of {:.1} MB", *done as f64 / 1e6, *t as f64 / 1e6),
            _ => format!("Downloading {:.1} MB", *done as f64 / 1e6),
        },
        JobPhase::Parsing { items } => format!("Reading {items} entries"),
        JobPhase::Writing { done, total } => match total {
            Some(t) if *t > 0 => format!("Saving {done} of {t}"),
            _ => format!("Saving {done}"),
        },
        JobPhase::Matching => "Matching the guide…".into(),
        JobPhase::Indexing => "Indexing…".into(),
        JobPhase::Finishing => "Finishing…".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(kind: SourceKind) -> Form {
        Form { kind: Some(kind), ..Default::default() }
    }

    #[test]
    fn xtream_server_is_normalized_and_required_fields_point_at_themselves() {
        let mut f = form(SourceKind::Xtream);
        assert_eq!(f.draft().unwrap_err().0, Field::Server);
        f.server = "panel.example.com:8080/get.php?x=1".into();
        assert_eq!(f.draft().unwrap_err().0, Field::Username);
        f.username = "me".into();
        assert_eq!(f.draft().unwrap_err().0, Field::Password);
        f.password = "secret".into();
        let d = f.draft().unwrap();
        assert_eq!(d.xtream.as_ref().unwrap().server, "http://panel.example.com:8080");
        assert_eq!(d.name, "panel.example.com", "default name from the host");
        assert_eq!(f.fields(), vec![Field::Name, Field::Icon, Field::Color, Field::Server, Field::Username, Field::Password, Field::Continue]);
    }

    #[test]
    fn get_php_urls_offer_the_xtream_login() {
        let mut f = form(SourceKind::M3uUrl);
        assert_eq!(f.draft().unwrap_err().0, Field::Url);
        f.url = "http://panel.example:8080/get.php?username=u1&password=p1&type=m3u_plus".into();
        assert!(f.fields().contains(&Field::UseXtream));
        assert!(f.use_xtream());
        assert_eq!(f.kind, Some(SourceKind::Xtream));
        assert_eq!((f.username.as_str(), f.password.as_str()), ("u1", "p1"));
        let mut plain = form(SourceKind::M3uUrl);
        plain.url = "example.com/list.m3u".into();
        assert!(!plain.fields().contains(&Field::UseXtream));
        assert_eq!(plain.draft().unwrap().url.as_deref(), Some("http://example.com/list.m3u"));
        let mut file = form(SourceKind::M3uFile);
        file.path = "/tmp/My List.m3u".into();
        assert_eq!(file.draft().unwrap().name, "My List");
    }

    #[test]
    fn probe_failures_route_to_fields_or_retry() {
        assert_eq!(classify("login rejected by provider", SourceKind::Xtream), Failure::Field(Field::Username));
        assert_eq!(classify("account expired", SourceKind::Xtream), Failure::Field(Field::Username));
        assert_eq!(classify("HTTP 401 from http://x/", SourceKind::M3uUrl), Failure::Field(Field::Url));
        assert_eq!(classify("unexpected response: This looks like a web page, not a playlist", SourceKind::M3uUrl), Failure::Field(Field::Url));
        assert_eq!(classify("network error: connection refused", SourceKind::Xtream), Failure::Unreachable);
        assert_eq!(classify("HTTP 503 from http://x/", SourceKind::M3uUrl), Failure::Unreachable);
        assert_eq!(classify("HTTP 404 from http://x/", SourceKind::Xtream), Failure::Field(Field::Server));
        assert_eq!(classify("I/O error on /x.m3u: not found", SourceKind::M3uFile), Failure::Field(Field::Path));
        assert_eq!(failure_text("account banned"), "This account is banned. Check it with your provider.");
        assert_eq!(failure_text("unexpected response: this looks like a web page"), "This looks like a web page");
    }

    #[test]
    fn estimates_say_what_was_counted() {
        let mut r = ProbeResult {
            kind: SourceKind::M3uUrl,
            xtream: None,
            account: None,
            estimate_channels: Some(1234),
            suggested_name: None,
            epg_urls: vec![],
        };
        assert_eq!(estimate_text(&r).as_deref(), Some("1234 channels in the playlist"));
        r.kind = SourceKind::Xtream;
        r.estimate_channels = Some(12);
        assert_eq!(estimate_text(&r).as_deref(), Some("12 live TV categories"));
        r.estimate_channels = None;
        assert_eq!(estimate_text(&r), None);
    }

    #[test]
    fn account_lines_flag_unusable_accounts() {
        let a = AccountInfo {
            status: AccountStatus::Expired,
            exp_date: Some(1_790_000_000),
            max_connections: Some(2),
            active_cons: Some(1),
            allowed_formats: vec!["ts".into(), "m3u8".into()],
            ..Default::default()
        };
        let l = account_lines(&a);
        assert!(l[0].0.starts_with("Expired on ") && l[0].1);
        assert!(l.iter().any(|(s, _)| s == "1/2 connections in use"));
        assert!(l.iter().any(|(s, _)| s == "Formats: ts, m3u8"));
        let ok = AccountInfo { status: AccountStatus::Active, exp_date: Some(crate::model::now() + 86_400 * 30), ..Default::default() };
        assert!(!account_lines(&ok)[0].1);
    }

    #[test]
    fn back_walks_the_steps_from_the_start_point() {
        let mut s = OnboardingScreen::default();
        s.go(Step::Welcome);
        assert!(!s.back(), "nothing before Welcome");
        s.go(Step::Type);
        assert!(s.back());
        assert_eq!(s.step, Step::Welcome);
        s.start = OnboardingStart::AddSource;
        s.go(Step::Type);
        assert!(!s.back(), "Settings ▸ Add starts at Type: Back leaves");
        s.choose_kind(SourceKind::M3uUrl);
        assert_eq!(s.step, Step::Details);
        assert_eq!(s.current_field(), Some(Field::Url));
        assert!(s.editing, "the first field takes the keyboard");
        assert_eq!(s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
        s.editing = false;
        assert_eq!(s.key_contexts(), vec![KeyContext::Modal]);
        assert!(s.back());
        assert_eq!(s.step, Step::Type);
    }

    /// Mouse users get the Esc rung as a button: Back between steps, Cancel on the first step
    /// of a card opened from Settings / a file / a restored playlist, none on first-run Welcome
    /// or while importing. Restore copy is pluralised.
    #[test]
    fn back_button_labels_and_restore_copy() {
        let mut s = OnboardingScreen::default();
        s.go(Step::Welcome);
        assert_eq!(s.back_label(), None);
        s.go(Step::Type);
        assert_eq!(s.back_label(), Some("Back"));
        s.go(Step::Importing);
        assert_eq!(s.back_label(), None);
        s.start = OnboardingStart::AddSource;
        s.go(Step::Type);
        assert_eq!(s.back_label(), Some("Cancel"), "Settings ▸ Add: leaves the card");
        assert!(!s.back(), "…through the app's Back");
        s.go(Step::Details);
        assert_eq!(s.back_label(), Some("Back"));
        assert_eq!(restored_text(1, 0), "Restored 1 playlist");
        assert_eq!(restored_text(2, 1), "Restored 2 playlists (1 matched an existing one)");
        assert_eq!(restored_text(3, 2), "Restored 3 playlists (2 matched existing ones)");
    }

    /// DESIGN §13.10: "Open Live TV" works as soon as the new source's channels are in the
    /// library (the import rebuilds it after the channels phase), while movies, series and the
    /// guide are still importing.
    #[test]
    fn open_live_tv_enables_after_the_channels_phase() {
        use super::super::home::testkit::{self, harness, has_label, rig};
        let (mut t, one) = rig();
        // a source that already had channels doesn't count
        testkit::seed(&mut t, |c| {
            let s = testkit::source(c, "Old")?;
            testkit::channels(c, s, 2)
        });
        let mut h = harness(t, one, OnboardingScreen::default(), egui::vec2(1000.0, 800.0));
        let app = h.state_mut();
        app.s.start = OnboardingStart::AddSource;
        app.s.form.kind = Some(SourceKind::Xtream);
        app.s.go(Step::Importing);
        app.s.import =
            Import { job: Some(JobId(u64::MAX)), had_channels: app.t.lib.live.channels.iter().map(|c| c.source_id).collect(), ..Default::default() };
        h.run_steps(2);
        assert!(has_label(&h, "Open Live TV") && has_label(&h, "Channels"));
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert!(h.state_mut().nav().is_empty(), "disabled while the channels import");
        // the channels phase finished: the rebuilt library has the new source's channels
        testkit::seed(&mut h.state_mut().t, |c| {
            let s = testkit::source(c, "New")?;
            testkit::channels(c, s, 3)
        });
        h.run_steps(2);
        assert!(h.state().s.import.channels_done);
        assert!(has_label(&h, "Movies & series"), "an Xtream import lists the VOD phase");
        h.state_mut().act(Action::Activate);
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Switch(Route::Live { group: None, channel: None })]);
        assert_eq!(h.state().s.step, Step::Importing, "the import continues in the background");
    }

    #[test]
    fn phases_read_well() {
        assert_eq!(phase_text(&JobPhase::Downloading { done: 2_500_000, total: Some(10_000_000) }), "Downloading 2.5 of 10.0 MB");
        assert_eq!(phase_text(&JobPhase::Writing { done: 10, total: Some(20) }), "Saving 10 of 20");
    }
}
