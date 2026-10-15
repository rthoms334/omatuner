//! Fullscreen player + OSD (DESIGN §13.8): VideoHost Fullscreen, fixed dark scrim, OSD type
//! scale `layout::osd_scale`, accent = `tokens.osd_accent`.
//! * INFO BANNER (passive): shown on zap / any key / mouse move, hides after
//!   `osd_timeout_ms`; NEVER changes the key context (arrows keep zapping/seeking).
//! * CONTROLS FOCUS MODE: entered only with Enter / `i` / Tab (`FocusOsdControls`), never by
//!   a click (a click just activates the control); pushes `PlayerOsd`; Esc (`LeaveOsdControls`)
//!   or Back (Alt+←, the mouse back button) leaves it, banner stays.
//! * Esc ladder (no focus mode): hide banner → leave window fullscreen → leave the player
//!   (mini-player keeps playing). Esc never SHOWS the banner.
//! * Side panels (`c` channels, `g` mini guide, `h`/→ history): stack
//!   `[List|ListFiltering, PlayerLive]`; ↑/↓ move the cursor (no zap), Enter tunes, ←/Esc close
//!   (← also while the panel filters; Esc clears the filter first).
//!   Stream info `I`.
//! * Numeric entry (digits): stack `[NumericEntry, PlayerLive]`, 1.5 s auto-commit.
//! * End of playback: a finished episode → "Next episode in 10 s" card over the video. A
//!   finished movie (marked watched by the player) returns to its detail page; that part is
//!   driven by the always-alive movie detail screen, so it is done once.
//! * Mouse: double-click toggles window fullscreen; wheel = volume (only here); right-click =
//!   the player menu; a click on the video closes a side panel / leaves focus mode; the top
//!   bar's ← leaves the player (like `p`); the bottom bar ends with the volume (speaker = mute,
//!   slider 0–130 % with a mark at 100 %, wheel ±5); panels have a ✕ and tune on one click;
//!   a failed stream offers Retry / Last channel buttons; the banner stays while the pointer
//!   is over a bar and the pointer hides with the banner.
//! * Video-only (`ctx.video_only`, the popped-out window, DESIGN §12.3): the passive banner
//!   is a one-strip OSD (title · volume, now playing or position; with the pointer in the
//!   window or paused, transport / volume / pop-in / stop buttons instead of the volume and the
//!   key hint); the full OSD comes back only in controls focus mode or with a side panel;
//!   double-click pops the window back in, a drag moves the window; nothing playing shows a
//!   compact card with the way back.
//! * Controls row: transport controls (previous / play / next, skips, fullscreen) are glyphs,
//!   the rest glyph + label; when that doesn't fit every control is a glyph, and controls
//!   focus mode only walks the controls actually drawn.
//! * Numeric entry: any key that isn't part of it commits the number, then acts (like the
//!   guide).
//!
//! Owner: WP-11. Split without behaviour change (batch base-B) so the batch's UI tracks each
//! own a file:
//! * `mod.rs` (this): the screen, keys, layout — MV-2 (multi-view layout).
//! * `osd.rs`: the controls and bars. `panels.rs`: side panels — Q-ui adds the `u` panel.
//! * `banner.rs`: the info banner and flashes. `cards.rs`: end-of-playback and state cards —
//!   Q-ui (Up next card). `shield.rs`: the zap shield card — S-ui (S4).
//!   `multiview.rs`: the multi-view pictures, chrome, keys and mouse — MV-2.
//! * Multi-view (DESIGN §35, `ctx.multiview.on()`): both pictures in the layout (side by side,
//!   big + small, picture in picture — always in the popped-out window); `ctx.player` is the
//!   focused picture's, so the OSD, panels, cards and keys act on it, volume and mute on the
//!   picture with the sound (`UiCtx::play`). The app puts `MultiView` above the player's base
//!   context: `w` next picture, Ctrl+arrows, `M` sound here, `W` swap, `t` layout, `q` close
//!   this picture (Esc never closes one); the right-click menu starts with them.
//!
//! * Casting (DESIGN §40; `C`, the OSD's Cast control, the player menu): the picker
//!   (`overlays::cast_picker`) and the PIN overlay open from here; while `snap.cast` is set the
//!   video host draws the casting card with Stop casting / Play here (and a failure's own
//!   buttons, `cards.rs`), the controls row drops audio, subtitles and pop-out, the volume
//!   reads "TV remote", and the keys that do nothing on a TV say so in the banner.
//!
//! Everything over the video uses the fixed OSD colours (`osd_scrim`, `osd_text`,
//! `osd_accent`), never theme colours, so it is legible on any theme (FIXED DECISION 6). Keys
//! that manage the window or the app (F11, Esc, ?, /) don't show the banner. Zapping before
//! the zap list exists (a startup `--play` before the library loaded) does nothing. While the
//! parental session is locked, ↑/↓ skip locked channels (the UI picks the neighbour and tunes
//! it with `ZapTo`), and also what the resolver's zap skips (≥ 2 recent failures, DRM), since
//! `ZapTo` tunes exactly the channel it is given.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Rect};

use crate::keymap::{Action, KeyContext};
use crate::model::{ChannelId, Collection, EpisodeId, ItemRef, MediaRef, PlayOrigin, PlayableKind, Quality};
use crate::msg::{AppEvent, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerEvent, PlayerSnapshot, PlayerState};
use crate::store::library::LiveIndex;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::epg_grid::{hhmm, time_left, time_range};
use crate::ui::layout::osd_scale;
use crate::ui::overlays::{ContextMenuSpec, OverlayResult};
use crate::ui::screens::live::{self, GuideStatus, menu_item, playing_channel};
use crate::ui::video_host::{Placement, StateAction, osd_text};
use crate::ui::widgets::badge::{InlineBadge, name_with_quality};
use crate::ui::widgets::list::{ListOutcome, ListState, virtual_list};
use crate::ui::widgets::slider::{SliderLook, SliderSpec};
use crate::ui::widgets::{badge, glyphs, logo, slider};
use crate::ui::{NavRequest, OverlayRequest, Route, Screen, ScreenId, UiCtx};

mod banner;
mod cards;
mod multiview;
mod osd;
mod panels;
mod reveal;
mod shield;

use banner::*;
use cards::*;
use osd::*;
use panels::*;
use reveal::*;

const NUMERIC_COMMIT: Duration = Duration::from_millis(1500);

/// The catalogue kind of what plays (whose playlists the OSD's playlist mark counts).
fn playable_item_kind(k: &crate::model::PlayableKind) -> Option<crate::model::ItemKind> {
    use crate::model::{ItemKind, PlayableKind};
    match k {
        PlayableKind::Live | PlayableKind::Catchup { .. } => Some(ItemKind::Channel),
        PlayableKind::Movie => Some(ItemKind::Movie),
        PlayableKind::Episode { .. } => Some(ItemKind::Series),
        PlayableKind::Url => None,
    }
}
const FLASH: Duration = Duration::from_millis(1500);
/// Zoom steps cycled by `V` (mpv `video-zoom`, log2).
const ZOOMS: [f64; 4] = [0.0, 0.1, 0.25, 0.5];
/// The volume slider's range (mpv's `volume-max` default) and 100 % mark; wheel step.
const VOLUME_MAX: f32 = 130.0;
const VOLUME_WHEEL: f32 = 5.0;
/// How long the volume the user just set is shown while mpv's echo is on its way.
const VOLUME_ECHO: Duration = Duration::from_secs(1);
/// A pointer still for this long (s) on an OSD bar no longer holds the banner up.
const POINTER_REST: f32 = 10.0;

/// The player's "Play from" menu: copy `k` is `SWITCH_COPY + k` (above typed digits).
const SWITCH_COPY: u8 = 100;

#[derive(Default)]
pub struct PlayerScreen {
    banner_until: Option<Instant>,
    osd_focus: bool,
    control: usize,
    row: OsdRow,
    panel: Option<SidePanel>,
    numeric: Option<(String, Instant)>,
    info: bool,
    info_refreshed: Option<Instant>,
    flash: Option<(Flash, Instant)>,
    flash_text: Option<(String, Instant)>,
    next_episode: Option<NextEpisode>,
    zoom: usize,
    toggle_fullscreen: bool,
    /// Live content (`PlayerLive`) vs seekable (`PlayerVod`), from the snapshot.
    live_ctx: bool,
    wheel: f32,
    /// Controls drawn in the last OSD frame (focus mode never lands on an undrawn one).
    visible: Vec<Control>,
    /// Height of the bottom OSD bar drawn this frame (the next-episode card sits above it).
    bottom_bar: Option<f32>,
    /// Esc hid the banner that a pause keeps up (cleared by anything that shows it).
    banner_dismissed: bool,
    /// An episode ended: its successor is looked up within the frame budget.
    next_lookup: Option<EpisodeId>,
    guide: GuideStatus,
    /// The OSD bars drawn this frame (the banner stays while the pointer is over one).
    bars: Vec<Rect>,
    /// The volume the slider / wheel just set, shown until mpv reports it (its echo arrives a
    /// frame or more later; without this the knob jumps back while dragging).
    volume_local: Option<(f32, Instant)>,
    /// When a volume change last unmuted (`ToggleMute` is sent once, not on every drag step
    /// before mpv reports the mute gone).
    unmuting: Option<Instant>,
    /// The other copy the error card offers for the failed load generation (looked up once per
    /// failure, within the frame budget; DESIGN §29.15).
    other_copy: Option<(u64, Option<crate::ui::copies::OtherCopy>)>,
    /// The copies the Ctrl+O menu listed during playback (DESIGN §29.15).
    switch_copies: Vec<crate::ui::copies::OtherCopy>,
    /// Multi-view's layout of the last frame and its captions (`multiview.rs`).
    mv: multiview::MvScreen,
    /// The banner's programmes' categories (the spoiler guard's sport rule; `reveal.rs`).
    banner_cats: reveal::BannerCats,
}

/// The OSD scale of the current window.
fn scale_of(ui: &egui::Ui) -> f32 {
    osd_scale(ui.ctx().content_rect().width())
}

