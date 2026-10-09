//! mpv instance creation and per-file load arguments. Owner: WP-07. Skeleton implements the
//! init option set (verified options from the spike + protocols research).

use libmpv2::Mpv;

use crate::config::{Config, SubtitleAuto};
use crate::error::Result;
use crate::model::Playable;

/// Create the single mpv instance (`vo=libmpv`; video goes through the render API).
pub fn create(cfg: &Config) -> Result<Mpv> {
    let p = &cfg.player;
    let hwdec = cfg.effective_hwdec();
    let readahead = if p.instant_load { 1 } else { p.cache_secs.max(1) };
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "libmpv")?;
        init.set_option("hwdec", hwdec.as_str())?;
        init.set_option("idle", "yes")?;
        // keep-open=no: live EOF must produce END_FILE so the reconnect logic sees it
        init.set_option("keep-open", "no")?;
        init.set_option("terminal", "no")?;
        init.set_option("input-default-bindings", "no")?;
        init.set_option("input-vo-keyboard", "no")?;
        init.set_option("osc", "no")?;
        init.set_option("ytdl", "no")?;
        init.set_option("config", "no")?;
        init.set_option("video-timing-offset", "0")?; // don't make render() wait for display time
        init.set_option("audio-client-name", crate::APP_ID)?;
        init.set_option("cache", "yes")?;
        init.set_option("demuxer-readahead-secs", readahead as i64)?;
        init.set_option("cache-secs", readahead as i64)?;
        init.set_option("demuxer-max-bytes", format!("{}MiB", p.demuxer_max_mib).as_str())?;
        init.set_option("demuxer-max-back-bytes", "32MiB")?;
        init.set_option("network-timeout", p.network_timeout_s as i64)?;
        init.set_option(
            "stream-lavf-o",
            "reconnect=1,reconnect_streamed=1,reconnect_on_network_error=1,reconnect_delay_max=5",
        )?;
        if p.instant_load {
            init.set_option("cache-pause-initial", "no")?;
            init.set_option("cache-pause-wait", "0.5")?;
        }
        init.set_option("user-agent", p.user_agent.as_str())?;
        init.set_option("alang", p.audio_langs.join(",").as_str())?;
        init.set_option("slang", p.sub_langs.join(",").as_str())?;
        match p.sub_auto {
            SubtitleAuto::Off => init.set_option("sid", "no")?,
            SubtitleAuto::Forced => init.set_option("subs-fallback-forced", "always")?,
            SubtitleAuto::Always => init.set_option("subs-fallback", "yes")?,
        }
        init.set_option("sub-scale", p.sub_scale)?;
        init.set_option("volume", p.volume)?;
        init.set_option("mute", if p.muted { "yes" } else { "no" })?;
        init.set_option("hr-seek", "yes")?;
        for (k, v) in &p.mpv_options {
            init.set_option(k.as_str(), v.as_str())?;
        }
        Ok(())
    })?;
    Ok(mpv)
}

/// mpv option-value quoting: `%<bytes>%<value>` is accepted by every mpv option parser and
/// survives `,` `=` `"` inside values (header lists, URLs with commas).
pub fn quote(value: &str) -> String { format!("%{}%{}", value.len(), value) }

/// Arguments for `loadfile <url> replace -1 <options>` (mpv ≥ 0.38: the index `-1` is REQUIRED
/// before the options argument; verified in the local mpv 0.41 man page).
/// Per-file options: `force-media-title` (never derive titles from credential URLs),
/// `user-agent`, `referrer`, `http-header-fields` (Origin + extras), `start` (resume).
pub fn loadfile_args(p: &Playable) -> Vec<String> {
    let mut opts: Vec<String> = vec![format!("force-media-title={}", quote(&p.title))];
    if let Some(ua) = &p.headers.user_agent {
        opts.push(format!("user-agent={}", quote(ua)));
    }
    if let Some(r) = &p.headers.referrer {
        opts.push(format!("referrer={}", quote(r)));
    }
    let mut fields: Vec<String> = Vec::new();
    if let Some(o) = &p.headers.origin {
        fields.push(format!("Origin: {o}"));
    }
    for (k, v) in &p.headers.extra {
        // mpv splits http-header-fields on ','; escape commas inside values
        fields.push(format!("{k}: {}", v.replace(',', "\\,")));
    }
    if !fields.is_empty() {
        opts.push(format!("http-header-fields={}", quote(&fields.join(","))));
    }
    if let Some(s) = p.start_at {
        opts.push(format!("start={s:.1}"));
    }
    vec![p.url.expose().clone(), "replace".into(), "-1".into(), opts.join(",")]
}
