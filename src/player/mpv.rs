//! mpv instance creation, the settings shared by init and `ApplyConfig`, and per-file load
//! arguments. Owner: WP-07. Init option set verified in the spike + protocols research.

use std::collections::BTreeMap;
use std::ffi::{CStr, CString};

use libmpv2::Mpv;
use libmpv2_sys as sys;

use crate::config::{Config, SubtitleAuto};
use crate::error::Result;
use crate::model::Playable;

/// User `mpv_options` that can never be changed at runtime: the render API owns the VO.
const RUNTIME_DENY: &[&str] = &["vo", "gpu-context", "gpu-api", "wid", "config", "idle", "keep-open"];

/// Create the single mpv instance (`vo=libmpv`; video goes through the render API).
pub fn create(cfg: &Config) -> Result<Mpv> {
    let p = &cfg.player;
    let mpv = Mpv::with_initializer(|init| {
        init.set_option("vo", "libmpv")?;
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
        init.set_option("demuxer-max-back-bytes", "32MiB")?;
        init.set_option(
            "stream-lavf-o",
            "reconnect=1,reconnect_streamed=1,reconnect_on_network_error=1,reconnect_delay_max=5",
        )?;
        for (name, value) in runtime_settings(cfg) {
            init.set_option(name, value.as_str())?;
        }
        init.set_option("volume", p.volume)?;
        init.set_option("mute", if p.muted { "yes" } else { "no" })?;
        init.set_option("hr-seek", "yes")?;
        // power-user options go last; a bad one is logged and skipped instead of making the
        // whole player (and the app) unusable
        for (k, v) in &p.mpv_options {
            if let Err(e) = init.set_option(k.as_str(), v.as_str()) {
                tracing::warn!("ignoring mpv option {k:?}: {}", crate::redact::redact(&e.to_string()));
            }
        }
        Ok(())
    })?;
    Ok(mpv)
}

/// Every config-derived setting that can change while running (DESIGN §11.5 "Settings apply
/// live"), as `(name, value)` in mpv's option syntax. Applied at init and again by
/// `PlayerCommand::ApplyConfig`, so the two paths cannot drift apart.
pub(super) fn runtime_settings(cfg: &Config) -> Vec<(&'static str, String)> {
    let p = &cfg.player;
    let readahead = if p.instant_load { 1 } else { p.cache_secs.max(1) }.to_string();
    vec![
        ("hwdec", cfg.effective_hwdec()),
        ("cache-secs", readahead.clone()),
        ("demuxer-readahead-secs", readahead),
        ("demuxer-max-bytes", format!("{}MiB", p.demuxer_max_mib)),
        ("network-timeout", p.network_timeout_s.to_string()),
        // mpv defaults (no / 1 s) unless "instant load" asks for the shortest start
        ("cache-pause-initial", "no".into()),
        ("cache-pause-wait", if p.instant_load { "0.5" } else { "1" }.into()),
        ("user-agent", p.user_agent.clone()),
        ("alang", p.audio_langs.join(",")),
        ("slang", p.sub_langs.join(",")),
        ("sid", if p.sub_auto == SubtitleAuto::Off { "no" } else { "auto" }.into()),
        ("subs-fallback-forced", if p.sub_auto == SubtitleAuto::Forced { "always" } else { "no" }.into()),
        ("subs-fallback", if p.sub_auto == SubtitleAuto::Always { "yes" } else { "default" }.into()),
        ("sub-scale", p.sub_scale.to_string()),
    ]
}

