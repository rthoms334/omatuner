//! The cog pop-up, anchored above its OSD button (DESIGN §13.8). Its pages share one
//! keyboard cursor; actions use the same paths as the controls and track pickers.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SettingsPage {
    Main,
    Tracks,
    Delay,
}

pub(super) struct SettingsMenu {
    page: SettingsPage,
    cursor: usize,
    parent: usize,
    scroll: bool,
    opened_frame: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Control(Control),
    Audio(i64),
    Subtitle(Option<i64>),
    LipSync,
    LipSyncReset,
    SubDelay,
}

struct MenuRow {
    choice: Choice,
    label: String,
    key: &'static str,
    selected: bool,
    heading: Option<String>,
}

impl Control {
    fn menu_key(self) -> &'static str {
        match self {
            Self::AudioSubtitles => "A / S",
            Self::Delay => "[ / ] · z / x",
            Self::Channels => "c",
            Self::Guide => "g",
            Self::History => "H",
            Self::UpNext => "u",
            Self::Info => "I",
            Self::PopOut => "Ctrl+Shift+O",
            Self::Cast => "C",
            Self::Fullscreen => "f",
            _ => "",
        }
    }
}

impl PlayerScreen {
    pub(super) fn settings_controls(snap: &PlayerSnapshot, narrow: bool, video_only: bool, optional: OptionalButtons) -> Vec<Control> {
        use Control as C;
        let mut rows = Vec::new();
        if snap.cast.is_none() {
            rows.extend([C::AudioSubtitles, C::Delay]);
        }
        if is_live(snap) {
            rows.extend([C::Channels, C::Guide, C::History]);
        }
        rows.extend(optional.panel.then_some(C::UpNext));
        rows.push(C::Info);
        if narrow {
            rows.extend(optional.cast.then_some(C::Cast));
            rows.push(C::Fullscreen);
            // wide windows have it on the row, beside Fullscreen
            if !video_only && snap.cast.is_none() {
                rows.push(C::PopOut);
            }
        }
        rows
    }

    pub(super) fn open_settings(&mut self, ctx: &UiCtx<'_>, page: SettingsPage) {
        self.settings = Some(SettingsMenu { page, cursor: 0, parent: 0, scroll: true, opened_frame: None });
        self.show_banner(ctx);
    }

    fn settings_rows(&self, ctx: &UiCtx<'_>) -> Vec<MenuRow> {
        let snap = ctx.player;
        let row = |choice, label: String, key| MenuRow { choice, label, key, selected: false, heading: None };
        match self.settings.as_ref().map(|m| m.page).unwrap_or(SettingsPage::Main) {
            SettingsPage::Main => Self::settings_controls(snap, self.controls_narrow, ctx.video_only, OptionalButtons::of(ctx))
                .into_iter()
                .map(|c| row(Choice::Control(c), c.label().into(), c.menu_key()))
                .collect(),
            SettingsPage::Tracks => {
                let mut rows = Vec::new();
                for (i, track) in snap.tracks.audio.iter().enumerate() {
                    rows.push(MenuRow {
                        selected: track.selected,
                        heading: (i == 0).then(|| "Audio".into()),
                        ..row(Choice::Audio(track.id), track.label(), "")
                    });
                }
                rows.push(MenuRow {
                    selected: snap.tracks.selected_sub().is_none(),
                    heading: Some(if rows.is_empty() { "No audio tracks · Subtitles" } else { "Subtitles" }.into()),
                    ..row(Choice::Subtitle(None), "Off".into(), "")
                });
                for track in &snap.tracks.subs {
                    rows.push(MenuRow { selected: track.selected, ..row(Choice::Subtitle(Some(track.id)), track.label(), "") });
                }
                rows
            }
            SettingsPage::Delay => vec![
                MenuRow {
                    heading: Some(snap.audio_output.label.clone()),
                    ..row(Choice::LipSync, format!("Delay: {}", crate::player::audio_output::offset_text(snap.lip_sync_ms)), "[ / ]")
                },
                row(Choice::LipSyncReset, "Reset delay".into(), ""),
                MenuRow {
                    heading: Some("Subtitles".into()),
                    ..row(Choice::SubDelay, format!("Subtitle delay: {:+.1} s", snap.sub_delay), "z / x · Z")
                },
            ],
        }
    }

