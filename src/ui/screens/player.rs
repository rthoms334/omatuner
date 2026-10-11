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
//!   `[List|ListFiltering, PlayerLive]`; ↑/↓ move the cursor (no zap), Enter tunes, ←/Esc close.
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
//! Owner: WP-11.
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
const NEXT_EPISODE_DELAY: Duration = Duration::from_secs(10);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PanelKind {
    Channels,
    MiniGuide,
    History,
}

struct SidePanel {
    kind: PanelKind,
    list: ListState,
    items: Vec<ChannelId>,
    haystacks: Vec<String>,
    rows: Vec<u32>,
}

impl SidePanel {
    fn refilter(&mut self) {
        self.rows = self.list.filter.apply(self.haystacks.iter().map(String::as_str));
        self.list.set_len(self.rows.len());
    }
    fn cursor_item(&self) -> Option<ChannelId> {
        self.rows.get(self.list.cursor).and_then(|&i| self.items.get(i as usize)).copied()
    }
}

/// OSD controls (controls focus mode moves between them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Control {
    Prev,
    PlayPause,
    Next,
    SkipBack,
    SkipForward,
    Audio,
    Subtitles,
    Channels,
    Guide,
    History,
    Info,
    Fullscreen,
    PopOut,
}

impl Control {
    fn glyph(self, paused: bool) -> &'static str {
        match self {
            Control::Prev => glyphs::PREVIOUS,
            Control::PlayPause => {
                if paused {
                    glyphs::PLAY
                } else {
                    glyphs::PAUSE
                }
            }
            Control::Next => glyphs::NEXT,
            Control::SkipBack => glyphs::SKIP_BACK,
            Control::SkipForward => glyphs::SKIP_FORWARD,
            Control::Audio => glyphs::AUDIO_TRACK,
            Control::Subtitles => glyphs::SUBTITLES,
            Control::Channels => glyphs::PLAYLIST,
            Control::Guide => glyphs::GUIDE,
            Control::History => glyphs::HISTORY,
            Control::Info => glyphs::INFO,
            Control::Fullscreen => glyphs::FULLSCREEN,
            Control::PopOut => glyphs::POP_OUT,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Control::Prev => "Previous channel",
            Control::PlayPause => "Play / pause",
            Control::Next => "Next channel",
            Control::SkipBack => "Back",
            Control::SkipForward => "Forward",
            Control::Audio => "Audio",
            Control::Subtitles => "Subtitles",
            Control::Channels => "Channels",
            Control::Guide => "Guide",
            Control::History => "History",
            Control::Info => "Info",
            Control::Fullscreen => "Fullscreen",
            Control::PopOut => "Pop out",
        }
    }
    /// Drawn as a glyph only (DESIGN §13.8 wireframe); the label stays the accessible name and
    /// the hover text.
    fn glyph_only(self) -> bool {
        matches!(
            self,
            Control::Prev | Control::PlayPause | Control::Next | Control::SkipBack | Control::SkipForward | Control::Fullscreen | Control::PopOut
        )
    }
}

/// Which OSD row has the controls focus (VOD has a seek bar row above the buttons).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum OsdRow {
    Seek,
    #[default]
    Buttons,
}

/// A transient message at the top of the video (track / delay / volume changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flash {
    Audio,
    Subtitles,
    SubDelay,
    SubSize,
    Volume,
    Aspect,
    Zoom,
}

struct NextEpisode {
    id: EpisodeId,
    label: String,
    at: Instant,
}

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
    let current = playing_channel(snap).or_else(|| match snap.playable.as_ref().map(|p| &p.media) {
        Some(MediaRef::Channel(c)) => Some(*c),
        _ => None,
    })?;
    let skip = |c: ChannelId| live_idx.get(c).is_none_or(|x| x.locked || x.unsupported || failing.contains(&live::channel_ref(x)));
    LiveIndex::neighbour(&zap.channels, current, forward, &skip)
}

impl PlayerScreen {
    fn show_banner(&mut self, ctx: &UiCtx<'_>) {
        self.banner_until = Some(Instant::now() + Duration::from_millis(u64::from(ctx.config.interface.osd_timeout_ms)));
        self.banner_dismissed = false;
    }

    /// The passive banner is up (a pause keeps it up until Esc).
    fn banner_shown(&self, snap: &PlayerSnapshot) -> bool {
        (snap.paused && !self.banner_dismissed) || self.banner_until.is_some_and(|t| Instant::now() < t)
    }

    fn banner_visible(&self, snap: &PlayerSnapshot) -> bool {
        self.osd_focus || self.panel.is_some() || self.banner_shown(snap)
    }

    /// The controls focus mode walks: those drawn last frame (a prefix of `controls`).
    fn active_controls(&self, snap: &PlayerSnapshot, narrow: bool, video_only: bool) -> Vec<Control> {
        let all = Self::controls(snap, narrow, video_only);
        let drawn: Vec<Control> = all.iter().copied().filter(|c| self.visible.contains(c)).collect();
        if drawn.is_empty() { all } else { drawn }
    }

    /// Below 700 pt (× rem) the controls row collapses (DESIGN §13.8).
    fn narrow(width: f32, theme: &Theme) -> bool {
        width < 700.0 * theme.rem()
    }

    /// The controls row; narrow keeps zapping (the only mouse path to it) and play / tracks /
    /// info. "Pop out" isn't offered in the popped-out window (its strip has the way back).
    fn controls(snap: &PlayerSnapshot, narrow: bool, video_only: bool) -> Vec<Control> {
        use Control as C;
        let live = is_live(snap);
        if narrow {
            let mut v = if live { vec![C::Prev, C::PlayPause, C::Next] } else { vec![C::PlayPause] };
            v.extend([C::Audio, C::Subtitles, C::Info]);
            return v;
        }
        let mut v = if live {
            vec![C::Prev, C::PlayPause, C::Next, C::Audio, C::Subtitles, C::Channels, C::Guide, C::History, C::Info, C::Fullscreen]
        } else {
            vec![C::SkipBack, C::PlayPause, C::SkipForward, C::Audio, C::Subtitles, C::Info, C::Fullscreen]
        };
        if !video_only {
            v.push(C::PopOut);
        }
        v
    }

    /// The volume to show: what the user just set until mpv reports it, else mpv's.
    fn shown_volume(&mut self, snap: &PlayerSnapshot) -> f32 {
        let mpv = snap.volume as f32;
        match self.volume_local {
            Some((v, at)) if (v - mpv).abs() >= 0.5 && at.elapsed() < VOLUME_ECHO => v,
            _ => {
                self.volume_local = None;
                mpv
            }
        }
    }

    /// Set the volume from the slider / wheel; a muted player is unmuted first (once).
    fn set_volume(&mut self, ctx: &mut UiCtx<'_>, v: f32) {
        if ctx.player.muted && self.unmuting.is_none_or(|t| t.elapsed() >= VOLUME_ECHO) {
            ctx.play(PlayerCommand::ToggleMute);
            self.unmuting = Some(Instant::now());
        }
        ctx.play(PlayerCommand::SetVolume(f64::from(v)));
        self.volume_local = Some((v, Instant::now()));
        // the pop-out strip's slider has no level readout next to it
        if ctx.video_only {
            self.flash = Some((Flash::Volume, Instant::now()));
        }
        self.show_banner(ctx);
    }

