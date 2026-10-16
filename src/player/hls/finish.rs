//! Finish a kept HLS recording into one seekable file (DESIGN §43 §3, §8.4).
//!
//! Each timestamp epoch is remuxed separately. The concat demuxer then offsets the epochs;
//! passing the original discontinuous HLS straight to mpv loses that timeline. All input
//! names come from our persisted index, never a provider playlist. A cancelled or failed run
//! keeps that index and its media; the next invocation safely replaces intermediate files.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::cast::relay::ffmpeg;

/// The durable index contains only local counters and transport metadata. No resource URLs,
/// titles or credentials survive the provider reader (DESIGN §43 §4, §6).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recording {
    pub tracks: Vec<Vec<Segment>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: u64,
    pub extension: String,
    pub start: f64,
    pub end: f64,
    pub discontinuity: u64,
    pub key: Option<(u64, String)>,
    pub map: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum FinishError {
    #[error("Finishing needs ffmpeg and ffprobe")]
    NeedsFfmpeg,
    #[error("The recording could not be read")]
    Input,
    #[error("The recording could not be finished")]
    Remux,
    #[error("The finished recording has the wrong duration")]
    Duration,
    #[error("Finishing was stopped")]
    Cancelled,
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = fs::OpenOptions::new().create(true).truncate(true).write(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

impl Recording {
    pub fn duration(&self) -> f64 {
        self.tracks.first().map_or(0.0, |t| t.iter().map(|s| s.end - s.start).sum())
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        write_private(&dir.join("index.part"), &bytes)?;
        fs::rename(dir.join("index.part"), dir.join("index.json"))
    }

    pub fn load(dir: &Path) -> Result<Self, FinishError> {
        let bytes = fs::read(dir.join("index.json")).map_err(|_| FinishError::Input)?;
        let index: Self = serde_json::from_slice(&bytes).map_err(|_| FinishError::Input)?;
        if index.tracks.is_empty() || index.tracks.len() > 2 || index.tracks.iter().any(Vec::is_empty) {
            return Err(FinishError::Input);
        }
        for track in &index.tracks {
            for s in track {
                if !matches!(s.extension.as_str(), "ts" | "m4s" | "mp4" | "aac" | "mp3" | "ac3" | "ec3")
                    || !s.start.is_finite()
                    || !s.end.is_finite()
                    || s.end <= s.start
                    || s.key
                        .as_ref()
                        .is_some_and(|(_, iv)| iv.strip_prefix("0x").is_none_or(|hex| hex.len() != 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit())))
                {
                    return Err(FinishError::Input);
                }
            }
        }
        Ok(index)
    }
}

fn playlist(segments: &[&Segment]) -> String {
    let target = segments.iter().map(|s| s.end - s.start).fold(1.0, f64::max).ceil();
    let mut out = format!("#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-TARGETDURATION:{target}\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for s in segments {
        match &s.key {
            Some((id, iv)) => out.push_str(&format!("#EXT-X-KEY:METHOD=AES-128,URI=\"{id}.key\",IV={iv}\n")),
            None => out.push_str("#EXT-X-KEY:METHOD=NONE\n"),
        }
        if let Some(id) = s.map {
            out.push_str(&format!("#EXT-X-MAP:URI=\"{id}.mp4\"\n"));
        }
        out.push_str(&format!("#EXTINF:{:.6},\n{}.{}\n", s.end - s.start, s.id, s.extension));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

async fn remux(caps: &ffmpeg::Caps, dir: &Path, args: &[String], cancel: &CancellationToken, progress: &impl Fn(f64)) -> Result<(), FinishError> {
    let mut argv: Vec<String> =
        ["-nostdin", "-hide_banner", "-nostats", "-loglevel", "error", "-y", "-progress", "pipe:1"].into_iter().map(str::to_owned).collect();
    argv.extend_from_slice(args);
    let report = ffmpeg::Progress { base_s: 0.0, watch_speed: false, on_slow: Box::new(|| {}) };
    let mut child = ffmpeg::spawn_in(&caps.ffmpeg, &argv, "download finish".into(), report, Some(dir)).map_err(|_| FinishError::Remux)?;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => {
                child.kill().await;
                return Err(FinishError::Cancelled);
            }
            exit = child.wait() => return if exit == ffmpeg::Exit::Done { Ok(()) } else { Err(FinishError::Remux) },
            _ = tick.tick() => {
                if let Some(s) = child.failure().media_s { progress(s.max(0.0)); }
            }
        }
    }
}

async fn duration(caps: &ffmpeg::Caps, path: &Path, cancel: &CancellationToken) -> Result<f64, FinishError> {
    let mut cmd = ffmpeg::command(&caps.ffprobe);
    cmd.args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
        .arg(path)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = tokio::select! {
        () = cancel.cancelled() => return Err(FinishError::Cancelled),
        out = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output()) =>
            out.map_err(|_| FinishError::Duration)?.map_err(|_| FinishError::Duration)?,
    };
    if !output.status.success() {
        return Err(FinishError::Duration);
    }
    String::from_utf8_lossy(&output.stdout).trim().parse::<f64>().ok().filter(|n| n.is_finite() && *n > 0.0).ok_or(FinishError::Duration)
}

