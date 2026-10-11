//! Onboarding wizard end to end (DESIGN §13.10, WP-12): the real `OnboardingScreen` in an
//! `egui_kittest` harness over `TestUi` (real services in a temp profile, headless mpv), talking
//! to the in-process mock Xtream panel (`support::mock_xtream`) and to local M3U files. Owner:
//! WP-12.
//!
//! The harness plays the app's part: it delivers bus events to the screen (`on_event`), swaps
//! in libraries built by `RebuildLibrary` jobs, and records navigation. Typing goes through
//! egui text events into the focused field, exactly like the window does.

mod support;

use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui;
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use support::mock_xtream::{self, MockAuth, MockOptions, MockXtream};
use tuner::jobs::{JobEvent, JobOutcome};
use tuner::keymap::{Action, KeyContext};
use tuner::model::SourceKind;
use tuner::msg::AppEvent;
use tuner::ui::screens::onboarding::OnboardingScreen;
use tuner::ui::testing::TestUi;
use tuner::ui::{NavRequest, OnboardingStart, Route, Screen};

/// One rig (services + mpv) at a time: the binary's memory stays flat.
static ONE: Mutex<()> = Mutex::new(());

struct App {
    t: TestUi,
    s: OnboardingScreen,
    _one: MutexGuard<'static, ()>,
}

impl App {
    fn act(&mut self, a: Action) -> bool {
        let s = &mut self.s;
        self.t.with_ctx(|ctx| s.on_action(ctx, a))
    }

    /// Bus events → the screen, like the app (library swaps first).
    fn pump(&mut self) {
        while let Ok(ev) = self.t.events.try_recv() {
            if let AppEvent::Job(JobEvent::Finished { outcome: Ok(JobOutcome::Library(lib)), .. }) = &ev {
                self.t.lib = lib.clone();
                self.t.favs = lib.favs.clone();
            }
            let s = &mut self.s;
            self.t.with_ctx(|ctx| s.on_event(ctx, &ev));
        }
    }

    fn sources(&self) -> Vec<tuner::model::Source> {
        tuner::store::sources::list_sources(&self.t.services().store.reader().unwrap()).unwrap()
    }
}

fn harness(start: OnboardingStart) -> Harness<'static, App> {
    harness_with(|_| start)
}

/// A rig whose profile `prepare` may seed first; it returns where the card starts.
fn harness_with(prepare: impl FnOnce(&TestUi) -> OnboardingStart) -> Harness<'static, App> {
    let one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let t = TestUi::new().expect("TestUi");
    let start = prepare(&t);
    let mut h = Harness::builder().with_size(egui::vec2(1000.0, 900.0)).with_step_dt(0.02).build_ui_state(
        |ui, app: &mut App| {
            app.pump();
            let s = &mut app.s;
            app.t.with_ctx(|ctx| s.ui(ui, ctx));
        },
        App { t, s: OnboardingScreen::default(), _one: one },
    );
    let app = h.state_mut();
    let s = &mut app.s;
    app.t.with_ctx(|ctx| s.on_enter(ctx, &Route::Onboarding(start)));
    h.run_steps(2);
    h
}

fn has(h: &Harness<'static, App>, label: &str) -> bool {
    h.query_all_by_label(label).next().is_some()
}
fn has_containing(h: &Harness<'static, App>, label: &str) -> bool {
    h.query_all_by_label_contains(label).next().is_some()
}

/// Step frames until `done` (20 s limit: imports run real jobs).
fn until(h: &mut Harness<'static, App>, what: &str, mut done: impl FnMut(&mut Harness<'static, App>) -> bool) {
    let start = Instant::now();
    while !done(h) {
        assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
        h.step();
    }
}

/// Type into the focused field (it takes focus the frame after it became current).
fn type_text(h: &mut Harness<'static, App>, text: &str) {
    h.run_steps(2);
    h.event(egui::Event::Text(text.into()));
    h.run_steps(1);
}

/// An action, then two frames (Enter is acted on at the end of the next frame; the frame
/// after shows the result).
fn press(h: &mut Harness<'static, App>, a: Action) {
    h.state_mut().act(a);
    h.run_steps(2);
}

/// A mock panel on its own runtime (kept alive by the caller).
fn panel(opts: MockOptions) -> (tokio::runtime::Runtime, MockXtream) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let mock = rt.block_on(mock_xtream::start(opts));
    (rt, mock)
}

fn host_port(base: &str) -> &str {
    base.trim_start_matches("http://")
}

