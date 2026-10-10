//! Onboarding/source-editor validation and creation. Owner: WP-06.
//!
//! Probe rules: an M3U draft that is really an Xtream login (a `get.php` URL, or entries that
//! are one account's stream URLs — DESIGN §27.2) is upgraded when that login works
//! (`upgraded_from`), unless the draft says `keep_m3u`; a failed login never fails the probe,
//! the M3U result stands. An M3U URL is fetched and sniffed (HTML → "This looks like a web
//! page, not a playlist"; HLS media → offer "Add as single stream"); Xtream does
//! `authenticate` and returns AccountInfo (expired accounts are reported but may still be saved).
//!
//! Details fixed by the implementation:
//! * The HTTP client offers no partial download, so an M3U URL is downloaded in full to a
//!   private temp file in the 0700 playlist cache (deleted afterwards) and every entry is
//!   counted: `estimate_channels` is then exact. Local files are parsed in place.
//! * Probe errors are the import errors: `AuthRejected`, `AccountUnavailable{status}`,
//!   `BadPayload` (web page / single HLS stream / not a playlist), network errors.
//! * Xtream `estimate_channels` = number of live categories (no 100 MB stream list here);
//!   `epg_urls` stays empty for Xtream (the guide URL carries the password and is created by
//!   the import).
//! * `add_source` validates nothing (the draft was probed); see the module docs of `jobs` for
//!   the never-refreshed duplicate reuse. A reused row takes the retried draft (name, password,
//!   User-Agent, settings, kind…) before its import and is enabled: the retry exists to fix
//!   those, and a restored source needing credentials is disabled until they are entered.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobPhase, ProbeResult, Progress, blocking, check, notify, write};
use crate::error::{Error, Result};
use crate::model::{HttpHeaders, Source, SourceDraft, SourceId, SourceKind, XtreamCredentials};
use crate::msg::DataScope;
use crate::sources::http::{DownloadOutcome, DownloadRequest, open_decompressed};
use crate::sources::m3u;
use crate::sources::xtream::XtreamClient;
use crate::sources::xtream::detect::PlaylistScan;