/// Apply what changed from `previous` (the config currently applied) to a running instance:
/// the [`runtime_settings`] whose value differs and changed user `mpv_options`. Unchanged
/// settings are not re-set: `sid` is a live track switch during playback, and `sub-scale` may
/// have been adjusted for the current stream. Raw options take effect from the next file for
/// most mpv options; options in [`RUNTIME_DENY`] (the VO belongs to the render API) and
/// removed keys need a restart.
pub(super) fn apply_runtime(mpv: &Mpv, cfg: &Config, previous: &Config) {
    let before: BTreeMap<_, _> = runtime_settings(previous).into_iter().collect();
    for (name, value) in runtime_settings(cfg) {
        if before.get(name) == Some(&value) {
            continue;
        }
        if let Err(e) = mpv.set_property(name, value.as_str()) {
            tracing::warn!("mpv {name}: {}", crate::redact::redact(&e.to_string()));
        }
    }
    let previous_user = &previous.player.mpv_options;
    for (k, v) in &cfg.player.mpv_options {
        if previous_user.get(k) == Some(v) {
            continue;
        }
        if RUNTIME_DENY.contains(&k.as_str()) {
            tracing::info!("mpv option {k:?} applies after a restart");
        } else if let Err(e) = mpv.set_property(k.as_str(), v.as_str()) {
            tracing::warn!("ignoring mpv option {k:?}: {}", crate::redact::redact(&e.to_string()));
        }
    }
    for k in previous_user.keys().filter(|k| !cfg.player.mpv_options.contains_key(*k)) {
        tracing::info!("removed mpv option {k:?} applies after a restart");
    }
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
        fields.push(format!("Origin: {}", o.replace(',', "\\,")));
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

/// Run `loadfile` with `args` (from [`loadfile_args`]) and return the new playlist entry id.
/// The id comes from the command result (`playlist_entry_id`, mpv ≥ 0.33), which cannot race
/// with anything else touching the playlist; `playlist/0/id` is the fallback. The error is a
/// generic mpv error string (never the URL).
pub(super) fn loadfile(mpv: &Mpv, args: &[String]) -> std::result::Result<Option<i64>, String> {
    let mut owned = Vec::with_capacity(args.len() + 1);
    owned.push(CString::from(c"loadfile"));
    for a in args {
        owned.push(CString::new(a.as_str()).map_err(|_| "invalid stream address (NUL byte)".to_owned())?);
    }
    let mut ptrs: Vec<*const std::os::raw::c_char> = owned.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: `ptrs` is a NULL-terminated array of valid C strings that outlive the call;
    // `node` is only read when mpv reports success and is freed with mpv's own function.
    unsafe {
        let mut node: sys::mpv_node = std::mem::zeroed();
        let rc = sys::mpv_command_ret(mpv.ctx.as_ptr(), ptrs.as_mut_ptr(), &mut node);
        if rc < 0 {
            return Err(sys::mpv_error_str(rc).to_owned());
        }
        let id = node_map_i64(&node, "playlist_entry_id");
        sys::mpv_free_node_contents(&mut node);
        Ok(id.or_else(|| mpv.get_property::<i64>("playlist/0/id").ok()))
    }
}

/// `node[key]` as an integer when `node` is a map.
///
/// # Safety
/// `node` must be a valid node returned by mpv.
unsafe fn node_map_i64(node: &sys::mpv_node, key: &str) -> Option<i64> {
    if node.format != sys::mpv_format_MPV_FORMAT_NODE_MAP {
        return None;
    }
    let list = unsafe { node.u.list.as_ref() }?;
    for i in 0..usize::try_from(list.num).ok()? {
        let (k, v) = unsafe { (CStr::from_ptr(*list.keys.add(i)), &*list.values.add(i)) };
        if k.to_bytes() == key.as_bytes() {
            return match v.format {
                sys::mpv_format_MPV_FORMAT_INT64 => Some(unsafe { v.u.int64 }),
                sys::mpv_format_MPV_FORMAT_DOUBLE => Some(unsafe { v.u.double_ } as i64),
                _ => None,
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HttpHeaders, MediaRef, PlayableKind};
    use crate::redact::Secret;

    fn playable(headers: HttpHeaders, start_at: Option<f64>) -> Playable {
        Playable {
            media: MediaRef::Url("u".into()),
            kind: PlayableKind::Live,
            item: None,
            source_id: None,
            title: "News, 24/7 = \"live\"".into(),
            subtitle: None,
            logo: None,
            number: None,
            url: Secret::new("http://h/u?a=1,2".into()),
            headers,
            start_at,
            duration_hint: None,
        }
    }

    #[test]
    fn loadfile_args_quote_every_value() {
        let p = playable(
            HttpHeaders {
                user_agent: Some("UA/1.0 (X11, Linux)".into()),
                referrer: Some("http://ref/a,b".into()),
                origin: Some("http://o".into()),
                extra: vec![("X-Token".into(), "a,b".into())],
            },
            Some(12.34),
        );
        let a = loadfile_args(&p);
        assert_eq!(a[..3], ["http://h/u?a=1,2".to_string(), "replace".into(), "-1".into()]);
        assert_eq!(
            a[3],
            "force-media-title=%19%News, 24/7 = \"live\",user-agent=%19%UA/1.0 (X11, Linux),referrer=%14%http://ref/a,b,\
             http-header-fields=%30%Origin: http://o,X-Token: a\\,b,start=12.3"
        );
    }

    #[test]
    fn loadfile_args_minimal() {
        let a = loadfile_args(&playable(HttpHeaders::default(), None));
        assert_eq!(a.len(), 4);
        assert_eq!(a[3], "force-media-title=%19%News, 24/7 = \"live\"");
    }

    #[test]
    fn quote_counts_bytes_not_chars() {
        assert_eq!(quote("é"), "%2%é");
    }

    #[test]
    fn runtime_settings_cover_every_live_setting() {
        let mut cfg = Config::default();
        cfg.player.hwdec = "vaapi".into();
        cfg.player.cache_secs = 7;
        cfg.player.sub_auto = SubtitleAuto::Off;
        let s: BTreeMap<_, _> = runtime_settings(&cfg).into_iter().collect();
        for name in [
            "hwdec", "cache-secs", "demuxer-readahead-secs", "demuxer-max-bytes", "network-timeout", "alang", "slang", "sid",
            "subs-fallback-forced", "subs-fallback", "sub-scale", "user-agent",
        ] {
            assert!(s.contains_key(name), "{name}");
        }
        assert_eq!(s["hwdec"], "vaapi");
        assert_eq!(s["cache-secs"], "7");
        assert_eq!(s["sid"], "no");
        cfg.player.instant_load = true;
        let s: BTreeMap<_, _> = runtime_settings(&cfg).into_iter().collect();
        assert_eq!(s["cache-secs"], "1");
        assert_eq!(s["demuxer-readahead-secs"], "1");
    }

    fn headless() -> Mpv {
        let mut cfg = Config::default();
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        create(&cfg).expect("mpv")
    }

    #[test]
    fn bad_user_option_is_skipped_not_fatal() {
        let mut cfg = Config::default();
        cfg.player.mpv_options.insert("vo".into(), "null".into());
        cfg.player.mpv_options.insert("ao".into(), "null".into());
        cfg.player.mpv_options.insert("no-such-option-xyz".into(), "1".into());
        cfg.player.mpv_options.insert("video-sync".into(), "display-resample".into());
        let mpv = create(&cfg).expect("mpv starts despite a bad option");
        assert_eq!(mpv.get_property::<String>("video-sync").unwrap(), "display-resample");
    }

    #[test]
    fn loadfile_returns_the_new_entry_id() {
        let mpv = headless();
        let missing = std::env::temp_dir().join("tuner-no-such-file.ts");
        let args = |title: &str| {
            loadfile_args(&Playable { url: Secret::new(missing.display().to_string()), ..playable(HttpHeaders::default(), None) })
                .into_iter()
                .map(|a| if a.starts_with("force-media-title") { format!("force-media-title={}", quote(title)) } else { a })
                .collect::<Vec<_>>()
        };
        let a = loadfile(&mpv, &args("a")).unwrap().expect("entry id");
        let b = loadfile(&mpv, &args("b")).unwrap().expect("entry id");
        assert!(b > a, "ids grow: {a} {b}");
        assert!(loadfile(&mpv, &["x\0y".to_string()]).is_err());
    }

    #[test]
    fn apply_runtime_sets_properties_and_user_options() {
        let mpv = headless();
        let mut cfg = Config::default();
        cfg.player.cache_secs = 3;
        cfg.player.network_timeout_s = 9;
        cfg.player.user_agent = "Tuner-Test/1".into();
        cfg.player.demuxer_max_mib = 64;
        cfg.player.mpv_options.insert("vo".into(), "gpu".into()); // denied at runtime
        cfg.player.mpv_options.insert("video-sync".into(), "display-resample".into());
        apply_runtime(&mpv, &cfg, &Config::default());
        assert_eq!(mpv.get_property::<f64>("cache-secs").unwrap(), 3.0);
        assert_eq!(mpv.get_property::<f64>("demuxer-readahead-secs").unwrap(), 3.0);
        assert_eq!(mpv.get_property::<f64>("network-timeout").unwrap(), 9.0);
        assert_eq!(mpv.get_property::<String>("user-agent").unwrap(), "Tuner-Test/1");
        assert_eq!(mpv.get_property::<i64>("demuxer-max-bytes").unwrap(), 64 << 20);
        assert_eq!(mpv.get_property::<String>("video-sync").unwrap(), "display-resample");
        assert!(mpv.get_property::<String>("vo").unwrap().starts_with("null"), "the VO is never swapped at runtime");
    }

    /// Review fix: an unrelated settings change must not reset the subtitle track the user
    /// picked (`sid` is a live track switch) nor a per-stream sub-scale adjustment.
    #[test]
    fn apply_runtime_leaves_unchanged_settings_alone() {
        let mpv = headless();
        let before = Config::default();
        mpv.set_property("sid", "3").unwrap();
        mpv.set_property("sub-scale", 1.4).unwrap();
        let mut cfg = before.clone();
        cfg.player.cache_secs = 6;
        apply_runtime(&mpv, &cfg, &before);
        assert_eq!(mpv.get_property::<f64>("cache-secs").unwrap(), 6.0);
        assert_eq!(mpv.get_property::<String>("sid").unwrap(), "3", "track choice kept");
        assert!((mpv.get_property::<f64>("sub-scale").unwrap() - 1.4).abs() < 1e-3, "per-stream sub scale kept");
        // the subtitle mode itself changed: applied
        let mut off = cfg.clone();
        off.player.sub_auto = SubtitleAuto::Off;
        apply_runtime(&mpv, &off, &cfg);
        assert_eq!(mpv.get_property::<String>("sid").unwrap(), "no");
    }
}
