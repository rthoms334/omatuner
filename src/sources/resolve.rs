//! MediaRef → Playable. Owner: WP-05b.
//!
//! Runs on the player controller thread through `Services`' resolver (own reader connection;
//! point queries only, < 2 ms): loads the channel/movie/episode and its
//! source, builds the URL (Xtream URL builders with credentials, M3U URL as-is, catch-up via
//! `catchup::build_url` with the source's server tz override → `AccountInfo.server_tz` → local),
//! merges headers (entry > source User-Agent > config default), applies `force-media-title`
//! semantics (title from catalogue, never from URL), and fills `start_at` from saved progress
//! (only if > `resume_min_s` and not finished; the UI decides Resume vs Start over before
//! calling with `resume: false` to drop it).
//!
//! Errors: DRM-protected channel → `Error::BadPayload("drm")` (the player maps it to
//! `PlayerErrorKind::Drm`); missing rows → `Error::NotFound`; catch-up outside the archive
//! window → `Error::NotFound("outside catch-up window")`.
//!
//! Details:
//! * Xtream live/catch-up container: the source's `live_format`, `Auto` deferring to
//!   `config.general.live_format`, then `urls::pick_live_ext` against the account's
//!   `allowed_formats`. Movies/episodes use their container extension (`mp4` when unknown).
//! * Native Xtream catch-up uses `urls::timeshift_url`; window, corrections, tvg-shift, user
//!   offset, granularity and server-local time all come from `sources::catchup` (one source of
//!   truth for the archive maths). M3U channels use `catchup::build_url` with their own flavour.
//! * Channels of a disabled source are not played (`NotFound`): stale references (history,
//!   reminders, MPRIS) must not open connections to a provider the user switched off.
//! * Titles: live and catch-up carry the channel name with the programme as `subtitle`
//!   (current programme for live, the archived one for catch-up; EPG lookups are best effort);
//!   episodes carry the series name with `S02E05 · Title`; ad-hoc URLs use the file name or the
//!   last path segment (falling back to the host when that would reveal a registered secret).
//! * Resume also skips positions already past `watched_pct` of a known duration (the player
//!   would mark them finished on the next save anyway).

use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::{
    CatchupKind, CatchupSpec, Channel, ChannelId, EpgChannelId, EpisodeId, HttpHeaders, ItemKind, ItemRef, LiveFormat, MediaRef, MovieId, Playable,
    PlayableKind, Source, SourceId, StreamLocator, Timestamp, Unsupported, WatchProgress, XtreamCredentials, XtreamStreamKind,
};
use crate::redact::Secret;
use crate::sources::catchup::{self, CatchupRequest};
use crate::sources::m3u;
use crate::sources::xtream::urls::{self, LiveExt};
use crate::store;

/// Container used when the provider sent no extension for a movie/episode.
const DEFAULT_VOD_EXT: &str = "mp4";

pub fn resolve(conn: &Connection, cfg: &Config, media: &MediaRef, resume: bool, now: Timestamp) -> Result<Playable> {
    if crate::downloads::ENABLED
        && let Some(local) = resolve_download(conn, cfg, media, resume, None)?
    {
        return Ok(local);
    }
    resolve_provider(conn, cfg, media, resume, now)
}

/// The live library supplies current title stacks and guards, including copies added or
/// re-keyed after the download was made (DESIGN §43 §3, §6).
pub fn resolve_with_view(
    conn: &Connection,
    cfg: &Config,
    media: &MediaRef,
    resume: bool,
    now: Timestamp,
    lib: &store::library::Library,
    view: crate::shared::ParentalView,
) -> Result<Playable> {
    if crate::downloads::ENABLED
        && let Some(local) = resolve_download(conn, cfg, media, resume, Some((lib, view)))?
    {
        return Ok(local);
    }
    resolve_provider(conn, cfg, media, resume, now)
}

/// Download workers resolve the provider even when an earlier recording exists.
pub fn resolve_provider(conn: &Connection, cfg: &Config, media: &MediaRef, resume: bool, now: Timestamp) -> Result<Playable> {
    match media {
        MediaRef::Download(_) => Err(Error::NotFound("download file missing".into())),
        MediaRef::Channel(id) => resolve_live(conn, cfg, media, *id, now),
        MediaRef::Catchup { channel, start, stop, catchup_id } => {
            resolve_catchup(conn, cfg, media, *channel, (*start, *stop), catchup_id.as_deref(), now)
        }
        MediaRef::Movie(id) => resolve_movie(conn, cfg, media, *id, resume),
        MediaRef::Episode(id) => resolve_episode(conn, cfg, media, *id, resume),
        MediaRef::Url(raw) => resolve_url(cfg, media, raw),
    }
}