/// The caller commits `done` only after this returns. Output is atomically replaced and the
/// source folder is removed only after verification. `progress` is a fraction in [0, 1].
pub async fn finish(dir: &Path, output: &Path, cancel: &CancellationToken, progress: impl Fn(f64)) -> Result<(), FinishError> {
    let index = Recording::load(dir)?;
    let caps = ffmpeg::caps().await.map_err(|_| FinishError::NeedsFfmpeg)?;
    let total = index.duration();
    let video = &index.tracks[0];
    let mut runs: Vec<Vec<&Segment>> = Vec::new();
    for seg in video {
        if runs.last().is_none_or(|run| run.last().is_none_or(|last| last.discontinuity != seg.discontinuity)) {
            runs.push(Vec::new());
        }
        runs.last_mut().expect("run was inserted").push(seg);
    }
    let dir = fs::canonicalize(dir).map_err(|_| FinishError::Input)?;
    let mut concat = String::from("ffconcat version 1.0\n");
    let mut completed = 0.0;
    for (n, run) in runs.iter().enumerate() {
        if cancel.is_cancelled() {
            return Err(FinishError::Cancelled);
        }
        let run_duration: f64 = run.iter().map(|s| s.end - s.start).sum();
        let input = format!("{n}.m3u8");
        write_private(&dir.join(&input), playlist(run).as_bytes()).map_err(|_| FinishError::Input)?;
        let mut args: Vec<String> =
            ["-protocol_whitelist", "file,crypto,data", "-allowed_extensions", "ALL", "-i", &input].into_iter().map(str::to_owned).collect();
        if let Some(audio) = index.tracks.get(1) {
            let start = run[0].start;
            let end = run.last().expect("nonempty").end;
            let audio: Vec<_> = audio.iter().filter(|s| s.end > start && s.start < end).collect();
            if audio.is_empty() {
                return Err(FinishError::Input);
            }
            let input = format!("{n}.a.m3u8");
            write_private(&dir.join(&input), playlist(&audio).as_bytes()).map_err(|_| FinishError::Input)?;
            args.extend(
                ["-protocol_whitelist", "file,crypto,data", "-allowed_extensions", "ALL", "-i", &input, "-map", "0:v:0", "-map", "1:a:0"]
                    .into_iter()
                    .map(str::to_owned),
            );
        } else {
            args.extend(["-map", "0:v?", "-map", "0:a?", "-map", "0:s?"].into_iter().map(str::to_owned));
        }
        let out = format!("{n}.mkv");
        write_private(&dir.join(&out), &[]).map_err(|_| FinishError::Input)?;
        args.extend(["-c", "copy", "-avoid_negative_ts", "make_zero", &out].into_iter().map(str::to_owned));
        remux(&caps, &dir, &args, cancel, &|s| progress(((completed + s.min(run_duration)) / (total * 2.0)).clamp(0.0, 0.5))).await?;
        concat.push_str(&format!("file '{out}'\nduration {run_duration:.6}\n"));
        completed += run_duration;
        progress((completed / (total * 2.0)).clamp(0.0, 0.5));
    }
    write_private(&dir.join("concat.txt"), concat.as_bytes()).map_err(|_| FinishError::Input)?;
    let part: PathBuf = output.with_extension("mkv.part");
    write_private(&part, &[]).map_err(|_| FinishError::Input)?;
    let part = fs::canonicalize(&part).map_err(|_| FinishError::Input)?;
    let mut args: Vec<String> =
        ["-f", "concat", "-safe", "1", "-i", "concat.txt", "-map", "0", "-c", "copy", "-f", "matroska"].into_iter().map(str::to_owned).collect();
    args.push(part.to_string_lossy().into_owned());
    remux(&caps, &dir, &args, cancel, &|s| progress((0.5 + s / (total * 2.0)).clamp(0.5, 0.99))).await?;
    if (duration(&caps, &part, cancel).await? - total).abs() > 2.0 {
        return Err(FinishError::Duration);
    }
    fs::File::open(&part).and_then(|f| f.sync_all()).map_err(|_| FinishError::Input)?;
    fs::rename(&part, output).map_err(|_| FinishError::Input)?;
    // A durable receipt closes the crash window between removing the kept folder and the
    // actor committing Done. Reconcile consumes it only beside a verified final file.
    write_private(&output.with_extension("mkv.ok"), total.to_string().as_bytes()).map_err(|_| FinishError::Input)?;
    if let Some(parent) = output.parent() {
        fs::File::open(parent).and_then(|f| f.sync_all()).map_err(|_| FinishError::Input)?;
    }
    fs::remove_dir_all(dir).map_err(|_| FinishError::Input)?;
    progress(1.0);
    Ok(())
}
