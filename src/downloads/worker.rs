//! A worker owns its provider connection until its transport has acknowledged closure
//! (DESIGN §43 §3, §5). HLS bytes use the existing live recorder, including its AES handling.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::downloads::{Job, Kind, State};
use crate::error::{Error, Result};
use crate::model::Playable;
use crate::player::hls::{Buffer, RecordingLimits, finish};
use crate::sources::http::{DownloadOutcome, DownloadRequest, HttpClient, MediaOptions, ResumeValidators};

#[derive(Debug, Clone, Default)]
pub struct Progress {
    pub bytes: u64,
    pub total: Option<u64>,
    pub validators: Option<ResumeValidators>,
    pub finishing: Option<f64>,
    pub store_name: Option<String>,
    pub partial: bool,
    pub capture_failed: bool,
    pub busy: bool,
}

pub struct Work {
    pub job: Job,
    pub playable: Option<Playable>,
    pub root: PathBuf,
    pub cache: PathBuf,
    pub budget: crate::player::budget::SharedBudget,
    pub adopted: Option<Arc<Buffer>>,
    pub http: HttpClient,
    pub max_mbps: f64,
    pub limits: super::limits::Limits,
    pub reserve: u64,
    pub available: u64,
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: CancellationToken,
    pub stop_recording: CancellationToken,
}

fn part(path: &Path) -> PathBuf {
    path.with_file_name(format!("{}.part", path.file_name().unwrap_or_default().to_string_lossy()))
}

impl Work {
    fn at_limit(&self) -> bool {
        self.job
            .source_id
            .is_some_and(|source| self.budget.usage().iter().any(|u| u.source == source && u.limit.is_some_and(|limit| u.in_use >= limit)))
    }

    pub async fn run(mut self) -> (Job, Result<()>) {
        let _inhibit = crate::cast::inhibit::SleepInhibitor::for_downloads().await;
        let result = self.run_inner().await;
        (self.job, result)
    }

    async fn finish(&mut self, dir: PathBuf, partial: bool) -> Result<()> {
        let capture_failed = self.job.reason.as_deref() == Some("connection failed")
            || self.job.fp.get("download_capture_failed").and_then(|v| v.as_bool()) == Some(true);
        self.job.state = State::Finishing;
        self.job.fp["download_partial"] = partial.into();
        self.job.fp["download_capture_failed"] = capture_failed.into();
        {
            let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
            progress.partial = partial;
            progress.capture_failed = capture_failed;
            progress.busy = false;
            progress.finishing = Some(0.0);
        }
        let output = self.root.join(format!("{}.mkv", self.job.id));
        let state = self.progress.clone();
        finish::finish(&dir, &output, &self.cancel, move |n| {
            state.lock().unwrap_or_else(|e| e.into_inner()).finishing = Some(n);
        })
        .await
        .map_err(|e| Error::other(e.to_string()))?;
        self.job.store_name = Some(format!("{}.mkv", self.job.id));
        self.job.bytes_done = std::fs::metadata(&output).map_err(|e| Error::io(&output, e))?.len();
        self.job.bytes_total = Some(self.job.bytes_done);
        self.job.state = if capture_failed {
            State::Failed
        } else if partial {
            State::Partial
        } else {
            State::Done
        };
        self.job.done_at = Some(crate::model::now());
        Ok(())
    }