/// Fill the Xtream form (the cursor starts on Server) and test it.
fn xtream_login(h: &mut Harness<'static, App>, server: &str, user: &str, password: &str) {
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal], "the server field owns the keyboard");
    type_text(h, server);
    press(h, Action::FocusNext);
    type_text(h, user);
    press(h, Action::FocusNext);
    type_text(h, password);
    // Enter in the last field tests the login
    press(h, Action::Activate);
}

#[test]
fn xtream_login_probes_imports_once_and_opens_live_tv() {
    let (_rt, mock) = panel(MockOptions::default());
    let mut h = harness(OnboardingStart::AddSource);
    assert!(has(&h, "Add a playlist") && has(&h, "Xtream login") && has(&h, "M3U URL") && has(&h, "M3U file"));
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Modal]);
    press(&mut h, Action::Activate); // Xtream login
    assert!(has_containing(&h, "http:// is added when missing"), "the server hint explains the normalization");
    // a path and no scheme: normalized to scheme://host:port
    xtream_login(&mut h, &format!("{}/panel/player_api.php?x=1", host_port(&mock.base)), "demo", "s3cret-onboarding-1");
    until(&mut h, "the probe", |h| has_containing(h, "Looks good"));
    // the estimate says what the Xtream probe counted: live categories, not channels
    let cats: Vec<serde_json::Value> = serde_json::from_slice(&std::fs::read(support::fixture("xtream/live_categories.json")).unwrap()).unwrap();
    assert!(has(&h, &format!("{} live TV categories", cats.len())), "estimate copy");
    assert!(has_containing(&h, "Active"), "account status");
    assert!(has(&h, "Up to 2 connections") || has_containing(&h, "connections in use"), "connections");
    // Continue, pressed repeatedly: one import only (no duplicate source)
    h.state_mut().act(Action::Activate);
    h.state_mut().act(Action::Activate);
    h.run_steps(1);
    h.state_mut().act(Action::Activate);
    until(&mut h, "the import to finish", |h| has(h, "All set"));
    let sources = h.state().sources();
    assert_eq!(sources.len(), 1, "one source, whatever was pressed");
    let x = sources[0].xtream.as_ref().unwrap();
    assert_eq!(x.server, mock.base, "server normalized");
    assert_eq!((x.username.as_str(), x.password.expose().as_str()), ("demo", "s3cret-onboarding-1"));
    assert!(!h.state().t.lib.live.channels.is_empty(), "channels are in the library");
    assert!(h.query_all_by_label_contains("channels").next().is_some(), "Done shows the counts");
    // Open Live TV
    press(&mut h, Action::Activate);
    assert_eq!(std::mem::take(&mut h.state_mut().t.nav), vec![NavRequest::Switch(Route::Live { group: None, channel: None })]);
}

#[test]
fn rejected_login_goes_back_to_the_offending_field() {
    let (_rt, mock) = panel(MockOptions { auth: MockAuth::Rejected, ..Default::default() });
    let mut h = harness(OnboardingStart::AddSource);
    press(&mut h, Action::Activate);
    xtream_login(&mut h, &mock.base, "demo", "wrong-password-onb-2");
    until(&mut h, "the rejection", |h| has(h, "The provider rejected this username or password."));
    // back on Details with the username field focused (it owns the keyboard)
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
    assert!(has(&h, "Xtream login"));
    assert!(h.state().sources().is_empty(), "nothing saved");
    // Esc leaves the field, Esc again goes back a step
    press(&mut h, Action::Back);
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Modal]);
    press(&mut h, Action::Back);
    assert!(has(&h, "How does your provider give you access?"));
}

#[test]
fn unreachable_server_offers_retry_and_add_anyway() {
    let mut h = harness(OnboardingStart::AddSource);
    press(&mut h, Action::Activate);
    // port 9 (discard) refuses connections
    xtream_login(&mut h, "127.0.0.1:9", "demo", "pw-onb-3");
    until(&mut h, "the network failure", |h| has_containing(h, "Couldn't reach the server"));
    assert!(has(&h, "Retry") && has(&h, "Add anyway") && has(&h, "Back"));
    // Retry probes again (and fails again); Back returns to the form
    press(&mut h, Action::Activate);
    until(&mut h, "the second failure", |h| has_containing(h, "Couldn't reach the server"));
    press(&mut h, Action::Right);
    press(&mut h, Action::Right);
    press(&mut h, Action::Activate);
    assert!(has(&h, "Xtream login"));
}

