//! Running agent CLIs (DESIGN §39.3). Everything here blocks: the service calls it on the
//! runtime's blocking pool, never on the UI thread.
//!
//! [`SystemRunner`] rules: argv with no shell; stdin closed (a CLI that wants to ask fails
//! instead of waiting); run from `$HOME` (no project scope, no per-folder trust prompt);
//! `GIT_TERMINAL_PROMPT=0` (a marketplace clone never asks for a password); stdout and stderr
//! drained on their own threads (a big answer can't fill a pipe and stall the child), at most
//! [`OUTPUT_CAP`] kept of each; the child leads its own process group, and on a timeout or at
//! shutdown the whole group is killed (node and git grandchildren too). Nothing here logs what a
//! command printed: it can hold other servers' secrets.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// A status read (`plugin list`, `mcp get`, `--version`). A first run in a fresh home took ~7 s.
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
/// A change (add, install, update, remove): a GitHub marketplace clone may take a while.
pub const CHANGE_TIMEOUT: Duration = Duration::from_secs(120);
/// Output kept per stream; the rest is read and dropped.
pub const OUTPUT_CAP: usize = 1 << 20;
/// How long to wait for the output after the child exited (a background grandchild may hold
/// the pipe open; its output isn't needed, and its group is killed so the readers end).
const DRAIN_WAIT: Duration = Duration::from_secs(2);

/// One command: argv (no shell), extra environment, a time limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    /// `argv[0]` = the program name (the service puts the path it found there).
    pub argv: Vec<String>,
    /// Extra child environment (Copilot: `COPILOT_AUTO_UPDATE=false`).
    pub env: Vec<(&'static str, String)>,
    pub timeout: Duration,
}