/// Read local metadata before source credentials: deleted and disabled playlists' saved
/// files still play, and a partial recording is only accepted by an explicit Downloads ref.
fn resolve_download(
    conn: &Connection,
    cfg: &Config,
    media: &MediaRef,
    resume: bool,
    context: Option<(&store::library::Library, crate::shared::ParentalView)>,
) -> Result<Option<Playable>> {
    use crate::downloads::{Kind, State};
    let explicit = matches!(media, MediaRef::Download(_));
    let item = match media {
        MediaRef::Movie(id) => store::vod::movie(conn, *id).ok().map(|m| ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key }),
        MediaRef::Episode(id) => store::vod::episode(conn, *id).ok().map(|e| ItemRef { source: e.source_id, kind: ItemKind::Episode, key: e.key }),
        _ => None,
    };
    let root = store::downloads::root(conn, &cfg.downloads)?;
    let copies = if let (Some(item), Some((lib, _))) = (&item, context) {
        store::watchlist::title_copies(conn, lib, item)?
    } else {
        item.clone().into_iter().collect()
    };
    let candidates = if let MediaRef::Download(id) = media {
        store::downloads::job(conn, *id)?.into_iter().collect()
    } else if item.is_some() {
        store::downloads::jobs(conn)?
    } else {
        Vec::new()
    };
    let candidate = candidates.into_iter().find(|j| {
        if !(j.state == State::Done || explicit && j.state == State::Partial) || !j.path(&root).is_some_and(|p| p.is_file()) {
            return false;
        }
        if let Some((lib, view)) = context {
            if crate::downloads::planning::concealed(conn, lib, view, j) {
                return false;
            }
            if !view.unlocked && crate::downloads::planning::locked(conn, lib, j) {
                return false;
            }
        } else if cfg.parental.pin_active() && j.fp.get("download_locked").and_then(|v| v.as_bool()) == Some(true) {
            return false;
        }
        explicit
            || j.item().is_some_and(|i| copies.contains(&i))
            || (context.is_none()
                && item.as_ref().is_some_and(|item| {
                    let encoded = serde_json::to_value(item).unwrap_or_default();
                    j.fp.get("download_copies").and_then(serde_json::Value::as_array).is_some_and(|cs| cs.contains(&encoded))
                }))
    });
    let Some(j) = candidate else { return Ok(None) };
    let Some(path) = j.path(&root) else { return Ok(None) };
    let item = if j.kind.recording() { None } else { j.item() };
    let progress = item.as_ref().map(|item| store::user::progress(conn, item)).transpose()?.flatten();
    let kind = match j.kind {
        Kind::Movie => PlayableKind::Movie,
        Kind::Episode => PlayableKind::Episode {
            season: j.fp.get("download_season").and_then(serde_json::Value::as_u64).unwrap_or(0) as u32,
            episode: j.fp.get("download_episode").and_then(serde_json::Value::as_u64).unwrap_or(0) as u32,
        },
        _ => PlayableKind::Catchup { start: j.prog_start.unwrap_or(0), stop: j.prog_stop.unwrap_or(0) },
    };
    Ok(Some(Playable {
        from_download: true,
        media: if j.kind.recording() || item.is_none() {
            MediaRef::Download(j.id)
        } else {
            crate::downloads::planning::media(conn, &j).unwrap_or_else(|_| media.clone())
        },
        kind,
        item,
        source_id: None,
        title: j.title,
        subtitle: j.subtitle,
        logo: None,
        number: None,
        url: Secret::new(path.to_string_lossy().into_owned()),
        headers: HttpHeaders::default(),
        start_at: progress.as_ref().filter(|_| resume).and_then(|p| resume_at(p, cfg)).or_else(|| {
            if resume && j.watched_at.is_none() {
                j.fp.get("download_position_s").and_then(|v| v.as_f64()).filter(|p| *p >= cfg.player.resume_min_s as f64)
            } else {
                None
            }
        }),
        duration_hint: j.fp.get("download_duration_s").and_then(serde_json::Value::as_f64),
    }))
}

/// Whether a catch-up of `channel` at `start` is possible (archive flag + window). Used by the
/// guide to show the ⟲ glyph without building URLs.
pub fn catchup_available(has_catchup: bool, catchup_days: u32, start: Timestamp, now: Timestamp) -> bool {
    has_catchup && start < now && start >= now - catchup_days as i64 * 86_400
}