#[test]
fn get_php_urls_sign_in_with_their_xtream_login_and_keep_the_playlist() {
    let (_rt, mock) = panel(MockOptions::default());
    let mut h = harness(OnboardingStart::Welcome);
    assert!(has(&h, "Welcome to Tuner") && has(&h, "Restore a backup"));
    press(&mut h, Action::Activate); // Add a playlist
    press(&mut h, Action::Down);
    press(&mut h, Action::Activate); // M3U URL
    assert!(has(&h, "M3U playlist URL"));
    let url = format!("{}/get.php?username=demo&password=pw-onb-4&type=m3u_plus&output=ts", mock.base);
    type_text(&mut h, &url);
    // Enter in the URL field tests it: the probe finds the Xtream login (DESIGN §27.3)
    press(&mut h, Action::Activate);
    until(&mut h, "the probe", |h| has_containing(h, "Xtream login found"));
    assert!(has(&h, "Keep as M3U"));
    press(&mut h, Action::Activate); // Continue (focused)
    until(&mut h, "the import to finish", |h| has(h, "All set"));
    let sources = h.state().sources();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].kind, SourceKind::Xtream);
    assert_eq!(sources[0].url.as_ref().map(|u| u.expose().as_str()), Some(url.as_str()), "the playlist is kept for switching back");
    assert_eq!(sources[0].m3u_origin(), Some(SourceKind::M3uUrl));
}

#[test]
fn restored_source_without_password_gets_its_credentials_and_no_duplicate() {
    let (_rt, mock) = panel(MockOptions::default());
    let base = mock.base.clone();
    let mut id = None;
    let mut h = harness_with(|t| {
        let s = t
            .services()
            .store
            .writer()
            .blocking(move |c| {
                // a source restored from a backup without passwords (never refreshed)
                c.execute(
                    "INSERT INTO source(name, kind, icon, xt_server, xt_username, xt_password, created_at, generation) VALUES ('Restored IPTV', 'xtream', '', ?1, 'demo', '', 0, 0)",
                    [&base],
                )?;
                Ok(tuner::model::SourceId(c.last_insert_rowid()))
            })
            .unwrap();
        id = Some(s);
        OnboardingStart::NeedsCredentials(s)
    });
    let id = id.unwrap();
    until(&mut h, "the restored source in the form", |h| h.state().s.key_contexts() == vec![KeyContext::TextInput, KeyContext::Modal]);
    assert!(has(&h, "Xtream login"));
    // the password field has the keyboard: type it and press Enter (the last field tests)
    type_text(&mut h, "restored-pw-onb-7");
    press(&mut h, Action::Activate);
    until(&mut h, "the probe", |h| has_containing(h, "Looks good"));
    press(&mut h, Action::Activate);
    until(&mut h, "the import", |h| has(h, "All set"));
    let sources = h.state().sources();
    assert_eq!(sources.len(), 1, "the restored row was reused");
    assert_eq!(sources[0].id, id);
    assert_eq!(sources[0].xtream.as_ref().unwrap().password.expose(), "restored-pw-onb-7");
    assert!(sources[0].status.last_refresh.is_some());
}

#[test]
fn m3u_file_without_a_guide_asks_for_one_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("Holiday List.m3u");
    std::fs::write(
        &path,
        "#EXTM3U\n#EXTINF:-1 tvg-id=\"one.tv\" group-title=\"News\",One News\nhttp://127.0.0.1:9/one.ts\n#EXTINF:-1 group-title=\"Kids\",Two Kids\nhttp://127.0.0.1:9/two.ts\n",
    )
    .unwrap();
    let mut h = harness(OnboardingStart::PlaylistFile(path.clone()));
    // Details with the name prefilled, the cursor on Continue
    assert!(has(&h, "M3U playlist file"));
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Modal]);
    press(&mut h, Action::Activate);
    until(&mut h, "the probe", |h| has(h, "2 channels in the playlist"));
    press(&mut h, Action::Activate); // Continue → the Guide step (no url-tvg)
    assert!(has(&h, "TV guide") && has(&h, "Skip"));
    // the URL field owns the keyboard: Tab to Skip
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::TextInput, KeyContext::Modal]);
    for _ in 0..3 {
        press(&mut h, Action::FocusNext);
    }
    press(&mut h, Action::Activate); // Skip
    until(&mut h, "the import", |h| has(h, "All set"));
    assert!(has(&h, "2 channels") || has_containing(&h, "2 channels"));
    let sources = h.state().sources();
    assert_eq!(sources.len(), 1);
    assert_eq!((sources[0].name.as_str(), sources[0].kind), ("Holiday List", SourceKind::M3uFile));
    let epg = tuner::store::sources::list_epg_sources(&h.state().t.services().store.reader().unwrap()).unwrap();
    assert!(epg.is_empty(), "skipped: no guide added");
    // the summary says there is no guide and offers the way to add one, from the keyboard
    until(&mut h, "the guide line", |h| has_containing(h, "No TV guide — add an XMLTV guide"));
    assert!(has(&h, "TV guide settings"));
    for _ in 0..3 {
        press(&mut h, Action::Down);
    }
    press(&mut h, Action::Activate);
    let nav = std::mem::take(&mut h.state_mut().t.nav);
    assert!(nav.contains(&NavRequest::Switch(Route::Settings(tuner::ui::SettingsPage::Epg))), "{nav:?}");
}

