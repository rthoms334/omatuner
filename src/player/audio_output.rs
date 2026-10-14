//! Local audio output identity for lip sync (DESIGN §13.8.1). Called on the controller thread;
//! pactl's two queries share a short timeout, including draining their output pipes.

use std::process::Stdio;
use std::time::Duration;

use libmpv2::Mpv;
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

/// mpv's explicit device wins, even if pactl is unavailable or has a different default.
pub(super) fn current(mpv: &Mpv) -> AudioOutput {
    if let Ok(name) = mpv.get_property::<String>("audio-device")
        && !name.is_empty()
        && name != "auto"
    {
        // mpv's device list can probe every backend, even when one was explicitly selected.
        // For PipeWire/Pulse, the bounded sink query supplies the human-readable description.
        let sink = name.strip_prefix("pulse/").or_else(|| name.strip_prefix("pipewire/"));
        let label = sink.and_then(|sink| query_sink(Some(sink))).map_or_else(|| name.clone(), |s| s.label);
        return AudioOutput { name, label };
    }
    query_sink(None).unwrap_or_default()
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
/// Missing descriptions keep the stable sink name. A missing sink or malformed list falls
/// back to the computer entry, never to a different sink from the list.
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
    if offset_ms == 0 { "Lip sync: in step".into() } else { format!("Sound {} · {}", offset_text(offset_ms), output.label) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
