//! Fullscreen player + OSD (DESIGN §13.8): VideoHost Fullscreen, fixed dark scrim, OSD type
//! scale `layout::osd_scale`, accent = `tokens.osd_accent`.
//! * INFO BANNER (passive): shown on zap / any key / mouse move, hides after
//!   `osd_timeout_ms`; NEVER changes the key context (arrows keep zapping/seeking).
//! * CONTROLS FOCUS MODE: entered only with Enter / `i` / Tab (`FocusOsdControls`) or a click
//!   on a control; pushes `PlayerOsd`; Esc (`LeaveOsdControls`) leaves it, banner stays.
//! * Esc ladder (no focus mode): hide banner → leave window fullscreen → leave the player
//!   (mini-player keeps playing). Esc never SHOWS the banner.
//! * Side panels (`c` channels, `g` mini guide, `h`/→ history): stack
//!   `[List|ListFiltering, PlayerLive]`; ↑/↓ move the cursor (no zap), Enter tunes, ←/Esc close.
//!   Stream info `I`.
//! * Numeric entry (digits): stack `[NumericEntry, PlayerLive]`, 1.5 s auto-commit.
//! * End of playback: a finished episode → "Next episode in 10 s" card over the video. A
//!   finished movie (marked watched by the player) returns to its detail page; that part is
//!   driven by the always-alive movie detail screen, so it is done once.
//! * Mouse: double-click toggles window fullscreen; wheel = volume (only here).
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
use crate::model::{ChannelId, Collection, EpisodeId, ItemRef, MediaRef, PlayOrigin, PlayableKind};
use crate::msg::{AppEvent, Toast};
use crate::player::{PlayRequest, PlayerCommand, PlayerEvent, PlayerSnapshot, PlayerState};
use crate::store::library::LiveIndex;
use crate::theme::{SpaceToken, Theme, TypeToken, with_alpha};
use crate::ui::epg_grid::{hhmm, time_left, time_range};
use crate::ui::layout::osd_scale;
use crate::ui::overlays::OverlayResult;
use crate::ui::screens::live::{self, GuideStatus, playing_channel};
use crate::ui::video_host::{Placement, osd_text};
use crate::ui::widgets::list::{ListOutcome, ListState};
use crate::ui::widgets::{glyphs, logo, slider};
use crate::ui::{NavRequest, OverlayRequest, Route, Screen, ScreenId, UiCtx};