// ---------------------------------------------------------------------------------------------
// per kind

fn resolve_live(conn: &Connection, cfg: &Config, media: &MediaRef, id: ChannelId, now: Timestamp) -> Result<Playable> {
    let ch = store::live::channel(conn, id)?;
    check_supported(&ch)?;
    let src = load_source(conn, ch.source_id)?;
    let url = stream_url(&ch.stream, &src, cfg)?;
    let subtitle = ch.epg_channel_id.and_then(|e| current_programme(conn, e, shown_shift(conn, &ch), now));
    Ok(Playable {
        from_download: false,
        media: media.clone(),
        kind: PlayableKind::Live,
        item: Some(channel_ref(&ch)),
        source_id: Some(ch.source_id),
        title: ch.display_name().to_owned(),
        subtitle,
        logo: ch.logo.clone(),
        number: ch.number,
        url: Secret::new(url),
        headers: merge_headers(&ch.headers, Some(&src), cfg),
        start_at: None,
        duration_hint: None,
    })
}

/// `(start, stop)` = displayed (tvg-shifted) programme times, as the guide shows them.
fn resolve_catchup(
    conn: &Connection,
    cfg: &Config,
    media: &MediaRef,
    id: ChannelId,
    (start, stop): (Timestamp, Timestamp),
    catchup_id: Option<&str>,
    now: Timestamp,
) -> Result<Playable> {
    let ch = store::live::channel(conn, id)?;
    check_supported(&ch)?;
    let spec = ch.catchup.as_ref().ok_or_else(|| Error::NotFound("no catch-up for this channel".into()))?;
    let src = load_source(conn, ch.source_id)?;
    let live_url = stream_url(&ch.stream, &src, cfg)?;
    let req = CatchupRequest {
        live_url: &live_url,
        spec,
        start,
        stop,
        now,
        // §4.6: only the provider's tvg-shift converts back; a user or rule shift means the
        // stream airs later, so its archive is at the shown time
        tvg_shift_min: ch.tvg_shift_min,
        user_offset_s: catchup_offset(&ch, &src),
        catchup_id: catchup_id.filter(|c| !c.trim().is_empty()),
        tz: server_tz(&src),
    };
    let url = match &ch.stream {
        StreamLocator::Xtream { stream_id, .. } => xtream_timeshift(&req, xtream_creds(&src)?, *stream_id, live_ext(&src, cfg))?,
        StreamLocator::Url { .. } => catchup::build_url(&req)?,
    };
    let epg_start = start.saturating_sub(i64::from(shown_shift(conn, &ch)) * 60);
    let subtitle = ch.epg_channel_id.and_then(|e| programme_at(conn, e, epg_start));
    Ok(Playable {
        from_download: false,
        media: media.clone(),
        kind: PlayableKind::Catchup { start, stop },
        item: Some(channel_ref(&ch)),
        source_id: Some(ch.source_id),
        title: ch.display_name().to_owned(),
        subtitle,
        logo: ch.logo.clone(),
        number: ch.number,
        url: Secret::new(url),
        headers: merge_headers(&ch.headers, Some(&src), cfg),
        start_at: None,
        duration_hint: archive_length(&req),
    })
}

fn resolve_movie(conn: &Connection, cfg: &Config, media: &MediaRef, id: MovieId, resume: bool) -> Result<Playable> {
    let m = store::vod::movie(conn, id)?;
    let src = load_source(conn, m.source_id)?;
    let url = stream_url(&m.stream, &src, cfg)?;
    let item = ItemRef { source: m.source_id, kind: ItemKind::Movie, key: m.key.clone() };
    let progress = store::user::progress(conn, &item)?;
    Ok(Playable {
        from_download: false,
        media: media.clone(),
        kind: PlayableKind::Movie,
        item: Some(item),
        source_id: Some(m.source_id),
        title: m.display_name().to_owned(),
        subtitle: m.year.map(|y| y.to_string()),
        logo: m.poster.clone(),
        number: None,
        url: Secret::new(url),
        headers: merge_headers(&HttpHeaders::default(), Some(&src), cfg),
        start_at: progress.as_ref().filter(|_| resume).and_then(|p| resume_at(p, cfg)),
        duration_hint: duration_hint(m.duration_s, progress.as_ref()),
    })
}

