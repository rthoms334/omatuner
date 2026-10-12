//! Side panels: channels, mini guide, history (owner WP-11). Q-ui adds Up next's `u` panel
//! here (DESIGN §32): what is still to play, Enter plays from here, Alt+↑/↓ moves, Delete
//! removes (Undo), letters filter, ←/Esc close; ✕ in the header, a click plays.

use super::*;
use crate::queue::ui::list::{Look, Mode, Out, QueueList};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PanelKind {
    Channels,
    MiniGuide,
    History,
    /// Up next (`u`).
    Queue,
}

pub(super) struct SidePanel {
    pub(super) kind: PanelKind,
    /// The cursor and filter (the queue panel's live here between frames too, so the key
    /// context follows its filter).
    pub(super) list: ListState,
    pub(super) items: Vec<ChannelId>,
    pub(super) haystacks: Vec<String>,
    pub(super) rows: Vec<u32>,
    /// The Up next panel's list.
    pub(super) queue: Option<Box<QueueList>>,
}

impl SidePanel {
    pub(super) fn refilter(&mut self) {
        self.rows = self.list.filter.apply(self.haystacks.iter().map(String::as_str));
        self.list.set_len(self.rows.len());
    }
    pub(super) fn cursor_item(&self) -> Option<ChannelId> {
        self.rows.get(self.list.cursor).and_then(|&i| self.items.get(i as usize)).copied()
    }
}

impl PlayerScreen {
    pub(super) fn open_panel(&mut self, ctx: &mut UiCtx<'_>, kind: PanelKind) {
        if self.panel.as_ref().is_some_and(|p| p.kind == kind) {
            self.panel = None;
            return;
        }
        if kind == PanelKind::Queue {
            let s = crate::queue::ui::snapshot(ctx);
            let mut q = Box::new(QueueList::default());
            q.sync(s.clone(), Mode::Panel, crate::queue::ui::sleep_mark(ctx));
            if let Some(id) = s.current.or(s.next) {
                q.select(id);
            }
            let list = std::mem::take(&mut q.list);
            self.panel = Some(SidePanel { kind, list, items: Vec::new(), haystacks: Vec::new(), rows: Vec::new(), queue: Some(q) });
            return;
        }
        let snap = ctx.player;
        let current = playing_channel(snap);
        let items: Vec<ChannelId> = match kind {
            PanelKind::Queue => Vec::new(),
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
        let mut p = SidePanel { kind, list: ListState::default(), items, haystacks, rows: Vec::new(), queue: None };
        p.refilter();
        if let Some(i) = current.and_then(|c| p.rows.iter().position(|&r| p.items[r as usize] == c)) {
            p.list.select(i);
        }
        self.panel = Some(p);
    }

    /// Run `f` on the Up next panel's list with its cursor and filter in place.
    fn with_queue<R>(&mut self, ctx: &UiCtx<'_>, f: impl FnOnce(&mut QueueList) -> R) -> Option<R> {
        let p = self.panel.as_mut()?;
        let q = p.queue.as_mut()?;
        std::mem::swap(&mut p.list, &mut q.list);
        q.sync(crate::queue::ui::snapshot(ctx), Mode::Panel, crate::queue::ui::sleep_mark(ctx));
        let r = f(q);
        std::mem::swap(&mut p.list, &mut q.list);
        Some(r)
    }

    /// The list's answer, in the panel: a play closes it, a toast flashes.
    fn queue_out(&mut self, ctx: &mut UiCtx<'_>, out: Out) -> Option<bool> {
        match out {
            Out::None => return None,
            Out::Handled | Out::Menu(_) => {}
            Out::Parent => self.panel = None,
            Out::Play { id, .. } => {
                let s = crate::queue::ui::snapshot(ctx);
                if !(s.driving && s.current == Some(id)) {
                    crate::queue::ui::send(ctx, crate::queue::QueueCommand::PlayAt(id));
                }
                self.panel = None;
                self.show_banner(ctx);
            }
            Out::Send(cmds) => {
                for c in cmds {
                    crate::queue::ui::send(ctx, c);
                }
            }
            Out::Remove(ids) => {
                let s = crate::queue::ui::snapshot(ctx);
                crate::queue::ui::remove(ctx, &s, ids);
            }
            Out::Toast(t) => self.flash_text = Some((t.into(), Instant::now())),
        }
        Some(true)
    }

    fn queue_panel_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if action == Action::Back {
            self.panel = None;
            return Some(true);
        }
        let out = self.with_queue(ctx, |q| q.handle(action))?;
        if let Some(h) = self.queue_out(ctx, out) {
            return Some(h);
        }
        match action {
            Action::Up | Action::Down | Action::PageUp | Action::PageDown | Action::Home | Action::End | Action::Activate => Some(true),
            Action::FilterInput(_) | Action::FilterBackspace | Action::FilterClear | Action::FilterDeleteWord => Some(true),
            // the panel toggle again (the card's List button, the OSD) closes it; a typed `u` filters
            Action::QueuePanel => {
                self.panel = None;
                Some(true)
            }
            _ => None,
        }
    }