    /// Speaker button, and with `slider` the volume slider and its level label (`label`: the
    /// label's rect). The wheel over either steps the volume.
    fn draw_volume(
        &mut self,
        ui: &egui::Ui,
        ctx: &mut UiCtx<'_>,
        p: &egui::Painter,
        speaker: Rect,
        slider: Option<(Rect, Option<Rect>)>,
        font: FontId,
        snap: &PlayerSnapshot,
    ) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        if !snap.muted {
            self.unmuting = None;
        }
        let level = self.shown_volume(snap);
        let muted = snap.muted;
        let tip = match (slider.is_some(), muted) {
            (true, false) => "Mute (m)".to_owned(),
            (true, true) => "Unmute (m)".to_owned(),
            (false, false) => format!("Volume {level:.0} % · wheel to change · click to mute (m)"),
            (false, true) => "Muted · click to unmute (m) · wheel to change".to_owned(),
        };
        let label = if muted { "Unmute" } else { "Mute" };
        let resp = osd_button(ui, p, theme, speaker, ui.id().with("osd-speaker"), volume_glyph(muted, level), font.clone(), label, Some(&tip), false);
        if resp.clicked() {
            ctx.play(PlayerCommand::ToggleMute);
            if slider.is_none() {
                self.flash = Some((Flash::Volume, Instant::now()));
            }
            self.show_banner(ctx);
        }
        if resp.hovered() {
            let notches = slider::wheel_notches(ui, resp.id);
            if notches != 0.0 {
                self.set_volume(ctx, (level + notches * VOLUME_WHEEL).clamp(0.0, VOLUME_MAX));
                if slider.is_none() {
                    self.flash = Some((Flash::Volume, Instant::now()));
                }
            }
        }
        let Some((sr, label_rect)) = slider else { return };
        let mut look = SliderLook::osd(t);
        if muted {
            look.fill = with_alpha(t.osd_text, 0.4);
            look.knob = look.fill;
        }
        let spec = SliderSpec { range: 0.0..=VOLUME_MAX, step: 1.0, marks: &[100.0], wheel_step: Some(VOLUME_WHEEL), label: "Volume", look };
        let mut v = level;
        let r = slider::slider_in(ui, sr, ui.id().with("osd-volume"), theme, &mut v, false, &spec);
        if r.changed() {
            self.set_volume(ctx, v);
        } else if muted && (r.clicked() || r.drag_started()) && self.unmuting.is_none() {
            // a click on the knob itself sets nothing new, but still means "unmute"
            ctx.play(PlayerCommand::ToggleMute);
            self.unmuting = Some(Instant::now());
            self.show_banner(ctx);
        }
        if let Some(lr) = label_rect {
            let text = if muted { "Muted".to_owned() } else { format!("{v:.0} %") };
            osd_text(p, lr.left_center(), Align2::LEFT_CENTER, &text, font, t.osd_text, f32::INFINITY);
        }
    }

    fn zap(&mut self, ctx: &mut UiCtx<'_>, forward: bool) {
        let snap = ctx.player.clone();
        // no zap list yet (startup `--play` before the library loaded): nothing to do
        if snap.zap.as_ref().is_none_or(|z| z.channels.is_empty()) {
            return;
        }
        if ctx.parental.is_unlocked(&ctx.config.parental) {
            ctx.play(PlayerCommand::Zap { forward, unlocked: true });
            return;
        }
        let failing = super::live::failing_set(ctx).unwrap_or_default();
        if let Some(target) = zap_target(&ctx.lib.live, &snap, forward, &failing) {
            ctx.play(PlayerCommand::ZapTo(target));
        }
    }

    /// Tune a channel from a panel / numeric entry (keeps the zap list); locked → PIN.
    fn tune(&mut self, ctx: &mut UiCtx<'_>, id: ChannelId, retry: Action) -> bool {
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
            ctx.play(PlayerCommand::ZapTo(id));
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

    fn open_panel(&mut self, ctx: &mut UiCtx<'_>, kind: PanelKind) {
        if self.panel.as_ref().is_some_and(|p| p.kind == kind) {
            self.panel = None;
            return;
        }
        let snap = ctx.player;
        let current = playing_channel(snap);
        let items: Vec<ChannelId> = match kind {
            PanelKind::History => ctx.lib.recent_channels.clone(),
            PanelKind::Channels | PanelKind::MiniGuide => match &snap.zap {
                // a zap list sent before parental control relocked may hold explicit channels
                // the library no longer has (DESIGN §20.3)
                Some(z) if !z.channels.is_empty() => z.channels.iter().copied().filter(|c| ctx.lib.live.get(*c).is_some()).collect(),
                _ => {
                    let g = current.and_then(|c| ctx.lib.live.get(c)).and_then(|c| c.group_ids.first().copied());
                    let coll = g.map_or(Collection::AllChannels, Collection::Group);
                    ctx.lib.live.collection(&coll, ctx.favs, &ctx.lib.recent_channels, ctx.config.interface.live_sort)
                }
            },
        };
        let haystacks = items
            .iter()
            .map(|id| {
                ctx.lib.live.get(*id).map_or_else(String::new, |c| match c.number {
                    Some(n) => format!("{n} {}", c.name),
                    None => c.name.to_string(),
                })
            })
            .collect();
        let mut p = SidePanel { kind, list: ListState::default(), items, haystacks, rows: Vec::new() };
        p.refilter();
        if let Some(i) = current.and_then(|c| p.rows.iter().position(|&r| p.items[r as usize] == c)) {
            p.list.select(i);
        }
        self.panel = Some(p);
    }

    fn panel_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        let p = self.panel.as_mut()?;
        match p.list.handle(action) {
            ListOutcome::Moved | ListOutcome::ContextMenu(_) => Some(true),
            ListOutcome::Activated(_) | ListOutcome::ActivatedAlt(_) => {
                if let Some(id) = p.cursor_item()
                    && self.tune(ctx, id, Action::Activate)
                {
                    self.panel = None;
                    self.show_banner(ctx);
                }
                Some(true)
            }
            ListOutcome::Parent => {
                self.panel = None;
                Some(true)
            }
            ListOutcome::FilterChanged => {
                p.refilter();
                Some(true)
            }
            ListOutcome::None => match action {
                Action::Back => {
                    self.panel = None;
                    Some(true)
                }
                Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::Activate => Some(true),
                Action::FilterInput(_) | Action::FilterBackspace | Action::FilterClear | Action::FilterDeleteWord => Some(true),
                // everything else (Space, volume…) reaches the player context below
                _ => None,
            },
        }
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

    fn activate_control(&mut self, ctx: &mut UiCtx<'_>, c: Control) {
        let cfg = &ctx.config.player;
        match c {
            Control::Prev => self.zap(ctx, false),
            Control::Next => self.zap(ctx, true),
            Control::PlayPause => ctx.play(PlayerCommand::TogglePause),
            Control::SkipBack => ctx.play(PlayerCommand::SeekBy(-f64::from(cfg.skip_small_s))),
            Control::SkipForward => ctx.play(PlayerCommand::SeekBy(f64::from(cfg.skip_small_s))),
            Control::Audio => ctx.open(OverlayRequest::Tracks { audio: true }),
            Control::Subtitles => ctx.open(OverlayRequest::Tracks { audio: false }),
            Control::Channels => self.open_panel(ctx, PanelKind::Channels),
            Control::Guide => self.open_panel(ctx, PanelKind::MiniGuide),
            Control::History => self.open_panel(ctx, PanelKind::History),
            Control::Info => self.toggle_info(ctx),
            Control::Fullscreen => self.toggle_fullscreen = true,
            Control::PopOut => ctx.navigate(NavRequest::TogglePopOut),
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
        let mut items = Vec::new();
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
        items.push(menu_item(glyphs::ASPECT, "Aspect ratio", Action::CycleAspect, true, k));
        items.push(menu_item(glyphs::DISPLAY, "Zoom", Action::CycleZoom, true, k));
        items.push(menu_item(glyphs::AUDIO_TRACK, "Audio…", Action::AudioMenu, true, k));
        items.push(menu_item(glyphs::SUBTITLES, "Subtitles…", Action::SubtitleMenu, true, k));
        items.push(menu_item(glyphs::INFO, "Stream info", Action::StreamInfo, true, k));
        if matches!(snap.playable.as_ref().map(|p| &p.kind), Some(PlayableKind::Catchup { .. })) {
            items.push(menu_item(glyphs::LIVE_DOT, "Jump to live", Action::JumpToLive, true, KeyContext::PlayerVod));
        }
        if ctx.video_only {
            items.push(menu_item(glyphs::POP_IN, "Back to the full window", Action::TogglePopOut, true, KeyContext::PopOut));
        } else {
            items.push(menu_item(glyphs::POP_OUT, "Pop out", Action::TogglePopOut, true, k));
        }
        items.push(menu_item(glyphs::MINI_PLAYER, "Keep playing in the mini player", Action::Minimize, true, k));
        items.push(menu_item(glyphs::STOP, "Stop and close", Action::StopAndClose, true, k));
        let title = snap.playable.as_ref().map(|p| p.title.clone()).unwrap_or_else(|| "Player".into());
        ctx.open(OverlayRequest::ContextMenu(ContextMenuSpec { title, items }));
    }

    fn toggle_info(&mut self, ctx: &UiCtx<'_>) {
        self.info = !self.info;
        if self.info {
            ctx.play(PlayerCommand::RefreshStreamInfo);
            self.info_refreshed = Some(Instant::now());
        }
    }

    /// Controls focus mode (`PlayerOsd`). Back (Alt+←, the mouse back button) leaves it like
    /// Esc; only what it handles shows the banner (an unhandled Back used to show it and then
    /// hide it again, forever).
    fn osd_action(&mut self, ctx: &mut UiCtx<'_>, action: Action, narrow: bool) -> Option<bool> {
        if !self.osd_focus {
            return None;
        }
        let snap = ctx.player.clone();
        let controls = self.active_controls(&snap, narrow, ctx.video_only);
        let n = controls.len();
        let seekable = !is_live(&snap) && snap.seekable;
        match action {
            Action::LeaveOsdControls | Action::Back => self.osd_focus = false,
            Action::Left | Action::Right if self.row == OsdRow::Seek && seekable => {
                let s = f64::from(ctx.config.player.skip_small_s);
                ctx.play(PlayerCommand::SeekBy(if action == Action::Left { -s } else { s }));
            }
            Action::Left | Action::FocusPrev => self.control = (self.control + n - 1) % n,
            Action::Right | Action::FocusNext => self.control = (self.control + 1) % n,
            Action::Up if seekable => self.row = OsdRow::Seek,
            Action::Down => self.row = OsdRow::Buttons,
            Action::Up => {}
            Action::Activate => {
                if self.row == OsdRow::Seek {
                    ctx.play(PlayerCommand::TogglePause);
                } else if let Some(c) = controls.get(self.control.min(n - 1)) {
                    self.activate_control(ctx, *c);
                }
            }
            _ => return None,
        }
        self.show_banner(ctx);
        Some(true)
    }

    fn end_of_playback(&mut self, ctx: &mut UiCtx<'_>) {
        // a finished movie goes back to its page from `movie_detail` (one owner for that
        // navigation); an episode gets the up-next card here, over the video
        if let Some(MediaRef::Episode(id)) = ctx.player.playable.as_ref().map(|p| &p.media) {
            self.next_lookup = Some(*id);
            self.find_next_episode(ctx);
        }
    }

    /// The episode after the one that ended (two indexed reads, inside the frame budget;
    /// retried next frame when the budget is spent).
    fn find_next_episode(&mut self, ctx: &mut UiCtx<'_>) {
        let Some(id) = self.next_lookup else { return };
        let db = ctx.db;
        let Some(next) = ctx.budget.run(|| {
            crate::store::vod::episode(db, id).and_then(|e| {
                let eps = crate::store::vod::episodes(db, e.series_id, None)?;
                Ok(eps.iter().position(|x| x.id == id).and_then(|i| eps.get(i + 1).cloned()))
            })
        }) else {
            return;
        };
        self.next_lookup = None;
        match next {
            Ok(Some(e)) => {
                self.next_episode = Some(NextEpisode {
                    id: e.id,
                    label: format!("S{:02}E{:02} · {}", e.season, e.episode, crate::ui::widgets::rows::episode_title(&e)),
                    at: Instant::now() + NEXT_EPISODE_DELAY,
                })
            }
            Ok(None) => {}
            Err(err) => tracing::debug!("next episode: {err}"),
        }
    }

    fn play_next_episode(&mut self, ctx: &mut UiCtx<'_>) {
        if let Some(n) = self.next_episode.take() {
            ctx.play(PlayerCommand::Play(PlayRequest {
                media: MediaRef::Episode(n.id),
                zap: None,
                origin: PlayOrigin::Fullscreen,
                resume: true,
                immediate: true,
            }));
            self.show_banner(ctx);
        }
    }

    // ------------------------------------------------------------------ drawing

    fn draw_osd(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let narrow = Self::narrow(rect.width(), theme);
        let pad = (theme.space(SpaceToken::PanelPadding) * s).round();
        let title_font = FontId::proportional((theme.font(TypeToken::Display) * s).round());
        let body_font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small_font = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let badge_font = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let p = ui.painter_at(rect);
        let playable = snap.playable.clone();
        let h24 = ctx.config.interface.clock_24h;
        let live = is_live(snap);

        // ---- top bar: ← · logo · number · title ……… clock (· volume when the bottom bar has
        // no level readout)
        let top_h = title_font.size + 2.0 * pad;
        let top = Rect::from_min_size(rect.min, egui::vec2(rect.width(), top_h));
        p.rect_filled(top, 0.0, t.osd_scrim);
        self.bars = vec![top];
        let back_side = top_h - pad;
        let back = Rect::from_min_size(egui::pos2(top.left() + pad / 2.0, top.center().y - back_side / 2.0), egui::vec2(back_side, back_side));
        let back_tip = if ctx.video_only {
            "Back to the full window · keeps playing in the mini player (p)"
        } else {
            "Back · keeps playing in the mini player (p)"
        };
        if osd_button(ui, &p, theme, back, ui.id().with("osd-back"), glyphs::ARROW_LEFT, body_font.clone(), "Leave the player", Some(back_tip), false)
            .clicked()
        {
            Self::leave(ctx);
        }
        let mut x = back.right() + pad / 2.0;
        let logo_size = (title_font.size * 1.2).round();
        if let Some(pl) = &playable {
            let lr = Rect::from_min_size(egui::pos2(x, top.center().y - logo_size / 2.0), egui::vec2(logo_size, logo_size));
            osd_logo(&p, theme, ctx.images, pl.logo.as_deref(), &pl.title, lr);
            x = lr.right() + pad / 2.0;
            if let Some(n) = pl.number {
                let r = osd_text(
                    &p,
                    egui::pos2(x, top.center().y),
                    Align2::LEFT_CENTER,
                    &n.to_string(),
                    body_font.clone(),
                    with_alpha(t.osd_text, 0.7),
                    f32::INFINITY,
                );
                x = r.right() + pad / 2.0;
            }
        }
        // narrow: the bottom bar has only the speaker, so the level stays up here
        let clock_right = if narrow {
            let vol = if snap.muted { format!("{} muted", glyphs::MUTED) } else { format!("{} {:.0} %", glyphs::VOLUME, snap.volume) };
            osd_text(&p, egui::pos2(top.right() - pad, top.center().y), Align2::RIGHT_CENTER, &vol, small_font.clone(), t.osd_text, f32::INFINITY)
                .left()
                - pad
        } else {
            top.right() - pad
        };
        let cr = osd_text(
            &p,
            egui::pos2(clock_right, top.center().y),
            Align2::RIGHT_CENTER,
            &hhmm(ctx.now, h24),
            small_font.clone(),
            t.osd_text,
            f32::INFINITY,
        );
        let ch = playable.as_ref().and_then(|p| match p.media {
            MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(id).cloned(),
            _ => None,
        });
        let title = playable.as_ref().map(|p| p.title.as_str()).unwrap_or("");
        // a zap shows which of the SD / HD / 4K duplicates this is before the picture does;
        // once it plays, the decoded badge on the right says it
        let decoded = snap.state == PlayerState::Playing && snap.video_size.is_some();
        let quality = ch.as_ref().filter(|_| live && !decoded).and_then(|c| ctx.quality(c));
        let badge = quality.map(|q| InlineBadge::quality(&p, theme, q, t.osd_text, s));
        // which playlist, left of the clock, whenever two or more have this kind (DESIGN §29)
        let mut right = cr.left() - pad;
        if let Some(src) =
            playable.as_ref().and_then(|pl| Some((playable_item_kind(&pl.kind)?, pl.source_id?))).and_then(|(k, id)| ctx.source_of(k, id))
        {
            let color = theme.source_color(src.color);
            let name_color = badge::legible_on(color, t.osd_scrim.to_opaque(), t.osd_text);
            let r = osd_text(
                &p,
                egui::pos2(right, top.center().y),
                Align2::RIGHT_CENTER,
                &src.name,
                small_font.clone(),
                name_color,
                (rect.width() * 0.25).round(),
            );
            let d = osd_text(
                &p,
                egui::pos2(r.left() - pad / 3.0, top.center().y),
                Align2::RIGHT_CENTER,
                glyphs::DOT,
                small_font.clone(),
                color,
                f32::INFINITY,
            );
            right = d.left() - pad;
        }
        let max_w = (right - x).max(0.0);
        name_with_quality(&p, theme, egui::pos2(x, top.center().y), title, title_font.clone(), t.osd_text, badge, max_w);

        // ---- bottom bar
        let nn = ch.as_ref().filter(|_| live).and_then(|c| live::now_next_of(ctx, c));
        let controls = Self::controls(snap, narrow, ctx.video_only);
        let ctrl_h = (theme.space(SpaceToken::ControlHeight) * s * 1.2).round();
        let line = body_font.size * 1.4;
        let lines = 3.0;
        let bottom_h = pad * 2.0 + line * lines + ctrl_h + pad / 2.0;
        let bottom = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - bottom_h), rect.max);
        p.rect_filled(bottom, 0.0, t.osd_scrim);
        self.bottom_bar = Some(bottom_h);
        self.bars.push(bottom);
        let inner = bottom.shrink2(egui::vec2(pad, pad));
        let mut y = inner.top();
        if live {
            match nn.as_ref().and_then(|n| n.now.clone()) {
                Some(now) => {
                    let tr = osd_text(
                        &p,
                        egui::pos2(inner.right(), y),
                        Align2::RIGHT_TOP,
                        &time_range(now.start, now.stop, h24),
                        small_font.clone(),
                        with_alpha(t.osd_text, 0.8),
                        f32::INFINITY,
                    );
                    osd_text(
                        &p,
                        egui::pos2(inner.left(), y),
                        Align2::LEFT_TOP,
                        &now.title,
                        body_font.clone(),
                        t.osd_text,
                        (tr.left() - pad - inner.left()).max(0.0),
                    );
                    y += line;
                    let left = osd_text(
                        &p,
                        egui::pos2(inner.right(), y + line / 2.0),
                        Align2::RIGHT_CENTER,
                        &time_left(now.stop - ctx.now),
                        small_font.clone(),
                        with_alpha(t.osd_text, 0.8),
                        f32::INFINITY,
                    );
                    let bar = Rect::from_min_max(
                        egui::pos2(inner.left(), y + line / 2.0 - 2.0 * s),
                        egui::pos2(left.left() - pad, y + line / 2.0 + 2.0 * s),
                    );
                    p.rect_filled(bar, t.rounding, with_alpha(t.osd_text, 0.25));
                    let fill = Rect::from_min_max(bar.min, egui::pos2(bar.left() + bar.width() * now.progress(ctx.now), bar.bottom()));
                    p.rect_filled(fill, t.rounding, t.osd_accent);
                }
                None => {
                    let msg = if ch.as_ref().is_some_and(|c| c.epg_channel_id.is_none()) {
                        self.guide.loading_label(ctx).unwrap_or_else(|| "No guide data".to_owned())
                    } else {
                        "No information".to_owned()
                    };
                    osd_text(&p, egui::pos2(inner.left(), y), Align2::LEFT_TOP, &msg, body_font.clone(), with_alpha(t.osd_text, 0.7), inner.width());
                    y += line;
                }
            }
            y += line;
            let next = nn.as_ref().and_then(|n| n.next.clone()).map(|n| format!("Next  {}  {}", hhmm(n.start, h24), n.title)).unwrap_or_default();
            let br = self.badges(
                &p,
                theme,
                snap,
                ch.as_ref().is_some_and(|c| c.has_catchup),
                egui::pos2(inner.right(), y + line / 2.0),
                badge_font.clone(),
                s,
            );
            osd_text(
                &p,
                egui::pos2(inner.left(), y + line / 2.0),
                Align2::LEFT_CENTER,
                &next,
                small_font.clone(),
                with_alpha(t.osd_text, 0.8),
                (br.left() - pad - inner.left()).max(0.0),
            );
            y += line;
        } else {
            let sub = playable.as_ref().and_then(|p| p.subtitle.clone()).unwrap_or_default();
            let kind = playable.as_ref().map(|p| p.kind.clone());
            let head = match kind {
                Some(PlayableKind::Catchup { start, stop }) => format!("{} · {}", glyphs::CATCHUP, time_range(start, stop, h24)),
                Some(PlayableKind::Episode { season, episode }) => format!("S{season:02}E{episode:02}"),
                _ => String::new(),
            };
            let line1 = [head, sub].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
            osd_text(&p, egui::pos2(inner.left(), y), Align2::LEFT_TOP, &line1, body_font.clone(), t.osd_text, inner.width());
            y += line;
            let pos = snap.position.unwrap_or(0.0);
            let dur = snap.duration.or_else(|| playable.as_ref().and_then(|p| p.duration_hint)).unwrap_or(0.0);
            let times = format!("{} / {}", clock(pos), clock(dur));
            let tr =
                osd_text(&p, egui::pos2(inner.right(), y + line / 2.0), Align2::RIGHT_CENTER, &times, small_font.clone(), t.osd_text, f32::INFINITY);
            let seek_rect = Rect::from_min_max(egui::pos2(inner.left(), y), egui::pos2(tr.left() - pad, y + line));
            let focused = self.osd_focus && self.row == OsdRow::Seek;
            let cache_end = snap.cache_secs.map(|c| pos + c);
            let req =
                ui.scope_builder(egui::UiBuilder::new().max_rect(seek_rect), |ui| slider::seek_bar(ui, theme, pos, dur, cache_end, focused)).inner;
            if let Some(to) = req {
                ctx.play(PlayerCommand::SeekTo(to));
                self.show_banner(ctx);
            }
            y += line;
            let br = self.badges(&p, theme, snap, false, egui::pos2(inner.right(), y + line / 2.0), badge_font.clone(), s);
            let hint = format!("−{} / +{} s", ctx.config.player.skip_small_s, ctx.config.player.skip_small_s);
            osd_text(
                &p,
                egui::pos2(inner.left(), y + line / 2.0),
                Align2::LEFT_CENTER,
                &hint,
                small_font.clone(),
                with_alpha(t.osd_text, 0.7),
                (br.left() - pad - inner.left()).max(0.0),
            );
            y += line;
        }

        // volume at the right end of the controls row: its room is reserved first, so the
        // controls give up space before it does (narrow: the speaker alone)
        let cy = y + pad / 2.0;
        let speaker_w = [glyphs::VOLUME, glyphs::MUTED]
            .iter()
            .map(|g| p.layout_no_wrap((*g).to_owned(), small_font.clone(), t.osd_text).size().x)
            .fold(0.0, f32::max)
            + pad;
        let speaker_x = |right: f32| Rect::from_min_size(egui::pos2(right - speaker_w, cy), egui::vec2(speaker_w, ctrl_h));
        let controls_right = if narrow {
            let speaker = speaker_x(inner.right());
            self.draw_volume(ui, ctx, &p, speaker, None, small_font.clone(), snap);
            speaker.left() - pad
        } else {
            let label_w =
                ["130 %", "Muted"].iter().map(|s| p.layout_no_wrap((*s).to_owned(), small_font.clone(), t.osd_text).size().x).fold(0.0, f32::max);
            let slider_w = (if rect.width() >= 1200.0 * theme.rem() { 112.0 } else { 80.0 }) * s;
            let label = Rect::from_min_size(egui::pos2(inner.right() - label_w, cy), egui::vec2(label_w, ctrl_h));
            let slider = Rect::from_min_size(egui::pos2(label.left() - pad / 2.0 - slider_w, cy), egui::vec2(slider_w, ctrl_h));
            let speaker = speaker_x(slider.left() - pad / 4.0);
            self.draw_volume(ui, ctx, &p, speaker, Some((slider, Some(label))), small_font.clone(), snap);
            speaker.left() - pad
        };

        // controls row: labelled where the design labels them; all glyphs when that doesn't
        // fit; whatever still doesn't fit isn't drawn (and focus mode skips it)
        let mut cx = inner.left();
        let mut clicked = None;
        let text_of = |c: Control, glyphs_only: bool| {
            if glyphs_only || c.glyph_only() {
                c.glyph(snap.paused).to_owned()
            } else {
                format!("{}  {}", c.glyph(snap.paused), control_text(c, snap))
            }
        };
        let row_width = |glyphs_only: bool| -> f32 {
            controls.iter().map(|c| p.layout_no_wrap(text_of(*c, glyphs_only), small_font.clone(), t.osd_text).size().x + pad * 1.5).sum()
        };
        let glyphs_only = narrow || row_width(false) > controls_right - inner.left();
        let mut drawn = Vec::with_capacity(controls.len());
        for (i, c) in controls.iter().enumerate() {
            let text = text_of(*c, glyphs_only);
            let w = p.layout_no_wrap(text.clone(), small_font.clone(), t.osd_text).size().x;
            let r = Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(w + pad, ctrl_h));
            if r.right() > controls_right {
                break;
            }
            drawn.push(*c);
            let focused = self.osd_focus && self.row == OsdRow::Buttons && i == self.control.min(controls.len() - 1);
            let tip = (glyphs_only || c.glyph_only()).then(|| control_text(*c, snap));
            let resp = osd_button(ui, &p, theme, r, ui.id().with(("osd-control", i)), &text, small_font.clone(), c.label(), tip.as_deref(), focused);
            if resp.clicked() {
                clicked = Some((i, *c));
            }
            cx = r.right() + pad / 2.0;
        }
        self.control = self.control.min(drawn.len().saturating_sub(1));
        self.visible = drawn;
        if let Some((i, c)) = clicked {
            // a click activates the control but doesn't enter controls focus mode (DESIGN
            // §13.8): the arrows keep zapping / seeking afterwards
            self.row = OsdRow::Buttons;
            self.control = i;
            self.show_banner(ctx);
            self.activate_control(ctx, c);
        }
    }

    /// The video-only window's banner: one bottom strip — title and volume, then what's on
    /// (live) or the position (VOD), and the way back when there is room. With the pointer in
    /// the window (or paused) the right side holds buttons instead of the volume and the hint:
    /// previous / play / next (live) or skips (VOD), speaker (+ slider from 360 px), pop in,
    /// stop.
    fn draw_compact_osd(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let pad = theme.space(SpaceToken::Sm);
        let title_font = FontId::proportional(theme.font(TypeToken::Body));
        let small_font = FontId::proportional(theme.font(TypeToken::Caption));
        let line = title_font.size * 1.35;
        let strip = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - 2.0 * pad - line - small_font.size * 1.35), rect.max);
        let p = ui.painter_at(rect);
        p.rect_filled(strip, 0.0, t.osd_scrim);
        self.bottom_bar = Some(strip.height());
        self.bars = vec![strip];
        let inner = strip.shrink(pad);
        let buttons = snap.paused || ui.ctx().pointer_hover_pos().is_some_and(|pos| rect.contains(pos));
        let text_right = if buttons {
            self.draw_strip_buttons(ui, ctx, &p, inner, title_font.clone(), snap) - pad
        } else {
            let vol = if snap.muted { format!("{} muted", glyphs::MUTED) } else { format!("{} {:.0} %", glyphs::VOLUME, snap.volume) };
            osd_text(&p, inner.right_top(), Align2::RIGHT_TOP, &vol, small_font.clone(), t.osd_text, f32::INFINITY).left() - pad
        };
        let playable = snap.playable.as_ref();
        let mut title = playable.map(|p| p.title.clone()).unwrap_or_default();
        if snap.paused {
            title = format!("{} {title}", glyphs::PAUSE);
        }
        osd_text(&p, inner.left_top(), Align2::LEFT_TOP, &title, title_font, t.osd_text, (text_right - inner.left()).max(0.0));
        let second = if is_live(snap) {
            let ch = playable.and_then(|p| match p.media {
                MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(id).cloned(),
                _ => None,
            });
            ch.and_then(|c| live::now_next_of(ctx, &c)).and_then(|n| n.now).map(|now| now.title.to_string()).unwrap_or_default()
        } else {
            let dur = snap.duration.or_else(|| playable.and_then(|p| p.duration_hint)).unwrap_or(0.0);
            format!("{} / {}", clock(snap.position.unwrap_or(0.0)), clock(dur))
        };
        let second_pos = egui::pos2(inner.left(), inner.top() + line);
        let mut second_w = inner.width();
        if buttons {
            second_w = (text_right - inner.left()).max(0.0);
        } else if strip.width() >= 360.0 {
            let hint = osd_text(
                &p,
                egui::pos2(inner.right(), second_pos.y),
                Align2::RIGHT_TOP,
                "o · Esc  back",
                small_font.clone(),
                with_alpha(t.osd_text, 0.5),
                f32::INFINITY,
            );
            second_w = (hint.left() - pad - inner.left()).max(0.0);
        }
        osd_text(&p, second_pos, Align2::LEFT_TOP, &second, small_font, with_alpha(t.osd_text, 0.8), second_w);
    }

    /// The pop-out strip's buttons, right-aligned in `inner` (the strip's two text lines high);
    /// returns their left edge. The text keeps room: the slider only from 560 pt, previous /
    /// next only from 420 pt (the wheel over the speaker still steps the volume).
    fn draw_strip_buttons(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, p: &egui::Painter, inner: Rect, font: FontId, snap: &PlayerSnapshot) -> f32 {
        #[derive(Clone, Copy)]
        enum B {
            Prev,
            PlayPause,
            Next,
            PopIn,
            Stop,
        }
        let theme = ctx.theme;
        let live = is_live(snap);
        let side = inner.height();
        let gap = theme.space(SpaceToken::Xxs);
        let square = |right: f32| Rect::from_min_size(egui::pos2(right - side, inner.top()), egui::vec2(side, side));
        let rem = theme.rem();
        let wide = inner.width() >= 560.0 * rem;
        let skips = inner.width() >= 420.0 * rem;
        // right to left: stop, pop in, volume, next, play, previous
        let stop = square(inner.right());
        let pop_in = square(stop.left() - gap);
        let slider_w = 64.0 * rem;
        let slider = wide.then(|| Rect::from_min_size(egui::pos2(pop_in.left() - gap - slider_w, inner.top()), egui::vec2(slider_w, side)));
        let speaker = square(slider.map_or(pop_in.left(), |s| s.left()) - gap);
        let next = square(speaker.left() - gap);
        let play = if skips { square(next.left() - gap) } else { next };
        let prev = square(play.left() - gap);
        self.draw_volume(ui, ctx, p, speaker, slider.map(|s| (s, None)), font.clone(), snap);
        let (prev_label, next_label) =
            if live { ("Previous channel", "Next channel") } else { (Control::SkipBack.label(), Control::SkipForward.label()) };
        let (prev_glyph, next_glyph) = if live { (glyphs::PREVIOUS, glyphs::NEXT) } else { (glyphs::SKIP_BACK, glyphs::SKIP_FORWARD) };
        let buttons = [
            (B::Prev, prev, prev_glyph, prev_label, prev_label),
            (B::PlayPause, play, if snap.paused { glyphs::PLAY } else { glyphs::PAUSE }, "Play / pause", "Play / pause (Space)"),
            (B::Next, next, next_glyph, next_label, next_label),
            (B::PopIn, pop_in, glyphs::POP_IN, "Back to the full window", "Back to the full window (o · Esc · double-click)"),
            (B::Stop, stop, glyphs::CLOSE, "Stop and close", "Stop and close (q)"),
        ];
        let mut clicked = None;
        for (i, (b, r, glyph, label, tip)) in buttons.into_iter().enumerate() {
            if !skips && matches!(b, B::Prev | B::Next) {
                continue;
            }
            if osd_button(ui, p, theme, r, ui.id().with(("strip-button", i)), glyph, font.clone(), label, Some(tip), false).clicked() {
                clicked = Some(b);
            }
        }
        let skip = f64::from(ctx.config.player.skip_small_s);
        match clicked {
            Some(B::Prev) if live => self.zap(ctx, false),
            Some(B::Next) if live => self.zap(ctx, true),
            Some(B::Prev) => ctx.play(PlayerCommand::SeekBy(-skip)),
            Some(B::Next) => ctx.play(PlayerCommand::SeekBy(skip)),
            Some(B::PlayPause) => ctx.play(PlayerCommand::TogglePause),
            Some(B::PopIn) => ctx.navigate(NavRequest::TogglePopOut),
            Some(B::Stop) => Self::stop_and_leave(ctx),
            None => {}
        }
        if clicked.is_some() {
            self.show_banner(ctx);
        }
        if skips { prev.left() } else { play.left() }
    }

    /// `[HD] [5.1] [CC] [⟲]` right-aligned at `right_center`; returns their extent.
    fn badges(&self, p: &egui::Painter, theme: &Theme, snap: &PlayerSnapshot, catchup: bool, right_center: egui::Pos2, font: FontId, s: f32) -> Rect {
        let t = &theme.tokens;
        let mut labels: Vec<String> = Vec::new();
        if let Some(q) = snap.video_size.and_then(|(w, h)| Quality::of_video(w, h)) {
            labels.push(q.label().into());
        }
        if snap.stream_info.as_ref().is_some_and(|i| i.is_hdr()) {
            labels.push("HDR".into());
        }
        if let Some(chs) = snap.tracks.selected_audio().and_then(|a| a.channels) {
            labels.push(match chs {
                1 => "Mono".into(),
                2 => "Stereo".into(),
                6 => "5.1".into(),
                8 => "7.1".into(),
                n => format!("{n} ch"),
            });
        }
        if !snap.tracks.subs.is_empty() {
            labels.push("CC".into());
        }
        if catchup {
            labels.push(glyphs::CATCHUP.into());
        }
        let mut right = right_center.x;
        let mut extent = Rect::from_center_size(right_center, egui::Vec2::ZERO);
        let gap = theme.space(SpaceToken::Sm) * s;
        for l in labels.iter().rev() {
            let g = p.layout_no_wrap(l.clone(), font.clone(), t.osd_text);
            let r = Rect::from_min_size(
                egui::pos2(right - g.size().x - 2.0 * gap, right_center.y - g.size().y / 2.0 - gap / 2.0),
                g.size() + egui::vec2(2.0 * gap, gap),
            );
            p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, with_alpha(t.osd_text, 0.6)), egui::StrokeKind::Inside);
            p.galley(r.min + egui::vec2(gap, gap / 2.0), g, t.osd_text);
            extent = extent.union(r);
            right = r.left() - gap;
        }
        extent
    }

    /// The side panel (channels / mini guide / history) on the right of the video: a
    /// `virtual_list` (wheel scrolling; the hover cursor follows only real pointer motion, the
    /// view follows only key moves), a ✕ in the header, one click on a row tunes it (the panel
    /// stays open for the next one).
    fn draw_panel(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        if self.panel.is_none() {
            return;
        }
        let loading = self.guide.loading_label(ctx);
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        let w = (340.0 * theme.rem() * s).min(rect.width() * 0.6);
        let area = Rect::from_min_max(
            egui::pos2(rect.right() - w - pad, rect.top() + pad * 6.0),
            egui::pos2(rect.right() - pad, rect.bottom() - pad * 12.0),
        );
        if area.height() < 60.0 {
            return;
        }
        // the panel's background takes the clicks and the wheel that miss a row (a click would
        // otherwise reach the video under it and close the panel)
        let _ = ui.interact(area, ui.id().with("panel-bg"), egui::Sense::click());
        let p = ui.painter_at(rect);
        p.rect_filled(area, t.rounding, t.osd_scrim);
        let caption = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let Some(panel) = &mut self.panel else { return };
        let title = match panel.kind {
            PanelKind::Channels => "CHANNELS (c)",
            PanelKind::MiniGuide => "NOW / NEXT (g)",
            PanelKind::History => "HISTORY (h)",
        };
        let head_h = (caption.size * 2.0).round();
        let close = Rect::from_min_size(egui::pos2(area.right() - pad / 2.0 - head_h, area.top() + pad / 2.0), egui::vec2(head_h, head_h));
        let q = panel.list.filter.query();
        let head = if q.is_empty() { title.to_owned() } else { format!("{title}  ·  {q}") };
        osd_text(
            &p,
            egui::pos2(area.left() + pad, close.center().y),
            Align2::LEFT_CENTER,
            &head,
            caption.clone(),
            with_alpha(t.osd_text, 0.7),
            close.left() - area.left() - 2.0 * pad,
        );
        let closed = osd_button(ui, &p, theme, close, ui.id().with("panel-close"), glyphs::CLOSE, small.clone(), "Close", Some("Close (Esc)"), false)
            .clicked();
        let list_top = close.bottom() + pad / 2.0;
        let mini = panel.kind == PanelKind::MiniGuide;
        let row_h = (if mini { font.size * 2.8 } else { font.size * 2.0 }).round();
        let playing = playing_channel(ctx.player);
        if panel.rows.is_empty() {
            let msg = if q.is_empty() { "Nothing here yet" } else { "No matches" };
            osd_text(
                &p,
                egui::pos2(area.left() + pad, list_top + row_h / 2.0),
                Align2::LEFT_CENTER,
                msg,
                small.clone(),
                with_alpha(t.osd_text, 0.7),
                area.width() - 2.0 * pad,
            );
        }
        let list_rect =
            Rect::from_min_max(egui::pos2(area.left() + pad / 2.0, list_top), egui::pos2(area.right() - pad / 2.0, area.bottom() - pad / 2.0));
        let SidePanel { kind, list, items, rows, .. } = panel;
        let mut tune = None;
        // the panel lists Live TV's channels: marks as Live TV shows them (none inside one playlist)
        let sources = crate::ui::data::SourceMarks::for_view(
            ctx.config.interface.playlist_on_cards,
            &ctx.lib.sources,
            crate::model::ItemKind::Channel,
            crate::ui::scope::Scope::current(ctx, crate::ui::scope::ScopeKind::Live).is_all(),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
            virtual_list(ui, theme, ("player-panel", *kind as u8), list, row_h, |ui, rc| {
                let (r, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                let id = items[rows[rc.index] as usize];
                let Some(ch) = ctx.lib.live.get(id).cloned() else { return resp };
                let source = sources.of(ch.source_id);
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, badge::label_with_source(&ch.name, source)));
                // the second click of a double-click would tune it again
                if resp.clicked() && !resp.double_clicked() {
                    tune = Some(id);
                }
                let p = ui.painter();
                let cursor = rc.state != crate::ui::widgets::RowState::Idle;
                if cursor {
                    p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
                }
                let color = if cursor { t.osd_accent } else { t.osd_text };
                let mut x = r.left() + pad / 2.0;
                let num = ch.number.map(|n| n.to_string()).unwrap_or_default();
                let line_y = if mini { r.top() + row_h * 0.33 } else { r.center().y };
                let nr = osd_text(
                    p,
                    egui::pos2(x + 3.0 * small.size, line_y),
                    Align2::RIGHT_CENTER,
                    &num,
                    small.clone(),
                    with_alpha(t.osd_text, 0.7),
                    f32::INFINITY,
                );
                x = nr.right().max(x + 3.0 * small.size) + pad / 2.0;
                if playing == Some(id) {
                    let g = osd_text(p, egui::pos2(x, line_y), Align2::LEFT_CENTER, glyphs::PLAY, small.clone(), t.osd_accent, f32::INFINITY);
                    x = g.right() + pad / 3.0;
                }
                // the playlist's name at the right edge, in its colour (several playlists, DESIGN §29)
                let mut right = r.right() - pad / 2.0;
                if let Some(src) = source {
                    let color = badge::legible_on(theme.source_color(src.color), t.osd_scrim.to_opaque(), t.osd_text);
                    let m = osd_text(p, egui::pos2(right, line_y), Align2::RIGHT_CENTER, &src.name, small.clone(), color, (r.width() * 0.2).round());
                    right = m.left() - pad / 2.0;
                }
                let quality = ctx.quality(&ch).map(|q| InlineBadge::quality(p, theme, q, with_alpha(t.osd_text, 0.7), s));
                name_with_quality(p, theme, egui::pos2(x, line_y), &ch.name, font.clone(), color, quality, right - x);
                if mini {
                    let nn = live::now_next_of(ctx, &ch);
                    let h24 = ctx.config.interface.clock_24h;
                    let detail = match nn {
                        Some(n) => {
                            let now = n.now.map(|p| p.title.to_string()).unwrap_or_else(|| "No information".into());
                            match n.next {
                                Some(nx) => format!("{now} · {} {}", hhmm(nx.start, h24), nx.title),
                                None => now,
                            }
                        }
                        None if ch.epg_channel_id.is_none() => loading.clone().unwrap_or_else(|| "No guide data".into()),
                        None => "…".into(),
                    };
                    let p = ui.painter();
                    osd_text(
                        p,
                        egui::pos2(x, r.top() + row_h * 0.72),
                        Align2::LEFT_CENTER,
                        &detail,
                        small.clone(),
                        with_alpha(t.osd_text, 0.7),
                        r.right() - pad / 2.0 - x,
                    );
                }
                resp
            });
        });
        // a click tunes (the list already moved its cursor there); a locked channel asks for
        // the PIN first and then tunes the cursor row
        if let Some(id) = tune
            && self.tune(ctx, id, Action::Activate)
        {
            self.show_banner(ctx);
        }
        if closed {
            self.panel = None;
        }
    }

    fn draw_info(&self, ui: &egui::Ui, ctx: &UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        let font = FontId::monospace((theme.font(TypeToken::Body) * s).round());
        let mut lines: Vec<String> = Vec::new();
        match &snap.stream_info {
            Some(i) => {
                let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "–".into());
                let res = match (i.width, i.height) {
                    (Some(w), Some(h)) => format!("{w}×{h}"),
                    _ => "–".into(),
                };
                lines.push(format!("Video    {} · {res} · {}", opt(&i.video_codec), i.fps.map_or_else(|| "–".into(), |f| format!("{f:.2} fps"))));
                // the provider's name and quality word against what plays (DESIGN §13.0.1)
                let channel = snap.playable.as_ref().filter(|p| p.kind.is_live()).and_then(|p| match p.media {
                    MediaRef::Channel(id) => ctx.lib.live.get(id),
                    _ => None,
                });
                if let Some(raw) = channel.and_then(|c| c.provider_name.as_deref()) {
                    lines.push(format!("Provider {raw}"));
                }
                if let Some(c) = channel.and_then(|c| c.quality?.claimed) {
                    let played = i.width.zip(i.height).and_then(|(w, h)| Quality::of_video(w, h));
                    lines.push(match played {
                        Some(p) if p == c => format!("Quality  named {} · as labelled", c.label()),
                        Some(p) => format!("Quality  named {} · plays {}", c.label(), p.label()),
                        None => format!("Quality  named {} · not verified", c.label()),
                    });
                }
                lines.push(format!("Format   {}{}", opt(&i.pixel_format), if i.is_hdr() { " · HDR" } else { "" }));
                lines.push(format!(
                    "Audio    {} · {} · {}",
                    opt(&i.audio_codec),
                    opt(&i.audio_channels),
                    i.samplerate.map_or_else(|| "–".into(), |r| format!("{r} Hz"))
                ));
                let kbps = |b: Option<f64>| b.map_or_else(|| "–".into(), |b| format!("{:.0} kb/s", b / 1000.0));
                lines.push(format!("Bitrate  video {} · audio {}", kbps(i.video_bitrate), kbps(i.audio_bitrate)));
                lines.push(format!("Decoder  {}", i.hwdec.clone().filter(|h| !h.is_empty() && h != "no").unwrap_or_else(|| "software".into())));
                lines.push(format!("Dropped  {}", i.dropped_frames.map_or_else(|| "–".into(), |d| d.to_string())));
                lines.push(format!("Cache    {}", i.cache_secs.map_or_else(|| "–".into(), |c| format!("{c:.1} s"))));
                if let Some(c) = &i.container {
                    lines.push(format!("Stream   {c}"));
                }
            }
            None => lines.push("Reading stream info…".into()),
        }
        let p = ui.painter_at(rect);
        let lh = font.size * 1.4;
        let w = (420.0 * s).min(rect.width() - 2.0 * pad);
        let area = Rect::from_min_size(egui::pos2(rect.left() + pad, rect.top() + pad * 6.0), egui::vec2(w, lh * lines.len() as f32 + 2.0 * pad));
        p.rect_filled(area, t.rounding, t.osd_scrim);
        for (i, l) in lines.iter().enumerate() {
            osd_text(
                &p,
                egui::pos2(area.left() + pad, area.top() + pad + i as f32 * lh),
                Align2::LEFT_TOP,
                l,
                font.clone(),
                t.osd_text,
                w - 2.0 * pad,
            );
        }
    }

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

    /// The other copy offered for the current failure, if any.
    fn offered(&self, snap: &PlayerSnapshot) -> Option<&crate::ui::copies::OtherCopy> {
        let (generation, other) = self.other_copy.as_ref()?;
        if *generation != snap.load_generation || !matches!(snap.state, PlayerState::Error(_)) {
            return None;
        }
        other.as_ref()
    }

    /// Look the failed title's other copy up once per failure (a few indexed reads, within the
    /// frame budget) and hand its label to the error card.
    fn update_other_copy(&mut self, ctx: &mut UiCtx<'_>, snap: &PlayerSnapshot) {
        let vod = snap.playable.as_ref().is_some_and(|p| matches!(p.media, MediaRef::Movie(_) | MediaRef::Episode(_)));
        if !(vod && matches!(snap.state, PlayerState::Error(_))) {
            ctx.video.set_other_copy(None);
            return;
        }
        if self.other_copy.as_ref().is_none_or(|(g, _)| *g != snap.load_generation) {
            let unlocked = ctx.parental.is_unlocked(&ctx.config.parental);
            let (db, lib, order, now) = (ctx.db, ctx.lib, ctx.config.interface.copy_order, ctx.now);
            let found = ctx.budget.run(|| {
                let plan_of = |movies: bool, item: u32| crate::store::copies::plan(db, &lib.vod, movies, item, unlocked, order, now).ok();
                crate::ui::copies::other_copy(db, lib, plan_of, snap)
            });
            if let Some(other) = found {
                self.other_copy = Some((snap.load_generation, other));
            }
        }
        ctx.video.set_other_copy(self.offered(snap).map(|o| o.label.clone()));
    }

    /// Nothing playing: what to do about it and a button for it. In the full window a click
    /// anywhere goes back; popped out, the button (or a double-click) pops the window back in.
    fn idle_ui(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let p = ui.painter_at(rect);
        p.rect_filled(rect, 0.0, Color32::BLACK);
        let (title, hint, title_font, hint_font) = if ctx.video_only {
            ("Nothing playing", "o · Esc", TypeToken::Subtitle, TypeToken::BodySmall)
        } else {
            ("Nothing is playing", "Esc goes back", TypeToken::Title, TypeToken::Body)
        };
        let background = ui.interact(rect, ui.id().with("player-idle"), egui::Sense::click());
        osd_text(&p, rect.center(), Align2::CENTER_BOTTOM, title, FontId::proportional(theme.font(title_font)), t.osd_text, rect.width());
        let font = FontId::proportional(theme.font(hint_font));
        let hr =
            osd_text(&p, rect.center() + egui::vec2(0.0, 8.0), Align2::CENTER_TOP, hint, font.clone(), with_alpha(t.osd_text, 0.7), rect.width());
        let (text, label) = if ctx.video_only {
            (format!("{}  Back to the full window", glyphs::POP_IN), "Back to the full window".to_owned())
        } else {
            // like every other back button: says where it goes
            let label = ctx.back.map_or_else(|| "Back".to_owned(), |b| format!("Back to {}", b.label));
            (format!("{}  {label}", glyphs::CHEVRON_LEFT), label)
        };
        let pad = theme.space(SpaceToken::Md);
        let size = p.layout_no_wrap(text.clone(), font.clone(), t.osd_text).size() + egui::vec2(2.0 * pad, pad);
        let br = Rect::from_center_size(egui::pos2(rect.center().x, hr.bottom() + pad + size.y / 2.0), size);
        p.rect_stroke(br, t.rounding, egui::Stroke::new(1.0, with_alpha(t.osd_text, 0.6)), egui::StrokeKind::Inside);
        let button = osd_button(ui, &p, theme, br, ui.id().with("player-idle-back"), &text, font, &label, None, false).clicked();
        if ctx.video_only {
            if button || background.double_clicked() {
                ctx.navigate(NavRequest::TogglePopOut);
            }
        } else if button || background.clicked() {
            ctx.navigate(NavRequest::Back);
        }
    }

    fn draw_overlays(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let p = ui.painter_at(rect);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        // numeric entry
        if let Some((digits, _)) = &self.numeric {
            let font = FontId::proportional((theme.font(TypeToken::DisplayLarge) * s * 1.4).round());
            let name = digits
                .parse::<u32>()
                .ok()
                .and_then(|n| ctx.lib.live.by_number(n))
                .and_then(|id| ctx.lib.live.get(id))
                .map(|c| c.name.to_string())
                .unwrap_or_default();
            let g = p.layout_no_wrap(format!("{digits}_"), font, t.osd_accent);
            let r = Rect::from_min_size(
                egui::pos2(rect.right() - g.size().x - 3.0 * pad, rect.top() + pad * 6.0),
                g.size() + egui::vec2(2.0 * pad, 2.0 * pad),
            );
            let name_font = FontId::proportional((theme.font(TypeToken::Body) * s).round());
            let extra = if name.is_empty() { 0.0 } else { (name_font.size * 2.0).round() };
            p.rect_filled(r.union(Rect::from_min_size(r.left_bottom(), egui::vec2(r.width(), extra))), t.rounding, t.osd_scrim);
            p.galley(r.min + egui::vec2(pad, pad), g, t.osd_accent);
            if !name.is_empty() {
                osd_text(&p, egui::pos2(r.center().x, r.bottom() + extra / 2.0), Align2::CENTER_CENTER, &name, name_font, t.osd_text, r.width());
            }
        }
        // flash message (track / delay / volume changes)
        let text = self
            .flash
            .filter(|(_, at)| at.elapsed() < FLASH)
            .map(|(f, _)| flash_text(f, snap, &self.zoom))
            .or_else(|| self.flash_text.as_ref().filter(|(_, at)| at.elapsed() < FLASH).map(|(t, _)| t.clone()));
        if let Some(text) = text {
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let g = p.layout_no_wrap(text, font, t.osd_text);
            let r = Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + pad * 9.0), g.size() + egui::vec2(2.0 * pad, pad));
            p.rect_filled(r, t.rounding, t.osd_scrim);
            p.galley(r.min + egui::vec2(pad, pad / 2.0), g, t.osd_text);
        }
        // next episode card: a click on it plays now; its buttons play now / cancel
        if let Some((at, label)) = self.next_episode.as_ref().map(|n| (n.at, n.label.clone())) {
            let left = at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
            let w = (420.0 * s).min(rect.width() - 2.0 * pad);
            let button_h = (small.size * 2.0).round();
            let h = font.size * 2.6 + button_h + 2.5 * pad;
            // above the bottom OSD bar while it shows, else near the bottom edge
            let bottom = rect.bottom() - self.bottom_bar.unwrap_or(0.0) - 2.0 * pad;
            let r = Rect::from_min_size(egui::pos2(rect.right() - w - pad, (bottom - h).max(rect.top() + pad)), egui::vec2(w, h));
            let card = ui.interact(r, ui.id().with("next-episode"), egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
            p.rect_filled(r, t.rounding, t.osd_scrim);
            p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
            osd_text(
                &p,
                egui::pos2(r.left() + pad, r.top() + pad),
                Align2::LEFT_TOP,
                &format!("Next episode in {left} s"),
                font.clone(),
                t.osd_accent,
                w - 2.0 * pad,
            );
            osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad + font.size * 1.3), Align2::LEFT_TOP, &label, font, t.osd_text, w - 2.0 * pad);
            let mut x = r.left() + pad / 2.0;
            let mut button = |text: String, label: &str, tip: &str, i: u8| {
                let bw = p.layout_no_wrap(text.clone(), small.clone(), t.osd_text).size().x + pad;
                let br = Rect::from_min_size(egui::pos2(x, r.bottom() - pad / 2.0 - button_h), egui::vec2(bw, button_h));
                x = br.right() + pad / 2.0;
                osd_button(ui, &p, theme, br, ui.id().with(("next-episode-button", i)), &text, small.clone(), label, Some(tip), false).clicked()
            };
            // the keys stay on the buttons (the card used to be keys only)
            let play = button(format!("{}  Play now · Enter", glyphs::PLAY), "Play now", "Play now (Enter)", 0);
            let cancel = button(format!("{}  Cancel · Esc", glyphs::CLOSE), "Cancel", "Cancel (Esc)", 1);
            if cancel {
                self.next_episode = None;
            } else if play || card.clicked() {
                self.play_next_episode(ctx);
            }
        }
    }
}

