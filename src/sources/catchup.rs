//! Catch-up URL building for every flavour. Owner: WP-05a. DESIGN §10 (semantics follow Kodi
//! pvr.iptvsimple / UniPlayer; re-implemented, not copied — Kodi is GPL-2).
//!
//! `CatchupRequest.start/stop` are the DISPLAYED times (`ProgrammeSlim::shifted` by the
//! channel's tvg-shift); the archive is indexed in EPG time, hence:
//! Effective programme start = `start − correction_s − tvg_shift_min·60 + offset_s(user)`;
//! duration = `stop − start` (3600 if unknown), capped so `start + duration <= now`.
//! Granularity: 1 s if the template uses `{utc}`, `${start}`, `{S}` or `{offset:1}`, else
//! 60 s (start floored to the minute).
//!
//! | kind              | URL |
//! |-------------------|-----|
//! | Default           | expand(`template`) (no template → like Append) |
//! | Append            | live URL + expand(`template`) |
//! | Shift             | live URL + (`?`/`&`) `utc={utc}&lutc={lutc}` |
//! | Flussonic(-Hls/-Ts)| rewrite `…/mpegts` → `…/timeshift_abs-{utc}.ts`, `…/index.m3u8` → `…/timeshift_rel-{offset:1}.m3u8`, `…/mono.m3u8` → `…/mono-timeshift_rel-{offset:1}.m3u8` |
//! | Xc                | `{host}/timeshift/{u}/{p}/{duration:60}/{Y}-{m}-{d}:{H}-{M}/{id}.{ext}` (server-local) |
//! | Vod               | expand(`template`) or `{catchup-id}` |
//!
//! Placeholders: `{utc}` `${start}` `{utcend}` `${end}` `{lutc}` `${now}` `${timestamp}`
//! `{duration}` `${duration}` `{duration:X}` `{offset:X}` `${offset}` `{Y}{m}{d}{H}{M}{S}`
//! (local = server tz), `{utc:FMT}` `${start:FMT}` `{utcend:FMT}` `${end:FMT}` `{lutc:FMT}`
//! `${now:FMT}` (FMT letters Y m d H M S → strftime), `{catchup-id}`.

use std::sync::LazyLock;

use regex::Regex;

use crate::error::{Error, Result};
use crate::model::{CatchupKind, CatchupSpec, Timestamp};

/// Everything needed to build one catch-up URL.
#[derive(Debug, Clone)]
pub struct CatchupRequest<'a> {
    pub live_url: &'a str,
    pub spec: &'a CatchupSpec,
    pub start: Timestamp,
    pub stop: Timestamp,
    pub now: Timestamp,
    pub tvg_shift_min: i32,
    /// Per-channel user offset (seconds, may be negative).
    pub user_offset_s: i64,
    pub catchup_id: Option<&'a str>,
    /// Timezone for `{Y}{m}{d}{H}{M}{S}` (server tz for Xtream, local tz otherwise).
    pub tz: jiff::tz::TimeZone,
}

/// Query appended by `Shift` (and by `Default`/`Append` without a template).
const SHIFT_QUERY: &str = "?utc={utc}&lutc={lutc}";
/// Duration used when the programme has no usable stop.
const DEFAULT_DURATION_S: i64 = 3600;

/// `scheme://host` / channel path / file stem / `mpegts` | `.m3u8` / query (Kodi-compatible).
static FLUSSONIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(https?://[^/]+)/(.*)/([^/]*)(mpegts|\.m3u8)(\?.+=.+)?$").expect("static regex"));
/// `scheme://host` / user / pass / stream id / extension; `live/` optional, query ignored.
static XC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(https?://[^/]+)/(?:live/)?([^/]+)/([^/]+)/([^/.?]+)(?:\.([A-Za-z0-9]+))?(?:\?.*)?$").expect("static regex")
});

