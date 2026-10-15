//! Local audio output identity for audio delay (DESIGN §13.8.1). A helper thread runs pactl;
//! its two queries share a short timeout, including draining their output pipes.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde_json::Value;
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOutput {
    pub name: String,
    pub label: String,
}

impl Default for AudioOutput {
    fn default() -> Self {
        Self { name: "this-computer".into(), label: "This computer".into() }
    }
}

/// The device is read by the controller; only the worker calls pactl. A failed description
/// lookup stays a failure so the controller can keep the explicit device's last good label.
fn current(device: &str) -> Option<AudioOutput> {
    if device != "auto" && !device.is_empty() {
        let sink = device.strip_prefix("pulse/").or_else(|| device.strip_prefix("pipewire/"));
        let label = match sink {
            Some(sink) => query_sink(Some(sink))?.label,
            None => device.to_owned(),
        };
        return Some(AudioOutput { name: device.to_owned(), label });
    }
    query_sink(None)
}

pub(super) type Query = Arc<dyn Fn(&str) -> Option<AudioOutput> + Send + Sync>;

pub(super) struct LookupResult {
    pub device: String,
    pub output: Option<AudioOutput>,
}

/// One bounded lookup at a time; reconfig bursts during a zap share the same check.
pub(super) struct Lookup {
    query: Query,
    tx: Sender<LookupResult>,
    pub results: Receiver<LookupResult>,
    pending: bool,
    last_check: Option<Instant>,
}

impl Default for Lookup {
    fn default() -> Self {
        Self::new(Arc::new(current))
    }
}

impl Lookup {
    pub fn new(query: Query) -> Self {
        let (tx, results) = crossbeam_channel::unbounded();
        Self { query, tx, results, pending: false, last_check: None }
    }

    pub fn due(&self, now: Instant, interval: Duration) -> bool {
        !self.pending && self.last_check.is_none_or(|at| now.saturating_duration_since(at) >= interval)
    }

    pub fn request(&mut self, device: String, now: Instant) {
        if !self.due(now, Duration::from_millis(1500)) {
            return;
        }
        self.last_check = Some(now);
        let query = self.query.clone();
        let tx = self.tx.clone();
        match std::thread::Builder::new().name("audio-output".into()).spawn(move || {
            let output = query(&device);
            let _ = tx.send(LookupResult { device, output });
        }) {
            Ok(_) => self.pending = true,
            Err(e) => tracing::warn!("couldn't start audio output lookup: {e}"),
        }
    }

    pub fn complete(&mut self) {
        self.pending = false;
    }
}

fn query_sink(explicit: Option<&str>) -> Option<AudioOutput> {
    // This thread has no Tokio runtime. A current-thread runtime lets the timeout also cover
    // reading large sink lists (waiting for exit before reading can fill the child's pipe).
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
        let name = match explicit {
            Some(name) => serde_json::to_string(name).ok()?,
            None => pactl(&["get-default-sink"], deadline).await?,
        };
        let sinks = pactl(&["list", "sinks"], deadline).await?;
        parse_pactl(&name, &sinks)
    })
}

async fn pactl(args: &[&str], deadline: tokio::time::Instant) -> Option<String> {
    let mut child = tokio::process::Command::new("pactl")
        .arg("--format=json")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let mut bytes = Vec::new();
    let result = tokio::time::timeout_at(deadline, async { tokio::try_join!(child.wait(), stdout.read_to_end(&mut bytes)) }).await;
    match result {
        Ok(Ok((status, _))) if status.success() => String::from_utf8(bytes).ok(),
        _ => {
            // Kill and reap before dropping this short-lived runtime, including on timeout.
            let _ = child.kill().await;
            None
        }
    }
}

/// `get-default-sink` can return a bare name even with `--format=json`; `list sinks` is JSON.
/// Missing descriptions keep the stable sink name. A missing sink or malformed list
/// returns no output, never a different sink from the list.
fn parse_pactl(default: &str, sinks: &str) -> Option<AudioOutput> {
    let default = default.trim();
    let name = if default.starts_with('"') { serde_json::from_str::<String>(default).ok()? } else { default.to_owned() };
    if name.is_empty() || name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let list: Value = serde_json::from_str(sinks).ok()?;
    let sink = list.as_array()?.iter().find(|s| s.get("name").and_then(Value::as_str) == Some(&name))?;
    let label = sink.get("description").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(&name).to_owned();
    Some(AudioOutput { name, label })
}