/// One OSD button (controls row, ←, volume, panel ✕, next-episode card, pop-out strip, the
/// failed stream's recovery buttons): OSD colours, hover fill, accent focus ring, pointing
/// hand. `label` is the accessible name; `tooltip` the hover text.
fn osd_button(
    ui: &egui::Ui,
    p: &egui::Painter,
    theme: &Theme,
    rect: Rect,
    id: egui::Id,
    text: &str,
    font: FontId,
    label: &str,
    tooltip: Option<&str>,
    focused: bool,
) -> egui::Response {
    let t = &theme.tokens;
    let resp = ui.interact(rect, id, egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let resp = match tooltip {
        Some(tip) => resp.on_hover_text(tip),
        None => resp,
    };
    if resp.hovered() || focused {
        p.rect_filled(rect, t.rounding, with_alpha(t.osd_text, 0.12));
    }
    if focused {
        p.rect_stroke(rect, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
    }
    let color = if focused { t.osd_accent } else { t.osd_text };
    let g = p.layout_no_wrap(text.to_owned(), font, color);
    p.galley(rect.center() - g.size() / 2.0, g, color);
    resp
}

/// The speaker glyph for a level (0–130 %).
fn volume_glyph(muted: bool, level: f32) -> &'static str {
    match level {
        _ if muted => glyphs::MUTED,
        l if l < 34.0 => glyphs::VOLUME_LOW,
        l if l < 67.0 => glyphs::VOLUME_MEDIUM,
        _ => glyphs::VOLUME,
    }
}

/// Channel logo on the OSD: the image when loaded, else a monogram tile in OSD colours (the
/// theme's logo placeholder would be illegible on the dark scrim in light themes).
fn osd_logo(p: &egui::Painter, theme: &Theme, images: &mut crate::images::ImageCache, url: Option<&str>, name: &str, r: Rect) {
    let t = &theme.tokens;
    if let Some(crate::images::ImageState::Ready(tex)) = url.filter(|u| !u.is_empty()).map(|u| images.get(u, crate::images::ImageKind::Logo)) {
        let sz = tex.size_vec2();
        let k = (r.width() / sz.x.max(1.0)).min(r.height() / sz.y.max(1.0));
        let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
        p.image(tex.id(), Rect::from_center_size(r.center(), sz * k), uv, Color32::WHITE);
        return;
    }
    p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
    p.text(r.center(), Align2::CENTER_CENTER, logo::monogram(name), FontId::proportional((r.height() * 0.4).round().max(6.0)), t.osd_text);
}

/// "41:20" / "1:02:13".
fn clock(seconds: f64) -> String {
    let s = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    let (h, m, s) = (s / 3600, (s / 60) % 60, s % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// Control label with the live value (track names).
fn control_text(c: Control, snap: &PlayerSnapshot) -> String {
    match c {
        Control::Audio => snap.tracks.selected_audio().map_or_else(|| "Audio".into(), |a| a.label()),
        Control::Subtitles => snap.tracks.selected_sub().map_or_else(|| "Off".into(), |a| a.label()),
        other => other.label().into(),
    }
}

fn flash_text(f: Flash, snap: &PlayerSnapshot, zoom: &usize) -> String {
    match f {
        Flash::Audio => format!("{} {}", glyphs::AUDIO_TRACK, snap.tracks.selected_audio().map_or_else(|| "No audio track".into(), |a| a.label())),
        Flash::Subtitles => format!("{} {}", glyphs::SUBTITLES, snap.tracks.selected_sub().map_or_else(|| "Subtitles off".into(), |a| a.label())),
        Flash::SubDelay => format!("Subtitle delay {:+.1} s", snap.sub_delay),
        Flash::SubSize => format!("Subtitle size {:.0} %", snap.sub_scale * 100.0),
        Flash::Volume => {
            if snap.muted {
                format!("{} Muted", glyphs::MUTED)
            } else {
                format!("{} {:.0} %", glyphs::VOLUME, snap.volume)
            }
        }
        Flash::Aspect => format!("{} Aspect {}", glyphs::ASPECT, snap.aspect.label()),
        Flash::Zoom => format!("Zoom {:.0} %", (2f64.powf(ZOOMS[*zoom % ZOOMS.len()]) - 1.0) * 100.0),
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
        match action {
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
                self.control = self.active_controls(ctx.player, narrow, ctx.video_only).iter().position(|c| *c == Control::PlayPause).unwrap_or(0);
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
                if let Some(ch) = playing_channel(ctx.player).and_then(|id| ctx.lib.live.get(id).cloned()) {
                    live::toggle_favourite(ctx, live::channel_ref(&ch), &ch.name);
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
        let rect = ui.available_rect_before_wrap();
        let theme = ctx.theme;
        let snap = ctx.player.clone();
        self.live_ctx = is_live(&snap);
        self.bottom_bar = None;
        self.bars.clear();
        self.find_next_episode(ctx);
        if std::mem::take(&mut self.toggle_fullscreen) {
            let fs = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        if snap.state == PlayerState::Idle && snap.playable.is_none() {
            self.idle_ui(ui, ctx, rect);
            return;
        }
        self.update_other_copy(ctx, &snap);
        let resp = ctx.video.paint(ui, rect, Placement::Fullscreen, theme, ctx.images, &snap);
        // popped out, a drag on the video moves the window (only there: a drag-sensing video
        // counts as hovered under every button drawn over it, see the wheel and cursor below)
        let resp = if ctx.video_only { resp.interact(egui::Sense::drag()) } else { resp };
        // a failed / reconnecting stream: Retry / Last channel buttons under the message
        let p = ui.painter_at(rect);
        for (i, b) in ctx.video.state_buttons().to_vec().into_iter().enumerate() {
            p.rect_stroke(b.rect, theme.tokens.rounding, egui::Stroke::new(1.0, with_alpha(theme.tokens.osd_text, 0.6)), egui::StrokeKind::Inside);
            let (label, tip, action) = match b.action {
                StateAction::Retry => ("Retry", "Retry (r)", Action::Retry),
                StateAction::LastChannel => ("Last channel", "Last channel (Backspace)", Action::LastChannel),
                StateAction::OtherCopy => ("Play from another copy", "Play from another copy (Ctrl+O)", Action::PlayFrom),
                StateAction::Stop => ("Stop", "Stop and close (q)", Action::StopAndClose),
            };
            if osd_button(ui, &p, theme, b.rect, ui.id().with(("state-button", i)), &b.text, b.font, label, Some(tip), false).clicked() {
                self.on_action(ctx, action);
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
                ctx.play(PlayerCommand::AdjustVolume(f64::from(steps) * 5.0));
                self.flash = Some((Flash::Volume, Instant::now()));
                self.show_banner(ctx);
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
        h.state().t.services().player.handle().snapshot()
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

    /// Click at 50 % of the slider → 65 % (of 0–130); a wheel notch over it → +5; the speaker
    /// mutes; moving the slider while muted unmutes.
    #[test]
    fn the_osd_volume_slider_sets_the_volume_and_the_speaker_mutes() {
        let mut h = player(WIDE);
        let c = h.get_by_label("Volume").rect().center();
        h.hover_at(c);
        h.drag_at(c);
        h.drop_at(c);
        until(&mut h, "volume 65 %", |h| {
            sync(h);
            real(h).volume == 65.0
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
            real(h).volume == 70.0
        });
        assert!(!real(&h).muted);
        h.get_by_label("Mute").click();
        until(&mut h, "muted", |h| {
            sync(h);
            real(h).muted
        });
        h.run_steps(1);
        assert!(has_label(&h, "Unmute"), "the speaker says what a click does");
        h.hover_at(c);
        h.drag_at(c);
        h.drop_at(c);
        until(&mut h, "unmuted at 65 %", |h| {
            sync(h);
            !real(h).muted && real(h).volume == 65.0
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
            real(h).volume >= 70.0
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
            real(h).paused != paused
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
}
