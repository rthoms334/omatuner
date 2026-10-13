//! The update helper through pkexec (DESIGN §41.5): `pkexec --disable-internal-agent
//! /usr/lib/tuner/tuner-update install <path>` or `… passwordless on|off`, no shell, stdin
//! closed, the output captured (capped), on a blocking thread with a time limit. pkexec asks
//! polkit's agent (the desktop's password dialog) unless the user's rule allows the action;
//! Tuner never sees a password.
//!
//! [`outcome`] reads the exit status: pkexec's 126 (the dialog was dismissed) and 127 (not
//! allowed, or no agent to ask), then the helper's own codes (see `src/bin/tuner-update.rs`).
//! A stop (quitting) only stops waiting: once pkexec has started the helper it runs as root and
//! can't be killed from here — an install already going finishes, which is what pacman needs.

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

/// The longest an install may take: the password dialog, then pacman (which may fetch a new
/// dependency).
pub const INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// The longest the passwordless switch may wait for the password.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(3 * 60);
/// The most of a message kept.
const MESSAGE_CAP: usize = 2048;

/// How a helper run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Exit 0 with what it did ("installed omatuner 0.2.6-1 (was 0.2.5-1)").
    Done(String),
    /// pkexec 126: the password dialog was dismissed.
    Dismissed,
    /// pkexec 127 and no authentication agent answered.
    NoAgent,
    /// pkexec 127 otherwise: polkit said no.
    NotAuthorized,
    /// Helper 3: a check failed; nothing was installed.
    Refused(String),
    /// Helper 2 or 4, or pkexec failing to start: why.
    Failed(String),
    /// Helper 5: pacman is busy.
    Busy,
    /// Helper 6: this build has no release key.
    NoKey,
    TimedOut,
    /// Quitting: no longer waited for.
    Stopped,
}

impl Outcome {
    /// One line for Settings and toasts ("" for the quiet ones).
    pub fn text(&self) -> String {
        match self {
            Outcome::Done(s) => s.clone(),
            Outcome::Dismissed | Outcome::Stopped => String::new(),
            Outcome::NoAgent => "no polkit agent answered to ask for your password (is one running in your session?)".into(),
            Outcome::NotAuthorized => "polkit didn't allow it (it needs an administrator's password)".into(),
            Outcome::Refused(m) | Outcome::Failed(m) => m.clone(),
            Outcome::Busy => "pacman was busy with something else".into(),
            Outcome::NoKey => "this build of Tuner has no release key".into(),
            Outcome::TimedOut => "nobody answered the password dialog".into(),
        }
    }
}

/// `pkexec … <helper> <args…>` (element 0 is the program).
pub fn argv(pkexec: &[OsString], helper: &std::path::Path, args: &[&OsStr]) -> Vec<OsString> {
    let mut a = pkexec.to_vec();
    a.push(helper.as_os_str().to_owned());
    a.extend(args.iter().map(|s| s.to_os_string()));
    a
}

/// Run `argv` and wait for it, at most `timeout`, until `stop`. Blocking: call it from the
/// blocking pool.
pub fn run(argv: &[OsString], timeout: Duration, stop: &CancellationToken) -> Outcome {
    let Some((program, args)) = argv.split_first() else { return Outcome::Failed("nothing to run".into()) };
    let mut cmd = Command::new(program);
    // English messages: "authentication agent" is matched below; pkexec cleans the environment
    // of the program it starts, so this is only for pkexec's own text
    cmd.args(args).env("LC_ALL", "C").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("could not start pkexec: {e}")),
    };
    // drained on their own threads (a chatty pacman can't fill a pipe), the end kept
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::Builder::new().name("update-helper-out".into()).spawn(move || {
            let mut kept: Vec<u8> = Vec::new();
            if let Some(mut p) = pipe {
                let mut buf = [0u8; 4096];
                while let Ok(n @ 1..) = p.read(&mut buf) {
                    kept.extend_from_slice(&buf[..n]);
                    if kept.len() > 2 * MESSAGE_CAP {
                        kept.drain(..kept.len() - MESSAGE_CAP);
                    }
                }
            }
            kept
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let started = Instant::now();
    // closes the dialog while pkexec is still ours; a helper already running is root's (EPERM)
    // and is left to finish
    fn give_up(child: &mut std::process::Child, why: Outcome) -> Outcome {
        if child.kill().is_ok() {
            let _ = child.wait();
        }
        why
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if stop.is_cancelled() => return give_up(&mut child, Outcome::Stopped),
            Ok(None) if started.elapsed() >= timeout => return give_up(&mut child, Outcome::TimedOut),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Outcome::Failed(format!("could not wait for pkexec: {e}")),
        }
    };
    let joined = |h: std::io::Result<std::thread::JoinHandle<Vec<u8>>>| h.ok().and_then(|h| h.join().ok()).unwrap_or_default();
    outcome(status.code(), &joined(out), &joined(err))
}