fn resolve_episode(conn: &Connection, cfg: &Config, media: &MediaRef, id: EpisodeId, resume: bool) -> Result<Playable> {
    let ep = store::vod::episode(conn, id)?;
    let series = store::vod::series(conn, ep.series_id)?;
    let src = load_source(conn, ep.source_id)?;
    let url = stream_url(&ep.stream, &src, cfg)?;
    let item = ItemRef { source: ep.source_id, kind: ItemKind::Episode, key: ep.key.clone() };
    let progress = store::user::progress(conn, &item)?;
    Ok(Playable {
        from_download: false,
        media: media.clone(),
        kind: PlayableKind::Episode { season: ep.season, episode: ep.episode },
        item: Some(item),
        source_id: Some(ep.source_id),
        title: series.display_name().to_owned(),
        subtitle: Some(episode_label(ep.season, ep.episode, &ep.title)),
        logo: ep.still.clone().or_else(|| series.cover.clone()),
        number: None,
        url: Secret::new(url),
        headers: merge_headers(&HttpHeaders::default(), Some(&src), cfg),
        start_at: progress.as_ref().filter(|_| resume).and_then(|p| resume_at(p, cfg)),
        duration_hint: duration_hint(ep.duration_s, progress.as_ref()),
    })
}

/// Ad-hoc stream URL (Kodi pipe headers allowed) or local file (path or `file://`).
fn resolve_url(cfg: &Config, media: &MediaRef, raw: &str) -> Result<Playable> {
    let raw = raw.trim();
    // A file whose name contains `|` is still a file: only split headers off other inputs.
    let (addr, pairs) = if is_local(raw) && Path::new(raw).exists() { (raw.to_owned(), Vec::new()) } else { m3u::split_pipe_headers(raw) };
    if addr.is_empty() {
        return Err(Error::NotFound("empty stream address".into()));
    }
    let mut headers = HttpHeaders::default();
    for (name, value) in &pairs {
        apply_header(&mut headers, name, value);
    }
    let title = match local_path(&addr) {
        Some(path) => {
            if !path.exists() {
                return Err(Error::NotFound("file not found".into()));
            }
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
        }
        None => {
            headers = merge_headers(&headers, None, cfg);
            url_title(&addr)
        }
    };
    Ok(Playable {
        from_download: false,
        media: media.clone(),
        kind: PlayableKind::Url,
        item: None,
        source_id: None,
        title,
        subtitle: None,
        logo: None,
        number: None,
        url: Secret::new(addr),
        headers,
        start_at: None,
        duration_hint: None,
    })
}

// ---------------------------------------------------------------------------------------------
// helpers

fn check_supported(ch: &Channel) -> Result<()> {
    match ch.unsupported {
        Some(Unsupported::Drm) => Err(Error::BadPayload("drm".into())),
        None => Ok(()),
    }
}

fn channel_ref(ch: &Channel) -> ItemRef {
    ItemRef { source: ch.source_id, kind: ItemKind::Channel, key: ch.key.clone() }
}

fn load_source(conn: &Connection, id: SourceId) -> Result<Source> {
    let src = store::sources::get_source(conn, id)?;
    if !src.enabled {
        return Err(Error::NotFound("source is disabled".into()));
    }
    Ok(src)
}

/// The shift the guide shows the channel with (B8, DESIGN §31): the user's, else a Guide fixes
/// rule row's, else the provider's tvg-shift — the same `COALESCE` as the library's.
fn shown_shift(conn: &Connection, ch: &Channel) -> i32 {
    let q = conn
        .prepare_cached(
            "SELECT COALESCE(o.shift_min, ce.shift_min, c.tvg_shift_min) FROM channel c
             LEFT JOIN item_override o ON o.source_id = c.source_id AND o.kind = 'channel' AND o.item_key = c.key
             LEFT JOIN channel_epg ce ON ce.channel_id = c.id
             WHERE c.id = ?1",
        )
        .and_then(|mut s| s.query_row([ch.id], |r| r.get::<_, i32>(0)));
    q.unwrap_or(ch.tvg_shift_min)
}

/// The catch-up offset: the channel's own (0 included), else its playlist's
/// (`SourceSettings::guide`, DESIGN §31), else none.
fn catchup_offset(ch: &Channel, src: &Source) -> i64 {
    ch.catchup_offset_s.or(src.settings.guide.catchup_offset_s).unwrap_or(0)
}

fn xtream_creds(src: &Source) -> Result<&XtreamCredentials> {
    src.xtream.as_ref().ok_or_else(|| Error::Config("Xtream stream in a source without Xtream credentials".into()))
}