const NUMERIC_COMMIT: Duration = Duration::from_millis(1500);
const NEXT_EPISODE_DELAY: Duration = Duration::from_secs(10);
const FLASH: Duration = Duration::from_millis(1500);
/// Zoom steps cycled by `V` (mpv `video-zoom`, log2).
const ZOOMS: [f64; 4] = [0.0, 0.1, 0.25, 0.5];

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
    Guide,
    History,
    Info,
    Fullscreen,
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
            Control::Guide => glyphs::GUIDE,
            Control::History => glyphs::HISTORY,
            Control::Info => glyphs::INFO,
            Control::Fullscreen => glyphs::FULLSCREEN,
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
            Control::Guide => "Guide",
            Control::History => "History",
            Control::Info => "Info",
            Control::Fullscreen => "Fullscreen",
        }
    }
    /// Drawn as a glyph only (DESIGN §13.8 wireframe); the label stays the accessible name and
    /// the hover text.
    fn glyph_only(self) -> bool {
        matches!(self, Control::Prev | Control::PlayPause | Control::Next | Control::SkipBack | Control::SkipForward | Control::Fullscreen)
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
    fn active_controls(&self, snap: &PlayerSnapshot, narrow: bool) -> Vec<Control> {
        let all = Self::controls(snap, narrow);
        let drawn: Vec<Control> = all.iter().copied().filter(|c| self.visible.contains(c)).collect();
        if drawn.is_empty() { all } else { drawn }
    }

    /// Below 700 pt (× rem) the controls row collapses (DESIGN §13.8).
    fn narrow(width: f32, theme: &Theme) -> bool {
        width < 700.0 * theme.rem()
    }

    fn controls(snap: &PlayerSnapshot, narrow: bool) -> Vec<Control> {
        use Control as C;
        if narrow {
            return vec![C::PlayPause, C::Audio, C::Subtitles, C::Info];
        }
        if is_live(snap) {
            vec![C::Prev, C::PlayPause, C::Next, C::Audio, C::Subtitles, C::Guide, C::History, C::Info, C::Fullscreen]
        } else {
            vec![C::SkipBack, C::PlayPause, C::SkipForward, C::Audio, C::Subtitles, C::Info, C::Fullscreen]
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
                Some(z) if !z.channels.is_empty() => z.channels.to_vec(),
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
            Control::Guide => self.open_panel(ctx, PanelKind::MiniGuide),
            Control::History => self.open_panel(ctx, PanelKind::History),
            Control::Info => self.toggle_info(ctx),
            Control::Fullscreen => self.toggle_fullscreen = true,
        }
    }

    fn toggle_info(&mut self, ctx: &UiCtx<'_>) {
        self.info = !self.info;
        if self.info {
            ctx.play(PlayerCommand::RefreshStreamInfo);
            self.info_refreshed = Some(Instant::now());
        }
    }

    /// Controls focus mode (`PlayerOsd`).
    fn osd_action(&mut self, ctx: &mut UiCtx<'_>, action: Action, narrow: bool) -> Option<bool> {
        if !self.osd_focus {
            return None;
        }
        let snap = ctx.player.clone();
        let controls = self.active_controls(&snap, narrow);
        let n = controls.len();
        let seekable = !is_live(&snap) && snap.seekable;
        self.show_banner(ctx);
        match action {
            Action::LeaveOsdControls => self.osd_focus = false,
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
                    label: format!("S{:02}E{:02} · {}", e.season, e.episode, e.title),
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

        // ---- top bar: logo · number · title ……… clock · volume
        let top_h = title_font.size + 2.0 * pad;
        let top = Rect::from_min_size(rect.min, egui::vec2(rect.width(), top_h));
        p.rect_filled(top, 0.0, t.osd_scrim);
        let mut x = top.left() + pad;
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
        let vol = if snap.muted { format!("{} muted", glyphs::MUTED) } else { format!("{} {:.0} %", glyphs::VOLUME, snap.volume) };
        let vr =
            osd_text(&p, egui::pos2(top.right() - pad, top.center().y), Align2::RIGHT_CENTER, &vol, small_font.clone(), t.osd_text, f32::INFINITY);
        let cr = osd_text(
            &p,
            egui::pos2(vr.left() - pad, top.center().y),
            Align2::RIGHT_CENTER,
            &hhmm(ctx.now, h24),
            small_font.clone(),
            t.osd_text,
            f32::INFINITY,
        );
        let title = playable.as_ref().map(|p| p.title.as_str()).unwrap_or("");
        osd_text(&p, egui::pos2(x, top.center().y), Align2::LEFT_CENTER, title, title_font.clone(), t.osd_text, (cr.left() - pad - x).max(0.0));

        // ---- bottom bar
        let ch = playable.as_ref().and_then(|p| match p.media {
            MediaRef::Channel(id) | MediaRef::Catchup { channel: id, .. } => ctx.lib.live.get(id).cloned(),
            _ => None,
        });
        let nn = ch.as_ref().filter(|_| live).and_then(|c| live::now_next_of(ctx, c));
        let controls = Self::controls(snap, narrow);
        let ctrl_h = (theme.space(SpaceToken::ControlHeight) * s * 1.2).round();
        let line = body_font.size * 1.4;
        let lines = 3.0;
        let bottom_h = pad * 2.0 + line * lines + ctrl_h + pad / 2.0;
        let bottom = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - bottom_h), rect.max);
        p.rect_filled(bottom, 0.0, t.osd_scrim);
        self.bottom_bar = Some(bottom_h);
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

        // controls row: labelled where the design labels them; all glyphs when that doesn't
        // fit; whatever still doesn't fit isn't drawn (and focus mode skips it)
        let mut cx = inner.left();
        let cy = y + pad / 2.0;
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
        let glyphs_only = narrow || row_width(false) > inner.width();
        let mut drawn = Vec::with_capacity(controls.len());
        for (i, c) in controls.iter().enumerate() {
            let g = p.layout_no_wrap(text_of(*c, glyphs_only), small_font.clone(), t.osd_text);
            let r = Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(g.size().x + pad, ctrl_h));
            if r.right() > inner.right() {
                break;
            }
            drawn.push(*c);
            let resp = ui.interact(r, ui.id().with(("osd-control", i)), egui::Sense::click());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, c.label()));
            let resp = if glyphs_only || c.glyph_only() { resp.on_hover_text(control_text(*c, snap)) } else { resp };
            let focused = self.osd_focus && self.row == OsdRow::Buttons && i == self.control.min(controls.len() - 1);
            if resp.hovered() || focused {
                p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
            }
            if focused {
                p.rect_stroke(r, t.rounding, egui::Stroke::new(1.0, t.osd_accent), egui::StrokeKind::Inside);
            }
            p.galley(egui::pos2(r.left() + pad / 2.0, r.center().y - g.size().y / 2.0), g, if focused { t.osd_accent } else { t.osd_text });
            if resp.clicked() {
                clicked = Some((i, *c));
            }
            cx = r.right() + pad / 2.0;
        }
        self.control = self.control.min(drawn.len().saturating_sub(1));
        self.visible = drawn;
        if let Some((i, c)) = clicked {
            // a click on a control enters controls focus mode on it
            self.osd_focus = true;
            self.row = OsdRow::Buttons;
            self.control = i;
            self.show_banner(ctx);
            self.activate_control(ctx, c);
        }
    }

    /// `[HD] [5.1] [CC] [⟲]` right-aligned at `right_center`; returns their extent.
    fn badges(&self, p: &egui::Painter, theme: &Theme, snap: &PlayerSnapshot, catchup: bool, right_center: egui::Pos2, font: FontId, s: f32) -> Rect {
        let t = &theme.tokens;
        let mut labels: Vec<String> = Vec::new();
        if let Some((_, h)) = snap.video_size {
            labels.push(match h {
                h if h >= 2000 => "4K".into(),
                h if h >= 700 => "HD".into(),
                _ => "SD".into(),
            });
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

    fn draw_panel(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        if self.panel.is_none() {
            return;
        }
        let loading = self.guide.loading_label(ctx);
        let Some(panel) = &mut self.panel else { return };
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
        let p = ui.painter_at(rect);
        p.rect_filled(area, t.rounding, t.osd_scrim);
        let caption = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let title = match panel.kind {
            PanelKind::Channels => "CHANNELS (c)",
            PanelKind::MiniGuide => "NOW / NEXT (g)",
            PanelKind::History => "HISTORY (h)",
        };
        let q = panel.list.filter.query();
        let head = if q.is_empty() { title.to_owned() } else { format!("{title}  ·  {q}") };
        osd_text(
            &p,
            egui::pos2(area.left() + pad, area.top() + pad),
            Align2::LEFT_TOP,
            &head,
            caption.clone(),
            with_alpha(t.osd_text, 0.7),
            area.width() - 2.0 * pad,
        );
        let list_top = area.top() + pad + caption.size * 2.0;
        let mini = panel.kind == PanelKind::MiniGuide;
        let row_h = (if mini { font.size * 2.8 } else { font.size * 2.0 }).round();
        let visible = (((area.bottom() - pad - list_top) / row_h).floor() as usize).max(1);
        panel.list.page = visible;
        let first = panel.list.cursor.saturating_sub(visible.saturating_sub(1) / 2).min(panel.rows.len().saturating_sub(visible));
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
        let pointer_moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        let mut activate = None;
        for (k, ri) in (first..panel.rows.len()).take(visible).enumerate() {
            let id = panel.items[panel.rows[ri] as usize];
            let Some(ch) = ctx.lib.live.get(id).cloned() else { continue };
            let r = Rect::from_min_size(egui::pos2(area.left() + pad / 2.0, list_top + k as f32 * row_h), egui::vec2(area.width() - pad, row_h));
            let resp = ui.interact(r, ui.id().with(("panel-row", ri)), egui::Sense::click());
            resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, ch.name.as_ref()));
            if pointer_moved && resp.hovered() {
                panel.list.cursor = ri;
            }
            if resp.clicked() {
                panel.list.cursor = ri;
            }
            if resp.double_clicked() {
                activate = Some(id);
            }
            let cursor = ri == panel.list.cursor;
            if cursor {
                p.rect_filled(r, t.rounding, with_alpha(t.osd_text, 0.12));
            }
            let color = if cursor { t.osd_accent } else { t.osd_text };
            let mut x = r.left() + pad / 2.0;
            let num = ch.number.map(|n| n.to_string()).unwrap_or_default();
            let line_y = if mini { r.top() + row_h * 0.33 } else { r.center().y };
            let nr = osd_text(
                &p,
                egui::pos2(x + 3.0 * small.size, line_y),
                Align2::RIGHT_CENTER,
                &num,
                small.clone(),
                with_alpha(t.osd_text, 0.7),
                f32::INFINITY,
            );
            x = nr.right().max(x + 3.0 * small.size) + pad / 2.0;
            if playing == Some(id) {
                let g = osd_text(&p, egui::pos2(x, line_y), Align2::LEFT_CENTER, glyphs::PLAY, small.clone(), t.osd_accent, f32::INFINITY);
                x = g.right() + pad / 3.0;
            }
            osd_text(&p, egui::pos2(x, line_y), Align2::LEFT_CENTER, &ch.name, font.clone(), color, r.right() - pad / 2.0 - x);
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
                osd_text(
                    &p,
                    egui::pos2(x, r.top() + row_h * 0.72),
                    Align2::LEFT_CENTER,
                    &detail,
                    small.clone(),
                    with_alpha(t.osd_text, 0.7),
                    r.right() - pad / 2.0 - x,
                );
            }
        }
        if let Some(id) = activate
            && self.tune(ctx, id, Action::Activate)
        {
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

    fn draw_overlays(&mut self, ui: &egui::Ui, ctx: &UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
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
        // next episode card
        if let Some(n) = &self.next_episode {
            let left = n.at.saturating_duration_since(Instant::now()).as_secs_f32().ceil() as u64;
            let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
            let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
            let w = (420.0 * s).min(rect.width() - 2.0 * pad);
            let h = font.size * 3.6 + 2.0 * pad;
            // above the bottom OSD bar while it shows, else near the bottom edge
            let bottom = rect.bottom() - self.bottom_bar.unwrap_or(0.0) - 2.0 * pad;
            let r = Rect::from_min_size(egui::pos2(rect.right() - w - pad, (bottom - h).max(rect.top() + pad)), egui::vec2(w, h));
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
            osd_text(&p, egui::pos2(r.left() + pad, r.top() + pad + font.size * 1.3), Align2::LEFT_TOP, &n.label, font, t.osd_text, w - 2.0 * pad);
            osd_text(
                &p,
                egui::pos2(r.left() + pad, r.bottom() - pad),
                Align2::LEFT_BOTTOM,
                "Enter play now · Esc cancel",
                small,
                with_alpha(t.osd_text, 0.7),
                w - 2.0 * pad,
            );
        }
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
            Action::Back | Action::ToggleWindowFullscreen | Action::ToggleHelp | Action::OpenSearch | Action::Quit | Action::ShowJobs
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
                self.control = self.active_controls(ctx.player, narrow).iter().position(|c| *c == Control::PlayPause).unwrap_or(0);
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
            Action::Minimize => ctx.navigate(NavRequest::Back),
            Action::StopAndClose => {
                ctx.play(PlayerCommand::Stop);
                ctx.navigate(NavRequest::Back);
            }
            Action::Retry => {
                if matches!(ctx.player.state, PlayerState::Error(_) | PlayerState::Reconnecting { .. }) {
                    ctx.play(PlayerCommand::Reconnect);
                }
            }
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
                if ctx.player.previous_live.is_some() {
                    ctx.play(PlayerCommand::LastChannel { unlocked: ctx.parental.is_unlocked(&ctx.config.parental) });
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
        self.find_next_episode(ctx);
        if std::mem::take(&mut self.toggle_fullscreen) {
            let fs = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        }
        if snap.state == PlayerState::Idle && snap.playable.is_none() {
            let p = ui.painter_at(rect);
            p.rect_filled(rect, 0.0, Color32::BLACK);
            let t = &theme.tokens;
            osd_text(
                &p,
                rect.center(),
                Align2::CENTER_BOTTOM,
                "Nothing is playing",
                FontId::proportional(theme.font(TypeToken::Title)),
                t.osd_text,
                rect.width(),
            );
            osd_text(
                &p,
                rect.center() + egui::vec2(0.0, 8.0),
                Align2::CENTER_TOP,
                "Esc goes back",
                FontId::proportional(theme.font(TypeToken::Body)),
                with_alpha(t.osd_text, 0.7),
                rect.width(),
            );
            return;
        }
        let resp = ctx.video.paint(ui, rect, Placement::Fullscreen, theme, ctx.images, &snap);
        // mouse: motion shows the banner, double-click toggles window fullscreen, wheel = volume
        if ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO) && resp.hovered() {
            self.show_banner(ctx);
        }
        if resp.double_clicked() {
            let fs = ui.input(|i| i.viewport().fullscreen).unwrap_or(false);
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!fs));
        } else if resp.clicked() && self.osd_focus {
            self.osd_focus = false;
        }
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
            self.draw_osd(ui, ctx, rect, &snap);
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
    }

    fn on_overlay_result(&mut self, ctx: &mut UiCtx<'_>, result: OverlayResult) {
        if let OverlayResult::Unlocked(a) = result {
            self.on_action(ctx, a);
        }
    }
}