/// Build the URL. Errors: `NotFound` when outside the archive window, `Parse` when a
/// Flussonic/XC rewrite regex doesn't match the live URL.
///
/// The window is checked on the displayed start (what the guide shows ⟲ for); a programme
/// whose effective archive start is not yet in the past is also `NotFound`. Only template
/// parts are expanded, never the live URL itself (it may contain credentials or braces), and
/// error messages never contain the URL.
pub fn build_url(req: &CatchupRequest<'_>) -> Result<String> {
    if !in_window(req.spec, req.start, req.now) {
        return Err(Error::NotFound("outside catch-up window".into()));
    }
    let effective = effective_start(req);
    if effective >= req.now {
        return Err(Error::NotFound("programme not in the archive yet".into()));
    }
    // Corrections move the start by hours; one moving it more than a day past the window is
    // a bogus provider value, not an archived programme.
    if effective < req.now.saturating_sub(i64::from(req.spec.days) * 86_400 + 86_400) {
        return Err(Error::NotFound("outside catch-up window".into()));
    }
    let live = req.live_url.trim();
    let template = req.spec.template.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let url = match req.spec.kind {
        CatchupKind::Default => match template {
            Some(t) if t.contains("://") => expand(t, req),
            // A relative `catchup-source` (`?utc=…`) only makes sense appended (as in Kodi).
            Some(t) => append(live, &expand(t, req)),
            None => append(live, &expand(SHIFT_QUERY, req)),
        },
        CatchupKind::Append => append(live, &expand(template.unwrap_or(SHIFT_QUERY), req)),
        CatchupKind::Shift => append(live, &expand(SHIFT_QUERY, req)),
        CatchupKind::Flussonic | CatchupKind::FlussonicHls | CatchupKind::FlussonicTs => flussonic(live, req)?,
        CatchupKind::Xc => xc(live, req)?,
        CatchupKind::Vod => expand(template.unwrap_or("{catchup-id}"), req),
    };
    if url.trim().is_empty() {
        return Err(Error::NotFound("no catch-up URL for this programme".into()));
    }
    Ok(url)
}