pub fn offset_text(offset_ms: i32) -> String {
    match offset_ms.cmp(&0) {
        std::cmp::Ordering::Less => format!("{} ms earlier", offset_ms.unsigned_abs()),
        std::cmp::Ordering::Greater => format!("{offset_ms} ms later"),
        std::cmp::Ordering::Equal => "in step".into(),
    }
}

pub fn adjustment_text(output: &AudioOutput, offset_ms: i32) -> String {
    if offset_ms == 0 { "Delay: in step".into() } else { format!("Delay: sound {} · {}", offset_text(offset_ms), output.label) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lip_sync_lookup_runs_off_thread_with_one_in_flight_and_debounces_triggers() {
        let (started_tx, started_rx) = crossbeam_channel::unbounded();
        let (release_tx, release_rx) = crossbeam_channel::unbounded();
        let caller = std::thread::current().id();
        let mut lookup = Lookup::new(Arc::new(move |_| {
            started_tx.send(std::thread::current().id()).unwrap();
            release_rx.recv().unwrap();
            None
        }));
        let now = Instant::now();
        lookup.request("auto".into(), now);
        assert_ne!(started_rx.recv_timeout(Duration::from_secs(5)).unwrap(), caller);
        lookup.request("auto".into(), now + Duration::from_secs(2));
        assert!(!lookup.due(now + Duration::from_secs(20), Duration::from_secs(15)), "still in flight");
        release_tx.send(()).unwrap();
        assert!(lookup.results.recv_timeout(Duration::from_secs(5)).unwrap().output.is_none());
        lookup.complete();
        lookup.request("auto".into(), now + Duration::from_millis(1499));
        assert!(started_rx.is_empty(), "the in-flight and debounced triggers started no workers");
        assert!(!lookup.due(now + Duration::from_secs(14), Duration::from_secs(15)));
        assert!(lookup.due(now + Duration::from_secs(15), Duration::from_secs(15)));
        lookup.request("auto".into(), now + Duration::from_millis(1500));
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        release_tx.send(()).unwrap();
        lookup.results.recv_timeout(Duration::from_secs(5)).unwrap();
        lookup.complete();
    }

    #[test]
    fn pactl_selects_the_default_by_name_and_keeps_its_description() {
        let sinks = r#"[null, {"name":"hdmi","description":"HDMI"},
            {"name":"audio_effect.j413-convolver","description":"MacBook Air Speakers"}]"#;
        let want = AudioOutput { name: "audio_effect.j413-convolver".into(), label: "MacBook Air Speakers".into() };
        assert_eq!(parse_pactl("audio_effect.j413-convolver\n", sinks), Some(want.clone()));
        assert_eq!(parse_pactl("\"audio_effect.j413-convolver\"\n", sinks), Some(want));
        assert_eq!(
            parse_pactl("42\n", r#"[{"name":"42","description":"USB speakers"}]"#),
            Some(AudioOutput { name: "42".into(), label: "USB speakers".into() })
        );
    }

    #[test]
    fn pactl_missing_or_odd_output() {
        for default in ["", " ", "null", "42", "{}", "[]", "\"\"", "two\nlines", "Connection failure"] {
            assert_eq!(parse_pactl(default, r#"[{"name":"speakers"}]"#), None, "{default:?}");
        }
        for sinks in ["", "not json", "null", "{}", "[]", "[null,3,{}]", r#"[{"name":"headphones"}]"#] {
            assert_eq!(parse_pactl("speakers", sinks), None, "{sinks:?}");
        }
        for detail in ["", r#", "description":null"#, r#", "description":123"#, r#", "description":"  ""#] {
            assert_eq!(
                parse_pactl("speakers", &format!(r#"[{{"name":"speakers"{detail}}}]"#)),
                Some(AudioOutput { name: "speakers".into(), label: "speakers".into() })
            );
        }
    }
}
