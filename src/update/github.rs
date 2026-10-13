//! The GitHub Releases API (DESIGN §41.2): one request at start and every 6 h through the app's
//! own HTTP client (its proxy settings, `system-proxy`), no token. Unauthenticated calls get 60
//! an hour per address; each one is conditional (`If-None-Match` with the last ETag, kept with
//! the last answer in the cache), and GitHub doesn't count a 304. A 403/429 with the limit
//! used up waits for `X-RateLimit-Reset` (or `Retry-After`).
//!
//! [`pick`] chooses: drafts and pre-releases never; of the stable tags (`v<x>.<y>.<z>`, no
//! suffix) the newest, with its package for this architecture and that package's `.minisig`
//! when both are attached.

use std::path::Path;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::verify::{self, SIGNATURE_SUFFIX};
use super::{Asset, Package, Release, VERSION, stable_version};
use crate::model::{HttpHeaders, Timestamp};
use crate::sources::http::HttpClient;

/// The largest answer read (20 releases with their notes are about 100 KiB).
const MAX_ANSWER: u64 = 4 << 20;
/// How long to wait when GitHub says "slow down" without saying until when.
const LIMIT_WAIT_S: i64 = 3600;
/// The last answer and its ETag, in the updates folder.
const ANSWER_FILE: &str = "releases.json";
const ETAG_FILE: &str = "releases.etag";

/// A release as the API lists it (the fields Tuner reads).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GhRelease {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub assets: Vec<GhAsset>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GhAsset {
    pub name: String,
    pub browser_download_url: String,
    #[serde(default)]
    pub size: u64,
}

/// Why a check got no list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The hourly limit is used up: not before then (unix seconds).
    Limited { until: Timestamp },
    /// Redacted, one line.
    Failed(String),
}

/// The User-Agent of every update request (GitHub asks for a meaningful one).
pub fn user_agent() -> String {
    format!("Tuner/{VERSION} (+https://github.com/rthoms334/omatuner)")
}

/// The API's headers: its JSON and a pinned API version.
pub fn api_headers() -> HttpHeaders {
    HttpHeaders {
        user_agent: Some(user_agent()),
        extra: vec![("Accept".into(), "application/vnd.github+json".into()), ("X-GitHub-Api-Version".into(), "2022-11-28".into())],
        ..Default::default()
    }
}

/// A release file's headers (the download follows GitHub's redirect to its storage).
pub fn asset_headers() -> HttpHeaders {
    HttpHeaders { user_agent: Some(user_agent()), ..Default::default() }
}

/// The releases, asked conditionally; the answer and its ETag are kept in `dir` (the updates
/// folder) for the next 304.
pub async fn fetch(http: &HttpClient, api: &str, dir: &Path, now: Timestamp, cancel: &CancellationToken) -> Result<Vec<GhRelease>, FetchError> {
    let (answer, etag_file) = (dir.join(ANSWER_FILE), dir.join(ETAG_FILE));
    // an ETag without the answer it stands for is worthless
    let etag = match tokio::fs::try_exists(&answer).await {
        Ok(true) => tokio::fs::read_to_string(&etag_file).await.ok().map(|e| e.trim().to_owned()).filter(|e| !e.is_empty()),
        _ => None,
    };
    let resp = http.get_conditional(api, &api_headers(), etag.as_deref(), MAX_ANSWER, cancel).await.map_err(|e| FetchError::Failed(e.to_string()))?;
    let body = match resp.status {
        200..=299 => {
            // keep it for the next 304 (best effort: without it the next check asks in full)
            if tokio::fs::write(&answer, &resp.body).await.is_ok() {
                match &resp.etag {
                    Some(e) => drop(tokio::fs::write(&etag_file, e).await),
                    None => drop(tokio::fs::remove_file(&etag_file).await),
                }
            }
            resp.body.to_vec()
        }
        304 => match tokio::fs::read(&answer).await {
            Ok(b) => b,
            Err(_) => {
                let _ = tokio::fs::remove_file(&etag_file).await;
                return Err(FetchError::Failed("the saved list of releases is gone; Tuner asks again".into()));
            }
        },
        403 | 429 if resp.status == 429 || resp.rate_remaining == Some(0) || resp.retry_after_s.is_some() => {
            let until = resp
                .retry_after_s
                .map(|s| now + i64::try_from(s).unwrap_or(LIMIT_WAIT_S))
                .or(resp.rate_reset)
                .unwrap_or(now + LIMIT_WAIT_S)
                .clamp(now + 60, now + 24 * 3600);
            return Err(FetchError::Limited { until });
        }
        status => return Err(FetchError::Failed(format!("GitHub answered HTTP {status}"))),
    };
    serde_json::from_slice::<Vec<GhRelease>>(&body).map_err(|e| {
        let _ = std::fs::remove_file(&etag_file);
        FetchError::Failed(format!("GitHub's list of releases didn't read: {e}"))
    })
}