    fn settings_back(&mut self) {
        let Some(menu) = self.settings.as_mut() else { return };
        if menu.page == SettingsPage::Main {
            self.settings = None;
        } else {
            menu.page = SettingsPage::Main;
            menu.cursor = menu.parent;
            menu.scroll = true;
        }
    }

    fn choose_setting(&mut self, ctx: &mut UiCtx<'_>, choice: Choice) {
        match choice {
            Choice::Control(Control::AudioSubtitles | Control::Delay) => {
                let menu = self.settings.as_mut().unwrap();
                menu.parent = menu.cursor;
                menu.page = if choice == Choice::Control(Control::AudioSubtitles) { SettingsPage::Tracks } else { SettingsPage::Delay };
                menu.cursor = 0;
                menu.scroll = true;
            }
            Choice::Control(control) => {
                self.settings = None;
                self.activate_control(ctx, control);
            }
            Choice::Audio(id) => {
                ctx.play(PlayerCommand::SetAudioTrack(Some(id)));
                self.settings = None;
            }
            Choice::Subtitle(id) => {
                ctx.play(PlayerCommand::SetSubtitleTrack(id));
                self.settings = None;
            }
            Choice::LipSync | Choice::LipSyncReset => ctx.edit_lip_sync(ctx.player.audio_output.clone(), None),
            Choice::SubDelay => ctx.play(PlayerCommand::ResetSubDelay),
        }
        self.show_banner(ctx);
    }

    fn step_setting(ctx: &mut UiCtx<'_>, choice: Choice, up: bool) {
        match choice {
            Choice::LipSync => ctx.edit_lip_sync(ctx.player.audio_output.clone(), Some(if up { 1 } else { -1 })),
            Choice::SubDelay => ctx.play(PlayerCommand::AdjustSubDelay(if up { 0.1 } else { -0.1 })),
            _ => {}
        }
    }

    /// A cast may have begun with a sub-page open; never send local track / delay commands
    /// to it. The main page also re-evaluates conditional controls each frame.
    fn settle_settings(&mut self, snap: &PlayerSnapshot) {
        if snap.cast.is_some() && self.settings.as_ref().is_some_and(|m| m.page != SettingsPage::Main) {
            self.settings_back();
        }
    }