impl Cmd {
    pub fn new(argv: &[&str], timeout: Duration) -> Self {
        Self { argv: argv.iter().map(|s| (*s).to_owned()).collect(), env: Vec::new(), timeout }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdOutput {
    /// `None` when a signal ended it.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// The program isn't there (any more).
    NotFound,
    TimedOut,
    /// Tuner is closing.
    Stopped,
    Spawn(String),
}

/// Where agent CLIs run: the system (production), nowhere ([`NullRunner`], off the desktop), or
/// a script (`fake::FakeRunner`, tests). Blocking; called on the blocking pool only.
pub trait CommandRunner: Send + Sync + 'static {
    /// The program's path when it can run here (PATH, then [`ConnectEnv::extra_dirs`]).
    fn which(&self, program: &str, env: &ConnectEnv) -> Option<PathBuf>;
    fn run(&self, cmd: &Cmd, env: &ConnectEnv) -> Result<CmdOutput, RunError>;
}

/// Paths and versions the connectors read. Tests point it at a temp dir.
#[derive(Debug, Clone)]
pub struct ConnectEnv {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME` or `~/.config`.
    pub xdg_config: PathBuf,
    /// `$CLAUDE_CONFIG_DIR` (where Claude Code keeps `.claude.json` instead of the home).
    pub claude_config_dir: Option<PathBuf>,
    /// `$PATH`.
    pub path: OsString,
    /// Searched after PATH: a Tuner started from the launcher may lack the shell's PATH (mise
    /// shims, npm / bun globals).
    pub extra_dirs: Vec<PathBuf>,
    /// The packaged marketplace (`setup::SHARE_DIR`) when it's installed.
    pub share: Option<PathBuf>,
    /// This Tuner's version.
    pub tuner_version: String,
}

impl ConnectEnv {
    /// This process's environment.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let home = var("HOME").unwrap_or_else(|| PathBuf::from("/"));
        Self {
            xdg_config: var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")),
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            path: std::env::var_os("PATH").unwrap_or_default(),
            extra_dirs: extra_dirs(&home),
            share: crate::agent::setup::local_marketplace().map(Path::to_path_buf),
            tuner_version: env!("CARGO_PKG_VERSION").to_owned(),
            home,
        }
    }

    /// A home of its own (tests): no PATH, no packaged marketplace.
    pub fn rooted(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            xdg_config: home.join(".config"),
            claude_config_dir: None,
            path: OsString::new(),
            extra_dirs: Vec::new(),
            share: None,
            tuner_version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }

    /// The child's PATH: ours, then the extra directories.
    fn child_path(&self) -> OsString {
        let dirs: Vec<PathBuf> = std::env::split_paths(&self.path).chain(self.extra_dirs.iter().cloned()).collect();
        std::env::join_paths(dirs).unwrap_or_else(|_| self.path.clone())
    }
}

/// Where npm, bun, volta and mise put agent CLIs (`~/.local/bin` for Claude's own installer).
fn extra_dirs(home: &Path) -> Vec<PathBuf> {
    [".local/bin", ".local/share/mise/shims", ".bun/bin", ".npm-global/bin", ".volta/bin"].iter().map(|d| home.join(d)).collect()
}

/// An executable file at `p`.
fn executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The production runner. `stop` = [`SystemRunner::stopper`]: set at shutdown, it kills what
/// runs and refuses new commands.
#[derive(Default)]
pub struct SystemRunner {
    stop: Arc<AtomicBool>,
}

impl SystemRunner {
    pub fn stopper(&self) -> Arc<AtomicBool> {
        self.stop.clone()
    }
}

impl CommandRunner for SystemRunner {
    fn which(&self, program: &str, env: &ConnectEnv) -> Option<PathBuf> {
        if program.contains('/') {
            return executable(Path::new(program)).then(|| PathBuf::from(program));
        }
        std::env::split_paths(&env.path).chain(env.extra_dirs.iter().cloned()).map(|d| d.join(program)).find(|p| p.is_absolute() && executable(p))
    }

    fn run(&self, cmd: &Cmd, env: &ConnectEnv) -> Result<CmdOutput, RunError> {
        use std::os::unix::process::CommandExt;
        if self.stop.load(Ordering::SeqCst) {
            return Err(RunError::Stopped);
        }
        let (program, args) = cmd.argv.split_first().ok_or_else(|| RunError::Spawn("empty command".into()))?;
        let mut c = Command::new(program);
        c.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("NO_COLOR", "1")
            .env("PATH", env.child_path())
            .envs(cmd.env.iter().map(|(k, v)| (*k, v.as_str())))
            .process_group(0);
        // a missing home would fail the spawn as "not found" — the program is there
        if env.home.is_dir() {
            c.current_dir(&env.home);
        }
        let mut child = c.spawn().map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => RunError::NotFound,
            _ => RunError::Spawn(e.to_string()),
        })?;
        let (tx, rx) = crossbeam_channel::bounded::<(bool, Vec<u8>)>(2);
        let mut readers = 0;
        for (is_err, stream) in [
            (false, child.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>)),
            (true, child.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)),
        ] {
            let Some(stream) = stream else { continue };
            let tx = tx.clone();
            let spawned = std::thread::Builder::new().name("agent-cli-out".into()).spawn(move || {
                let _ = tx.send((is_err, drain(stream)));
            });
            match spawned {
                Ok(_) => readers += 1,
                Err(_) => tracing::warn!("agent CLI: couldn't start an output reader"),
            }
        }
        drop(tx);
        let end = Instant::now() + cmd.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if self.stop.load(Ordering::SeqCst) || Instant::now() >= end => {
                    kill_group(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(if self.stop.load(Ordering::SeqCst) { RunError::Stopped } else { RunError::TimedOut });
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => {
                    kill_group(child.id());
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(RunError::Spawn(e.to_string()));
                }
            }
        };
        let mut out = CmdOutput { code: status.code(), ..Default::default() };
        let until = Instant::now() + DRAIN_WAIT;
        let mut read = 0;
        while read < readers {
            let Ok((is_err, bytes)) = rx.recv_deadline(until) else { break };
            read += 1;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if is_err { out.stderr = text } else { out.stdout = text }
        }
        if read < readers {
            // what the CLI left running still holds the output open: end it, and so the readers
            kill_group(child.id());
        }
        Ok(out)
    }
}

/// Read a stream to its end, keeping the first [`OUTPUT_CAP`] bytes.
fn drain(mut r: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = OUTPUT_CAP.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    kept
}

/// SIGKILL the process group led by `pid` (the CLI and what it started). Best effort, argv.
fn kill_group(pid: u32) {
    let _ = Command::new("kill").args(["-KILL", "--", &format!("-{pid}")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// Runs nothing (off the desktop: unit and UI tests, `TUNER_NO_DESKTOP`): every agent is
/// "not found".
pub struct NullRunner;

impl CommandRunner for NullRunner {
    fn which(&self, _: &str, _: &ConnectEnv) -> Option<PathBuf> {
        None
    }
    fn run(&self, _: &Cmd, _: &ConnectEnv) -> Result<CmdOutput, RunError> {
        Err(RunError::NotFound)
    }
}

#[cfg(test)]
pub(crate) mod fake {
    use std::collections::HashSet;
    use std::sync::Mutex;

    use super::*;

    type Rule = (Vec<String>, Result<CmdOutput, RunError>);

    /// Scripted answers by argv prefix (first match wins; an unmatched command answers
    /// `NotFound`); records every argv it runs, `argv[0]` reduced to the program name. `found`
    /// = the programs `which` reports (as `/fake/bin/<program>`).
    #[derive(Default)]
    pub(crate) struct FakeRunner {
        found: HashSet<String>,
        rules: Mutex<Vec<Rule>>,
        calls: Mutex<Vec<Vec<String>>>,
        /// Every run takes this long (the queue tests).
        delay: Duration,
    }

    impl FakeRunner {
        pub(crate) fn found(mut self, programs: &[&str]) -> Self {
            self.found.extend(programs.iter().map(|p| (*p).to_owned()));
            self
        }

        pub(crate) fn on(self, argv_prefix: &[&str], out: Result<CmdOutput, RunError>) -> Self {
            self.rules.lock().unwrap().push((argv_prefix.iter().map(|s| (*s).to_owned()).collect(), out));
            self
        }

        pub(crate) fn on_ok(self, argv_prefix: &[&str], stdout: &str) -> Self {
            self.on(argv_prefix, Ok(CmdOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() }))
        }

        pub(crate) fn on_fail(self, argv_prefix: &[&str], code: i32, stdout: &str, stderr: &str) -> Self {
            self.on(argv_prefix, Ok(CmdOutput { code: Some(code), stdout: stdout.into(), stderr: stderr.into() }))
        }

        pub(crate) fn slow(mut self, delay: Duration) -> Self {
            self.delay = delay;
            self
        }

        /// Replace the answer for a prefix (e.g. the status after a connect).
        pub(crate) fn set(&self, argv_prefix: &[&str], out: Result<CmdOutput, RunError>) {
            let key: Vec<String> = argv_prefix.iter().map(|s| (*s).to_owned()).collect();
            let mut rules = self.rules.lock().unwrap();
            rules.retain(|(k, _)| *k != key);
            rules.insert(0, (key, out));
        }

        pub(crate) fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        /// The calls as command lines ("codex mcp add tuner -- tuner mcp").
        pub(crate) fn lines(&self) -> Vec<String> {
            self.calls().iter().map(|a| a.join(" ")).collect()
        }
    }

    impl CommandRunner for FakeRunner {
        fn which(&self, program: &str, _: &ConnectEnv) -> Option<PathBuf> {
            self.found.contains(program).then(|| PathBuf::from(format!("/fake/bin/{program}")))
        }

        fn run(&self, cmd: &Cmd, _: &ConnectEnv) -> Result<CmdOutput, RunError> {
            let mut argv = cmd.argv.clone();
            if let Some(first) = argv.first_mut()
                && let Some(name) = Path::new(first.as_str()).file_name()
            {
                *first = name.to_string_lossy().into_owned();
            }
            self.calls.lock().unwrap().push(argv.clone());
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let rules = self.rules.lock().unwrap();
            rules.iter().find(|(k, _)| argv.starts_with(k)).map_or(Err(RunError::NotFound), |(_, out)| out.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Cmd {
        Cmd { argv: vec!["/bin/sh".into(), "-c".into(), script.into()], env: Vec::new(), timeout }
    }

    fn env() -> ConnectEnv {
        let mut e = ConnectEnv::rooted(&std::env::temp_dir());
        e.path = std::env::var_os("PATH").unwrap_or_default();
        e
    }

    /// A CLI that hangs is killed with what it started, quickly, and never freezes the caller.
    #[test]
    fn a_hanging_command_times_out_and_its_group_dies() {
        let pidfile = tempfile::NamedTempFile::new().unwrap();
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.path().display());
        let started = Instant::now();
        let r = SystemRunner::default().run(&sh(&script, Duration::from_millis(300)), &env());
        assert_eq!(r, Err(RunError::TimedOut));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        let pid = std::fs::read_to_string(pidfile.path()).unwrap().trim().to_owned();
        std::thread::sleep(Duration::from_millis(100));
        let alive = std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z "));
        assert!(!alive, "the grandchild sleep {pid} survived");
    }

    /// More output than a pipe holds can't stall the child; what's kept is capped. Stdin is
    /// closed (`cat` ends at once), and the extra env reaches the child.
    #[test]
    fn output_is_drained_capped_and_stdin_is_closed() {
        let r = SystemRunner::default().run(&sh("head -c 2097152 /dev/zero; echo done >&2", Duration::from_secs(20)), &env()).unwrap();
        assert_eq!((r.code, r.stdout.len(), r.stderr.trim()), (Some(0), OUTPUT_CAP, "done"));
        let r = SystemRunner::default().run(&sh("cat; echo end", Duration::from_secs(5)), &env()).unwrap();
        assert_eq!(r.stdout, "end\n");
        let mut cmd = sh("echo $COPILOT_AUTO_UPDATE $GIT_TERMINAL_PROMPT; pwd; exit 3", Duration::from_secs(5));
        cmd.env.push(("COPILOT_AUTO_UPDATE", "false".into()));
        let r = SystemRunner::default().run(&cmd, &env()).unwrap();
        let home = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!((r.code, r.stdout.lines().next()), (Some(3), Some("false 0")));
        assert_eq!(r.stdout.lines().nth(1).map(Path::new), Some(home.as_path()), "runs from the home directory");
    }

    #[test]
    fn missing_programs_are_not_found_and_stop_refuses() {
        let runner = SystemRunner::default();
        let missing = Cmd::new(&["/nonexistent/tuner-no-such-cli"], Duration::from_secs(1));
        assert_eq!(runner.run(&missing, &env()), Err(RunError::NotFound));
        assert!(runner.which("tuner-no-such-cli", &env()).is_none());
        assert_eq!(runner.which("sh", &env()).map(|p| p.is_absolute()), Some(true));
        // a program only in an extra directory (launched without the shell's PATH)
        let dir = tempfile::tempdir().unwrap();
        let prog = dir.path().join("fake-agent");
        std::fs::write(&prog, "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&prog, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut e = env();
        e.path = OsString::new();
        e.extra_dirs = vec![dir.path().to_path_buf()];
        assert_eq!(runner.which("fake-agent", &e), Some(prog));
        runner.stopper().store(true, Ordering::SeqCst);
        assert_eq!(runner.run(&sh("true", Duration::from_secs(1)), &env()), Err(RunError::Stopped));
    }
}