/// Playable URL of a catalogue stream. Xtream URLs are built here from the source credentials
/// (never stored in catalogue rows).
fn stream_url(stream: &StreamLocator, src: &Source, cfg: &Config) -> Result<String> {
    match stream {
        StreamLocator::Url { url } => {
            let url = url.trim();
            if url.is_empty() {
                return Err(Error::NotFound("stream has no address".into()));
            }
            Ok(url.to_owned())
        }
        StreamLocator::Xtream { kind, stream_id, ext } => {
            let c = xtream_creds(src)?;
            Ok(match kind {
                XtreamStreamKind::Live => urls::live_url(c, *stream_id, live_ext(src, cfg)),
                XtreamStreamKind::Movie => urls::movie_url(c, *stream_id, vod_ext(ext.as_deref())),
                XtreamStreamKind::Series => urls::episode_url(c, &stream_id.to_string(), vod_ext(ext.as_deref())),
            })
        }
    }
}

/// Source format → config default (when the source says `Auto`) → allowed formats.
fn live_ext(src: &Source, cfg: &Config) -> LiveExt {
    let pref = match src.settings.live_format {
        LiveFormat::Auto => cfg.general.live_format,
        f => f,
    };
    let allowed = src.status.account.as_ref().map_or(&[][..], |a| a.allowed_formats.as_slice());
    urls::pick_live_ext(pref, allowed)
}

fn vod_ext(ext: Option<&str>) -> &str {
    ext.map(|e| e.trim().trim_start_matches('.')).filter(|e| !e.is_empty()).unwrap_or(DEFAULT_VOD_EXT)
}

fn non_blank(s: Option<&str>) -> Option<&str> {
    s.filter(|s| !s.trim().is_empty())
}

/// Entry headers win; the User-Agent falls back to the source's, then the configured default.
fn merge_headers(entry: &HttpHeaders, src: Option<&Source>, cfg: &Config) -> HttpHeaders {
    let mut h = entry.clone();
    if non_blank(h.user_agent.as_deref()).is_none() {
        h.user_agent = non_blank(src.and_then(|s| s.settings.user_agent.as_deref()))
            .or_else(|| non_blank(Some(cfg.player.user_agent.as_str())))
            .map(str::to_owned);
    }
    h
}

/// Timezone of the provider's archive clock: source override → reported `server_info.timezone`
/// → local. Unknown names are skipped (logged), never fatal.
fn server_tz(src: &Source) -> jiff::tz::TimeZone {
    let reported = src.status.account.as_ref().and_then(|a| a.server_tz.as_deref());
    for name in [src.settings.server_tz.as_deref(), reported].into_iter().flatten().map(str::trim).filter(|n| !n.is_empty()) {
        match jiff::tz::TimeZone::get(name) {
            Ok(tz) => return tz,
            Err(e) => tracing::warn!("source {}: unknown server timezone {name:?}: {e}", src.id),
        }
    }
    jiff::tz::TimeZone::system()
}

/// Native Xtream catch-up: `urls::timeshift_url` with the start and length that
/// `sources::catchup` computes for the Xc flavour (minute granularity, server-local time,
/// minutes rounded up), so servers with a path prefix work too.
fn xtream_timeshift(req: &CatchupRequest<'_>, c: &XtreamCredentials, stream_id: i64, ext: LiveExt) -> Result<String> {
    // Window / archive / correction checks and their errors belong to `build_url`; the Shift
    // flavour cannot fail on the URL's shape, so only those checks can reject here.
    let check = CatchupSpec { kind: CatchupKind::Shift, ..req.spec.clone() };
    catchup::build_url(&CatchupRequest { spec: &check, ..req.clone() })?;
    let when = catchup::expand("{duration:60} {Y}-{m}-{d}:{H}-{M}", req);
    let bad = || Error::parse(None, "catch-up: could not compute the archive start");
    let (minutes, start) = when.split_once(' ').ok_or_else(bad)?;
    let minutes: u32 = minutes.parse().map_err(|_| bad())?;
    let start = jiff::civil::DateTime::strptime("%Y-%m-%d:%H-%M", start).map_err(|_| bad())?;
    Ok(urls::timeshift_url(c, stream_id, start, minutes.max(1), ext))
}

/// Length of the archived stretch in seconds, as `sources::catchup` computes it (exact start,
/// capped at now in archive time: a "+1" channel's current programme is already complete).
fn archive_length(req: &CatchupRequest<'_>) -> Option<f64> {
    let span = catchup::expand("${start} ${duration}", req);
    let secs: i64 = span.split_once(' ')?.1.parse().ok()?;
    (secs > 0).then_some(secs as f64)
}

