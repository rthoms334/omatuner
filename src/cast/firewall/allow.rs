//! The Allow / Remove runner (DESIGN §40 §9.4 "Remedy"). OWNER: CAST-7.
//!
//! Spawns `pkexec --disable-internal-agent …` (`super::argv`) with no shell, stdin null and
//! stderr captured, on the runtime, capped at `super::PKEXEC_TIMEOUT`; Cancel, the timeout and
//! quitting kill it (closing polkit's dialog). Maps the exit status (`man pkexec`: 126 dismissed,
//! 127 not authorised / no agent; the helper's 2 bad arguments, 3 ufw failed) and then a re-read
//! of `user.rules` to an `AllowOutcome`. Tests use a fake `pkexec` script, never the real one.

use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::{AllowOutcome, FirewallEnv, Rule, Verb, argv, ufw};
use crate::redact::redact;

/// The most of ufw's message that is kept (its tail; DESIGN §9.4 "Failed").
const MESSAGE_CAP: usize = 2048;

/// [`super::run`] with the cap as a parameter, so tests wait milliseconds rather than 120 s.
pub async fn run_with(env: &FirewallEnv, verb: Verb, rule: &Rule, cancel: CancellationToken, timeout: Duration) -> AllowOutcome {
    let argv = argv(env, verb, rule);
    let mut cmd = Command::new(&argv[0]);
    // English messages: "no authentication agent" is matched below. pkexec cleans the
    // environment of what it starts; this is only for pkexec's own text.
    cmd.args(&argv[1..]).env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return AllowOutcome::Failed { message: format!("could not start pkexec: {e}") },
    };
    let mut stderr = child.stderr.take().expect("stderr is piped");
    // Read concurrently so a chatty child can't fill the pipe; keeps the last MESSAGE_CAP bytes.
    let reader = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = stderr.read(&mut buf).await {
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 2 * MESSAGE_CAP {
                tail.drain(..tail.len() - MESSAGE_CAP);
            }
        }
        tail
    });
    let status = tokio::select! {
        st = child.wait() => st,
        () = cancel.cancelled() => return stop(&mut child, reader, AllowOutcome::Cancelled).await,
        () = tokio::time::sleep(timeout) => return stop(&mut child, reader, AllowOutcome::TimedOut).await,
    };
    // The pipe closes with the process; a grandchild holding it open must not hold us.
    let tail = tokio::time::timeout(Duration::from_secs(1), reader).await.ok().and_then(Result::ok).unwrap_or_default();
    match status {
        Ok(status) => outcome(env, verb, rule, status, &tail).await,
        Err(e) => AllowOutcome::Failed { message: format!("could not wait for pkexec: {e}") },
    }
}

/// Kill pkexec (closing polkit's dialog), reap it, and report `outcome`.
async fn stop(child: &mut tokio::process::Child, reader: tokio::task::JoinHandle<Vec<u8>>, outcome: AllowOutcome) -> AllowOutcome {
    let _ = child.kill().await;
    reader.abort();
    outcome
}

/// The exit status of pkexec (`man pkexec`) and, for 0, the re-read of `user.rules`.
async fn outcome(env: &FirewallEnv, verb: Verb, rule: &Rule, status: ExitStatus, stderr: &[u8]) -> AllowOutcome {
    match status.code() {
        Some(0) => {
            let present = match tokio::fs::read_to_string(env.etc_ufw.join("user.rules")).await {
                Ok(text) => Some(ufw::tagged(&ufw::parse(&text)).contains(rule)),
                Err(_) => None,
            };
            match (verb, present) {
                (Verb::Allow { .. }, Some(true)) | (Verb::Remove, Some(false)) => AllowOutcome::Added,
                _ => AllowOutcome::AddedUnverified,
            }
        }
        // The user dismissed polkit's dialog.
        Some(126) => AllowOutcome::Cancelled,
        // "Not authorized", or no agent to ask ("No authentication agent found").
        Some(127) if String::from_utf8_lossy(stderr).to_ascii_lowercase().contains("authentication agent") => AllowOutcome::NoAgent,
        Some(127) => AllowOutcome::NotAuthorized,
        Some(_) => AllowOutcome::Failed { message: message(stderr) },
        None => AllowOutcome::Failed { message: "pkexec was terminated by a signal".to_owned() },
    }
}

/// ufw's message: redacted, at most [`MESSAGE_CAP`] bytes (the end of it), trimmed.
fn message(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = redact(text.trim());
    let mut start = text.len().saturating_sub(MESSAGE_CAP);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].trim().to_owned()
}