    pub(super) fn settings_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        self.settings.as_ref()?;
        self.settle_settings(ctx.player);
        let rows = self.settings_rows(ctx);
        let menu = self.settings.as_mut().unwrap();
        menu.cursor = menu.cursor.min(rows.len().saturating_sub(1));
        match action {
            Action::Up | Action::FocusPrev => {
                menu.cursor = menu.cursor.saturating_sub(1);
                menu.scroll = true;
            }
            Action::Down | Action::FocusNext => {
                menu.cursor = (menu.cursor + 1).min(rows.len().saturating_sub(1));
                menu.scroll = true;
            }
            Action::Back | Action::LeaveOsdControls | Action::Left | Action::FilterBackspace => self.settings_back(),
            Action::PlayerSettings => self.settings = None,
            Action::Activate | Action::Right => {
                if let Some(row) = rows.get(menu.cursor) {
                    if action == Action::Right && matches!(row.choice, Choice::LipSync | Choice::SubDelay) {
                        Self::step_setting(ctx, row.choice, true);
                    } else {
                        self.choose_setting(ctx, row.choice);
                    }
                }
            }
            // Adjustments keep the pop-up open. Other shortcuts dismiss it before opening
            // a panel / overlay or navigating, so it cannot cover the next view.
            Action::PlayPause
            | Action::Mute
            | Action::VolumeUp
            | Action::VolumeDown
            | Action::CycleAudio
            | Action::CycleSubtitle
            | Action::LipSyncEarlier
            | Action::LipSyncLater
            | Action::SubDelayMinus
            | Action::SubDelayPlus
            | Action::SubDelayReset
            | Action::SubSizeMinus
            | Action::SubSizePlus => return None,
            _ => {
                self.settings = None;
                return None;
            }
        }
        self.show_banner(ctx);
        Some(true)
    }

    pub(super) fn draw_settings(&mut self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, screen: Rect) {
        if self.settings.is_none() {
            return;
        }
        self.settle_settings(ctx.player);
        let rows = self.settings_rows(ctx);
        let menu = self.settings.as_mut().unwrap();
        menu.cursor = menu.cursor.min(rows.len().saturating_sub(1));
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let pad = theme.space(SpaceToken::PopupPadding) * s;
        let row_h = (theme.space(SpaceToken::PopupRowHeight) + theme.space(SpaceToken::Lg)) * s;
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let anchor =
            self.settings_anchor.unwrap_or_else(|| Rect::from_min_max(screen.right_bottom() - egui::vec2(row_h, row_h), screen.right_bottom()));
        let width = (420.0 * theme.rem() * s).min(screen.width() - 2.0 * pad);
        let headings = rows.iter().filter(|r| r.heading.is_some()).count();
        let height =
            (row_h * (rows.len() + 1) as f32 + headings as f32 * row_h * 0.65 + 2.0 * pad).min((anchor.top() - screen.top() - pad).max(row_h * 2.0));
        let pos = egui::pos2((anchor.right() - width).max(screen.left() + pad), (anchor.top() - height).max(screen.top()));
        let panel = Rect::from_min_size(pos, egui::vec2(width, height));
        let frame = ui.ctx().cumulative_frame_nr();
        let opening = *menu.opened_frame.get_or_insert(frame) == frame;
        let mut close = false;
        // The same catcher / foreground layering as the existing track picker. It prevents
        // an outside click from also pausing, zapping or leaving OSD focus.
        egui::Area::new(ui.id().with("settings-catcher")).movable(false).order(egui::Order::Foreground).fixed_pos(screen.min).show(ui.ctx(), |ui| {
            let response = ui.allocate_rect(screen, egui::Sense::click_and_drag());
            // The click that opened the cog must not also dismiss it.
            close = !opening && (response.clicked() || response.secondary_clicked());
        });
        let mut back = false;
        let mut chosen = None;
        let mut step = None;
        let scroll = std::mem::take(&mut menu.scroll);
        let moved = ui.input(|i| i.pointer.delta() != egui::Vec2::ZERO);
        egui::Area::new(ui.id().with("settings-popup")).movable(false).order(egui::Order::Tooltip).fixed_pos(panel.min).show(ui.ctx(), |ui| {
            ui.set_min_size(panel.size());
            ui.set_max_size(panel.size());
            ui.painter().rect_filled(panel, t.rounding, t.osd_scrim);
            let inner = panel.shrink(pad);
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::top_down(egui::Align::Min)));
            ui.spacing_mut().item_spacing = egui::Vec2::ZERO;
            let title = match menu.page {
                SettingsPage::Main => "Player settings",
                SettingsPage::Tracks => "Audio & subtitles",
                SettingsPage::Delay => "Delay",
            };
            let (header, _) = ui.allocate_exact_size(egui::vec2(inner.width(), row_h), egui::Sense::hover());
            let back_rect = Rect::from_min_size(header.min, egui::vec2(row_h, row_h));
            let close_rect = Rect::from_min_size(egui::pos2(header.right() - row_h, header.top()), egui::vec2(row_h, row_h));
            let title_left = if menu.page != SettingsPage::Main {
                back = osd_button(
                    &ui,
                    ui.painter(),
                    theme,
                    back_rect,
                    ui.id().with("back"),
                    glyphs::ARROW_LEFT,
                    font.clone(),
                    "Back to player settings",
                    Some("Back (← / Backspace / Esc)"),
                    false,
                )
                .clicked();
                back_rect.right()
            } else {
                header.left()
            };
            osd_text(
                ui.painter(),
                egui::pos2(title_left, header.center().y),
                Align2::LEFT_CENTER,
                title,
                font.clone(),
                t.osd_text,
                close_rect.left() - title_left,
            );
            close |= osd_button(
                &ui,
                ui.painter(),
                theme,
                close_rect,
                ui.id().with("close"),
                glyphs::CLOSE,
                font.clone(),
                "Close player settings",
                Some("Close"),
                false,
            )
            .clicked();
            egui::ScrollArea::vertical()
                .id_salt(("settings-rows", menu.page as u8))
                .max_height((inner.height() - row_h).max(0.0))
                .auto_shrink([false, false])
                .show(&mut ui, |ui| {
                    for (i, row) in rows.iter().enumerate() {
                        if let Some(heading) = &row.heading {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h * 0.65), egui::Sense::hover());
                            osd_text(
                                ui.painter(),
                                r.left_center(),
                                Align2::LEFT_CENTER,
                                heading,
                                small.clone(),
                                with_alpha(t.osd_text, 0.7),
                                r.width(),
                            );
                        }
                        let (r, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), egui::Sense::hover());
                        if moved && ui.rect_contains_pointer(r) {
                            menu.cursor = i;
                        }
                        let focused = i == menu.cursor;
                        if focused && scroll {
                            ui.scroll_to_rect(r, None);
                        }
                        let stepped = matches!(row.choice, Choice::LipSync | Choice::SubDelay);
                        let main = if stepped { Rect::from_min_max(r.min, r.right_bottom() - egui::vec2(row_h * 2.0, 0.0)) } else { r };
                        let response = osd_button(
                            ui,
                            ui.painter(),
                            theme,
                            main,
                            ui.id().with(("setting", i)),
                            "",
                            font.clone(),
                            &row.label,
                            Some(if stepped { "Click to reset; use ‹ / › to adjust" } else { &row.label }),
                            focused,
                        );
                        if response.clicked() {
                            chosen = Some(row.choice);
                        }
                        let color = if focused { t.osd_accent } else { t.osd_text };
                        let mark = match row.choice {
                            Choice::Control(c) => c.glyph(ctx.player),
                            _ if row.selected => glyphs::CHECK,
                            _ => "",
                        };
                        osd_text(
                            ui.painter(),
                            main.left_center() + egui::vec2(pad / 2.0, 0.0),
                            Align2::LEFT_CENTER,
                            mark,
                            font.clone(),
                            if row.selected { t.osd_accent } else { color },
                            row_h,
                        );
                        let subpage = matches!(row.choice, Choice::Control(Control::AudioSubtitles | Control::Delay));
                        let key = if subpage { format!("{}  ›", row.key) } else { row.key.into() };
                        let kr = osd_text(
                            ui.painter(),
                            main.right_center() - egui::vec2(pad / 2.0, 0.0),
                            Align2::RIGHT_CENTER,
                            &key,
                            small.clone(),
                            with_alpha(t.osd_text, 0.7),
                            main.width() * 0.4,
                        );
                        let left = main.left_center() + egui::vec2(row_h, 0.0);
                        osd_text(ui.painter(), left, Align2::LEFT_CENTER, &row.label, font.clone(), color, (kr.left() - pad - left.x).max(0.0));
                        if stepped {
                            for (up, glyph) in [(false, "‹"), (true, "›")] {
                                let r =
                                    Rect::from_min_size(egui::pos2(main.right() + if up { row_h } else { 0.0 }, r.top()), egui::vec2(row_h, row_h));
                                let label = match (row.choice, up) {
                                    (Choice::LipSync, false) => "Sound earlier",
                                    (Choice::LipSync, true) => "Sound later",
                                    (_, false) => "Subtitles earlier",
                                    _ => "Subtitles later",
                                };
                                if osd_button(
                                    ui,
                                    ui.painter(),
                                    theme,
                                    r,
                                    ui.id().with(("step", i, up)),
                                    glyph,
                                    font.clone(),
                                    label,
                                    Some(label),
                                    false,
                                )
                                .clicked()
                                {
                                    step = Some((row.choice, up));
                                }
                            }
                        }
                    }
                });
        });
        if close {
            self.settings = None;
        } else if back {
            self.settings_back();
        } else if let Some((choice, up)) = step {
            Self::step_setting(ctx, choice, up);
        } else if let Some(choice) = chosen {
            self.choose_setting(ctx, choice);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::ui::screens::home::testkit::{App, has_label, painted, until};
    use crate::ui::screens::player::tests::{WIDE, playable, player, press, real, shown};

    type H = egui_kittest::Harness<'static, App<PlayerScreen>>;

    fn key(h: &mut H, key: egui::Key) {
        let event = egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE };
        let stack = h.state().s.key_contexts();
        let action = crate::keymap::resolve(&stack, &event).unwrap();
        assert!(h.state_mut().act(action), "{stack:?}: {key:?}");
    }

    fn choose(h: &mut H, control: Control) {
        assert!(h.state_mut().act(Action::PlayerSettings));
        let app = h.state_mut();
        let rows = app.t.with_ctx(|ctx| app.s.settings_rows(ctx));
        app.s.settings.as_mut().unwrap().cursor = rows.iter().position(|r| r.choice == Choice::Control(control)).unwrap();
        key(h, egui::Key::Enter);
    }

    /// Pop out is a row button right after Fullscreen (not in the cog), and the VOD seek bar
    /// carries no "−n / +n s" hint.
    #[test]
    fn pop_out_sits_beside_fullscreen_and_the_skip_hint_is_gone() {
        let mut h = player(WIDE);
        let mut snap = (*shown(&h)).clone();
        snap.playable = Some(Arc::new(crate::model::Playable { kind: PlayableKind::Movie, ..(*playable()).clone() }));
        snap.seekable = true;
        snap.position = Some(60.0);
        snap.duration = Some(3600.0);
        h.state_mut().t.snapshot = Arc::new(snap);
        h.run_steps(3);
        let fullscreen = h.get_by_label("Fullscreen").rect();
        let pop_out = h.get_by_label("Pop out").rect();
        let cog = h.get_by_label("Player settings").rect();
        assert!(fullscreen.right() <= pop_out.left() && pop_out.right() <= cog.left(), "{fullscreen:?} {pop_out:?} {cog:?}");
        let n = h.state().t.config.player.skip_small_s;
        assert!(!painted(&h).iter().any(|t| t.contains(&format!("−{n} / +{n} s"))), "{:?}", painted(&h));
        h.get_by_label("Pop out").click();
        h.run_steps(1);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::TogglePopOut]);
        let app = h.state_mut();
        app.act(Action::PlayerSettings);
        let rows = app.t.with_ctx(|ctx| app.s.settings_rows(ctx));
        assert!(!rows.iter().any(|r| r.choice == Choice::Control(Control::PopOut)));
    }

    #[test]
    fn row_and_menu_contents_follow_playback_width_and_window() {
        use Control as C;
        let mut snap = PlayerSnapshot { playable: Some(playable()), ..Default::default() };
        let none = OptionalButtons::default();
        assert_eq!(
            PlayerScreen::controls(&snap, false, false, none),
            vec![C::Prev, C::Next, C::AudioSubtitles, C::Fullscreen, C::PopOut, C::Settings]
        );
        assert_eq!(
            PlayerScreen::controls(&snap, false, true, none),
            vec![C::PlayPause, C::Prev, C::Next, C::AudioSubtitles, C::Fullscreen, C::Settings]
        );
        assert_eq!(
            PlayerScreen::settings_controls(&snap, false, false, none),
            vec![C::AudioSubtitles, C::Delay, C::Channels, C::Guide, C::History, C::Info]
        );
        snap.seekable = true;
        snap.live_buffer = Some(crate::player::LiveBufferRange { start: 0.0, end: 100.0 });
        snap.position = Some(50.0);
        let optional = OptionalButtons { panel: true, next: true, cast: true };
        for popped in [false, true] {
            let transport = if popped { vec![C::SkipBack, C::PlayPause, C::SkipForward] } else { vec![] };
            // Pop out sits right after Fullscreen, and not in the popped-out window itself
            let pop_out = if popped { vec![] } else { vec![C::PopOut] };
            assert_eq!(
                PlayerScreen::controls(&snap, false, popped, optional),
                [
                    transport.clone(),
                    vec![C::BackToLive, C::Prev, C::Next, C::QueueNext, C::AudioSubtitles, C::Cast, C::Fullscreen],
                    pop_out,
                    vec![C::Settings]
                ]
                .concat()
            );
            assert!(
                !PlayerScreen::settings_controls(&snap, false, popped, optional).contains(&C::PopOut),
                "wide: Pop out is on the row, not in the cog"
            );
            assert_eq!(
                PlayerScreen::controls(&snap, true, popped, optional),
                [transport, vec![C::BackToLive, C::Prev, C::Next, C::QueueNext, C::AudioSubtitles, C::Settings]].concat()
            );
            let menu = PlayerScreen::settings_controls(&snap, true, popped, optional);
            assert!(menu.contains(&C::UpNext) && menu.contains(&C::Cast) && menu.contains(&C::Fullscreen));
            assert_eq!(menu.contains(&C::PopOut), !popped);
        }
        snap.playable = Some(Arc::new(crate::model::Playable { kind: PlayableKind::Movie, ..(*playable()).clone() }));
        assert_eq!(
            PlayerScreen::controls(&snap, false, false, optional),
            vec![C::QueueNext, C::AudioSubtitles, C::Cast, C::Fullscreen, C::PopOut, C::Settings]
        );
        assert_eq!(PlayerScreen::controls(&snap, true, false, optional), vec![C::QueueNext, C::AudioSubtitles, C::Settings]);
        assert_eq!(
            PlayerScreen::controls(&snap, true, true, optional),
            vec![C::SkipBack, C::PlayPause, C::SkipForward, C::QueueNext, C::AudioSubtitles, C::Settings]
        );
        assert_eq!(PlayerScreen::settings_controls(&snap, false, false, optional), vec![C::AudioSubtitles, C::Delay, C::UpNext, C::Info]);
        snap.cast = Some(crate::model::CastView {
            device: crate::model::DeviceId::from_bytes([1; 16]),
            name: "TV".into(),
            kind: crate::model::ReceiverKind::AppleTv,
            phase: crate::model::CastPhase::Playing,
            level: None,
            as_is: false,
            behind_live_s: None,
            can_seek: true,
            volume: false,
        });
        assert_eq!(PlayerScreen::controls(&snap, false, false, optional), vec![C::QueueNext, C::Cast, C::Fullscreen, C::Settings]);
        assert_eq!(PlayerScreen::controls(&snap, true, false, optional), vec![C::QueueNext, C::Settings]);
        assert_eq!(PlayerScreen::settings_controls(&snap, true, false, optional), vec![C::UpNext, C::Info, C::Cast, C::Fullscreen]);
    }

    #[test]
    fn cog_focus_space_and_escape_walk_the_pages_before_leaving_osd() {
        let mut h = player(WIDE);
        h.state_mut().act(Action::FocusOsdControls);
        h.run_steps(2);
        let count = h.state().s.visible.len();
        for _ in 0..count {
            if h.state().s.visible[h.state().s.control] == Control::Settings {
                break;
            }
            key(&mut h, egui::Key::ArrowRight);
        }
        assert_eq!(h.state().s.visible[h.state().s.control], Control::Settings);
        let paused = real(&h).paused;
        key(&mut h, egui::Key::Space);
        assert!(h.state().s.settings.is_some());
        assert_eq!(real(&h).paused, paused);
        key(&mut h, egui::Key::ArrowRight);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Tracks);
        for back in [egui::Key::ArrowLeft, egui::Key::Backspace] {
            key(&mut h, back);
            assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Main);
            key(&mut h, egui::Key::Enter);
        }
        key(&mut h, egui::Key::Escape);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Main);
        key(&mut h, egui::Key::Escape);
        assert!(h.state().s.settings.is_none() && h.state().s.osd_focus);
        key(&mut h, egui::Key::Escape);
        assert!(!h.state().s.osd_focus);
        assert!(h.state().s.banner_shown(&shown(&h)));
    }

    #[test]
    fn tracks_button_opens_the_same_page_with_selection_ticks_and_no_row_labels() {
        let mut h = player(WIDE);
        let mut snap = (*shown(&h)).clone();
        snap.tracks = crate::player::tracks::parse_track_list(
            r#"[
            {"id":1,"type":"audio","title":"English","selected":true},
            {"id":2,"type":"audio","title":"French"},
            {"id":3,"type":"sub","title":"Captions","selected":true}
        ]"#,
        );
        h.state_mut().t.snapshot = Arc::new(snap);
        h.run_steps(2);
        assert!(!painted(&h).iter().any(|s| s.contains("English") || s.contains("Captions")));
        h.get_by_label("Audio & subtitles").click();
        h.run_steps(3);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Tracks);
        let app = h.state_mut();
        let rows = app.t.with_ctx(|ctx| app.s.settings_rows(ctx));
        assert_eq!(rows.iter().filter(|r| r.selected).map(|r| r.choice).collect::<Vec<_>>(), vec![Choice::Audio(1), Choice::Subtitle(Some(3))]);
        assert_eq!(painted(&h).iter().filter(|s| s.as_str() == glyphs::CHECK).count(), 2);
        assert!(has_label(&h, "English") && has_label(&h, "French") && has_label(&h, "Captions") && has_label(&h, "Off"));
        h.get_by_label("French").click();
        h.run_steps(2);
        assert!(h.state().s.settings.is_none(), "picking a track closes just like the old picker");
        choose(&mut h, Control::AudioSubtitles);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Tracks);
        key(&mut h, egui::Key::Escape);
        key(&mut h, egui::Key::Escape);
        h.state_mut().act(Action::FocusOsdControls);
        for _ in 0..h.state().s.visible.len() {
            if h.state().s.visible[h.state().s.control] == Control::AudioSubtitles {
                break;
            }
            key(&mut h, egui::Key::ArrowRight);
        }
        assert_eq!(h.state().s.visible[h.state().s.control], Control::AudioSubtitles);
        key(&mut h, egui::Key::Enter);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Tracks);
    }

    #[test]
    fn menu_rows_keep_the_old_actions() {
        let mut h = player(egui::vec2(600.0, 400.0));
        for (control, kind) in
            [(Control::Channels, PanelKind::Channels), (Control::Guide, PanelKind::MiniGuide), (Control::History, PanelKind::History)]
        {
            choose(&mut h, control);
            assert_eq!(h.state().s.panel.as_ref().map(|p| p.kind), Some(kind));
            assert!(h.state().s.settings.is_none());
            h.state_mut().s.panel = None;
        }
        choose(&mut h, Control::Info);
        assert!(h.state().s.info && h.state().s.settings.is_none());
        choose(&mut h, Control::Info);
        assert!(!h.state().s.info, "Info still toggles");
        h.state_mut().nav();
        choose(&mut h, Control::PopOut);
        assert_eq!(h.state_mut().nav(), vec![NavRequest::TogglePopOut]);
        choose(&mut h, Control::Fullscreen);
        assert!(h.state().s.toggle_fullscreen);
        if crate::cast::ENABLED {
            choose(&mut h, Control::Cast);
            assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::CastPicker)]);
        }
        if crate::queue::ENABLED {
            choose(&mut h, Control::UpNext);
            assert_eq!(h.state().s.panel.as_ref().map(|p| p.kind), Some(PanelKind::Queue));
        }
    }

    #[test]
    fn a_moved_panel_control_still_toggles_its_existing_panel() {
        let mut h = player(WIDE);
        h.state_mut().act(Action::ChannelList);
        h.run_steps(2);
        h.get_by_label("Player settings").click();
        h.run_steps(3);
        assert!(h.state().s.panel.is_some() && h.state().s.settings.is_some());
        h.get_by_label("Channels").click();
        h.run_steps(2);
        assert!(h.state().s.panel.is_none() && h.state().s.settings.is_none());
    }

    #[test]
    fn delay_steps_and_resets_use_the_existing_output_and_player_controls() {
        let mut h = player(WIDE);
        choose(&mut h, Control::Delay);
        h.run_steps(3);
        h.get_by_label("Sound earlier").click();
        h.run_steps(2);
        h.state_mut().apply_config();
        let output = shown(&h).audio_output.name.clone();
        assert_eq!(h.state().t.config.player.lip_sync[&output].offset_ms, -50);
        key(&mut h, egui::Key::ArrowRight);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.player.lip_sync[&output].offset_ms, 0);
        h.state_mut().act(Action::LipSyncLater);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.player.lip_sync[&output].offset_ms, 50);
        h.get_by_label("Reset delay").click();
        h.run_steps(2);
        h.state_mut().apply_config();
        assert_eq!(h.state().t.config.player.lip_sync[&output].offset_ms, 0);
        h.get_by_label("Subtitles later").click();
        until(&mut h, "subtitle delay", |h| (real(h).sub_delay - 0.1).abs() < 0.01);
        h.state_mut().s.settings.as_mut().unwrap().cursor = 2;
        key(&mut h, egui::Key::Enter);
        until(&mut h, "subtitle delay reset", |h| real(h).sub_delay.abs() < 0.01);
        assert_eq!(h.state().s.settings.as_ref().unwrap().page, SettingsPage::Delay);
    }

    #[test]
    fn popup_rises_from_cog_holds_banner_and_catches_outside_clicks() {
        let mut h = player(WIDE);
        h.state_mut().act(Action::FocusOsdControls);
        h.get_by_label("Player settings").click();
        h.run_steps(3);
        let cog = h.get_by_label("Player settings").rect();
        assert!(h.get_by_label("Channels").rect().bottom() < cog.top());
        h.state_mut().s.banner_until = None;
        h.state_mut().s.banner_dismissed = true;
        assert!(h.state().s.banner_visible(&shown(&h)));
        let paused = real(&h).paused;
        let point = h.get_by_label("Centre play / pause").rect().center();
        h.hover_at(point);
        press(&h, point, egui::PointerButton::Primary);
        h.run_steps(3);
        assert!(h.state().s.settings.is_none() && h.state().s.osd_focus);
        assert_eq!(real(&h).paused, paused, "the outside click cannot activate the covered control");
    }

    #[test]
    fn other_overlays_dismiss_the_cog_before_their_shortcut_falls_through() {
        let mut h = player(WIDE);
        for action in [Action::ToggleHelp, Action::OpenSearch, Action::ShowJobs] {
            h.state_mut().act(Action::PlayerSettings);
            assert!(!h.state_mut().act(action), "the app handles {action:?}");
            assert!(h.state().s.settings.is_none());
        }
        h.state_mut().nav();
        h.state_mut().act(Action::PlayerSettings);
        assert!(h.state_mut().act(Action::AudioMenu));
        assert!(h.state().s.settings.is_none());
        assert_eq!(h.state_mut().nav(), vec![NavRequest::Open(OverlayRequest::Tracks { audio: true })]);
    }

    #[test]
    fn narrow_and_popped_out_rows_fit_and_multi_view_omits_cast_and_queue_next() {
        for popped in [false, true] {
            let mut h = player(egui::vec2(480.0, 360.0));
            h.state_mut().t.video_only = popped;
            let mut snap = (*shown(&h)).clone();
            snap.seekable = true;
            snap.live_buffer = Some(crate::player::LiveBufferRange { start: 0.0, end: 100.0 });
            snap.position = Some(50.0);
            h.state_mut().t.snapshot = Arc::new(snap);
            h.run_steps(3);
            let mut labels = if popped { vec!["Back", "Play / pause", "Forward"] } else { vec![] };
            labels.extend(["Back to live", "Previous channel", "Next channel", "Mute", "Audio & subtitles", "Player settings"]);
            let rects: Vec<_> = labels.iter().map(|s| h.get_by_label(s).rect()).collect();
            assert!(rects.windows(2).all(|pair| pair[0].right() <= pair[1].left()), "{rects:?}");
            assert!(rects.last().unwrap().right() <= 480.0);
            for label in ["Back", "Play / pause", "Forward"] {
                assert_eq!(has_label(&h, label), popped, "{label} belongs only to the pop-out row");
            }
            for label in ["Skip back", "Centre play / pause", "Skip forward"] {
                assert_eq!(has_label(&h, label), !popped, "{label} belongs only to the full window's picture");
                if !popped {
                    assert!(h.get_by_label(label).rect().bottom() < rects[0].top());
                }
            }
            assert!(!has_label(&h, "Volume") && !has_label(&h, "Fullscreen") && !has_label(&h, "Cast to a TV") && !has_label(&h, "Pop out"));
            h.state_mut().t.multiview.view.on = true;
            let optional = h.state_mut().t.with_ctx(|ctx| OptionalButtons::of(ctx));
            assert!(!optional.next && !optional.cast);
            h.state_mut().act(Action::PlayerSettings);
            let app = h.state_mut();
            let rows = app.t.with_ctx(|ctx| app.s.settings_rows(ctx));
            assert!(!rows.iter().any(|r| r.choice == Choice::Control(Control::Cast)));
            assert_eq!(rows.iter().any(|r| r.choice == Choice::Control(Control::PopOut)), !popped);
        }
    }
}