fn is_live(snap: &PlayerSnapshot) -> bool {
    crate::ui::video_host::is_live(snap.playable.as_ref().map(|p| &p.kind))
}

/// The neighbour to tune while the session is locked: skips locked channels and what the
/// controller's own zap would skip (`failing`: ≥ 2 failures in 24 h; DRM; vanished channels).
fn zap_target(live_idx: &LiveIndex, snap: &PlayerSnapshot, forward: bool, failing: &HashSet<ItemRef>) -> Option<ChannelId> {
    let zap = snap.zap.as_ref()?;
    // a copy standing in for the pick (multi-view) zaps from the pick's place in the list
    let current = snap.zap_anchor.or_else(|| playing_channel(snap)).or_else(|| match snap.playable.as_ref().map(|p| &p.media) {
        Some(MediaRef::Channel(c)) => Some(*c),
        _ => None,
    })?;
    let skip = |c: ChannelId| live_idx.get(c).is_none_or(|x| x.locked || x.unsupported || failing.contains(&live::channel_ref(x)));
    LiveIndex::neighbour(&zap.channels, current, forward, &skip)
}

impl PlayerScreen {
    fn zap(&mut self, ctx: &mut UiCtx<'_>, forward: bool) {
        let snap = ctx.player.clone();
        // no zap list yet (startup `--play` before the library loaded): nothing to do
        if snap.zap.as_ref().is_none_or(|z| z.channels.is_empty()) {
            return;
        }
        // the spoiler guard's zap shield: a shielded neighbour gets its card, never a tune
        if self.shield_zap(ctx, forward) {
            return;
        }
        if ctx.parental.is_unlocked(&ctx.config.parental) {
            ctx.play(PlayerCommand::Zap { forward, unlocked: true });
            return;
        }
        let failing = super::live::failing_set(ctx).unwrap_or_default();
        if let Some(target) = zap_target(&ctx.lib.live, &snap, forward, &failing) {
            ctx.play(PlayerCommand::ZapTo { id: target, unlocked: false });
        }
    }

    /// Tune a channel from a panel / numeric entry (keeps the zap list); locked → PIN.
    fn tune(&mut self, ctx: &mut UiCtx<'_>, id: ChannelId, retry: Action) -> bool {
        if self.shield_tune(ctx, id) {
            return true;
        }
        if playing_channel(ctx.player) == Some(id) {
            return true;
        }
        let Some(ch) = ctx.lib.live.get(id) else { return false };
        if ch.unsupported {
            ctx.toast(Toast::info("DRM-protected — can't be played"));
            return false;
        }
        if ch.locked && !ctx.parental.is_unlocked(&ctx.config.parental) {
            let reason = format!("“{}” is locked", ch.name);
            ctx.open(OverlayRequest::Pin { then: Box::new(retry), reason });
            return false;
        }
        if ctx.player.zap.is_some() {
            let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
            ctx.play(PlayerCommand::ZapTo { id, unlocked });
        } else {
            let zap = live::primary_zap(ctx, id);
            ctx.play(PlayerCommand::Play(PlayRequest {
                media: MediaRef::Channel(id),
                zap,
                origin: PlayOrigin::Fullscreen,
                resume: false,
                immediate: true,
            }));
        }
        true
    }

    fn numeric_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        let (digits, at) = self.numeric.as_mut()?;
        match action {
            Action::Digit(d) => {
                if digits.len() < 5 {
                    digits.push(char::from(b'0' + d.min(9)));
                }
                *at = Instant::now();
                Some(true)
            }
            Action::FilterBackspace => {
                digits.pop();
                *at = Instant::now();
                if digits.is_empty() {
                    self.numeric = None;
                }
                Some(true)
            }
            Action::Back => {
                self.numeric = None;
                Some(true)
            }
            Action::Activate => {
                self.commit_numeric(ctx);
                Some(true)
            }
            // anything else commits what was typed, then acts
            _ => {
                self.commit_numeric(ctx);
                None
            }
        }
    }

    fn commit_numeric(&mut self, ctx: &mut UiCtx<'_>) {
        let Some((digits, _)) = self.numeric.take() else { return };
        let Ok(n) = digits.parse::<u32>() else { return };
        match ctx.lib.live.by_number(n) {
            Some(id) => {
                self.tune(ctx, id, Action::Activate);
                self.show_banner(ctx);
            }
            None => self.flash_text = Some((format!("No channel {n}"), Instant::now())),
        }
    }

    /// Leave the player, playback continuing in the mini player (`p`, the top bar's ←);
    /// popped out, the window goes back in too.
    fn leave(ctx: &mut UiCtx<'_>) {
        ctx.navigate(NavRequest::Back);
        if ctx.video_only {
            ctx.navigate(NavRequest::TogglePopOut);
        }
    }

    /// Stop and leave (`q`, the pop-out strip's ✕); popped out, the window goes back in too.
    fn stop_and_leave(ctx: &mut UiCtx<'_>) {
        ctx.play(PlayerCommand::Stop);
        Self::leave(ctx);
    }

    /// The player menu (right-click on the video): what the player's keys do, with their keys.
    fn open_menu(&mut self, ctx: &mut UiCtx<'_>) {
        let snap = ctx.player;
        let live = is_live(snap);
        let k = if live { KeyContext::PlayerLive } else { KeyContext::PlayerVod };
        // multi-view: what to do with the picture first (DESIGN §35)
        let mut items = self.multiview_menu(ctx);
        if live {
            items.push(menu_item(glyphs::PLAYLIST, "Channels…", Action::ChannelList, true, k));
            items.push(menu_item(glyphs::GUIDE, "Now / next…", Action::MiniGuide, true, k));
            items.push(menu_item(glyphs::HISTORY, "History…", Action::ChannelHistory, true, k));
            items.push(menu_item(glyphs::HISTORY, "Last channel", Action::LastChannel, snap.previous_live.is_some(), k));
            if let Some(ch) = playing_channel(snap).and_then(|id| ctx.lib.live.get(id)) {
                let fav = ctx.favs.is_fav(&live::channel_ref(ch));
                let (icon, label) = if fav { (glyphs::FAVOURITE_OFF, "Remove from Favourites") } else { (glyphs::FAVOURITE, "Add to Favourites") };
                items.push(menu_item(icon, label, Action::ToggleFavourite, true, k));
            }
        }
        if let Some(item) = snap.playable.as_ref().and_then(|p| p.item.as_ref()) {
            if item.kind != crate::model::ItemKind::Channel {
                items.push(menu_item(
                    glyphs::FAVOURITE,
                    if ctx.favs.is_fav(item) { "Remove from My list" } else { "Save to My list" },
                    Action::ToggleFavourite,
                    true,
                    k,
                ));
            }
            items.push(menu_item(
                glyphs::FAV_LIST,
                if item.kind == crate::model::ItemKind::Channel { "Favourites lists…" } else { "Watchlists…" },
                Action::AddToList,
                true,
                k,
            ));
        }
        // the TV has its own picture, tracks and window (DESIGN §40 §3.3)
        let casting = snap.cast.is_some();
        if !casting {
            items.push(menu_item(glyphs::ASPECT, "Aspect ratio", Action::CycleAspect, true, k));
            items.push(menu_item(glyphs::DISPLAY, "Zoom", Action::CycleZoom, true, k));
            items.push(menu_item(glyphs::AUDIO_TRACK, "Audio…", Action::AudioMenu, true, k));
            items.push(menu_item(glyphs::SUBTITLES, "Subtitles…", Action::SubtitleMenu, true, k));
        }
        items.push(menu_item(glyphs::INFO, "Stream info", Action::StreamInfo, true, k));
        if matches!(snap.playable.as_ref().map(|p| &p.kind), Some(PlayableKind::Catchup { .. })) {
            items.push(menu_item(glyphs::LIVE_DOT, "Jump to live", Action::JumpToLive, true, KeyContext::PlayerVod));
        }
        items.extend(self.cast_menu_items(ctx, k));
        if ctx.video_only {
            items.push(menu_item(glyphs::POP_IN, "Back to the full window", Action::TogglePopOut, true, KeyContext::PopOut));
        } else if !casting {
            items.push(menu_item(glyphs::POP_OUT, "Pop out", Action::TogglePopOut, true, k));
        }
        items.push(menu_item(glyphs::MINI_PLAYER, "Keep playing in the mini player", Action::Minimize, true, k));
        items.extend(crate::reminders::ui::player_menu_items(KeyContext::Global));
        items.push(menu_item(glyphs::STOP, "Stop and close", Action::StopAndClose, true, k));
        let title = snap.playable.as_ref().map(|p| p.title.clone()).unwrap_or_else(|| "Player".into());
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    // ------------------------------------------------------------------ drawing

    /// Ctrl+O while a movie or episode plays: the title's other copies, each from where
    /// playback is now (a few indexed reads, once per press).
    fn play_from_menu(&mut self, ctx: &mut UiCtx<'_>) {
        let snap = ctx.player.clone();
        let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
        let (db, lib, order, now) = (ctx.db, ctx.lib, ctx.config.interface.copy_order, ctx.now);
        let plan_of = |movies: bool, item: u32| crate::store::copies::plan(db, &lib.vod, movies, item, unlocked, order, now).ok();
        let copies = crate::ui::copies::other_copies(db, lib, plan_of, &snap);
        if copies.is_empty() {
            ctx.toast(crate::msg::Toast::info("This title has no other copy"));
            return;
        }
        let items = copies
            .iter()
            .take(usize::from(u8::MAX - SWITCH_COPY))
            .enumerate()
            .map(|(k, c)| crate::ui::overlays::MenuItem {
                icon: crate::ui::widgets::glyphs::PLAY,
                label: c.label.clone(),
                action: Action::Digit(SWITCH_COPY + k as u8),
                enabled: true,
                shortcut: None,
            })
            .collect();
        self.switch_copies = copies;
        ctx.open(OverlayRequest::ContextMenu(crate::ui::overlays::ContextMenuSpec { title: "Play from".into(), items }));
    }
}

impl Screen for PlayerScreen {
    fn id(&self) -> ScreenId {
        ScreenId::Player
    }

