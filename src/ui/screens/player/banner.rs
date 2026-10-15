//! The info banner, stream info and flashes (owner WP-11).

use super::*;
use crate::model::cast::{CastFeed, FeedPicture, FeedSound, FeedSurround, PositionSource};
use crate::model::{CastLevel, CastView};
use crate::player::tracks::StreamInfo;

/// A transient message at the top of the video (track / delay / volume changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flash {
    Audio,
    Subtitles,
    SubDelay,
    LipSync,
    SubSize,
    Volume,
    Aspect,
    Zoom,
}

/// The banner when a volume key or the speaker is used on a TV without a volume (AirPlay).
pub(super) const TV_VOLUME: &str = "Use the TV remote for volume";
/// The banner for a key that does nothing on a TV: tracks, subtitles, aspect and zoom.
pub(super) const NOT_WHILE_CASTING: &str = "Not available while casting";

/// The picture is on a TV that takes no volume from here (DESIGN §40 §11.3).
pub(super) fn tv_volume(snap: &PlayerSnapshot) -> bool {
    snap.cast.as_ref().is_some_and(|c| !c.volume)
}

pub(super) fn flash_text(f: Flash, snap: &PlayerSnapshot, zoom: &usize) -> String {
    match f {
        Flash::Audio => format!("{} {}", glyphs::AUDIO_TRACK, snap.tracks.selected_audio().map_or_else(|| "No audio track".into(), |a| a.label())),
        Flash::Subtitles => format!("{} {}", glyphs::SUBTITLES, snap.tracks.selected_sub().map_or_else(|| "Subtitles off".into(), |a| a.label())),
        Flash::SubDelay => format!("Subtitle delay {:+.1} s", snap.sub_delay),
        Flash::LipSync => crate::player::audio_output::adjustment_text(&snap.audio_output, snap.lip_sync_ms),
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

impl PlayerScreen {
    /// A banner line (like "No channel 7") that shows with the OSD for a moment.
    pub(super) fn cast_notice(&mut self, ctx: &UiCtx<'_>, text: &str) {
        self.flash_text = Some((text.to_owned(), Instant::now()));
        self.show_banner(ctx);
    }

    pub(super) fn show_banner(&mut self, ctx: &UiCtx<'_>) {
        self.banner_until = Some(Instant::now() + Duration::from_millis(u64::from(ctx.config.interface.osd_timeout_ms)));
        self.banner_dismissed = false;
    }

    /// The passive banner is up (a pause keeps it up until Esc).
    pub(super) fn banner_shown(&self, snap: &PlayerSnapshot) -> bool {
        (snap.paused && !self.banner_dismissed) || self.banner_until.is_some_and(|t| Instant::now() < t)
    }

    pub(super) fn banner_visible(&self, snap: &PlayerSnapshot) -> bool {
        self.osd_focus || self.panel.is_some() || self.banner_shown(snap)
    }

    pub(super) fn toggle_info(&mut self, ctx: &UiCtx<'_>) {
        self.info = !self.info;
        if self.info {
            ctx.play(PlayerCommand::RefreshStreamInfo);
            self.info_refreshed = Some(Instant::now());
        }
    }

    /// The stream info panel (`I`, the OSD's Info, the menu's Stream info): mpv's view of what
    /// plays, or while casting what the TV is fed ([`cast_lines`]). Drawn over the casting card.
    pub(super) fn draw_info(&self, ui: &egui::Ui, ctx: &mut UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
        let theme = ctx.theme;
        let t = &theme.tokens;
        let s = scale_of(ui);
        let pad = (theme.space(SpaceToken::PopupPadding) * s).round();
        let font = FontId::monospace((theme.font(TypeToken::Body) * s).round());
        let mut lines = match (&snap.cast, &snap.stream_info) {
            (Some(view), _) => cast_lines(ctx, snap, view),
            (None, Some(i)) => local_lines(ctx, snap, i),
            (None, None) => vec!["Reading stream info…".into()],
        };
        if snap.cast.is_none() {
            lines.push(format!("Lip sync {}", crate::player::audio_output::offset_text(snap.lip_sync_ms)));
            lines.push(format!("Output   {}", snap.audio_output.label));
        }
        let p = ui.painter_at(rect);
        let lh = font.size * 1.4;
        // the TV's path needs the longer lines
        let w = (if snap.cast.is_some() { 560.0 } else { 420.0 } * s).min(rect.width() - 2.0 * pad);
        let button_h = lh + pad;
        let area =
            Rect::from_min_size(egui::pos2(rect.left() + pad, rect.top() + pad * 6.0), egui::vec2(w, lh * lines.len() as f32 + 2.0 * pad + button_h));
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
        let copy_rect =
            Rect::from_min_max(egui::pos2(area.left() + pad, area.bottom() - pad - button_h), egui::pos2(area.right() - pad, area.bottom() - pad));
        if super::osd::osd_button(
            ui,
            &p,
            theme,
            copy_rect,
            ui.id().with("copy-stream-info"),
            &format!("{}  Copy stream info", crate::ui::widgets::glyphs::COPY),
            font,
            "Copy stream info",
            None,
            false,
        )
        .clicked()
        {
            ui.ctx().copy_text(lines.join("\n"));
            ctx.toast(Toast::info("Copied stream info"));
        }
    }
}

/// The provider's name and quality word for a live channel against what plays (DESIGN
/// §13.0.1): `Provider` and `Quality` lines.
fn provider_lines(ctx: &UiCtx<'_>, snap: &PlayerSnapshot, i: &StreamInfo) -> Vec<String> {
    let mut lines = Vec::new();
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
    lines
}

/// The info panel for local playback: mpv's view of the stream.
fn local_lines(ctx: &UiCtx<'_>, snap: &PlayerSnapshot, i: &StreamInfo) -> Vec<String> {
    let opt = |v: &Option<String>| v.clone().unwrap_or_else(|| "–".into());
    let res = match (i.width, i.height) {
        (Some(w), Some(h)) => format!("{w}×{h}"),
        _ => "–".into(),
    };
    let mut lines = vec![format!("Video    {} · {res} · {}", opt(&i.video_codec), i.fps.map_or_else(|| "–".into(), |f| format!("{f:.2} fps")))];
    lines.extend(provider_lines(ctx, snap, i));
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
    lines
}

/// The info panel while casting (DESIGN §40 §11.3 `RefreshStreamInfo`): what the TV is fed, as
/// the relay reports it at every launch (`StreamInfo::cast`). The TV and what it is
/// (`Device::make`, sanitized device strings only); the source as the relay knew it (ffprobe's
/// view, else the hints' when they sufficed and the probe was skipped, §8.1.2); each stream's way
/// to the TV (§8.3 levels, §8.4 surround); the segments, their pinned target (§8.1.4) and, for
/// a full-length item, their count (§8.5); how far behind live; where positions come from
/// (§7.3). mpv's decoder, drops, cache and bitrates say nothing here: mpv isn't playing it. No
/// address or URL can reach these lines (none is in a `StreamInfo` or a `Device`'s shown
/// strings).
fn cast_lines(ctx: &UiCtx<'_>, snap: &PlayerSnapshot, view: &CastView) -> Vec<String> {
    let cast = ctx.services.agent.cast();
    let tv = match cast.devices().into_iter().find(|d| d.id == view.device) {
        Some(d) => format!("{} · {}", view.name, d.make()),
        None => view.name.clone(),
    };
    let mut lines = vec![format!("Casting  {tv}")];
    let Some(i) = &snap.stream_info else {
        lines.push(format!("Starting on {}…", view.name));
        return lines;
    };
    let feed = i.cast.as_ref();
    let level = feed.map(|f| f.level).or(view.level);
    // the picture: the source, then its way to the TV
    if let Some(codec) = &i.video_codec {
        let mut parts = vec![codec.clone()];
        match (i.width, i.height) {
            (Some(w), Some(h)) => parts.push(format!("{w}×{h}")),
            (None, Some(h)) => parts.push(format!("{h} lines")),
            _ => {}
        }
        parts.extend(i.fps.map(|f| format!("{f:.2} fps")));
        parts.extend(feed.and_then(|f| f.interlaced).map(|on| if on { "interlaced" } else { "progressive" }.to_owned()));
        lines.push(format!("Video    {}", parts.join(" · ")));
    } else if feed.is_some_and(|f| f.picture == FeedPicture::None) {
        lines.push("Video    none (sound only)".into());
    }
    if let Some(way) = picture_way(feed, level) {
        lines.push(format!("  → TV   {way}"));
    }
    // the sound: the source, then the stereo AAC and surround renditions
    if let Some(codec) = &i.audio_codec {
        let mut parts = vec![codec.clone()];
        let layout = feed.and_then(|f| f.layout.clone());
        parts.extend(layout.or_else(|| i.audio_channels.as_ref().map(|c| format!("{c} ch"))));
        parts.extend(i.samplerate.map(|r| format!("{r} Hz")));
        lines.push(format!("Audio    {}", parts.join(" · ")));
    }
    if let Some(way) = feed.and_then(sound_way) {
        lines.push(format!("  → TV   {way}"));
    }
    if let Some(f) = feed {
        let mut parts = vec![if f.fmp4 { "fMP4" } else { "MPEG-TS" }.to_owned()];
        parts.extend(f.target_s.map(|t| format!("target {t} s")));
        // a film, an episode or catch-up the TV has whole (§8.5)
        parts.extend(f.full_length.map(|n| format!("full length, {n} segments")));
        lines.push(format!("Segments {}", parts.join(" · ")));
    }
    if is_live(snap) {
        if let Some(s) = view.behind_live_s {
            lines.push(format!("Behind   about {s} s"));
        }
    } else {
        lines.push(match cast.position_source() {
            Some(PositionSource::Reported) => "Position reported by the TV".to_owned(),
            Some(PositionSource::Estimated) => "Position estimated from what the TV fetched".to_owned(),
            None => "Position not reported yet".to_owned(),
        });
    }
    if feed.is_some_and(|f| !f.probed) {
        lines.push("Probe    skipped: mpv or an earlier cast told enough".into());
    }
    lines.extend(provider_lines(ctx, snap, i));
    if let Some(c) = &i.container {
        lines.push(format!("Stream   {c}"));
    }
    lines
}

/// The picture's way to the TV (DESIGN §40 §8.3), with its level; `None` without a picture.
/// Without the relay's feed (an older report) only the level is known.
fn picture_way(feed: Option<&CastFeed>, level: Option<CastLevel>) -> Option<String> {
    let tag = |l: CastLevel| match l {
        CastLevel::Original => "L1",
        CastLevel::Repackaged => "L2",
        CastLevel::Converted => "L3",
    };
    let named = |l: CastLevel| match l {
        CastLevel::Original => "as the provider sends it",
        CastLevel::Repackaged => "repackaged",
        CastLevel::Converted => "converted on this computer",
    };
    let Some(f) = feed else { return level.map(|l| format!("{} ({})", named(l), tag(l))) };
    Some(match (f.picture, f.level) {
        (FeedPicture::None, _) => return None,
        (FeedPicture::Copied, CastLevel::Original) => format!("as the provider sends it ({})", tag(f.level)),
        (FeedPicture::Copied, _) => format!("copied, repackaged ({})", tag(f.level)),
        (FeedPicture::Converted { deinterlaced: true }, _) => format!("deinterlaced and converted on this computer ({})", tag(f.level)),
        (FeedPicture::Converted { deinterlaced: false }, _) => format!("converted on this computer ({})", tag(f.level)),
    })
}

/// The sound's way to the TV: the stereo rendition, and the surround one beside it (§8.4).
fn sound_way(f: &CastFeed) -> Option<String> {
    let stereo = match f.sound {
        FeedSound::None => None,
        FeedSound::Copied => Some("copied"),
        FeedSound::Stereo => Some("converted to AAC stereo"),
    };
    let surround = f.surround.map(|s| match s {
        FeedSurround::Copied => "surround copied",
        FeedSurround::Ac3 => "surround as AC-3 5.1",
    });
    let parts: Vec<&str> = stereo.into_iter().chain(surround).collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::cast::testing::FakeCast;
    use crate::cast::{CastHandle, Device, PairVariant, Pairing, Protocol};
    use crate::model::{CastPhase, ChannelId, DeviceId, Playable, ReceiverKind};
    use crate::redact::Secret;
    use crate::ui::screens::home::testkit::{harness, painted, rig};

    #[test]
    fn stream_info_copy_text_copies_the_displayed_diagnostics() {
        use crate::ui::testing::clicks;
        let (mut t, one) = rig();
        t.snapshot = Arc::new(PlayerSnapshot {
            state: crate::player::PlayerState::Playing,
            stream_info: Some(StreamInfo { video_codec: Some("h264".into()), ..Default::default() }),
            ..Default::default()
        });
        let mut h = harness(t, one, PlayerScreen { info: true, ..Default::default() }, egui::vec2(1600.0, 900.0));
        clicks::click_text(&mut h, &format!("{}  Copy stream info", crate::ui::widgets::glyphs::COPY));
        let copied = clicks::copied_text(&h).expect("clipboard command");
        assert!(copied.starts_with("Video    h264 · "), "{copied}");
        assert!(copied.contains("\nDecoder  software\n"), "{copied}");
        assert_eq!(h.state().t.toasts.last().map(|t| t.text.as_str()), Some("Copied stream info"));
        assert!(h.state().s.info);
    }

    #[test]
    fn double_click_over_stream_info_toggles_fullscreen() {
        use crate::ui::testing::clicks;
        let (mut t, one) = rig();
        t.snapshot = Arc::new(PlayerSnapshot {
            state: crate::player::PlayerState::Playing,
            stream_info: Some(StreamInfo { video_codec: Some("h264".into()), ..Default::default() }),
            ..Default::default()
        });
        let mut h = harness(t, one, PlayerScreen { info: true, ..Default::default() }, egui::vec2(1600.0, 900.0));
        let over_info = clicks::text_rect(&h, "Video    h264 · – · –").center();
        clicks::click_at(&mut h, over_info);
        clicks::click_at(&mut h, over_info);
        assert!(
            h.output().viewport_output[&egui::ViewportId::ROOT].commands.iter().any(|c| matches!(c, egui::ViewportCommand::Fullscreen(true))),
            "the video receives the double-click through Stream info"
        );
        assert!(h.state().s.info);
    }

    /// Field report: "Info had no effect while casting". The panel shows over the casting card:
    /// the TV and what it is, "Starting on …" until the relay's first report, then what the TV
    /// is fed (the source, each stream's way, the segments, how far behind live, that the hints
    /// stood in for the probe), and none of mpv's decoder, drops or cache.
    #[test]
    fn the_info_panel_while_casting_says_what_the_tv_is_fed() {
        let (t, one) = rig();
        let tv = DeviceId::from_bytes([1; 16]);
        let fake = Arc::new(FakeCast::default());
        fake.script(|s| {
            let d = Device::new(tv, "Living Room", ReceiverKind::AppleTv, Protocol::AirPlay);
            s.devices = vec![Device { model: Some("AppleTV14,1".into()), pairing: Pairing::Paired(PairVariant::Hkp3), ..d }];
        });
        t.services().agent.set_cast(Some(CastHandle::new(fake)));
        let mut h = harness(t, one, PlayerScreen { info: true, ..PlayerScreen::default() }, egui::vec2(1600.0, 900.0));
        let view = CastView {
            device: tv,
            name: "Living Room".into(),
            kind: ReceiverKind::AppleTv,
            phase: CastPhase::Playing,
            level: Some(CastLevel::Repackaged),
            as_is: false,
            behind_live_s: Some(15),
            can_seek: false,
            volume: false,
        };
        let playable = Arc::new(Playable {
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
        });
        let snap = PlayerSnapshot { state: PlayerState::Playing, playable: Some(playable), cast: Some(view), ..Default::default() };
        h.state_mut().t.snapshot = Arc::new(snap.clone());
        h.run_steps(2);
        let texts = painted(&h);
        let casting = "Casting  Living Room · Apple TV 4K (3rd generation)";
        assert!(texts.iter().any(|t| t == casting) && texts.iter().any(|t| t == "Starting on Living Room…"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Reading stream info…"), "{texts:?}");

        let feed = CastFeed {
            probed: false,
            level: CastLevel::Repackaged,
            picture: FeedPicture::Copied,
            sound: FeedSound::Stereo,
            surround: Some(FeedSurround::Copied),
            fmp4: true,
            target_s: Some(6),
            full_length: None,
            interlaced: Some(false),
            layout: Some("5.1(side)".into()),
        };
        let info = StreamInfo {
            video_codec: Some("h264 (High@4.0)".into()),
            width: Some(1920),
            height: Some(1080),
            fps: Some(25.0),
            audio_codec: Some("ac3".into()),
            audio_channels: Some("6".into()),
            samplerate: Some(48000),
            container: Some("mpegts".into()),
            cast: Some(feed),
            ..StreamInfo::default()
        };
        h.state_mut().t.snapshot = Arc::new(PlayerSnapshot { stream_info: Some(info.clone()), ..snap.clone() });
        h.run_steps(2);
        let texts = painted(&h);
        for want in [
            casting,
            "Video    h264 (High@4.0) · 1920×1080 · 25.00 fps · progressive",
            "  → TV   copied, repackaged (L2)",
            "Audio    ac3 · 5.1(side) · 48000 Hz",
            "  → TV   converted to AAC stereo · surround copied",
            "Segments fMP4 · target 6 s",
            "Behind   about 15 s",
            "Probe    skipped: mpv or an earlier cast told enough",
            "Stream   mpegts",
        ] {
            assert!(texts.iter().any(|t| t == want), "{want}: {texts:?}");
        }
        for gone in ["Decoder", "Dropped", "Cache", "Bitrate", "Starting on"] {
            assert!(!texts.iter().any(|t| t.starts_with(gone)), "{gone}: {texts:?}");
        }
        // painted after the card, so it shows over it
        let card = texts.iter().position(|t| t.ends_with("Playing on Living Room")).expect("the casting card");
        assert!(texts.iter().position(|t| t == casting).is_some_and(|p| p > card), "{texts:?}");

        // a full-length film (§8.5): the whole playlist's segment count on the Segments line
        let mut info = info;
        info.cast.as_mut().unwrap().full_length = Some(1084);
        h.state_mut().t.snapshot = Arc::new(PlayerSnapshot { stream_info: Some(info), ..snap });
        h.run_steps(2);
        let texts = painted(&h);
        let want = "Segments fMP4 · target 6 s · full length, 1084 segments";
        assert!(texts.iter().any(|t| t == want), "{want}: {texts:?}");
    }
}