/// What pkexec's (and so the helper's) exit status means.
pub fn outcome(code: Option<i32>, stdout: &[u8], stderr: &[u8]) -> Outcome {
    let said = message(stderr);
    match code {
        Some(0) => Outcome::Done(message(stdout)),
        Some(126) => Outcome::Dismissed,
        Some(127) if said.to_ascii_lowercase().contains("authentication agent") => Outcome::NoAgent,
        Some(127) => Outcome::NotAuthorized,
        Some(3) => Outcome::Refused(said),
        Some(5) => Outcome::Busy,
        Some(6) => Outcome::NoKey,
        Some(2) => Outcome::Failed(format!("the update helper refused its arguments: {said}")),
        Some(4) if said.is_empty() => Outcome::Failed("pacman failed".into()),
        Some(4) => Outcome::Failed(said),
        Some(n) => Outcome::Failed(format!("pkexec exited with {n}{}", if said.is_empty() { String::new() } else { format!(": {said}") })),
        None => Outcome::Failed("pkexec was stopped by a signal".into()),
    }
}

/// The helper's message: its `tuner-update: ` prefix gone, redacted, the last line that says
/// something, at most [`MESSAGE_CAP`] bytes.
fn message(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let line = text.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
    let line = line.strip_prefix("tuner-update: ").unwrap_or(line);
    let line = crate::redact::redact(line);
    let mut end = line.len().min(MESSAGE_CAP);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_mean_what_pkexec_and_the_helper_say() {
        let o = |code, err: &str| outcome(Some(code), b"installed omatuner 0.2.6-1 (was 0.2.5-1)\n", err.as_bytes());
        assert_eq!(o(0, ""), Outcome::Done("installed omatuner 0.2.6-1 (was 0.2.5-1)".into()));
        assert_eq!(o(126, "Error executing command as another user: Request dismissed"), Outcome::Dismissed);
        assert_eq!(o(127, "Error executing command as another user: No authentication agent found."), Outcome::NoAgent);
        assert_eq!(o(127, "Error executing command as another user: Not authorized"), Outcome::NotAuthorized);
        assert_eq!(
            o(3, "tuner-update: omatuner-0.2.6-1-x86_64.pkg.tar.zst: the signature doesn't match the file\n"),
            Outcome::Refused("omatuner-0.2.6-1-x86_64.pkg.tar.zst: the signature doesn't match the file".into())
        );
        assert_eq!(
            o(4, "warning: x\nerror: failed to commit transaction (conflicting files)\n"),
            Outcome::Failed("error: failed to commit transaction (conflicting files)".into())
        );
        assert_eq!(o(5, "tuner-update: pacman is busy"), Outcome::Busy);
        assert_eq!(o(6, ""), Outcome::NoKey);
        assert!(matches!(o(2, "tuner-update: must be started by pkexec"), Outcome::Failed(m) if m.ends_with("must be started by pkexec")));
        assert!(matches!(outcome(None, b"", b""), Outcome::Failed(_)));
    }

    /// The argv is pkexec's, then the helper's: one argument each, no shell.
    #[test]
    fn argv_is_pkexec_then_the_helper() {
        let pkexec: Vec<OsString> = vec!["/usr/bin/pkexec".into(), "--disable-internal-agent".into()];
        let a =
            argv(&pkexec, std::path::Path::new(super::super::HELPER), &[OsStr::new("install"), OsStr::new("/home/u/.cache/tuner/update/a b.pkg")]);
        assert_eq!(
            a,
            ["/usr/bin/pkexec", "--disable-internal-agent", "/usr/lib/tuner/tuner-update", "install", "/home/u/.cache/tuner/update/a b.pkg"]
        );
    }

    /// A program that doesn't answer is given up on (and killed while it is still ours); a
    /// stop stops the wait. Real shell utilities only.
    #[test]
    fn a_hung_program_times_out_and_a_stop_stops_waiting() {
        let sleeper: Vec<OsString> = vec!["/bin/sleep".into(), "30".into()];
        let t = Instant::now();
        assert_eq!(run(&sleeper, Duration::from_millis(300), &CancellationToken::new()), Outcome::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(5));
        let stop = CancellationToken::new();
        stop.cancel();
        assert_eq!(run(&sleeper, Duration::from_secs(30), &stop), Outcome::Stopped);
        let missing: Vec<OsString> = vec!["/nonexistent/pkexec".into()];
        assert!(
            matches!(run(&missing, Duration::from_secs(1), &CancellationToken::new()), Outcome::Failed(m) if m.starts_with("could not start pkexec"))
        );
        let sh: Vec<OsString> = vec!["/bin/sh".into(), "-c".into(), "echo 'tuner-update: nope' >&2; exit 3".into()];
        assert_eq!(run(&sh, Duration::from_secs(10), &CancellationToken::new()), Outcome::Refused("nope".into()));
    }
}