pub async fn probe(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<ProbeResult> {
    progress.set(JobPhase::Connecting);
    let cfg = crate::config::current(&ctx.config);
    let ua = draft.settings.user_agent.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| cfg.player.user_agent.clone());
    if draft.kind == SourceKind::Xtream {
        let creds = draft.xtream.clone().ok_or_else(|| Error::Config("enter the server, username and password".into()))?;
        return probe_xtream(ctx, creds, ua, None, cancel).await;
    }
    let upgrade = !draft.settings.keep_m3u;

    // a pasted get.php URL: try its login before downloading anything
    let get_php = draft.url.as_deref().filter(|_| upgrade && draft.kind == SourceKind::M3uUrl).and_then(m3u::detect_xtream_url);
    if let Some(creds) = get_php.clone() {
        match probe_xtream(ctx, creds, ua.clone(), Some(draft.kind), cancel).await {
            Err(e) if !matches!(e, Error::Cancelled) => tracing::debug!("probe: the playlist's Xtream login failed: {e}"),
            r => return r,
        }
    }

    let (mut result, found) = probe_m3u(ctx, draft, ua.clone(), progress, cancel).await?;
    // entries that are one Xtream account (§27.2); a failed login never fails the probe
    if let Some(creds) = found.filter(|_| upgrade && get_php.is_none()) {
        match probe_xtream(ctx, creds, ua, Some(draft.kind), cancel).await {
            Ok(mut r) => {
                r.suggested_name = result.suggested_name.take().or(r.suggested_name);
                return Ok(r);
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => tracing::debug!("probe: the playlist's Xtream login failed: {e}"),
        }
    }
    Ok(result)
}

/// Sign in with an Xtream login. `upgraded_from` = the M3U draft kind it was found in.
async fn probe_xtream(
    ctx: &JobContext,
    creds: XtreamCredentials,
    ua: String,
    upgraded_from: Option<SourceKind>,
    cancel: &CancellationToken,
) -> Result<ProbeResult> {
    crate::redact::register_secret(&creds.username);
    crate::redact::register_secret(creds.password.expose());
    let client = XtreamClient::new(ctx.http.clone(), creds.clone(), Some(ua));
    let (_, account) = client.authenticate(crate::model::now(), cancel).await?;
    let estimate = match client.live_categories(cancel).await {
        Ok(c) => u32::try_from(c.len()).ok(),
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        Err(e) => {
            tracing::debug!("probe: live categories: {e}");
            None
        }
    };
    Ok(ProbeResult {
        kind: SourceKind::Xtream,
        suggested_name: host_name(&creds.server),
        xtream: Some(creds),
        account: Some(account),
        estimate_channels: estimate,
        epg_urls: Vec::new(),
        upgraded_from,
    })
}

/// Download (URL) or read (file) and count the playlist; also returns the Xtream login its
/// entries carry, if any.
async fn probe_m3u(
    ctx: &JobContext,
    draft: &SourceDraft,
    ua: String,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<(ProbeResult, Option<XtreamCredentials>)> {
    match draft.kind {
        SourceKind::Xtream => Err(Error::Config("enter the server, username and password".into())),
        SourceKind::M3uFile => {
            let path = draft.path.clone().ok_or_else(|| Error::Config("choose a playlist file".into()))?;
            let suggested = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let (count, epg_urls, found) = count_entries(path, None, progress.clone(), cancel.clone()).await?;
            let r = ProbeResult {
                kind: SourceKind::M3uFile,
                xtream: None,
                account: None,
                estimate_channels: Some(count),
                suggested_name: suggested,
                epg_urls,
                upgraded_from: None,
            };
            Ok((r, found))
        }
        SourceKind::M3uUrl => {
            let url = draft.url.as_deref().map(str::trim).filter(|u| !u.is_empty()).ok_or_else(|| Error::Config("enter the playlist URL".into()))?;
            // query values may be credentials: keep them out of logs from here on
            if let Ok(u) = url::Url::parse(url) {
                for (_, v) in u.query_pairs() {
                    crate::redact::register_secret(&v);
                }
            }
            let digest = blake3::hash(url.as_bytes()).to_hex();
            let dest = ctx.paths.playlist_cache().join(format!("probe-{}-{}.m3u", digest.get(..16).unwrap_or("x"), progress.id.0));
            let req = DownloadRequest {
                url: url.to_owned().into(),
                dest: dest.clone(),
                headers: HttpHeaders { user_agent: Some(ua), ..Default::default() },
                etag: None,
                last_modified: None,
            };
            let p = progress.clone();
            let path = match ctx.http.download(&req, &move |done, total| p.set(JobPhase::Downloading { done, total }), cancel).await? {
                DownloadOutcome::Downloaded { path, .. } => path,
                DownloadOutcome::NotModified => return Err(Error::BadPayload("the server answered “not modified” to a fresh request".into())),
            };
            let guard = Remove(path.clone());
            let base = url::Url::parse(url).ok();
            let r = count_entries(path, base, progress.clone(), cancel.clone()).await;
            drop(guard);
            let (count, epg_urls, found) = r?;
            let r = ProbeResult {
                kind: SourceKind::M3uUrl,
                xtream: None,
                account: None,
                estimate_channels: Some(count),
                suggested_name: host_name(url),
                epg_urls,
                upgraded_from: None,
            };
            Ok((r, found))
        }
    }
}

/// Removes a temp file when dropped.
struct Remove(PathBuf);

impl Drop for Remove {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// "http://panel.example:8080/x" → "panel.example".
fn host_name(url: &str) -> Option<String> {
    let t = url.trim();
    let with_scheme = if t.contains("://") { t.to_owned() } else { format!("http://{t}") };
    url::Url::parse(&with_scheme).ok()?.host_str().map(|h| h.trim_start_matches("www.").to_owned()).filter(|h| !h.is_empty())
}

/// Parse a playlist file and count its entries (blocking pool). Returns (entries, url-tvg, the
/// Xtream login of its entries if they are one account).
async fn count_entries(
    path: PathBuf,
    base: Option<url::Url>,
    progress: Progress,
    cancel: CancellationToken,
) -> Result<(u32, Vec<String>, Option<XtreamCredentials>)> {
    blocking(move || {
        let parser = m3u::parse(open_decompressed(&path)?, base.as_ref()).map_err(super::import_source::m3u_error)?;
        let mut parser = parser;
        let mut n = 0u64;
        let mut scan = Some(PlaylistScan::default());
        for e in parser.by_ref() {
            match e {
                Ok(e) => {
                    n += 1;
                    if scan.as_mut().is_some_and(|s| !s.push(&e.url)) {
                        scan = None;
                    }
                }
                Err(Error::Parse { .. }) => {}
                Err(e) => return Err(e),
            }
            if n.is_multiple_of(1_000) {
                check(&cancel)?;
                progress.set(JobPhase::Parsing { items: n });
            }
        }
        if n == 0 {
            return Err(Error::BadPayload("the playlist contains no channels".into()));
        }
        Ok((u32::try_from(n).unwrap_or(u32::MAX), parser.header().tvg_urls.clone(), scan.and_then(PlaylistScan::finish)))
    })
    .await
}

/// Same locator as `draft`: two Xtream logins by server + username, anything else by the
/// playlist URL / file (also across kinds: an M3U playlist and its upgraded draft, §27.3).
fn same_locator(s: &Source, draft: &SourceDraft) -> bool {
    match (s.kind, draft.kind) {
        (SourceKind::Xtream, SourceKind::Xtream) => match (&s.xtream, &draft.xtream) {
            (Some(a), Some(b)) => a.server.trim_end_matches('/') == b.server.trim_end_matches('/') && a.username == b.username,
            _ => false,
        },
        _ => {
            let url = s.url.as_ref().map(|u| u.expose().trim()).filter(|u| !u.is_empty());
            (url.is_some() && url == draft.url.as_deref().map(str::trim)) || (s.path.is_some() && s.path == draft.path)
        }
    }
}

/// Overwrite the user-editable fields of `src` with `draft` (status, order and id stay). The
/// source is enabled: entering the credentials of a restored source (disabled until then) is
/// the user asking for it to work.
fn apply_draft(src: &mut Source, draft: &SourceDraft) {
    src.name = draft.name.clone();
    src.icon = draft.icon.clone();
    src.color = draft.color;
    src.url = draft.url.clone().map(Into::into);
    src.path = draft.path.clone();
    src.xtream = draft.xtream.clone();
    src.settings = draft.settings.clone();
    src.enabled = true;
}

/// Insert the source and run its first refresh inline (progress continues on the same job).
pub async fn add_source(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<SourceId> {
    check(cancel)?;
    // find-or-insert in ONE writer closure: an add racing this one (same locator, another job)
    // sees the row this one saved and reuses it instead of inserting a second source
    let (d, now) = (draft.clone(), crate::model::now());
    let (id, reused) = write(ctx, move |c| {
        let saved = crate::store::sources::list_sources(c)?.into_iter().find(|s| s.status.last_refresh.is_none() && same_locator(s, &d));
        match saved {
            Some(mut src) => {
                apply_draft(&mut src, &d);
                crate::store::sources::update_source(c, &src)?;
                if src.kind != d.kind {
                    crate::store::sources::set_source_kind(c, src.id, d.kind, d.xtream.as_ref())?;
                }
                Ok((src.id, true))
            }
            None => Ok((crate::store::sources::insert_source(c, &d, now)?, false)),
        }
    })
    .await?;
    if reused {
        tracing::info!("adding {}: importing the source saved by an earlier attempt", draft.name);
    }
    notify(ctx, DataScope::Sources);
    if let Err(e) = super::import_source::refresh_source(ctx, id, true, progress, cancel).await {
        if !matches!(e, Error::Cancelled) {
            tracing::warn!(target: "tuner::jobs", "{} was saved; its first import failed (refresh it from Settings ▸ Playlists)", draft.name);
        }
        return Err(e);
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_names() {
        assert_eq!(host_name("http://www.panel.example:8080/get.php?x=1").as_deref(), Some("panel.example"));
        assert_eq!(host_name("panel.example").as_deref(), Some("panel.example"));
        assert_eq!(host_name("::"), None);
    }

    #[test]
    fn count_is_exact_for_small_files() {
        let dir = std::env::temp_dir().join(format!("tuner-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.m3u");
        std::fs::write(&p, "#EXTM3U url-tvg=\"http://g/e.xml\"\n#EXTINF:-1,A\nhttp://h/a.ts\n#EXTINF:-1,B\nhttp://h/b.ts\n").unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let (n, urls, login) = rt.block_on(count_entries(p.clone(), None, Progress::detached(), CancellationToken::new())).unwrap();
        assert_eq!((n, urls), (2, vec!["http://g/e.xml".to_owned()]));
        assert!(login.is_none());
        // entries that are one account's stream URLs carry its login
        std::fs::write(&p, "#EXTM3U\n#EXTINF:-1,A\nhttp://h/live/u/p/1.ts\n#EXTINF:-1,B\nhttp://h/movie/u/p/2.mkv\n").unwrap();
        let (n, _, login) = rt.block_on(count_entries(p.clone(), None, Progress::detached(), CancellationToken::new())).unwrap();
        assert_eq!((n, login.map(|c| (c.server, c.username))), (2, Some(("http://h".to_owned(), "u".to_owned()))));
        std::fs::write(&p, "<!DOCTYPE html><html><body>expired</body></html>").unwrap();
        let e = rt.block_on(count_entries(p.clone(), None, Progress::detached(), CancellationToken::new())).unwrap_err();
        assert!(e.to_string().contains("web page"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