    fn key_contexts(&self) -> Vec<KeyContext> {
        // the context is only decided by the content; the banner never changes it
        let base = if self.live_ctx { KeyContext::PlayerLive } else { KeyContext::PlayerVod };
        if self.numeric.is_some() {
            vec![KeyContext::NumericEntry, base]
        } else if let Some(p) = &self.panel {
            vec![p.list.key_context(), base]
        } else if self.osd_focus {
            vec![KeyContext::PlayerOsd, base]
        } else {
            vec![base]
        }
    }

    fn allows_mini_player(&self) -> bool {
        false
    }

    fn on_enter(&mut self, ctx: &mut UiCtx<'_>, _route: &Route) {
        self.osd_focus = false;
        self.panel = None;
        self.numeric = None;
        self.visible.clear();
        self.live_ctx = is_live(ctx.player);
        if ctx.player.state != PlayerState::Idle {
            self.show_banner(ctx);
        } else {
            self.banner_until = None;
        }
    }

    fn on_event(&mut self, ctx: &mut UiCtx<'_>, ev: &AppEvent) {
        self.guide.on_event(ev);
        // only while the fullscreen player was on screen last frame
        if ctx.video.placement() != Placement::Fullscreen {
            return;
        }
        match ev {
            AppEvent::Player(PlayerEvent::AudioOutputChanged { previous: Some(previous), output, offset_ms })
                if ctx.player.cast.is_none() && previous.name != output.name =>
            {
                self.flash = None;
                self.cast_notice(ctx, &format!("Lip sync for {}: {}", output.label, crate::player::audio_output::offset_text(*offset_ms)));
            }
            // an episode stopped early (not watched) gets no up-next card
            AppEvent::Player(PlayerEvent::Ended { finished: true, .. }) => self.end_of_playback(ctx),
            AppEvent::Player(PlayerEvent::NowPlaying(p)) => {
                self.live_ctx = p.kind.is_live();
                self.next_episode = None;
                self.next_lookup = None;
                self.visible.clear();
                self.show_banner(ctx);
            }
            _ => {}
        }
    }