/// The newest stable release (no drafts, no pre-releases, a plain `v<x>.<y>.<z>` tag), with
/// its package for `arch` and that package's signature when both are attached (the newest
/// package version of that release, should it have several).
pub fn pick(releases: &[GhRelease], arch: &str) -> Option<Release> {
    releases
        .iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| stable_version(&r.tag_name).map(|v| (r, v)))
        .max_by(|(_, a), (_, b)| verify::vercmp(a, b))
        .map(|(r, version)| Release { version: version.to_owned(), page: r.html_url.clone(), package: package_of(r, version, arch) })
}

/// The release's package for `arch` with its `.minisig`. Where they are downloaded from
/// matters little: what installs is what verifies against the release key.
fn package_of(r: &GhRelease, version: &str, arch: &str) -> Option<Package> {
    let asset = |a: &GhAsset| Asset { name: a.name.clone(), url: a.browser_download_url.clone(), size: a.size };
    r.assets
        .iter()
        .filter_map(|a| verify::package_file(&a.name).filter(|f| f.pkgver == version && f.arch == arch).map(|f| (a, f.version())))
        .filter_map(|(a, v)| {
            let sig_name = format!("{}{SIGNATURE_SUFFIX}", a.name);
            let sig = r.assets.iter().find(|s| s.name == sig_name)?;
            Some(Package { version: v, file: asset(a), signature: asset(sig) })
        })
        .max_by(|a, b| verify::vercmp(&a.version, &b.version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str) -> GhAsset {
        GhAsset { name: name.into(), browser_download_url: format!("https://github.com/rthoms334/omatuner/releases/download/x/{name}"), size: 10 }
    }

    /// A release with the files release.yml uploads, for both architectures.
    fn release(tag: &str, prerelease: bool, signed: bool) -> GhRelease {
        let v = tag.trim_start_matches('v');
        let mut assets = Vec::new();
        for arch in ["x86_64", "aarch64"] {
            let pkg = format!("omatuner-{v}-1-{arch}.pkg.tar.zst");
            assets.push(asset(&pkg));
            assets.push(asset(&format!("omatuner-{v}-{arch}.tar.gz")));
            if signed {
                assets.push(asset(&format!("{pkg}.minisig")));
            }
        }
        assets.push(asset("SHA256SUMS"));
        GhRelease {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            html_url: format!("https://github.com/rthoms334/omatuner/releases/tag/{tag}"),
            assets,
        }
    }

    #[test]
    fn picks_the_newest_stable_release_and_this_architectures_signed_package() {
        let list = vec![
            release("v0.3.0-rc1", true, true),
            release("v0.2.10", false, true),
            release("v0.2.9", false, true),
            GhRelease { draft: true, ..release("v0.4.0", false, true) },
            // a suffix GitHub wasn't told was a pre-release
            release("v0.3.0rc2", false, true),
        ];
        let r = pick(&list, "aarch64").unwrap();
        assert_eq!(r.version, "0.2.10", "numbers by value, no drafts or pre-releases");
        assert_eq!(r.page, "https://github.com/rthoms334/omatuner/releases/tag/v0.2.10");
        let p = r.package.unwrap();
        assert_eq!((p.version.as_str(), p.file.name.as_str()), ("0.2.10-1", "omatuner-0.2.10-1-aarch64.pkg.tar.zst"));
        assert_eq!(p.signature.name, "omatuner-0.2.10-1-aarch64.pkg.tar.zst.minisig");
        assert!(pick(&list, "riscv64").unwrap().package.is_none(), "told, not installable");
        // an unsigned package is no package
        assert!(pick(&[release("v0.2.11", false, false)], "x86_64").unwrap().package.is_none());
        assert_eq!(pick(&[release("v1.0.0-rc1", true, true)], "x86_64"), None);
        assert_eq!(pick(&[], "x86_64"), None);
    }

    /// The API's real shape (fields Tuner doesn't read are ignored).
    #[test]
    fn reads_the_apis_json() {
        let json = r#"[{"url":"https://api.github.com/repos/rthoms334/omatuner/releases/1","tag_name":"v0.2.6","name":"Tuner v0.2.6",
            "draft":false,"prerelease":false,"html_url":"https://github.com/rthoms334/omatuner/releases/tag/v0.2.6","body":"notes",
            "assets":[{"name":"omatuner-0.2.6-1-x86_64.pkg.tar.zst","size":9876543,"content_type":"application/zstd",
            "browser_download_url":"https://github.com/rthoms334/omatuner/releases/download/v0.2.6/omatuner-0.2.6-1-x86_64.pkg.tar.zst"},
            {"name":"omatuner-0.2.6-1-x86_64.pkg.tar.zst.minisig","size":329,
            "browser_download_url":"https://github.com/rthoms334/omatuner/releases/download/v0.2.6/omatuner-0.2.6-1-x86_64.pkg.tar.zst.minisig"}]}]"#;
        let list: Vec<GhRelease> = serde_json::from_str(json).unwrap();
        let p = pick(&list, "x86_64").unwrap().package.unwrap();
        assert_eq!(p.file.size, 9876543);
        assert!(p.file.url.ends_with("/v0.2.6/omatuner-0.2.6-1-x86_64.pkg.tar.zst"));
    }
}
