//! The info banner, stream info and flashes (owner WP-11).

use super::*;

/// A transient message at the top of the video (track / delay / volume changes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flash {
    Audio,
    Subtitles,
    SubDelay,
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

/// What a cast's quality level is called in the info panel (DESIGN §40 §8.3).
fn level_label(l: crate::model::CastLevel) -> &'static str {
    use crate::model::CastLevel as L;
    match l {
        L::Original => "as the provider sends it",
        L::Repackaged => "repackaged",
        L::Converted => "converted on this computer",
    }
}

pub(super) fn flash_text(f: Flash, snap: &PlayerSnapshot, zoom: &usize) -> String {
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

    pub(super) fn draw_info(&self, ui: &egui::Ui, ctx: &UiCtx<'_>, rect: Rect, snap: &PlayerSnapshot) {
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
                // the probe's view of the stream (DESIGN §40 §8.2): the decoder, dropped frames
                // and cache are mpv's, which isn't playing it
                match &snap.cast {
                    Some(c) => {
                        lines.push(format!("Casting  {} · {}", c.name, c.level.map_or("starting", level_label)));
                        if let Some(s) = c.behind_live_s {
                            lines.push(format!("Behind   about {s} s"));
                        }
                    }
                    None => {
                        lines.push(format!(
                            "Decoder  {}",
                            i.hwdec.clone().filter(|h| !h.is_empty() && h != "no").unwrap_or_else(|| "software".into())
                        ));
                        lines.push(format!("Dropped  {}", i.dropped_frames.map_or_else(|| "–".into(), |d| d.to_string())));
                        lines.push(format!("Cache    {}", i.cache_secs.map_or_else(|| "–".into(), |c| format!("{c:.1} s"))));
                    }
                }
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
}
