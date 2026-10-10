//! Onboarding/source-editor validation and creation. Owner: WP-06.
//!
//! Probe rules: a pasted URL matching `m3u::detect_xtream_url` is upgraded to Xtream; an M3U
//! URL is fetched with a 1 MiB range/limit and sniffed (HTML → "This looks like a web page,
//! not a playlist"; HLS media → offer "Add as single stream"); Xtream does `authenticate`
//! and returns AccountInfo (expired accounts are reported but may still be saved).
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
//!   the never-refreshed duplicate reuse.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::{JobContext, JobPhase, ProbeResult, Progress, blocking, check, notify, read, write};
use crate::error::{Error, Result};
use crate::model::{HttpHeaders, Source, SourceDraft, SourceId, SourceKind};
use crate::msg::DataScope;
use crate::sources::http::{DownloadOutcome, DownloadRequest, open_decompressed};
use crate::sources::m3u;
use crate::sources::xtream::XtreamClient;

pub async fn probe(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<ProbeResult> {
    progress.set(JobPhase::Connecting);
    let cfg = crate::config::current(&ctx.config);
    let ua = draft.settings.user_agent.clone().filter(|u| !u.trim().is_empty()).unwrap_or_else(|| cfg.player.user_agent.clone());
    let detected = match draft.kind {
        SourceKind::M3uUrl => draft.url.as_deref().and_then(m3u::detect_xtream_url),
        _ => None,
    };
    if let Some(creds) = detected.or_else(|| draft.xtream.clone().filter(|_| draft.kind == SourceKind::Xtream)) {
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
        return Ok(ProbeResult {
            kind: SourceKind::Xtream,
            suggested_name: host_name(&creds.server),
            xtream: Some(creds),
            account: Some(account),
            estimate_channels: estimate,
            epg_urls: Vec::new(),
        });
    }

    match draft.kind {
        SourceKind::Xtream => Err(Error::Config("enter the server, username and password".into())),
        SourceKind::M3uFile => {
            let path = draft.path.clone().ok_or_else(|| Error::Config("choose a playlist file".into()))?;
            let suggested = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let (count, epg_urls) = count_entries(path, None, progress.clone(), cancel.clone()).await?;
            Ok(ProbeResult { kind: SourceKind::M3uFile, xtream: None, account: None, estimate_channels: Some(count), suggested_name: suggested, epg_urls })
        }
        SourceKind::M3uUrl => {
            let url = draft.url.as_deref().map(str::trim).filter(|u| !u.is_empty()).ok_or_else(|| Error::Config("enter the playlist URL".into()))?;
            // query values may be credentials: keep them out of logs from here on
            if let Ok(u) = url::Url::parse(url) {
                for (_, v) in u.query_pairs() {
                    crate::redact::register_secret(&v);
                }
            }
            let dest = ctx.paths.playlist_cache().join(format!("probe-{}.m3u", blake3::hash(url.as_bytes()).to_hex().get(..16).unwrap_or("x")));
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
            let (count, epg_urls) = r?;
            Ok(ProbeResult {
                kind: SourceKind::M3uUrl,
                xtream: None,
                account: None,
                estimate_channels: Some(count),
                suggested_name: host_name(url),
                epg_urls,
            })
        }
    }
}

/// Removes a temp file when dropped.
struct Remove(PathBuf);

impl Drop for Remove {
    fn drop(&mut self) { let _ = std::fs::remove_file(&self.0); }
}

/// "http://panel.example:8080/x" → "panel.example".
fn host_name(url: &str) -> Option<String> {
    let t = url.trim();
    let with_scheme = if t.contains("://") { t.to_owned() } else { format!("http://{t}") };
    url::Url::parse(&with_scheme).ok()?.host_str().map(|h| h.trim_start_matches("www.").to_owned()).filter(|h| !h.is_empty())
}

/// Parse a playlist file and count its entries (blocking pool). Returns (entries, url-tvg).
async fn count_entries(path: PathBuf, base: Option<url::Url>, progress: Progress, cancel: CancellationToken) -> Result<(u32, Vec<String>)> {
    blocking(move || {
        let parser = m3u::parse(open_decompressed(&path)?, base.as_ref()).map_err(super::import_source::m3u_error)?;
        let mut parser = parser;
        let mut n = 0u64;
        for e in parser.by_ref() {
            match e {
                Ok(_) => n += 1,
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
        Ok((u32::try_from(n).unwrap_or(u32::MAX), parser.header().tvg_urls.clone()))
    })
    .await
}

/// Same locator (kind + URL / file / server + username) as `draft`.
fn same_locator(s: &Source, draft: &SourceDraft) -> bool {
    s.kind == draft.kind
        && match s.kind {
            SourceKind::M3uUrl => s.url.as_ref().map(|u| u.expose().trim()) == draft.url.as_deref().map(str::trim),
            SourceKind::M3uFile => s.path.is_some() && s.path == draft.path,
            SourceKind::Xtream => match (&s.xtream, &draft.xtream) {
                (Some(a), Some(b)) => a.server.trim_end_matches('/') == b.server.trim_end_matches('/') && a.username == b.username,
                _ => false,
            },
        }
}

/// Insert the source and run its first refresh inline (progress continues on the same job).
pub async fn add_source(ctx: &JobContext, draft: &SourceDraft, progress: &Progress, cancel: &CancellationToken) -> Result<SourceId> {
    check(cancel)?;
    let d = draft.clone();
    let reuse = read(ctx, move |c| {
        Ok(crate::store::sources::list_sources(c)?.into_iter().find(|s| s.status.last_refresh.is_none() && same_locator(s, &d)).map(|s| s.id))
    })
    .await?;
    let id = match reuse {
        Some(id) => {
            tracing::info!("adding {}: retrying the import of the source saved by an earlier attempt", draft.name);
            id
        }
        None => {
            let d = draft.clone();
            let now = crate::model::now();
            let id = write(ctx, move |c| crate::store::sources::insert_source(c, &d, now)).await?;
            notify(ctx, DataScope::Sources);
            id
        }
    };
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
        let (n, urls) = rt.block_on(count_entries(p.clone(), None, Progress::detached(), CancellationToken::new())).unwrap();
        assert_eq!((n, urls), (2, vec!["http://g/e.xml".to_owned()]));
        std::fs::write(&p, "<!DOCTYPE html><html><body>expired</body></html>").unwrap();
        let e = rt.block_on(count_entries(p.clone(), None, Progress::detached(), CancellationToken::new())).unwrap_err();
        assert!(e.to_string().contains("web page"), "{e}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