/// Title of the programme on air at `now` (displayed time), if the guide knows it.
fn current_programme(conn: &Connection, ch: EpgChannelId, shift_min: i32, now: Timestamp) -> Option<String> {
    match store::epg::now_next(conn, &[(ch, shift_min)], now) {
        Ok(mut m) => m.remove(&(ch, shift_min)).and_then(|nn| nn.now).map(|p| p.title.to_string()),
        Err(e) => {
            tracing::debug!("now/next for the OSD failed: {e}");
            None
        }
    }
}

/// Title of the programme covering `at` (EPG time).
fn programme_at(conn: &Connection, ch: EpgChannelId, at: Timestamp) -> Option<String> {
    match store::epg::window(conn, ch, at, at.saturating_add(1)) {
        Ok(rows) => rows.into_iter().find(|p| p.contains(at)).map(|p| p.title.to_string()),
        Err(e) => {
            tracing::debug!("catch-up programme lookup failed: {e}");
            None
        }
    }
}

/// Resume position, if worth offering: past `resume_min_s`, not finished and not already past
/// the watched threshold.
fn resume_at(p: &WatchProgress, cfg: &Config) -> Option<f64> {
    let pos = p.position_s;
    if p.finished || !pos.is_finite() || pos <= f64::from(cfg.player.resume_min_s) {
        return None;
    }
    if let Some(d) = p.duration_s.filter(|d| d.is_finite() && *d > 0.0)
        && pos / d * 100.0 >= f64::from(cfg.player.watched_pct)
    {
        return None;
    }
    Some(pos)
}

fn duration_hint(meta_s: Option<u32>, progress: Option<&WatchProgress>) -> Option<f64> {
    meta_s.filter(|d| *d > 0).map(f64::from).or_else(|| progress.and_then(|p| p.duration_s).filter(|d| d.is_finite() && *d > 0.0))
}

/// `S02E05 · Title` (just `S02E05` when the provider sent no title).
fn episode_label(season: u32, episode: u32, title: &str) -> String {
    let title = title.trim();
    if title.is_empty() { format!("S{season:02}E{episode:02}") } else { format!("S{season:02}E{episode:02} · {title}") }
}

/// Same mapping as the M3U parser: well-known headers get their dedicated fields.
fn apply_header(h: &mut HttpHeaders, name: &str, value: &str) {
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || value.is_empty() {
        return;
    }
    match name.to_ascii_lowercase().as_str() {
        "user-agent" => h.user_agent = Some(value.to_owned()),
        "referer" | "referrer" => h.referrer = Some(value.to_owned()),
        "origin" => h.origin = Some(value.to_owned()),
        _ => match h.extra.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(name)) {
            Some(slot) => slot.1 = value.to_owned(),
            None => h.extra.push((name.to_owned(), value.to_owned())),
        },
    }
}

/// No URL scheme (`proto://`), or `file://`.
fn is_local(addr: &str) -> bool {
    !addr.contains("://") || addr.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("file://"))
}

fn local_path(addr: &str) -> Option<PathBuf> {
    if !is_local(addr) {
        return None;
    }
    if addr.contains("://") {
        // `file://` URL; an unparsable one is still looked up literally (and reported missing).
        return Some(url::Url::parse(addr).ok().and_then(|u| u.to_file_path().ok()).unwrap_or_else(|| PathBuf::from(addr)));
    }
    Some(PathBuf::from(addr))
}