    async fn record_hls(&mut self, p: Playable) -> Result<()> {
        let dir = self.root.join(self.job.id.to_string());
        self.job.store_name = Some(format!("{}/", self.job.id));
        self.job.fp["download_hls"] = true.into();
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).store_name = self.job.store_name.clone();
        if !self.job.kind.recording() && dir.exists() {
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        }
        let p = p.clone();
        let ua = self.http.default_user_agent();
        let limits = RecordingLimits { reserve_bytes: self.reserve, max_bytes: self.available, limits: Some(self.limits.clone()) };
        let root = dir.clone();
        let archive = self.job.kind != Kind::Live;
        let buffer = if let Some(buffer) = self.adopted.take() {
            buffer
        } else {
            self.limits.reset(0);
            let buffer = Arc::new(
                tokio::task::spawn_blocking(move || Buffer::start_keep(&p, &root, &ua, limits, archive))
                    .await
                    .map_err(Error::other)?
                    .map_err(Error::other)?,
            );
            if !archive
                && let Some(p) = &self.playable
                && let Some(source) = p.source_id
            {
                self.budget.register_live(p.media.clone(), self.job.id, source, &buffer);
            }
            buffer
        };
        self.job.store_name = Some(format!("{}/", self.job.id));
        let mut partial = false;
        let start = crate::model::now();
        let stop = if self.job.kind == Kind::Live {
            self.job.stop()
        } else {
            self.job.stop().zip(self.job.start()).map(|(stop, begin)| start + stop - begin)
        };
        let mut retry_at = None;
        let mut retries = 0;
        loop {
            let s = buffer.snapshot();
            {
                let mut progress = self.progress.lock().unwrap_or_else(|e| e.into_inner());
                progress.bytes = s.bytes;
            }
            if s.partial {
                partial = true;
                self.job.reason = Some("space".into());
                break;
            }
            if self.cancel.is_cancelled() || self.stop_recording.is_cancelled() || stop.is_some_and(|stop| crate::model::now() >= stop) {
                break;
            }
            let busy = s.failed && s.failure == Some(crate::player::PlayerErrorKind::HttpForbidden) && self.at_limit();
            self.progress.lock().unwrap_or_else(|e| e.into_inner()).busy = busy;
            if busy {
                let at = retry_at.get_or_insert_with(|| std::time::Instant::now() + Duration::from_secs(30));
                if std::time::Instant::now() >= *at {
                    buffer.retry();
                    retry_at = None;
                }
            } else if s.failed {
                if retries >= 3 {
                    partial = true;
                    self.job.reason = Some("connection failed".into());
                    break;
                }
                let at = retry_at.get_or_insert_with(|| std::time::Instant::now() + Duration::from_secs([30, 120, 600][retries]));
                if std::time::Instant::now() >= *at {
                    buffer.retry();
                    retries += 1;
                    retry_at = None;
                }
            } else {
                retry_at = None;
            }
            if archive && s.ended {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let tail = buffer.snapshot();
        let duration = tail.duration;
        partial |= tail.failed;
        let empty_busy = duration <= 0.0 && buffer.snapshot().failure == Some(crate::player::PlayerErrorKind::HttpForbidden) && self.at_limit();
        let cache = self.cache.clone();
        tokio::task::spawn_blocking(move || {
            if Arc::strong_count(&buffer) > 1 {
                buffer.finish_recording(&cache).map_err(Error::other)?;
            }
            drop(buffer);
            Ok::<_, Error>(())
        })
        .await
        .map_err(Error::other)??;
        self.job.fp["download_duration_s"] = duration.into();
        self.job.fp["download_partial"] = partial.into();
        if empty_busy {
            let _ = std::fs::remove_dir_all(&dir);
            self.job.store_name = None;
            self.progress.lock().unwrap_or_else(|e| e.into_inner()).store_name = None;
            return Err(Error::Http { status: 403, url: String::new() });
        }
        if self.cancel.is_cancelled() {
            self.job.state = State::Finishing;
            return Err(Error::Cancelled);
        }
        self.finish(dir, partial).await
    }

    async fn run_inner(&mut self) -> Result<()> {
        if self.job.state == State::Finishing {
            let dir = self.job.path(&self.root).ok_or_else(|| Error::other("Recording folder missing"))?;
            return self
                .finish(
                    dir,
                    self.job.fp.get("download_partial").and_then(|v| v.as_bool()) == Some(true) || self.job.reason.as_deref() == Some("space"),
                )
                .await;
        }
        let p = self.playable.as_ref().ok_or_else(|| Error::other("not_found"))?;
        let hls = url::Url::parse(p.url.expose()).is_ok_and(|u| u.path().to_ascii_lowercase().ends_with(".m3u8"));
        if hls {
            return self.record_hls(p.clone()).await;
        }
        let extension = url::Url::parse(p.url.expose())
            .ok()
            .and_then(|u| u.path().rsplit('.').next().map(str::to_owned))
            .filter(|e| {
                matches!(
                    e.as_str(),
                    "mp4" | "mkv" | "ts" | "m4v" | "webm" | "avi" | "mpg" | "mpeg" | "mov" | "m2ts" | "aac" | "mp3" | "ac3" | "ec3" | "flac" | "ogg"
                )
            })
            .unwrap_or_else(|| "mp4".into());
        let mut live = self.job.kind == Kind::Live;
        let name = self.job.store_name.clone().unwrap_or_else(|| format!("{}.{}", self.job.id, if live { "ts" } else { &extension }));
        self.job.store_name = Some(name.clone());
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).store_name = self.job.store_name.clone();
        let dest = self.root.join(name);
        let _reader = if self.job.kind == Kind::Live {
            let reader = super::reader::Reader::start(part(&dest)).await.map_err(Error::other)?;
            if let Some(source) = p.source_id {
                self.budget.register_raw(p.media.clone(), self.job.id, source, &reader);
            }
            Some(super::reader::Recording(reader))
        } else {
            None
        };
        let stream_stop = if self.job.kind == Kind::Live {
            self.job.stop()
        } else {
            self.job.stop().zip(self.job.start()).map(|(end, start)| crate::model::now() + end - start)
        };
        let mut gaps = 0u32;
        let mut gap_seconds = 0i64;
        loop {
            let offset = tokio::fs::metadata(part(&dest)).await.map_or(0, |m| m.len());
            let validators = self.job.fp.get("download_validators").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
            let req = DownloadRequest {
                url: p.url.clone(),
                dest: dest.clone(),
                headers: p.headers.clone(),
                etag: None,
                last_modified: None,
                resume_from: Some((offset, validators)),
            };
            let until = if live { stream_stop } else { None };
            let opts = MediaOptions {
                limits: Some(self.limits.clone()),
                max_mbps: self.max_mbps,
                reserve_bytes: self.reserve,
                max_bytes: Some(self.available),
                until,
                stream_for_s: if self.job.kind == Kind::Catchup {
                    self.job.stop().zip(self.job.start()).map(|(end, start)| (end - start).max(0) as u64)
                } else {
                    None
                },
            };
            let progress = self.progress.clone();
            let headers = self.progress.clone();
            let transfer = self.cancel.child_token();
            let stop = self.stop_recording.clone();
            let control = transfer.clone();
            let stop_task = tokio::spawn(async move {
                stop.cancelled().await;
                control.cancel();
            });
            let result = self
                .http
                .download_media(
                    &req,
                    &opts,
                    &move |bytes, total| {
                        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
                        p.bytes = bytes;
                        p.total = total;
                    },
                    &move |v| {
                        headers.lock().unwrap_or_else(|e| e.into_inner()).validators = Some(v);
                    },
                    &transfer,
                )
                .await;
            stop_task.abort();
            if self.job.kind == Kind::Catchup
                && self.progress.lock().unwrap_or_else(|e| e.into_inner()).validators.as_ref().is_some_and(|v| v.streamed)
            {
                live = true;
            }
            if live && (self.stop_recording.is_cancelled() || self.cancel.is_cancelled()) {
                tokio::fs::rename(part(&dest), &dest).await.map_err(|e| Error::io(&dest, e))?;
                self.job.bytes_done = tokio::fs::metadata(&dest).await.map_err(|e| Error::io(&dest, e))?.len();
                break;
            }
            match result {
                Err(Error::Other(ref reason)) if reason == "hls" => {
                    let p = p.clone();
                    let _ = tokio::fs::remove_file(part(&dest)).await;
                    return self.record_hls(p).await;
                }
                Ok(DownloadOutcome::Downloaded { bytes, .. }) => {
                    self.job.bytes_done = bytes;
                    self.job.bytes_total = Some(bytes);
                    break;
                }
                Ok(_) => return Err(Error::other("The provider sent no file")),
                Err(Error::Other(ref e)) if e == "space" && live => {
                    tokio::fs::rename(part(&dest), &dest).await.map_err(|e| Error::io(&dest, e))?;
                    self.job.bytes_done = tokio::fs::metadata(&dest).await.map_err(|e| Error::io(&dest, e))?.len();
                    self.job.state = State::Partial;
                    self.job.reason = Some("space".into());
                    self.job.done_at = Some(crate::model::now());
                    return Ok(());
                }
                Err(e) if !live || self.cancel.is_cancelled() || (matches!(e, Error::Http { status: 403, .. }) && self.at_limit()) => return Err(e),
                Err(_) => {
                    let start = crate::model::now();
                    gaps += 1;
                    if gaps > 3 {
                        tokio::fs::rename(part(&dest), &dest).await.map_err(|e| Error::io(&dest, e))?;
                        self.job.bytes_done = tokio::fs::metadata(&dest).await.map_err(|e| Error::io(&dest, e))?.len();
                        self.job.state = State::Failed;
                        self.job.reason = Some("connection failed".into());
                        self.job.done_at = Some(crate::model::now());
                        return Ok(());
                    }
                    let delay = [30, 120, 600][gaps as usize - 1];
                    let delay = until.map_or(delay, |end| delay.min((end - start).max(0) as u64));
                    tokio::select! {
                        ()=self.cancel.cancelled()=>{},
                        ()=self.stop_recording.cancelled()=>{},
                        _=tokio::time::sleep(Duration::from_secs(delay))=>{}
                    }
                    gap_seconds += crate::model::now() - start;
                    self.job.reason = Some(format!("gaps:{gaps}:{gap_seconds}"));
                    if self.cancel.is_cancelled() || self.stop_recording.is_cancelled() || until.is_some_and(|stop| crate::model::now() >= stop) {
                        tokio::fs::rename(part(&dest), &dest).await.map_err(|e| Error::io(&dest, e))?;
                        self.job.bytes_done = tokio::fs::metadata(&dest).await.map_err(|e| Error::io(&dest, e))?.len();
                        break;
                    }
                }
            }
        }
        self.job.state = State::Done;
        self.job.done_at = Some(crate::model::now());
        Ok(())
    }
}