    pub(super) fn panel_action(&mut self, ctx: &mut UiCtx<'_>, action: Action) -> Option<bool> {
        if self.panel.as_ref()?.kind == PanelKind::Queue {
            return self.queue_panel_action(ctx, action);
        }
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

    /// The side panel (channels / mini guide / history) on the right of the video: a
    /// `virtual_list` (wheel scrolling; the hover cursor follows only real pointer motion, the
    /// view follows only key moves), a ✕ in the header, one click on a row tunes it (the panel
    /// stays open for the next one).
    pub(super) fn draw_panel(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
        match self.panel.as_ref().map(|p| p.kind) {
            None => return,
            Some(PanelKind::Queue) => return self.draw_queue_panel(ui, ctx, rect),
            Some(_) => {}
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
            PanelKind::Queue => "UP NEXT (u)",
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
                // the playlist's code chip at the right edge (several playlists, DESIGN §13.0 / §29);
                // the full name is the chip's tooltip and in the row's label
                let mut right = r.right() - pad / 2.0;
                if let Some(src) = source {
                    let chip = badge::CodeChip::new(p, theme, src, small.clone(), t.osd_scrim.to_opaque(), t.osd_text, 0.0);
                    let m = chip.paint(p, theme, egui::pos2(right, line_y), Align2::RIGHT_CENTER, None);
                    ui.interact(m, resp.id.with("source"), egui::Sense::hover())
                        .on_hover_text(egui::RichText::new(&src.name).size(theme.font(TypeToken::BodySmall)));
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

    /// The Up next panel: the same place and look as the other panels, the shared list in OSD
    /// colours, and "n to go · ends ~03:12" under it.
    fn draw_queue_panel(&mut self, ui: &mut egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect) {
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
        let _ = ui.interact(area, ui.id().with("panel-bg"), egui::Sense::click());
        let p = ui.painter_at(rect);
        p.rect_filled(area, t.rounding, t.osd_scrim);
        let caption = FontId::proportional((theme.font(TypeToken::Caption) * s).round());
        let font = FontId::proportional((theme.font(TypeToken::Title) * s).round());
        let small = FontId::proportional((theme.font(TypeToken::Body) * s).round());
        let snap = crate::queue::ui::snapshot(ctx);
        let Some(panel) = &mut self.panel else { return };
        let Some(q) = panel.queue.as_mut() else { return };
        let head_h = (caption.size * 2.0).round();
        let close = Rect::from_min_size(egui::pos2(area.right() - pad / 2.0 - head_h, area.top() + pad / 2.0), egui::vec2(head_h, head_h));
        let query = panel.list.filter.query().to_owned();
        let head = if query.is_empty() { "UP NEXT (u)".to_owned() } else { format!("UP NEXT (u)  ·  {query}") };
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
        let foot_h = (small.size * 1.8).round();
        let footer = crate::queue::ui::summary(&snap, ctx.now, ctx.config.interface.clock_24h);
        let list_top = close.bottom() + pad / 2.0;
        let list_rect = Rect::from_min_max(
            egui::pos2(area.left() + pad / 2.0, list_top),
            egui::pos2(area.right() - pad / 2.0, area.bottom() - pad / 2.0 - if footer.is_some() { foot_h } else { 0.0 }),
        );
        std::mem::swap(&mut panel.list, &mut q.list);
        q.sync(snap.clone(), Mode::Panel, crate::queue::ui::sleep_mark(ctx));
        let row_h = (font.size * 2.0).round();
        let empty = q.lines().is_empty();
        let out = if empty {
            let msg = if query.is_empty() { "Nothing to play next · Ctrl+N adds" } else { "No matches" };
            osd_text(
                &p,
                egui::pos2(area.left() + pad, list_top + row_h / 2.0),
                Align2::LEFT_CENTER,
                msg,
                small.clone(),
                with_alpha(t.osd_text, 0.7),
                area.width() - 2.0 * pad,
            );
            Out::None
        } else {
            ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| q.ui(ui, ctx, &Look::osd(theme), row_h)).inner
        };
        std::mem::swap(&mut panel.list, &mut q.list);
        if let Some(text) = footer {
            osd_text(
                &p,
                egui::pos2(area.left() + pad, area.bottom() - pad / 2.0 - foot_h / 2.0),
                Align2::LEFT_CENTER,
                &text,
                small,
                with_alpha(t.osd_text, 0.7),
                area.width() - 2.0 * pad,
            );
        }
        // a click puts the cursor on a row, a double-click plays it
        let out = match out {
            Out::Handled => Out::None,
            o => o,
        };
        self.queue_out(ctx, out);
        if closed {
            self.panel = None;
        }
    }
}