/// Display title of an ad-hoc network URL: the final path segment (percent-decoded), else the
/// host. Never shows userinfo, the query or anything the redactor would hide.
///
/// Only the real final segment counts (a trailing `/` means there is none): stepping back to an
/// earlier one could pick the password of `http://panel/live/user/pass/`. The segment must also
/// survive redaction of the whole URL, which catches credentials that are only recognisable by
/// their position (Xtream paths), not just registered secrets.
fn url_title(addr: &str) -> String {
    let Ok(u) = url::Url::parse(addr) else { return "Stream".into() };
    let host = u.host_str().map(str::to_owned);
    let redacted = url::Url::parse(&crate::redact::redact(u.as_str())).ok();
    let redacted_last = redacted.as_ref().and_then(|r| r.path_segments()?.next_back());
    let segment = u
        .path_segments()
        .and_then(|mut s| s.next_back())
        .filter(|s| !s.is_empty() && redacted_last == Some(*s))
        .map(percent_decode)
        .filter(|s| !s.trim().is_empty() && crate::redact::redact(s) == s.as_str());
    segment.or(host).unwrap_or_else(|| "Stream".into())
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AccountInfo, SourceColor, SourceKind, SourceSettings, SourceStatus};

    fn src(settings: SourceSettings, account: Option<AccountInfo>) -> Source {
        Source {
            id: SourceId(1),
            name: "s".into(),
            kind: SourceKind::Xtream,
            icon: String::new(),
            color: SourceColor::Accent,
            url: None,
            path: None,
            xtream: Some(XtreamCredentials { server: "http://h:8080".into(), username: "u".into(), password: Secret::new("p".into()) }),
            settings,
            enabled: true,
            sort_index: 0,
            created_at: 0,
            status: SourceStatus { account, ..Default::default() },
        }
    }

    fn account(formats: &[&str], tz: Option<&str>) -> AccountInfo {
        AccountInfo { allowed_formats: formats.iter().map(|s| s.to_string()).collect(), server_tz: tz.map(str::to_owned), ..Default::default() }
    }

    #[test]
    fn live_format_pick() {
        let mut cfg = Config::default();
        let with = |f: LiveFormat, acc: Option<AccountInfo>| src(SourceSettings { live_format: f, ..Default::default() }, acc);
        assert_eq!(live_ext(&with(LiveFormat::Auto, None), &cfg), LiveExt::M3u8, "no account info → m3u8");
        assert_eq!(live_ext(&with(LiveFormat::Auto, Some(account(&["ts", "rtmp"], None))), &cfg), LiveExt::Ts);
        assert_eq!(live_ext(&with(LiveFormat::Auto, Some(account(&["M3U8", "ts"], None))), &cfg), LiveExt::M3u8);
        assert_eq!(live_ext(&with(LiveFormat::Ts, Some(account(&["m3u8"], None))), &cfg), LiveExt::Ts, "explicit choice wins");
        assert_eq!(live_ext(&with(LiveFormat::M3u8, Some(account(&["ts"], None))), &cfg), LiveExt::M3u8);
        cfg.general.live_format = LiveFormat::Ts;
        assert_eq!(live_ext(&with(LiveFormat::Auto, None), &cfg), LiveExt::Ts, "Auto defers to the config default");
        assert_eq!(live_ext(&with(LiveFormat::M3u8, None), &cfg), LiveExt::M3u8);
    }

    #[test]
    fn header_precedence() {
        let mut cfg = Config::default();
        cfg.player.user_agent = "Cfg/1".into();
        let entry = HttpHeaders { user_agent: Some("Entry/1".into()), referrer: Some("http://r/".into()), ..Default::default() };
        let with_ua = src(SourceSettings { user_agent: Some("Src/1".into()), ..Default::default() }, None);
        let no_ua = src(SourceSettings { user_agent: Some("  ".into()), ..Default::default() }, None);

        let h = merge_headers(&entry, Some(&with_ua), &cfg);
        assert_eq!(h.user_agent.as_deref(), Some("Entry/1"));
        assert_eq!(h.referrer.as_deref(), Some("http://r/"));
        assert_eq!(merge_headers(&HttpHeaders::default(), Some(&with_ua), &cfg).user_agent.as_deref(), Some("Src/1"));
        assert_eq!(merge_headers(&HttpHeaders::default(), Some(&no_ua), &cfg).user_agent.as_deref(), Some("Cfg/1"), "blank source UA");
        assert_eq!(merge_headers(&HttpHeaders::default(), None, &cfg).user_agent.as_deref(), Some("Cfg/1"));
        let blank_entry = HttpHeaders { user_agent: Some(String::new()), ..Default::default() };
        assert_eq!(merge_headers(&blank_entry, Some(&with_ua), &cfg).user_agent.as_deref(), Some("Src/1"));
        cfg.player.user_agent = String::new();
        assert_eq!(merge_headers(&HttpHeaders::default(), None, &cfg).user_agent, None, "no UA at all is allowed");
    }

    #[test]
    fn server_tz_cascade() {
        let tz = |s: Option<&str>, a: Option<&str>| {
            server_tz(&src(SourceSettings { server_tz: s.map(str::to_owned), ..Default::default() }, Some(account(&[], a))))
        };
        assert_eq!(tz(Some("Europe/Paris"), Some("Europe/London")).iana_name(), Some("Europe/Paris"));
        assert_eq!(tz(None, Some("Europe/London")).iana_name(), Some("Europe/London"));
        assert_eq!(tz(Some("Not/AZone"), Some("Europe/London")).iana_name(), Some("Europe/London"), "bad override skipped");
        assert_eq!(tz(Some(" "), None), jiff::tz::TimeZone::system());
        assert_eq!(tz(None, Some("garbage")), jiff::tz::TimeZone::system());
    }

    fn progress(pos: f64, dur: Option<f64>, finished: bool) -> WatchProgress {
        WatchProgress {
            item: ItemRef { source: SourceId(1), kind: ItemKind::Movie, key: crate::model::ItemKey("k".into()) },
            position_s: pos,
            duration_s: dur,
            updated_at: 0,
            finished,
            title: String::new(),
            fingerprint: Default::default(),
        }
    }

    #[test]
    fn resume_threshold() {
        let cfg = Config::default(); // resume_min_s 60, watched_pct 92
        assert_eq!(resume_at(&progress(60.0, None, false), &cfg), None, "must be strictly past the threshold");
        assert_eq!(resume_at(&progress(60.5, None, false), &cfg), Some(60.5));
        assert_eq!(resume_at(&progress(2480.0, Some(5400.0), false), &cfg), Some(2480.0));
        assert_eq!(resume_at(&progress(2480.0, Some(5400.0), true), &cfg), None, "finished");
        assert_eq!(resume_at(&progress(5000.0, Some(5400.0), false), &cfg), None, "past watched %");
        assert_eq!(resume_at(&progress(5000.0, Some(0.0), false), &cfg), Some(5000.0), "unknown duration");
        assert_eq!(resume_at(&progress(f64::NAN, None, false), &cfg), None);
    }

    #[test]
    fn vod_ext_and_labels() {
        assert_eq!(vod_ext(Some("mkv")), "mkv");
        assert_eq!(vod_ext(Some(".mkv")), "mkv");
        assert_eq!(vod_ext(Some(" ")), DEFAULT_VOD_EXT);
        assert_eq!(vod_ext(None), DEFAULT_VOD_EXT);
        assert_eq!(episode_label(2, 5, "The One"), "S02E05 · The One");
        assert_eq!(episode_label(10, 112, " "), "S10E112");
    }

    #[test]
    fn ad_hoc_titles_and_paths() {
        assert_eq!(url_title("http://h:1/vod/My%20Movie.mkv?token=abc"), "My Movie.mkv");
        assert_eq!(url_title("https://user:pw@cdn.example/"), "cdn.example");
        assert_eq!(url_title("rtsp://cam.local/stream1"), "stream1");
        assert_eq!(url_title("not a url"), "Stream");
        let secret = "resolve-unit-secret-7f3a";
        crate::redact::register_secret(secret);
        assert_eq!(url_title(&format!("http://h/{secret}")), "h", "a secret segment is never a title");
        // Unregistered Xtream credentials: a trailing `/` never steps back to the password, and a
        // segment the full-URL redaction hides is not used.
        assert_eq!(url_title("http://panel.example/live/user/pw-sekrit/"), "panel.example");
        assert_eq!(url_title("http://panel.example:8080/user/pw-sekrit/"), "panel.example");
        assert_eq!(url_title("http://h/live/user/pw/123.ts"), "123.ts");
        assert_eq!(url_title("http://h/a/b/"), "h");

        assert!(is_local("/tmp/a.mp4") && is_local("a.mp4") && is_local("FILE:///tmp/a.mp4"));
        assert!(!is_local("http://h/a.mp4") && !is_local("rtmp://h/x"));
        assert_eq!(local_path("file:///tmp/a%20b.mp4"), Some(PathBuf::from("/tmp/a b.mp4")));
        assert_eq!(local_path("http://h/a"), None);
        assert_eq!(percent_decode("a%2Fb%zz%4"), "a/b%zz%4");
    }

    #[test]
    fn pipe_header_mapping() {
        let mut h = HttpHeaders::default();
        for (k, v) in [("User-Agent", "UA"), ("Referer", "http://r"), ("ORIGIN", "http://o"), ("Cookie", "a=1"), ("cookie", "b=2"), ("X", " ")] {
            apply_header(&mut h, k, v);
        }
        assert_eq!(h.user_agent.as_deref(), Some("UA"));
        assert_eq!(h.referrer.as_deref(), Some("http://r"));
        assert_eq!(h.origin.as_deref(), Some("http://o"));
        assert_eq!(h.extra, vec![("Cookie".to_owned(), "b=2".to_owned())]);
    }
}