    fn on_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> bool {
        self.live_ctx = is_live(ctx.player);
        self.find_next_episode(ctx);
        if let Some(h) = self.numeric_action(ctx, action) {
            return h;
        }
        if let Some(h) = self.panel_action(ctx, action) {
            return h;
        }
        if let Some(h) = self.shield_action(ctx, action) {
            return h;
        }
        if let Some(h) = self.spoiler_action(ctx, action) {
            return h;
        }
        if let Some(h) = self.multiview_action(ctx, action) {
            return h;
        }
        if let Some(h) = self.queue_action(ctx, action) {
            return h;
        }
        if self.next_episode.is_some() {
            match action {
                Action::FocusOsdControls | Action::Activate => {
                    self.play_next_episode(ctx);
                    return true;
                }
                Action::Back => {
                    self.next_episode = None;
                    return true;
                }
                _ => {}
            }
        }
        let narrow = ctx.layout == crate::ui::LayoutClass::Compact;
        if let Some(h) = self.osd_action(ctx, action, narrow) {
            return h;
        }
        // keys that manage the window / app never show the banner
        if !matches!(
            action,
            Action::Back
                | Action::ToggleWindowFullscreen
                | Action::TogglePopOut
                | Action::ToggleHelp
                | Action::OpenSearch
                | Action::Quit
                | Action::ShowJobs
        ) {
            self.show_banner(ctx);
        }
        let skip_small = f64::from(ctx.config.player.skip_small_s);
        let skip_large = f64::from(ctx.config.player.skip_large_s);
        let live = self.live_ctx;
        let casting = ctx.player.cast.is_some();
        match action {
            Action::CastMenu if crate::cast::on(ctx.config) || casting => self.cast_menu(ctx),
            Action::StopCasting if casting => ctx.play(PlayerCommand::StopCasting { play_here: false }),
            // a TV with no volume, and keys for what the TV does itself (DESIGN §40 §11.3)
            Action::Mute | Action::VolumeUp | Action::VolumeDown if tv_volume(ctx.player) => self.cast_notice(ctx, TV_VOLUME),
            Action::CycleAudio
            | Action::CycleSubtitle
            | Action::AudioMenu
            | Action::SubtitleMenu
            | Action::LipSyncEarlier
            | Action::LipSyncLater
            | Action::SubDelayMinus
            | Action::SubDelayPlus
            | Action::SubDelayReset
            | Action::SubSizeMinus
            | Action::SubSizePlus
            | Action::CycleAspect
            | Action::CycleZoom
                if casting =>
            {
                self.cast_notice(ctx, NOT_WHILE_CASTING)
            }
            Action::PlayPause => ctx.play(PlayerCommand::TogglePause),
            Action::Mute => {
                ctx.play(PlayerCommand::ToggleMute);
                self.flash = Some((Flash::Volume, Instant::now()));
            }
            Action::VolumeUp | Action::VolumeDown => {
                ctx.play(PlayerCommand::AdjustVolume(if action == Action::VolumeUp { 5.0 } else { -5.0 }));
                self.flash = Some((Flash::Volume, Instant::now()));
            }
            Action::FocusOsdControls => {
                self.osd_focus = true;
                self.row = OsdRow::Buttons;
                self.control = self
                    .active_controls(ctx.player, narrow, ctx.video_only, osd::OptionalButtons::of(ctx))
                    .iter()
                    .position(|c| *c == Control::PlayPause)
                    .unwrap_or(0);
            }
            Action::LeaveOsdControls => self.osd_focus = false,
            Action::StreamInfo => self.toggle_info(ctx),
            Action::AudioMenu => ctx.open(OverlayRequest::Tracks { audio: true }),
            Action::SubtitleMenu => ctx.open(OverlayRequest::Tracks { audio: false }),
            Action::CycleAudio => {
                ctx.play(PlayerCommand::CycleAudio);
                self.flash = Some((Flash::Audio, Instant::now()));
            }
            Action::CycleSubtitle => {
                ctx.play(PlayerCommand::CycleSubtitle);
                self.flash = Some((Flash::Subtitles, Instant::now()));
            }
            Action::LipSyncEarlier | Action::LipSyncLater => {
                ctx.edit_lip_sync(ctx.player.audio_output.clone(), Some(if action == Action::LipSyncLater { 1 } else { -1 }));
                self.flash_text = None;
                self.flash = Some((Flash::LipSync, Instant::now()));
            }
            Action::SubDelayMinus | Action::SubDelayPlus => {
                ctx.play(PlayerCommand::AdjustSubDelay(if action == Action::SubDelayPlus { 0.1 } else { -0.1 }));
                self.flash = Some((Flash::SubDelay, Instant::now()));
            }
            Action::SubDelayReset => {
                ctx.play(PlayerCommand::ResetSubDelay);
                self.flash = Some((Flash::SubDelay, Instant::now()));
            }
            Action::SubSizeMinus | Action::SubSizePlus => {
                ctx.play(PlayerCommand::AdjustSubScale(if action == Action::SubSizePlus { 0.1 } else { -0.1 }));
                self.flash = Some((Flash::SubSize, Instant::now()));
            }
            Action::CycleAspect => {
                ctx.play(PlayerCommand::CycleAspect);
                self.flash = Some((Flash::Aspect, Instant::now()));
            }
            Action::CycleZoom => {
                self.zoom = (self.zoom + 1) % ZOOMS.len();
                ctx.play(PlayerCommand::SetZoom(ZOOMS[self.zoom]));
                self.flash = Some((Flash::Zoom, Instant::now()));
            }
            // popped out: leave the player AND put the window back (`p` ends in the mini player)
            Action::Minimize => Self::leave(ctx),
            Action::StopAndClose => Self::stop_and_leave(ctx),
            Action::Retry => {
                if matches!(ctx.player.state, PlayerState::Error(_) | PlayerState::Reconnecting { .. }) {
                    ctx.play(PlayerCommand::Reconnect);
                }
            }
            // after an error: the title's other copy, where playback stopped (never by itself)
            Action::PlayFrom => match self.offered(ctx.player) {
                Some(other) => ctx.play(PlayerCommand::PlayCopy(other.play.clone())),
                None if matches!(ctx.player.state, PlayerState::Error(_)) => ctx.toast(crate::msg::Toast::info("This title has no other copy")),
                // during playback: "Play from…" the title's other copies, where it is now
                None => self.play_from_menu(ctx),
            },
            Action::ToggleFavourite => {
                if let Some((item, title)) = ctx.player.playable.as_ref().and_then(|p| p.item.clone().map(|item| (item, p.title.clone()))) {
                    if item.kind == crate::model::ItemKind::Channel {
                        live::toggle_favourite(ctx, item, &title);
                    } else {
                        crate::ui::watchlists::toggle_my_list(ctx, &item);
                    }
                }
            }
            Action::AddToList => {
                if let Some((item, title)) = ctx.player.playable.as_ref().and_then(|p| p.item.clone().map(|i| (i, p.title.clone()))) {
                    crate::ui::screens::home::kit::list_picker(ctx, &item, &title);
                }
            }
            Action::Back => {
                // Esc hides the banner first; it never shows it
                if self.info {
                    self.info = false;
                    return true;
                }
                // the visible banner — also the one a pause keeps up — is the next rung
                if self.banner_shown(ctx.player) {
                    self.banner_until = None;
                    self.banner_dismissed = true;
                    return true;
                }
                return false;
            }
            Action::ZapPrev | Action::ZapNext if live => self.zap(ctx, action == Action::ZapNext),
            Action::LastChannel if live => {
                let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
                let prev = ctx.player.previous_live.as_ref().map(|p| p.media.clone());
                // the controller ignores a recall onto a locked channel while locked: ask first
                let locked = match &prev {
                    Some(MediaRef::Channel(id)) if !unlocked => ctx.lib.live.get(*id).filter(|c| c.locked).map(|c| c.name.clone()),
                    _ => None,
                };
                if let Some(name) = locked {
                    ctx.open(OverlayRequest::Pin { then: Box::new(Action::LastChannel), reason: format!("“{name}” is locked") });
                } else if prev.is_some() {
                    ctx.play(PlayerCommand::LastChannel { unlocked });
                }
            }
            Action::ChannelHistory if live => self.open_panel(ctx, PanelKind::History),
            Action::ChannelList if live => self.open_panel(ctx, PanelKind::Channels),
            Action::MiniGuide if live => self.open_panel(ctx, PanelKind::MiniGuide),
            Action::Digit(d) if live => self.numeric = Some((char::from(b'0' + d.min(9)).to_string(), Instant::now())),
            Action::SeekBack if !live => ctx.play(PlayerCommand::SeekBy(-skip_small)),
            Action::SeekForward if !live => ctx.play(PlayerCommand::SeekBy(skip_small)),
            Action::SeekBackBig if !live => ctx.play(PlayerCommand::SeekBy(-skip_large)),
            Action::SeekForwardBig if !live => ctx.play(PlayerCommand::SeekBy(skip_large)),
            Action::JumpToLive => {
                if matches!(ctx.player.playable.as_ref().map(|p| &p.kind), Some(PlayableKind::Catchup { .. })) {
                    ctx.play(PlayerCommand::JumpToLive);
                }
            }
            Action::Home if !live => ctx.play(PlayerCommand::SeekTo(0.0)),
            _ => return false,
        }
        true
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>) {
        // popped out, the scheduled-playback strip (`reminders::ui`) draws over the top of the
        // picture as a foreground area
        let rect = ui.available_rect_before_wrap();
        let theme = ctx.theme;
        let snap = ctx.player.clone();
        self.live_ctx = is_live(&snap);
        self.bottom_bar = None;
        self.bars.clear();
        self.find_next_episode(ctx);
        self.settle_reveal(ctx);
        if std::mem::take(&mut self.toggle_fullscreen) {
            let fs = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        if snap.state == PlayerState::Idle && snap.playable.is_none() && !ctx.multiview.on() {
            self.idle_ui(ui, ctx, rect);
            return;
        }
        self.update_other_copy(ctx, &snap);
        self.update_cast_context(ctx, &snap);
        // multi-view: both pictures in their layout (the response covers every picture)
        let resp = match self.paint_multiview(ui, ctx, rect) {
            Some(r) => r,
            None => ctx.video.paint(ui, rect, Placement::Fullscreen, theme, ctx.images, &snap),
        };
        // popped out, a drag on the video moves the window (only there: a drag-sensing video
        // counts as hovered under every button drawn over it, see the wheel and cursor below)
        let resp = if ctx.video_only { resp.interact(egui::Sense::drag()) } else { resp };
        self.draw_multiview(ui, ctx, rect);
        // a failed / reconnecting stream: Retry / Last channel buttons under the message
        let p = ui.painter_at(rect);
        for (i, b) in ctx.video.state_buttons().to_vec().into_iter().enumerate() {
            p.rect_stroke(b.rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(theme.tokens.osd_text, 0.6)), egui::StrokeKind::Inside);
            let (label, tip, action) = match b.action {
                StateAction::Retry => ("Retry", "Retry (r)", Some(Action::Retry)),
                StateAction::LastChannel => ("Last channel", "Last channel (Backspace)", Some(Action::LastChannel)),
                StateAction::OtherCopy => ("Play from another copy", "Play from another copy (Ctrl+O)", Some(Action::PlayFrom)),
                StateAction::CopyError => ("Copy error", "Copy error", None),
                StateAction::Stop => ("Stop", "Stop and close (q)", Some(Action::StopAndClose)),
                // the casting card's own (DESIGN §40 §3.3, §3.4)
                cast => {
                    let label = cast.cast_label().unwrap_or_default();
                    (label, label, None)
                }
            };
            if osd_button(ui, &p, theme, b.rect, ui.id().with(("state-button", i)), &b.text, b.font, label, Some(tip), false).clicked() {
                match action {
                    Some(a) => {
                        self.on_action(ctx, a);
                    }
                    None if b.action == StateAction::CopyError => {
                        if let PlayerState::Error(e) = &snap.state {
                            let (head, detail) = crate::ui::video_host::error_copy(e.kind, &e.message);
                            ui.ctx().copy_text(if detail.is_empty() { head } else { format!("{head}\n{detail}") });
                            ctx.toast(Toast::info("Copied error"));
                        }
                    }
                    None => self.cast_state_action(ctx, b.action, &snap),
                }
            }
        }
        // mouse: motion shows the banner, double-click toggles window fullscreen (pops the
        // window back in when popped out, where a drag moves the window), wheel = volume, a
        // click closes a side panel / leaves controls focus mode, right-click = player menu
        if ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) && resp.hovered() {
            self.show_banner(ctx);
        }
        if resp.double_clicked() && ctx.video_only {
            ctx.navigate(NavRequest::TogglePopOut);
        } else if resp.double_clicked() {
            let fs = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        } else if resp.clicked() && self.panel.is_some() {
            self.panel = None;
        } else if resp.clicked() && self.osd_focus {
            self.osd_focus = false;
        } else if resp.secondary_clicked() {
            self.show_banner(ctx);
            self.open_menu(ctx);
        }
        if ctx.video_only && resp.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        // numeric entry auto-commit
        if let Some((_, at)) = &self.numeric {
            if at.elapsed() >= NUMERIC_COMMIT {
                self.commit_numeric(ctx);
            } else {
                ui.ctx().request_repaint_after(NUMERIC_COMMIT - at.elapsed());
            }
        }
        // next-episode countdown
        if let Some(n) = &self.next_episode {
            if Instant::now() >= n.at {
                self.play_next_episode(ctx);
            } else {
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            }
        }
        // stream info refresh every 2 s while open
        if self.info {
            if self.info_refreshed.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) {
                ctx.play(PlayerCommand::RefreshStreamInfo);
                self.info_refreshed = Some(Instant::now());
            }
            ui.ctx().request_repaint_after(Duration::from_millis(500));
            self.draw_info(ui, ctx, rect, &snap);
        }
        if self.banner_visible(&snap) {
            if ctx.video_only && !self.osd_focus && self.panel.is_none() {
                self.draw_compact_osd(ui, ctx, rect, &snap);
            } else {
                self.draw_osd(ui, ctx, rect, &snap);
            }
            self.draw_multiview_bar(ui, ctx, rect);
            if let Some(t) = self.banner_until {
                let left = t.saturating_duration_since(Instant::now());
                if !left.is_zero() {
                    ui.ctx().request_repaint_after(left);
                }
            }
            // the clock and programme progress move
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }
        self.draw_panel(ui, ctx, rect);
        self.draw_overlays(ui, ctx, rect, &snap);
        self.draw_shield(ui, ctx, rect, &snap);
        if self.flash.is_some_and(|(_, at)| at.elapsed() < FLASH) || self.flash_text.as_ref().is_some_and(|(_, at)| at.elapsed() < FLASH) {
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        }
        // wheel = volume, after everything drawn over the video: the speaker / slider and the
        // panel's list take their own scroll first (they consume it)
        if resp.hovered() {
            self.wheel += ui.input(|i| i.smooth_scroll_delta.y);
            let steps = (self.wheel / 40.0).trunc();
            if steps != 0.0 {
                self.wheel -= steps * 40.0;
                if tv_volume(&snap) {
                    self.cast_notice(ctx, TV_VOLUME);
                } else {
                    ctx.play(PlayerCommand::AdjustVolume(f64::from(steps) * 5.0));
                    self.flash = Some((Flash::Volume, Instant::now()));
                    self.show_banner(ctx);
                }
            }
        }
        // the banner stays while the pointer is in use on a bar (it would vanish under a drag of
        // the volume or the seek bar) — not under a pointer parked there for good, which would
        // keep it up for keyboard users; with the banner gone the pointer hides over the video
        // (not over a control that asked for its own cursor)
        let pointer_live = ui.input(|i| i.pointer.any_down() || i.pointer.time_since_last_movement() < POINTER_REST);
        if pointer_live && self.bars.iter().any(|b| ui.rect_contains_pointer(*b)) {
            self.show_banner(ctx);
        } else if !self.banner_visible(&snap)
            && self.next_episode.is_none()
            && resp.hovered()
            && ui.ctx().output(|o| o.cursor_icon == egui::CursorIcon::Default)
        {
            ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        }
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        match result {
            // the app handles the pop-out key itself; from the menu it comes here
            OverlayResult::Chosen(Action::TogglePopOut) => ctx.navigate(NavRequest::TogglePopOut),
            // a global key's entry (rui): its menu
            OverlayResult::Chosen(Action::SleepTimer) => ctx.open(OverlayRequest::SleepTimer),
            // the menu's "Close picture B" (multi-view; never a typed channel number)
            OverlayResult::Chosen(Action::Digit(multiview::CLOSE_OTHER)) if ctx.multiview.on() => {
                ctx.mv_request(crate::ui::multiview::MvRequest::Close(Some(ctx.multiview.view.unfocused())));
            }
            // the menu's casting items (codes above typed digits)
            OverlayResult::Chosen(Action::Digit(CAST_PLAY_HERE)) => ctx.play(PlayerCommand::StopCasting { play_here: true }),
            OverlayResult::Chosen(Action::Digit(CAST_CONVERT)) => ctx.services.agent.cast().convert_current(),
            OverlayResult::Chosen(Action::Digit(CAST_AS_IS)) => ctx.services.agent.cast().send_as_is(),
            // a copy of the Ctrl+O menu (never a typed channel number)
            OverlayResult::Chosen(Action::Digit(d)) if d >= SWITCH_COPY && !self.switch_copies.is_empty() => {
                if let Some(c) = self.switch_copies.get(usize::from(d - SWITCH_COPY)) {
                    ctx.play(PlayerCommand::PlayCopy(c.play.clone()));
                }
                self.switch_copies.clear();
            }
            OverlayResult::Unlocked(a) | OverlayResult::Chosen(a) => {
                self.on_action(ctx, a);
            }
            OverlayResult::Confirmed(_) | OverlayResult::Cancelled(_) => {}
        }
    }
}