/// DESIGN §28.6 end to end (tag detection, raw names and the summary are the core's): a
/// playlist whose categories name several countries asks what to show once its channels are
/// in; Enter stores the defaults (the user's country, every English-language tag, untagged)
/// with the tags it had as `known`; then the card finishes as usual.
#[test]
fn m3u_file_with_several_countries_asks_what_to_show() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("World.m3u");
    std::fs::write(
        &path,
        "#EXTM3U\n\
         #EXTINF:-1 group-title=\"UK | News\",UK News\nhttp://127.0.0.1:9/1.ts\n\
         #EXTINF:-1 group-title=\"FR | Sport\",FR Sport\nhttp://127.0.0.1:9/2.ts\n\
         #EXTINF:-1 group-title=\"AR | Kids\",AR Kids\nhttp://127.0.0.1:9/3.ts\n\
         #EXTINF:-1 group-title=\"Sports\",Sports One\nhttp://127.0.0.1:9/4.ts\n",
    )
    .unwrap();
    let mut h = harness(OnboardingStart::PlaylistFile(path));
    press(&mut h, Action::Activate);
    until(&mut h, "the probe", |h| has(h, "4 channels in the playlist"));
    press(&mut h, Action::Activate); // Continue → the Guide step: Tab to Skip
    for _ in 0..3 {
        press(&mut h, Action::FocusNext);
    }
    press(&mut h, Action::Activate);
    until(&mut h, "the picker", |h| has(h, "Choose what to show"));
    assert_eq!(h.state().s.key_contexts(), vec![KeyContext::Checklist, KeyContext::Modal]);
    assert!(has_containing(&h, "categories in 3 countries and languages"), "UK, France, Arabic");
    // Enter is ignored for a moment after the step appears on its own
    std::thread::sleep(std::time::Duration::from_millis(450));
    press(&mut h, Action::Activate);
    until(&mut h, "the import", |h| has(h, "All set"));
    let rule = h.state().sources()[0].settings.category_filter.clone().expect("a rule");
    let tag = |s: &str| tuner::model::CategoryTag(s.into());
    assert!(rule.keep.contains(&tag("GB")) && rule.keep.contains(&tag("lang:en")) && rule.keep_untagged);
    assert_eq!(rule.known, [tag("GB"), tag("FR"), tag("lang:ar")].into_iter().collect());
}

/// A one-channel local file imports too fast to observe the disabled "Open Live TV" here;
/// that it waits for the channels phase is covered by the unit test
/// `onboarding::tests::open_live_tv_enables_after_the_channels_phase`.
#[test]
fn m3u_file_with_url_tvg_skips_the_guide_step_and_imports_its_guide() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("withguide.m3u");
    std::fs::write(&path, "#EXTM3U url-tvg=\"http://127.0.0.1:9/guide.xml\"\n#EXTINF:-1 tvg-id=\"a.tv\",Alpha\nhttp://127.0.0.1:9/a.ts\n").unwrap();
    let mut h = harness(OnboardingStart::PlaylistFile(path));
    press(&mut h, Action::Activate);
    until(&mut h, "the probe", |h| has(h, "1 channel in the playlist"));
    assert!(has(&h, "Includes a TV guide"));
    press(&mut h, Action::Activate); // Continue → straight to importing
    assert!(!has(&h, "Skip"), "no Guide step for playlists with url-tvg");
    until(&mut h, "the import", |h| has(h, "All set"));
    assert_eq!(h.state().sources().len(), 1);
    // the importer created the provider guide from url-tvg
    let epg = tuner::store::sources::list_epg_sources(&h.state().t.services().store.reader().unwrap()).unwrap();
    assert_eq!(epg.len(), 1);
    // "Add another playlist" restarts at Type
    press(&mut h, Action::Down);
    press(&mut h, Action::Activate);
    assert!(has(&h, "How does your provider give you access?"));
}