/// Expand placeholders in `template` (tokenizer over `{…}` and `${…}`; unknown tokens left
/// verbatim).
///
/// Times use the granularity of `template` (module docs). `{Y}{m}{d}{H}{M}{S}` and the
/// formatted forms (`{utc:FMT}`, `${start:FMT}`, …) are rendered in `req.tz` (Kodi renders
/// them in local time; for Xtream the caller passes the server tz). `{duration:X}` rounds up
/// (a 90 s tail still needs its minute), `{offset:X}` rounds down. `{catchup-id}` without an
/// id expands to nothing.
pub fn expand(template: &str, req: &CatchupRequest<'_>) -> String {
    let t = Times::new(req, one_second_granularity(template));
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(i) = rest.find(['{', '$']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let open = if tail.starts_with("${") {
            2
        } else if tail.starts_with('{') {
            1
        } else {
            out.push('$');
            rest = &tail[1..];
            continue;
        };
        let Some(close) = tail[open..].find('}') else {
            out.push_str(tail);
            rest = "";
            break;
        };
        let name = &tail[open..open + close];
        if name.contains('{') {
            // `{{utc}`: keep the outer opener verbatim and rescan from the inner brace.
            out.push_str(&tail[..open]);
            rest = &tail[open..];
            continue;
        }
        match token(name, open == 2, &t, req) {
            Some(v) => out.push_str(&v),
            None => out.push_str(&tail[..open + close + 1]),
        }
        rest = &tail[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// 1 s granularity when the template asks for second precision, else 60 s.
fn one_second_granularity(template: &str) -> bool {
    ["{utc}", "${start}", "{S}", "{offset:1}"].iter().any(|t| template.contains(t))
}

/// Archive (EPG) start before granularity: displayed start − correction − tvg-shift + user offset.
/// Saturating: correction and offsets come from untrusted playlists.
fn effective_start(req: &CatchupRequest<'_>) -> Timestamp {
    req.start
        .saturating_sub(req.spec.correction_s)
        .saturating_sub(i64::from(req.tvg_shift_min) * 60)
        .saturating_add(req.user_offset_s)
}

/// Times resolved for one expansion.
struct Times {
    start: Timestamp,
    end: Timestamp,
    duration: i64,
    now: Timestamp,
}

impl Times {
    fn new(req: &CatchupRequest<'_>, one_second: bool) -> Times {
        let duration = if req.stop > req.start { req.stop.saturating_sub(req.start) } else { DEFAULT_DURATION_S };
        let exact = effective_start(req);
        let start = if one_second { exact } else { exact.saturating_sub(exact.rem_euclid(60)) };
        // Capped at now: the archive cannot serve the future part of a running programme.
        let end = exact.saturating_add(duration).min(req.now).max(start);
        Times { start, end, duration: end.saturating_sub(start), now: req.now }
    }
}

fn token(name: &str, dollar: bool, t: &Times, req: &CatchupRequest<'_>) -> Option<String> {
    if dollar {
        return Some(match name {
            "start" => t.start.to_string(),
            "end" => t.end.to_string(),
            "now" | "timestamp" => t.now.to_string(),
            "duration" => t.duration.to_string(),
            "offset" => t.now.saturating_sub(t.start).to_string(),
            _ => {
                let (base, fmt) = name.split_once(':')?;
                let at = match base {
                    "start" => t.start,
                    "end" => t.end,
                    "now" => t.now,
                    _ => return None,
                };
                format_time(at, fmt, &req.tz)?
            }
        });
    }
    Some(match name {
        "utc" => t.start.to_string(),
        "utcend" => t.end.to_string(),
        "lutc" => t.now.to_string(),
        "duration" => t.duration.to_string(),
        "catchup-id" => req.catchup_id.unwrap_or_default().to_owned(),
        "Y" | "m" | "d" | "H" | "M" | "S" => format_time(t.start, name, &req.tz)?,
        _ => {
            let (base, arg) = name.split_once(':')?;
            match base {
                "duration" => {
                    let x = divisor(arg)?;
                    // Rounded up without `duration + x - 1`, which overflows for a huge `x`.
                    (t.duration / x + i64::from(t.duration % x != 0)).to_string()
                }
                "offset" => t.now.saturating_sub(t.start).div_euclid(divisor(arg)?).to_string(),
                "utc" => format_time(t.start, arg, &req.tz)?,
                "utcend" => format_time(t.end, arg, &req.tz)?,
                "lutc" => format_time(t.now, arg, &req.tz)?,
                _ => return None,
            }
        }
    })
}

fn divisor(arg: &str) -> Option<i64> { arg.trim().parse::<i64>().ok().filter(|&x| x > 0) }

/// Render `at` in `tz` with Kodi-style letters (`Y` → 4-digit year, `m d H M S` → 2 digits);
/// every other character is copied.
fn format_time(at: Timestamp, fmt: &str, tz: &jiff::tz::TimeZone) -> Option<String> {
    use std::fmt::Write as _;
    let dt = tz.to_datetime(jiff::Timestamp::from_second(at).ok()?);
    let mut out = String::with_capacity(fmt.len() * 2);
    for c in fmt.chars() {
        let _ = match c {
            'Y' => write!(out, "{:04}", dt.year()),
            'm' => write!(out, "{:02}", dt.month()),
            'd' => write!(out, "{:02}", dt.day()),
            'H' => write!(out, "{:02}", dt.hour()),
            'M' => write!(out, "{:02}", dt.minute()),
            'S' => write!(out, "{:02}", dt.second()),
            other => {
                out.push(other);
                Ok(())
            }
        };
    }
    Some(out)
}

/// `live` + fragment: a leading `?`/`&` becomes the separator the live URL needs.
fn append(live: &str, frag: &str) -> String {
    match frag.strip_prefix(['?', '&']) {
        Some(q) => {
            let sep = if live.contains('?') { '&' } else { '?' };
            format!("{live}{sep}{q}")
        }
        None => format!("{live}{frag}"),
    }
}

fn flussonic(live: &str, req: &CatchupRequest<'_>) -> Result<String> {
    let c = FLUSSONIC_RE
        .captures(live)
        .ok_or_else(|| Error::parse(None, "catch-up: live URL is not a Flussonic stream URL"))?;
    let (host, path, stem) = (&c[1], &c[2], &c[3]);
    let query = c.get(5).map_or("", |m| m.as_str());
    let file = if &c[4] == "mpegts" {
        expand("timeshift_abs-{utc}.ts", req)
    } else if stem.is_empty() || stem == "index" {
        expand("timeshift_rel-{offset:1}.m3u8", req)
    } else {
        format!("{stem}-{}", expand("timeshift_rel-{offset:1}.m3u8", req))
    };
    Ok(format!("{host}/{path}/{file}{query}"))
}

fn xc(live: &str, req: &CatchupRequest<'_>) -> Result<String> {
    let c = XC_RE.captures(live).ok_or_else(|| Error::parse(None, "catch-up: live URL is not an Xtream stream URL"))?;
    let (host, user, pass, id) = (&c[1], &c[2], &c[3], &c[4]);
    let ext = match c.get(5).map(|m| m.as_str()) {
        Some(e) if e.eq_ignore_ascii_case("m3u8") => "m3u8",
        _ => "ts",
    };
    let when = expand("{duration:60}/{Y}-{m}-{d}:{H}-{M}", req);
    Ok(format!("{host}/timeshift/{user}/{pass}/{when}/{id}.{ext}"))
}

/// Is a programme starting at `start` inside the archive window at `now`?
/// (`now - days·86400 <= start < now`.)
pub fn in_window(spec: &CatchupSpec, start: Timestamp, now: Timestamp) -> bool {
    start < now && start >= now - spec.days as i64 * 86_400
}

/// Parse an M3U `catchup=` value (aliases: `catchup-type`; `xtream` → Xc, `timeshift` → Shift,
/// `fs` → Flussonic). Implemented in the skeleton (used by WP-03's M3U parser).
pub fn parse_kind(s: &str) -> Option<CatchupKind> {
    Some(match s.trim().to_ascii_lowercase().as_str() {
        "default" => CatchupKind::Default,
        "append" => CatchupKind::Append,
        "shift" | "timeshift" => CatchupKind::Shift,
        "flussonic" | "fs" => CatchupKind::Flussonic,
        "flussonic-hls" | "fs-hls" => CatchupKind::FlussonicHls,
        "flussonic-ts" | "fs-ts" => CatchupKind::FlussonicTs,
        "xc" | "xtream" => CatchupKind::Xc,
        "vod" => CatchupKind::Vod,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_picks_the_separator() {
        assert_eq!(append("http://h/a", "?x=1"), "http://h/a?x=1");
        assert_eq!(append("http://h/a?t=2", "?x=1"), "http://h/a?t=2&x=1");
        assert_eq!(append("http://h/a", "&x=1"), "http://h/a?x=1");
        assert_eq!(append("http://h/a", "/b.m3u8"), "http://h/a/b.m3u8");
    }

    #[test]
    fn second_granularity_tokens() {
        for t in ["{utc}", "a${start}b", "{S}", "{offset:1}"] {
            assert!(one_second_granularity(t), "{t}");
        }
        for t in ["{utcend}", "{utc:YmdHM}", "{offset:60}", "${end}", "{M}"] {
            assert!(!one_second_granularity(t), "{t}");
        }
    }

    #[test]
    fn format_letters_and_literals() {
        let tz = jiff::tz::TimeZone::fixed(jiff::tz::offset(2));
        // 2026-10-09 20:00:05 UTC = 22:00:05 at +02:00.
        assert_eq!(format_time(1_791_576_005, "Y-m-d H:M:S x", &tz).as_deref(), Some("2026-10-09 22:00:05 x"));
        assert_eq!(divisor("0"), None);
        assert_eq!(divisor("-5"), None);
        assert_eq!(divisor(" 60 "), Some(60));
    }
}