/// Mouse paths of the player (kittest over `TestUi`: a real headless mpv answers the commands,
/// the screen sees its state as a playing live channel).
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eframe::egui;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::model::Playable;
    use crate::redact::Secret;
    use crate::ui::screens::home::testkit::{App, harness, has_label, rig, until};

    type H = Harness<'static, App<PlayerScreen>>;

    const WIDE: egui::Vec2 = egui::vec2(1600.0, 900.0);

    fn playable() -> Arc<Playable> {
        Arc::new(Playable {
            media: MediaRef::Channel(ChannelId(1)),
            kind: PlayableKind::Live,
            item: None,
            source_id: None,
            title: "News".into(),
            subtitle: None,
            logo: None,
            number: Some(1),
            url: Secret::new("http://127.0.0.1/live.ts".into()),
            headers: Default::default(),
            start_at: None,
            duration_hint: None,
        })
    }

    fn real(h: &H) -> Arc<PlayerSnapshot> {
        h.state().t.services().tiles.focused().snapshot()
    }

    /// What the screen draws from (after [`sync`]). A wait checks this, not [`real`]: the
    /// player can change between the copy and the check, and a wait that passed on the newer
    /// state left the screen on the older one (the Unmute label then never came).
    fn shown(h: &H) -> Arc<PlayerSnapshot> {
        h.state().t.snapshot.clone()
    }

    /// The screen's snapshot: mpv's volume / mute / pause, playing a live channel.
    fn sync(h: &mut H) {
        let mut s = (*real(h)).clone();
        s.state = PlayerState::Playing;
        s.playable = Some(playable());
        h.state_mut().t.snapshot = Arc::new(s);
    }

    /// The player entered (banner up) in a window of `size`.
    fn player(size: egui::Vec2) -> H {
        let (t, one) = rig();
        let mut h = harness(t, one, PlayerScreen::default(), size);
        sync(&mut h);
        h.state_mut().enter(Route::Player);
        h.run_steps(3);
        h
    }

    fn press(h: &H, pos: egui::Pos2, button: egui::PointerButton) {
        for pressed in [true, false] {
            h.event(egui::Event::PointerButton { pos, button, pressed, modifiers: egui::Modifiers::NONE });
        }
    }

    #[test]
    fn lip_sync_first_identification_is_silent_and_only_a_new_name_flashes() {
        use crate::player::audio_output::AudioOutput;
        let mut h = player(WIDE);
        h.state_mut().t.video.begin_frame();
        let speakers = AudioOutput { name: "speakers".into(), label: "Speakers".into() };
        let event = |h: &mut H, previous, output| {
            let app = h.state_mut();
            let s = &mut app.s;
            app.t.with_ctx(|ctx| s.on_event(ctx, &AppEvent::Player(PlayerEvent::AudioOutputChanged { previous, output, offset_ms: -150 })));
        };
        event(&mut h, None, speakers.clone());
        assert!(h.state().s.flash_text.is_none());
        let renamed = AudioOutput { label: "Laptop speakers".into(), ..speakers.clone() };
        event(&mut h, Some(speakers), renamed.clone());
        assert!(h.state().s.flash_text.is_none());
        let headphones = AudioOutput { name: "headphones".into(), label: "Headphones".into() };
        event(&mut h, Some(renamed), headphones);
        assert_eq!(h.state().s.flash_text.as_ref().map(|(text, _)| text.as_str()), Some("Lip sync for Headphones: 150 ms earlier"));
    }

    /// Click at 50 % of the slider → 65 % (of 0–130); a wheel notch over it → +5; the speaker
    /// mutes; moving the slider while muted unmutes.
    #[test]
    fn the_osd_volume_slider_sets_the_volume_and_the_speaker_mutes() {
        let mut h = player(WIDE);
        // the banner stays up however slow mpv is to answer under a loaded test run
        let mut cfg = (*h.state().t.config).clone();
        cfg.interface.osd_timeout_ms = 60_000;
        h.state_mut().t.config = Arc::new(cfg);
        let c = h.get_by_label("Volume").rect().center();
        h.hover_at(c);
        h.drag_at(c);
        h.drop_at(c);
        until(&mut h, "volume 65 %", |h| {
            sync(h);
            shown(h).volume == 65.0
        });
        h.hover_at(c);
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: egui::vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            modifiers: egui::Modifiers::NONE,
        });
        until(&mut h, "one wheel notch up", |h| {
            sync(h);
            shown(h).volume == 70.0
        });
        assert!(!real(&h).muted);
        h.get_by_label("Mute").click();
        until(&mut h, "muted", |h| {
            sync(h);
            shown(h).muted
        });
        // the label follows the player's state a frame or more later (slow under a loaded test run)
        until(&mut h, "the speaker says what a click does (Unmute)", |h| has_label(h, "Unmute"));
        h.hover_at(c);
        h.drag_at(c);
        h.drop_at(c);
        until(&mut h, "unmuted at 65 %", |h| {
            sync(h);
            !shown(h).muted && shown(h).volume == 65.0
        });
        // half a notch over the video, then a notch over the speaker: the speaker's notch
        // alone counts (the video used to take it too, +10)
        let wheel = |h: &H, unit, dy| {
            h.event(egui::Event::MouseWheel { unit, delta: egui::vec2(0.0, dy), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE })
        };
        h.hover_at(egui::pos2(WIDE.x / 3.0, WIDE.y / 2.0));
        wheel(&h, egui::MouseWheelUnit::Point, 20.0);
        h.run_steps(30);
        h.hover_at(h.get_by_label("Mute").rect().center());
        wheel(&h, egui::MouseWheelUnit::Line, 1.0);
        until(&mut h, "one notch over the speaker", |h| {
            sync(h);
            shown(h).volume >= 70.0
        });
        h.run_steps(30);
        std::thread::sleep(Duration::from_millis(100));
        h.run_steps(2);
        assert_eq!(real(&h).volume, 70.0, "one notch, one step");
    }

    /// The banner stays while the pointer is in use on a bar, but a pointer parked there (a
    /// keyboard user's, pushed out of the way) doesn't keep it up for good.
    #[test]
    fn a_pointer_parked_on_a_bar_lets_the_banner_time_out() {
        let mut h = player(WIDE);
        let mut c = (*h.state().t.config).clone();
        c.interface.osd_timeout_ms = 300;
        h.state_mut().t.config = Arc::new(c);
        let banner = |h: &H| h.state().s.banner_shown(&real(h));
        // frames keep coming for 0.4 s (longer than the timeout), as they do on screen
        let frames = |h: &mut H| {
            for _ in 0..20 {
                std::thread::sleep(Duration::from_millis(20));
                h.run_steps(1);
            }
        };
        // on the bottom bar, beside the controls
        let at = h.get_by_label("Play / pause").rect().center() + egui::vec2(0.0, -40.0);
        h.hover_at(at);
        h.run_steps(1);
        assert!(h.state_mut().act(Action::ZapNext));
        frames(&mut h);
        assert!(banner(&h), "held while the pointer was just moved there");
        // 10 s of a still pointer (frames are 20 ms apart in egui's clock)
        h.run_steps(520);
        assert!(h.state_mut().act(Action::ZapNext));
        frames(&mut h);
        assert!(!banner(&h), "a parked pointer doesn't hold the banner");
    }

    /// The spoiler guard's zap shield acts before tuning: ↓ onto a shielded channel puts the
    /// card up and tunes nothing, ↓ on the card judges the next channel, Enter tunes it anyway.
    #[test]
    fn a_shielded_zap_tunes_nothing_until_watch_anyway() {
        use crate::guard::Guard;
        use crate::spoiler::snapshot::{GuardSnapshot, Settings, ShieldSpan, Topic, TopicMode, TopicSource};
        use crate::ui::screens::home::testkit::{channels, seed, source};
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = source(c, "TV")?;
            channels(c, s, 4)
        });
        // a Sky Sports News on channels 2 and 3 while "Arsenal" is guarded
        let span = ShieldSpan { start: t.now - 60, stop: t.now + 3600, topic: 0, is_event: false };
        let shields = [(ids[1], vec![span.clone()]), (ids[2], vec![span])].into_iter().collect();
        let topic = Topic::new("Arsenal", TopicSource::Follow, TopicMode::Always);
        let settings = Settings { shield_zap: true, ..Default::default() };
        let snap = GuardSnapshot::from_parts(settings, vec![(topic, vec!["arsenal".into()])], Default::default(), shields);
        t.services().agent.set_guard(Guard::with_snapshot(snap));
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        let mut s = (*real(&h)).clone();
        s.state = PlayerState::Playing;
        s.playable = Some(Arc::new(Playable { media: MediaRef::Channel(ids[0]), ..(*playable()).clone() }));
        s.zap = Some(crate::model::ZapList { collection: crate::model::Collection::AllChannels, channels: ids.clone().into() });
        h.state_mut().t.snapshot = Arc::new(s);
        h.state_mut().enter(Route::Player);
        h.run_steps(3);
        let tuned = |h: &H| real(h).playable.as_ref().map(|p| p.media.clone());
        assert!(h.state_mut().act(Action::ZapNext));
        h.run_steps(3);
        assert_eq!(h.state().s.shield_card(), Some(ids[1]), "the card, not channel 2");
        assert!(h.state_mut().act(Action::ZapNext));
        h.run_steps(3);
        assert_eq!(h.state().s.shield_card(), Some(ids[2]), "↓ on the card judges channel 3: shielded too");
        std::thread::sleep(Duration::from_millis(300));
        h.run_steps(2);
        assert_eq!(tuned(&h), None, "nothing was tuned");
        assert!(h.state_mut().act(Action::FocusOsdControls), "Enter: watch anyway");
        until(&mut h, "channel 3 tuned", |h| tuned(h) == Some(MediaRef::Channel(ids[2])));
        assert_eq!(h.state().s.shield_card(), None);
    }

    /// Multi-view: a shielded zap in picture B puts the card over picture B alone — picture A
    /// is neither covered nor paused — and its keys wait until B has the focus again.
    #[test]
    fn a_shield_card_in_multi_view_covers_only_the_picture_that_zapped() {
        use crate::guard::Guard;
        use crate::model::TileId;
        use crate::spoiler::snapshot::{GuardSnapshot, Settings, ShieldSpan, Topic, TopicMode, TopicSource};
        use crate::ui::multiview::{MultiView, MvFrame};
        use crate::ui::screens::home::testkit::{channels, seed, source};
        let (mut t, one) = rig();
        let ids = seed(&mut t, |c| {
            let s = source(c, "TV")?;
            channels(c, s, 5)
        });
        let span = ShieldSpan { start: t.now - 60, stop: t.now + 3600, topic: 0, is_event: false };
        let topic = Topic::new("Arsenal", TopicSource::Follow, TopicMode::Always);
        let settings = Settings { shield_zap: true, ..Default::default() };
        let snap = GuardSnapshot::from_parts(settings, vec![(topic, vec!["arsenal".into()])], Default::default(), [(ids[1], vec![span])].into());
        t.services().agent.set_guard(Guard::with_snapshot(snap));
        let picture = |ch: ChannelId| {
            let mut s = PlayerSnapshot { state: PlayerState::Playing, ..Default::default() };
            s.playable = Some(Arc::new(Playable { media: MediaRef::Channel(ch), ..(*playable()).clone() }));
            s.zap = Some(crate::model::ZapList { collection: crate::model::Collection::AllChannels, channels: ids.clone().into() });
            Arc::new(s)
        };
        let (a, b) = (picture(ids[4]), picture(ids[0]));
        let view = MultiView { on: true, focus: TileId::B, ..Default::default() };
        t.multiview = MvFrame { view, snaps: [Some(a.clone()), Some(b.clone())], starting: false };
        t.snapshot = b;
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        h.state_mut().enter(Route::Player);
        h.run_steps(3);
        assert!(h.state_mut().act(Action::ZapNext));
        h.run_steps(3);
        assert_eq!((h.state().s.shield_card(), h.state().s.shield_tile()), (Some(ids[1]), Some(TileId::B)));
        // the card's heading sits in picture B, not over A
        let rect_of = |h: &H, tile| h.state().s.mv_placed().iter().find(|p| p.tile == tile).map(|p| p.video).expect("placed");
        let heading = h
            .output()
            .shapes
            .iter()
            .find_map(|c| match &c.shape {
                egui::Shape::Text(t) if t.galley.job.text.starts_with(crate::ui::widgets::spoiler::HIDDEN) => Some(t.pos),
                _ => None,
            })
            .expect("the card's heading");
        assert!(rect_of(&h, TileId::B).contains(heading) && !rect_of(&h, TileId::A).contains(heading), "{heading:?}");
        // picture A plays on: no pause reached the app's player (picture A in this rig)
        std::thread::sleep(Duration::from_millis(300));
        h.run_steps(2);
        assert!(!real(&h).paused, "the other picture isn't paused");
        // focus on picture A: Enter is A's, the card stays over B
        h.state_mut().t.multiview.view.focus = TileId::A;
        h.state_mut().t.snapshot = a;
        h.run_steps(2);
        h.state_mut().act(Action::FocusOsdControls);
        h.run_steps(2);
        assert_eq!(h.state().s.shield_card(), Some(ids[1]), "nothing tuned on B from A's keys");
    }

    /// Ctrl+B in the player: the banner shows the guarded sub-title of what's on as a bar and
    /// says "Ctrl+B show spoilers"; Ctrl+B shows it ("… hide spoilers"); another picture (a zap)
    /// hides it again.
    #[test]
    fn ctrl_b_in_the_player_shows_the_banners_spoilers_until_the_picture_changes() {
        use std::collections::HashMap;

        use crate::guard::Guard;
        use crate::spoiler::snapshot::{GuardSnapshot, SeriesMark, Settings};
        use crate::ui::screens::home::testkit::{channels, epg, painted, seed, source};
        crate::spoiler::ui::hide_all();
        let (mut t, one) = rig();
        let now = t.now;
        let ids = seed(&mut t, move |c| {
            let s = source(c, "TV")?;
            let ids = channels(c, s, 3)?;
            epg(c, &ids, now - now.rem_euclid(1800) - 1800, 3, true)?;
            // channel 0 airs a later episode of a series you're part-way through
            c.execute(
                "UPDATE programme SET title = 'Severance', sub_title = 'Cold Harbor'
                 WHERE epg_channel_id = (SELECT epg_channel_id FROM channel_epg WHERE channel_id = ?1) AND start <= ?2 AND stop > ?2",
                rusqlite::params![ids[0].0, now],
            )?;
            Ok(ids)
        });
        let mark = SeriesMark { label: "Severance".into(), next_up: (2, 4), automatic: true, ..Default::default() };
        let snap = GuardSnapshot::from_parts(Settings::default(), vec![], HashMap::from([("severance".to_owned(), mark)]), Default::default());
        t.services().agent.set_guard(Guard::with_snapshot(snap));
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        let watch = |h: &mut H, ch: ChannelId| {
            let mut s = (*real(h)).clone();
            s.state = PlayerState::Playing;
            s.playable = Some(Arc::new(Playable { media: MediaRef::Channel(ch), ..(*playable()).clone() }));
            h.state_mut().t.snapshot = Arc::new(s);
        };
        watch(&mut h, ids[0]);
        h.state_mut().enter(Route::Player);
        h.run_steps(3);
        let key = crate::spoiler::ui::reveal_key_label();
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t.starts_with("Severance · ▆")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Cold Harbor")), "{texts:?}");
        assert!(texts.contains(&format!("{key} show spoilers")), "{texts:?}");
        assert!(h.state_mut().act(Action::RevealSpoilers));
        h.run_steps(2);
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t == "Severance · Cold Harbor"), "{texts:?}");
        assert!(texts.contains(&format!("{key} hide spoilers")), "{texts:?}");
        // another picture: the reveal goes, and stays gone back on channel 0
        watch(&mut h, ids[1]);
        h.run_steps(2);
        assert!(crate::spoiler::ui::reveals().is_empty());
        watch(&mut h, ids[0]);
        h.run_steps(2);
        assert!(!painted(&h).iter().any(|t| t.contains("Cold Harbor")));
        // Ctrl+B again shows it; Ctrl+B once more hides it
        assert!(h.state_mut().act(Action::RevealSpoilers));
        assert!(!crate::spoiler::ui::reveals().is_empty());
        assert!(h.state_mut().act(Action::RevealSpoilers));
        assert!(crate::spoiler::ui::reveals().is_empty());
    }

    /// The banner's "Title · sub-title" of an unrelated sport programme while a topic with
    /// "sport descriptions" is active: the sub-title is a bar (the guide's categories are read
    /// for the banner — the slim now row has none).
    #[test]
    fn the_banner_hides_an_unrelated_sport_sub_title_under_sport_descriptions() {
        use std::collections::HashMap;

        use crate::guard::Guard;
        use crate::spoiler::snapshot::{GuardSnapshot, Settings, Topic, TopicMode, TopicSource};
        use crate::ui::screens::home::testkit::{channels, epg, painted, seed, source};
        crate::spoiler::ui::hide_all();
        let (mut t, one) = rig();
        let now = t.now;
        let ids = seed(&mut t, move |c| {
            let s = source(c, "TV")?;
            let ids = channels(c, s, 2)?;
            epg(c, &ids, now - now.rem_euclid(1800) - 1800, 3, true)?;
            // channel 0 airs another match (a sport category; nothing names the topic)
            c.execute(
                "UPDATE programme SET title = 'Premier League', sub_title = 'Spurs 3-1 Leeds', categories = 'Sport\u{1f}Football'
                 WHERE epg_channel_id = (SELECT epg_channel_id FROM channel_epg WHERE channel_id = ?1) AND start <= ?2 AND stop > ?2",
                rusqlite::params![ids[0].0, now],
            )?;
            Ok(ids)
        });
        let mut topic = Topic::new("Arsenal", TopicSource::Follow, TopicMode::Always);
        topic.sport_descr = true;
        let snap = GuardSnapshot::from_parts(Settings::default(), vec![(topic, vec!["arsenal".into()])], HashMap::new(), Default::default());
        t.services().agent.set_guard(Guard::with_snapshot(snap));
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        let mut s = (*real(&h)).clone();
        s.state = PlayerState::Playing;
        s.playable = Some(Arc::new(Playable { media: MediaRef::Channel(ids[0]), ..(*playable()).clone() }));
        h.state_mut().t.snapshot = Arc::new(s);
        h.state_mut().enter(Route::Player);
        until(&mut h, "the banner's now line", |h| painted(h).iter().any(|t| t.starts_with("Premier League · ")));
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t.starts_with("Premier League · ▆")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Spurs 3-1 Leeds")), "{texts:?}");
        assert!(texts.contains(&format!("{} show spoilers", crate::spoiler::ui::reveal_key_label())), "{texts:?}");
    }

    /// Popped out: the strip under the pointer has the way back to the full window, and so does
    /// the card shown when nothing plays.
    #[test]
    fn the_pop_out_strip_and_idle_card_lead_back_to_the_full_window() {
        let (mut t, one) = rig();
        t.video_only = true;
        let size = egui::vec2(640.0, 360.0);
        let mut h = harness(t, one, PlayerScreen::default(), size);
        sync(&mut h);
        h.state_mut().enter(Route::Player);
        h.hover_at(egui::pos2(size.x / 2.0, size.y / 3.0));
        h.run_steps(3);
        assert!(has_label(&h, "Volume") && has_label(&h, "Stop and close"), "the strip's buttons");
        h.state_mut().nav();
        h.get_by_label("Back to the full window").click();
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::TogglePopOut]);
        h.state_mut().t.snapshot = Arc::new(PlayerSnapshot::default());
        h.run_steps(2);
        h.get_by_label("Back to the full window").click();
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::TogglePopOut]);
    }

    /// Below 700 pt: the speaker alone (it mutes, the wheel over it steps), and zapping stays.
    #[test]
    fn a_narrow_osd_has_the_speaker_but_no_slider() {
        let h = player(egui::vec2(600.0, 400.0));
        assert!(has_label(&h, "Mute"));
        assert!(!has_label(&h, "Volume"));
        assert!(has_label(&h, "Previous channel") && has_label(&h, "Next channel"));
    }

    /// A click on a control does what the control does but leaves the key context alone, so
    /// ↑/↓ keep zapping afterwards (it used to enter controls focus mode).
    #[test]
    fn a_click_on_a_control_acts_without_entering_controls_focus_mode() {
        let mut h = player(WIDE);
        let paused = real(&h).paused;
        h.get_by_label("Play / pause").click();
        until(&mut h, "pause toggled", |h| {
            sync(h);
            shown(h).paused != paused
        });
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PlayerLive]);
    }

    /// Back (Alt+←, the mouse back button) in controls focus mode leaves it like Esc, then hides
    /// the banner, then lets the app leave the player (it used to show and hide the banner
    /// forever).
    #[test]
    fn back_leaves_controls_focus_mode_then_hides_the_banner_then_leaves() {
        let mut h = player(WIDE);
        assert!(h.state_mut().act(Action::FocusOsdControls));
        assert!(h.state().s.key_contexts().contains(&KeyContext::PlayerOsd));
        assert!(h.state_mut().act(Action::Back));
        assert_eq!(h.state().s.key_contexts(), vec![KeyContext::PlayerLive], "focus mode left");
        assert!(h.state().s.banner_shown(&real(&h)), "the banner stays");
        assert!(h.state_mut().act(Action::Back), "hides the banner");
        assert!(!h.state_mut().act(Action::Back), "the app leaves the player");
    }

    /// The top bar's ← leaves the player like `p`.
    #[test]
    fn the_top_bar_back_button_leaves_the_player() {
        let mut h = player(WIDE);
        h.state_mut().nav();
        h.get_by_label("Leave the player").click();
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Back]);
    }

    #[test]
    fn error_card_copy_text_copies_redacted_details() {
        use crate::ui::testing::clicks;
        let (mut t, one) = rig();
        let message = "Failed: http://example.com/live/alice/secret/123.ts";
        t.snapshot = Arc::new(PlayerSnapshot {
            state: PlayerState::Error(crate::player::PlayerError { kind: crate::player::PlayerErrorKind::Network, message: message.into() }),
            ..Default::default()
        });
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        clicks::click_text(&mut h, &format!("{}  Copy error", glyphs::COPY));
        let copied = clicks::copied_text(&h).expect("clipboard command");
        assert_eq!(copied, format!("Network error\n{}", crate::redact::redact(message)));
        assert!(!copied.contains("alice") && !copied.contains("secret"));
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some("Copied error"));
        assert!(h.state_mut().nav().is_empty());
    }

    /// Right-click on the video opens the player menu; "Pop out" from it pops out (the app
    /// hands that action to the player, which must ask for it).
    #[test]
    fn right_click_opens_the_player_menu_and_its_pop_out_pops_out() {
        let mut h = player(WIDE);
        h.state_mut().nav();
        let c = egui::pos2(WIDE.x / 3.0, WIDE.y / 2.0);
        h.hover_at(c);
        press(&h, c, egui::PointerButton::Secondary);
        h.run_steps(2);
        let nav = h.state_mut().nav();
        let Some(NavRequest::Open(OverlayRequest::ContextMenu(spec))) = nav.last() else { panic!("no menu: {nav:?}") };
        let labels: Vec<&str> = spec.items.iter().map(|i| i.label.as_str()).collect();
        assert!(labels.contains(&"Pop out") && labels.contains(&"Stop and close"), "{labels:?}");
        h.state_mut().result(OverlayResult::Chosen(Action::TogglePopOut));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::TogglePopOut]);
    }

    /// ← closes a side panel while it filters too: `[ListFiltering, PlayerLive]` resolves ← to
    /// the list's `Left` (not the player's last channel), which steps out like Esc.
    #[test]
    fn left_closes_a_filtered_side_panel() {
        let mut h = player(WIDE);
        assert!(h.state_mut().act(Action::ChannelList));
        h.run_steps(2);
        assert!(h.state_mut().act(Action::FilterInput('x')));
        let stack = h.state().s.key_contexts();
        assert_eq!(stack[0], KeyContext::ListFiltering, "{stack:?}");
        let left = egui::Event::Key { key: egui::Key::ArrowLeft, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        assert_eq!(crate::keymap::resolve(&stack, &left), Some(Action::Left));
        assert!(h.state_mut().act(Action::Left));
        assert!(h.state().s.panel.is_none());
    }

    /// A side panel closes with its ✕ and with a click on the video beside it.
    #[test]
    fn a_side_panel_closes_with_its_close_button_and_a_click_on_the_video() {
        let mut h = player(WIDE);
        let open = |h: &mut H| {
            assert!(h.state_mut().act(Action::ChannelList));
            h.run_steps(2);
            assert!(h.state().s.panel.is_some());
        };
        open(&mut h);
        h.get_by_label("Close").click();
        h.run_steps(2);
        assert!(h.state().s.panel.is_none(), "✕");
        open(&mut h);
        let c = egui::pos2(WIDE.x / 3.0, WIDE.y / 2.0);
        h.hover_at(c);
        press(&h, c, egui::PointerButton::Primary);
        h.run_steps(2);
        assert!(h.state().s.panel.is_none(), "click on the video");
    }

    /// A movie that failed mid-film and is also in another playlist: the error card offers
    /// "Play from TV 2 at 41:20" (DESIGN §29.15), and Ctrl+O plays that copy from there.
    #[test]
    fn the_error_card_offers_the_titles_other_copy() {
        use crate::ui::screens::home::testkit::{movies, painted, seed, source};
        let (mut t, one) = rig();
        let (first, other) = seed(&mut t, |c| {
            let (a, b) = (source(c, "TV")?, source(c, "TV 2")?);
            let (ma, mb) = (movies(c, a, 2)?, movies(c, b, 2)?);
            crate::store::titles::refresh_all(c)?;
            Ok((ma[0], mb[0]))
        });
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        let mut s = (*real(&h)).clone();
        s.state = PlayerState::Error(crate::player::PlayerError { kind: crate::player::PlayerErrorKind::Network, message: "lost".into() });
        s.playable =
            Some(Arc::new(Playable { media: MediaRef::Movie(first), kind: PlayableKind::Movie, title: "Movie 0".into(), ..(*playable()).clone() }));
        s.last_position = Some(2480.0);
        h.state_mut().t.snapshot = Arc::new(s);
        h.state_mut().enter(Route::Player);
        h.run_steps(3);
        assert!(painted(&h).iter().any(|t| t.ends_with("Play from TV 2 at 41:20")), "{:?}", painted(&h));
        assert!(has_label(&h, "Play from another copy"));
        h.state_mut().act(Action::PlayFrom);
        until(&mut h, "the other copy", |h| real(h).playable.as_ref().is_some_and(|p| p.media == MediaRef::Movie(other)));
        assert_eq!(real(&h).playable.as_ref().and_then(|p| p.start_at), Some(2480.0), "from where it stopped");
    }

    /// Ctrl+O while a combined movie plays (no error): "Play from" lists the title's other
    /// copies from where playback is ("Play from TV 2 at 10:00"); picking one plays that copy
    /// there. A title with one copy says so instead of opening an empty menu.
    #[test]
    fn ctrl_o_during_playback_switches_to_another_copy_where_playback_is() {
        use crate::ui::screens::home::testkit::{movies, seed, source};
        let (mut t, one) = rig();
        let (first, other, alone) = seed(&mut t, |c| {
            let (a, b) = (source(c, "TV")?, source(c, "TV 2")?);
            let (ma, mb) = (movies(c, a, 3)?, movies(c, b, 2)?);
            crate::store::titles::refresh_all(c)?;
            // "Movie 2" is only in TV
            Ok((ma[0], mb[0], ma[2]))
        });
        let mut h = harness(t, one, PlayerScreen::default(), WIDE);
        let playing = |h: &mut H, media: MediaRef, at: f64| {
            let mut s = (*real(h)).clone();
            s.state = PlayerState::Playing;
            s.playable = Some(Arc::new(Playable { media, kind: PlayableKind::Movie, title: "Movie".into(), ..(*playable()).clone() }));
            s.position = Some(at);
            h.state_mut().t.snapshot = Arc::new(s);
        };
        playing(&mut h, MediaRef::Movie(first), 600.0);
        h.state_mut().enter(Route::Player);
        h.run_steps(2);
        h.state_mut().nav();
        assert!(h.state_mut().act(Action::PlayFrom));
        let nav = h.state_mut().nav();
        let Some(NavRequest::Open(OverlayRequest::ContextMenu(menu))) = nav.last() else { panic!("no menu: {nav:?}") };
        assert_eq!(menu.items.iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), vec!["Play from TV 2 at 10:00"]);
        let pick = menu.items[0].action;
        h.state_mut().result(OverlayResult::Chosen(pick));
        until(&mut h, "the other copy", |h| real(h).playable.as_ref().is_some_and(|p| p.media == MediaRef::Movie(other)));
        assert_eq!(real(&h).playable.as_ref().and_then(|p| p.start_at), Some(600.0), "from where playback was");
        // a title with one copy: a toast, no menu
        playing(&mut h, MediaRef::Movie(alone), 30.0);
        h.state_mut().t.toasts.clear();
        assert!(h.state_mut().act(Action::PlayFrom));
        assert!(!h.state_mut().nav().iter().any(|n| matches!(n, NavRequest::Open(_))));
        assert!(h.state().t.toasts.iter().any(|t| t.text == "This title has no other copy"), "{:?}", h.state().t.toasts);
    }

    /// Field fix: a live channel that played keeps reconnecting until stopped, so the overlay
    /// reads "attempt n" (never "(12/0)") and offers an obvious Stop that stops and leaves.
    #[test]
    fn an_endless_reconnect_shows_the_attempt_and_a_stop_button() {
        use crate::ui::screens::home::testkit::painted;
        let mut h = player(WIDE);
        let mut s = (*h.state().t.snapshot).clone();
        s.state = PlayerState::Reconnecting {
            attempt: 12,
            max: 0,
            at: std::time::Instant::now() + std::time::Duration::from_secs(20),
            reason: crate::player::PlayerErrorKind::ProviderError,
        };
        h.state_mut().t.snapshot = Arc::new(s);
        h.run_steps(3);
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t == "Reconnecting… attempt 12"), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("/0)")), "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("The provider can't deliver this channel")), "{texts:?}");
        h.state_mut().nav();
        h.get_by_label("Stop").click();
        h.run_steps(2);
        assert!(h.state_mut().nav().contains(&NavRequest::Back), "Stop leaves the player");
        until(&mut h, "stopped", |h| real(h).state == PlayerState::Idle);
    }

    // ---------------------------------------------------------------- casting (DESIGN §40)

    use crate::model::{CastErrorKind, CastLevel, CastPhase, CastView, DeviceId, ReceiverKind};

    const TV: DeviceId = DeviceId::from_bytes([1; 16]);

    /// The player while casting live TV to "Living Room" in `state`.
    fn casting(state: PlayerState) -> H {
        let mut h = player(WIDE);
        let view = CastView {
            device: TV,
            name: "Living Room".into(),
            kind: ReceiverKind::AppleTv,
            phase: CastPhase::Playing,
            level: Some(CastLevel::Repackaged),
            as_is: false,
            behind_live_s: Some(15),
            can_seek: false,
            volume: false,
        };
        let mut s = (*real(&h)).clone();
        s.state = state;
        s.playable = Some(playable());
        s.cast = Some(view);
        h.state_mut().t.snapshot = Arc::new(s);
        let mut cfg = (*h.state().t.config).clone();
        cfg.interface.osd_timeout_ms = 60_000;
        h.state_mut().t.config = Arc::new(cfg);
        h.run_steps(3);
        h
    }

    /// The card stands in for the picture ("Playing on Living Room", Live · about 15 s behind),
    /// with Stop casting and Play here; the OSD has the Cast control with the TV's name, no
    /// audio, subtitles or pop-out, and the speaker reads "TV remote".
    #[test]
    fn casting_shows_the_card_and_trims_the_osd() {
        use crate::ui::screens::home::testkit::painted;
        let h = casting(PlayerState::Playing);
        let texts = painted(&h);
        assert!(texts.iter().any(|t| t.ends_with("Playing on Living Room")), "{texts:?}");
        assert!(texts.iter().any(|t| t == "Live · about 15 s behind"), "{texts:?}");
        assert!(has_label(&h, "Stop casting") && has_label(&h, "Play here"));
        assert!(has_label(&h, "Cast to a TV") && texts.contains(&format!("{}  Living Room", glyphs::CAST_CONNECTED)), "{texts:?}");
        for gone in ["Audio", "Subtitles", "Pop out", "Mute", "Volume"] {
            assert!(!has_label(&h, gone), "{gone} stays hidden while casting");
        }
        assert!(has_label(&h, "Volume: use the TV remote") && has_label(&h, "Info") && has_label(&h, "Fullscreen"));
    }

    /// Without a cast the control is only there while casting is on — built in and not
    /// switched off in Settings ▸ Cast (`C` is inert otherwise: no picker without the service
    /// behind it) — and it never offers a TV to multi-view.
    #[test]
    fn the_cast_control_and_key_wait_for_the_feature() {
        let mut h = player(WIDE);
        assert_eq!(has_label(&h, "Cast to a TV"), crate::cast::ENABLED);
        h.state_mut().nav();
        assert_eq!(h.state_mut().act(Action::CastMenu), crate::cast::ENABLED);
        assert_eq!(h.state_mut().nav().contains(&NavRequest::Open(OverlayRequest::CastPicker)), crate::cast::ENABLED);

        let mut off = (*h.state().t.config).clone();
        off.cast.enabled = false;
        h.state_mut().t.config = Arc::new(off);
        h.run_steps(3);
        assert!(!has_label(&h, "Cast to a TV"), "switched off in Settings");
        h.state_mut().act(Action::CastMenu);
        assert!(!h.state_mut().nav().contains(&NavRequest::Open(OverlayRequest::CastPicker)));
    }

    /// While casting `C` opens the picker; the keys that do nothing on a TV say so in the
    /// banner instead of reaching mpv.
    #[test]
    fn casting_keys_open_the_picker_and_explain_themselves() {
        use crate::ui::screens::home::testkit::painted;
        let mut h = casting(PlayerState::Playing);
        h.state_mut().nav();
        assert!(h.state_mut().act(Action::CastMenu));
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastPicker)]);
        assert!(h.state_mut().act(Action::VolumeUp));
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "Use the TV remote for volume"));
        assert!(h.state_mut().act(Action::CycleAspect));
        h.run_steps(2);
        assert!(painted(&h).iter().any(|t| t == "Not available while casting"));
    }

    /// The player menu while casting: Stop casting, Play here and Convert this channel come
    /// with "Cast to a TV…"; what the TV does itself is left out.
    #[test]
    fn the_player_menu_while_casting_has_the_casting_items() {
        let mut h = casting(PlayerState::Playing);
        h.state_mut().nav();
        let c = egui::pos2(WIDE.x / 3.0, WIDE.y / 2.0);
        h.hover_at(c);
        press(&h, c, egui::PointerButton::Secondary);
        h.run_steps(2);
        let nav = h.state_mut().nav();
        let Some(NavRequest::Open(OverlayRequest::ContextMenu(spec))) = nav.last() else { panic!("no menu: {nav:?}") };
        let labels: Vec<&str> = spec.items.iter().map(|i| i.label.as_str()).collect();
        for want in ["Cast to a TV…", "Stop casting", "Play here", "Convert this channel", "Stream info"] {
            assert!(labels.contains(&want), "{want}: {labels:?}");
        }
        for gone in ["Aspect ratio", "Zoom", "Audio…", "Subtitles…", "Pop out"] {
            assert!(!labels.contains(&gone), "{gone}: {labels:?}");
        }
    }

    /// L3 forced by a remembered choice or failure: the menu's last casting item is **Send as
    /// is**, which asks the service to forget it (DESIGN §40 §8.3); converted for the format,
    /// Convert this channel stays, greyed.
    #[test]
    fn a_remembered_conversion_offers_send_as_is() {
        let menu = |as_is: bool| {
            let mut h = casting(PlayerState::Playing);
            let mut s = (*h.state().t.snapshot).clone();
            let view = s.cast.as_mut().unwrap();
            (view.level, view.as_is) = (Some(CastLevel::Converted), as_is);
            h.state_mut().t.snapshot = Arc::new(s);
            h.run_steps(2);
            h.state_mut().nav();
            let c = egui::pos2(WIDE.x / 3.0, WIDE.y / 2.0);
            h.hover_at(c);
            press(&h, c, egui::PointerButton::Secondary);
            h.run_steps(2);
            let nav = h.state_mut().nav();
            let Some(NavRequest::Open(OverlayRequest::ContextMenu(spec))) = nav.last() else { panic!("no menu: {nav:?}") };
            spec.items.iter().map(|i| (i.label.clone(), i.enabled)).collect::<Vec<_>>()
        };
        let items = menu(true);
        assert!(items.contains(&("Send as is".into(), true)) && !items.iter().any(|i| i.0 == "Convert this channel"), "{items:?}");
        let items = menu(false);
        assert!(items.contains(&("Convert this channel".into(), false)) && !items.iter().any(|i| i.0 == "Send as is"), "{items:?}");
    }

    /// A failure reads as one sentence with its own buttons; the buttons open what they say.
    #[test]
    fn a_cast_failure_shows_its_sentence_and_buttons() {
        use crate::ui::screens::home::testkit::painted;
        let failed = |k| PlayerState::Error(crate::player::PlayerError { kind: crate::player::PlayerErrorKind::Cast(k), message: String::new() });
        let mut h = casting(failed(CastErrorKind::Firewall));
        assert!(painted(&h).iter().any(|t| t == "Living Room couldn't connect back to Tuner. Your firewall is blocking it."), "{:?}", painted(&h));
        for b in ["Allow on firewall", "Show command", "Try again", "Play here"] {
            assert!(has_label(&h, b), "{b}");
        }
        assert!(!has_label(&h, "Stop casting"));
        h.state_mut().nav();
        h.get_by_label("Show command").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastFirewall)]);
        drop(h);
        let mut h = casting(failed(CastErrorKind::PairingLost));
        assert!(painted(&h).iter().any(|t| t == "Living Room forgot Tuner. This can happen after a TV update."));
        h.state_mut().nav();
        h.get_by_label("Pair").click();
        h.run_steps(2);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastPair { device: TV, then_cast: true })]);
    }
    #[test]
    fn watchlist_ctrl_f_keeps_catchup_ended_and_failed_channels_in_favourites() {
        use crate::ui::screens::home::testkit::{channels, seed, source};
        for state in [
            PlayerState::Playing,
            PlayerState::Ended,
            PlayerState::Error(crate::player::PlayerError { kind: crate::player::PlayerErrorKind::Network, message: "failed".into() }),
        ] {
            let (mut t, one) = rig();
            let (id, item) = seed(&mut t, |c| {
                let src = source(c, "TV")?;
                let id = channels(c, src, 1)?[0];
                let channel = crate::store::live::channel(c, id)?;
                Ok((id, crate::model::ItemRef { source: src, kind: crate::model::ItemKind::Channel, key: channel.key }))
            });
            let mut h = harness(t, one, PlayerScreen::default(), WIDE);
            let mut pl = (*playable()).clone();
            pl.item = Some(item.clone());
            pl.media = MediaRef::Catchup { channel: id, start: 100, stop: 200, catchup_id: None };
            pl.kind = PlayableKind::Catchup { start: 100, stop: 200 };
            h.state_mut().t.snapshot = Arc::new(PlayerSnapshot { state, playable: Some(Arc::new(pl)), ..Default::default() });
            h.state_mut().act(Action::ToggleFavourite);
            until(&mut h, "channel saved", |h| h.state().t.db.query_row("SELECT count(*) FROM fav_item", [], |r| r.get::<_, i64>(0)).unwrap() == 1);
            assert_eq!(h.state().t.db.query_row("SELECT count(*) FROM watchlist_item", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        }
    }
}
