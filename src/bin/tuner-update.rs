//! `tuner-update`: the update helper (DESIGN §41.5), the root program of Tuner's updates. Tuner
//! starts it only through `pkexec --disable-internal-agent /usr/lib/tuner/tuner-update …`;
//! pkexec picks the polkit action by this path and the first argument
//! (`packaging/polkit/io.github.rthoms334.omatuner.update.policy`):
//!
//! * `install <path>` — action `io.github.rthoms334.omatuner.update` (auth_admin_keep; the one
//!   action the passwordless rule may allow). Installs the package at `<path>` only when every
//!   check passes, in this order: the release key is real (not the placeholder); `<path>` is
//!   absolute and plain, every folder on the way is reached with `O_NOFOLLOW` from the one
//!   before (no symlinks; root's or the user's, not writable by others unless sticky), the last
//!   one is the user's and private (0700), and the file and its `<path>.minisig` are regular
//!   files of the user with one link, under the size caps; the file name is
//!   `omatuner-<ver>-<rel>-<arch>.pkg.tar.zst`; the bytes READ FROM THE OPENED FILE verify
//!   against the release key (pre-hashed minisign); pacman isn't busy. Those bytes are then
//!   written to a copy in root's private `/var/cache/omatuner/`, and only that copy is used:
//!   `pacman -Qip` must say name `omatuner`, this machine's architecture and the file name's
//!   version, `pacman -Q omatuner` must find it installed, and the new version must be newer
//!   (`vercmp`; the same version or a downgrade is refused). Then `pacman -U --noconfirm` on the
//!   copy, which is removed afterwards.
//! * `passwordless on|off` — action `…update-passwordless` (auth_admin: always the password).
//!   Writes or removes `/etc/polkit-1/rules.d/49-omatuner-update-<uid>.rules`, which lets the
//!   user who ran pkexec (`PKEXEC_UID`, named by `getent passwd`), while local and active, run
//!   `install` without a password. Nothing else.
//!
//! Both need euid 0 and `PKEXEC_UID`. Programs run with no shell, a clean environment (`PATH`,
//! `LC_ALL=C`) and stdin closed. The checks shared with the app (signature, file name,
//! `vercmp`) are `src/update/verify.rs`, included below: std, `minisign-verify` and `rustix`
//! only, never the `tuner` library.
//!
//! Exit codes: 0 done; 2 bad arguments, or not root through pkexec (nothing done); 3 refused —
//! a check failed, the reason on stderr, nothing installed; 4 pacman or the system change failed
//! (the end of its message); 5 pacman is busy (its database is locked: try later); 6 this build
//! has no release key. Never 126 or 127 (pkexec's own).

// the app uses all of it, the helper all but the placeholder's text (its tests do)
#[cfg_attr(not(test), allow(dead_code))]
#[path = "../update/verify.rs"]
mod verify;

#[cfg(test)]
#[path = "../update/test_signer.rs"]
mod test_signer;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use rustix::fs::{CWD, Mode, OFlags};

const EXIT_BAD_ARGS: u8 = 2;
const EXIT_REFUSED: u8 = 3;
const EXIT_FAILED: u8 = 4;
const EXIT_BUSY: u8 = 5;
const EXIT_NO_KEY: u8 = 6;
/// The most of a program's message that is passed back.
const TAIL_CAP: usize = 2048;
/// The polkit action the passwordless rule allows (the policy's `install` action).
const INSTALL_ACTION: &str = "io.github.rthoms334.omatuner.update";
/// `/etc/polkit-1/rules.d/49-omatuner-update-<uid>.rules`: before polkit's `50-default.rules`.
const RULE_PREFIX: &str = "49-omatuner-update-";
/// The longest path accepted.
const PATH_MAX: usize = 4096;

/// What the helper touches, injectable so the tests never run the real pacman, write to /etc
/// or need root.
struct Env {
    /// The effective uid.
    euid: Option<u32>,
    /// `PKEXEC_UID`, set by pkexec for the program it starts.
    pkexec_uid: Option<String>,
    /// The release key file's text ([`verify::RELEASE_KEY`]).
    key: String,
    /// This machine's pacman architecture.
    arch: &'static str,
    /// pacman (program and leading arguments).
    pacman: Vec<OsString>,
    /// getent (program and leading arguments).
    getent: Vec<OsString>,
    /// pacman's database lock.
    db_lock: PathBuf,
    /// Root's private folder for the verified copy.
    stage: PathBuf,
    /// Who owns `stage` (root).
    stage_owner: u32,
    /// polkit's rules folder.
    rules: PathBuf,
}

impl Env {
    fn real() -> Env {
        Env {
            euid: Some(rustix::process::geteuid().as_raw()),
            pkexec_uid: std::env::var("PKEXEC_UID").ok(),
            key: verify::RELEASE_KEY.to_owned(),
            arch: std::env::consts::ARCH,
            pacman: vec!["/usr/bin/pacman".into()],
            getent: vec!["/usr/bin/getent".into()],
            db_lock: "/var/lib/pacman/db.lck".into(),
            stage: "/var/cache/omatuner".into(),
            stage_owner: 0,
            rules: "/etc/polkit-1/rules.d".into(),
        }
    }
}

/// Why the helper stopped (its exit code).
#[derive(Debug)]
enum Failure {
    /// Exit 2: nothing was done.
    Bad(String),
    /// Exit 3: a check failed; nothing was installed.
    Refused(String),
    /// Exit 4: pacman, or writing the system file, failed.
    Failed(String),
    /// Exit 5: pacman's database is locked.
    Busy,
    /// Exit 6: the release key is the placeholder.
    NoKey,
}

impl Failure {
    fn code(&self) -> u8 {
        match self {
            Failure::Bad(_) => EXIT_BAD_ARGS,
            Failure::Refused(_) => EXIT_REFUSED,
            Failure::Failed(_) => EXIT_FAILED,
            Failure::Busy => EXIT_BUSY,
            Failure::NoKey => EXIT_NO_KEY,
        }
    }

    fn message(&self) -> String {
        match self {
            Failure::Bad(m) | Failure::Refused(m) | Failure::Failed(m) => m.clone(),
            Failure::Busy => "pacman is busy (its database is locked); try again when it has finished".into(),
            Failure::NoKey => "this build of Tuner has no release key, so it installs no updates".into(),
        }
    }
}

fn main() -> ExitCode {
    let args: Option<Vec<String>> = std::env::args_os().skip(1).map(|a| a.into_string().ok()).collect();
    match run(args, &Env::real()) {
        Ok(done) => {
            println!("{done}");
            ExitCode::SUCCESS
        }
        Err(f) => {
            let _ = writeln!(std::io::stderr(), "tuner-update: {}", f.message());
            ExitCode::from(f.code())
        }
    }
}

/// Check who started us and the arguments, then do the one thing asked. `args` is `None` when
/// one was not UTF-8.
fn run(args: Option<Vec<String>>, env: &Env) -> Result<String, Failure> {
    let args = args.ok_or_else(|| Failure::Bad("arguments are not UTF-8".into()))?;
    let uid = invoker(env)?;
    match args.as_slice() {
        [verb, path] if verb == "install" => install(env, uid, path),
        [verb, on] if verb == "passwordless" && (on == "on" || on == "off") => passwordless(env, uid, on == "on"),
        _ => Err(Failure::Bad("usage: install <package path> | passwordless on|off".into())),
    }
}

/// The user who ran pkexec: euid 0 and a numeric `PKEXEC_UID`.
fn invoker(env: &Env) -> Result<u32, Failure> {
    if env.euid != Some(0) {
        return Err(Failure::Bad("must run as root (through pkexec)".into()));
    }
    env.pkexec_uid
        .as_deref()
        .filter(|u| !u.is_empty() && u.len() <= 10 && u.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|u| u.parse().ok())
        .ok_or_else(|| Failure::Bad("must be started by pkexec".into()))
}

// ---------------------------------------------------------------------------------- install

fn install(env: &Env, uid: u32, path: &str) -> Result<String, Failure> {
    let key = verify::release_key(&env.key).ok_or(Failure::NoKey)?;
    let (dir, name) = open_private_dir(path, uid)?;
    let file = verify::package_file(name).ok_or_else(|| Failure::Bad(format!("{name:?} is not a Tuner package file name")))?;
    let package = read_owned(&dir, name, uid, verify::MAX_PACKAGE)?;
    let signature = read_owned(&dir, &format!("{name}{}", verify::SIGNATURE_SUFFIX), uid, verify::MAX_SIGNATURE)?;
    let signature = String::from_utf8(signature).map_err(|_| Failure::Refused(format!("{name}: the signature file isn't text")))?;
    // the bytes read from the opened file, never the path again
    verify::verify(&key, &package, &signature).map_err(|why| Failure::Refused(format!("{name}: {why}")))?;
    if env.db_lock.exists() {
        return Err(Failure::Busy);
    }
    let staged = stage(env, name, &package)?;
    let done = check_and_install(env, &staged, &file.version());
    let _ = std::fs::remove_file(&staged);
    done
}

/// What pacman says of the verified copy and of the installed package, then the install.
fn check_and_install(env: &Env, staged: &Path, file_version: &str) -> Result<String, Failure> {
    let info = pacman(env, &[OsStr::new("-Qip"), staged.as_os_str()])?;
    if !info.status.success() {
        return Err(Failure::Refused(format!("pacman can't read the package: {}", tail(&String::from_utf8_lossy(&info.stderr)))));
    }
    let info = String::from_utf8_lossy(&info.stdout);
    let (name, version, arch) = (field(&info, "Name"), field(&info, "Version"), field(&info, "Architecture"));
    if name != Some(verify::PACKAGE) {
        return Err(Failure::Refused(format!("the package is {}, not {}", name.unwrap_or("unnamed"), verify::PACKAGE)));
    }
    if arch != Some(env.arch) {
        return Err(Failure::Refused(format!("the package is for {}, this computer is {}", arch.unwrap_or("no architecture"), env.arch)));
    }
    let Some(version) = version.filter(|v| *v == file_version) else {
        return Err(Failure::Refused(format!("the package's version {} isn't its file name's {file_version}", version.unwrap_or("(none)"))));
    };
    let installed = pacman(env, &[OsStr::new("-Q"), OsStr::new(verify::PACKAGE)])?;
    let installed = String::from_utf8_lossy(&installed.stdout);
    let Some(installed) = installed.trim().strip_prefix(verify::PACKAGE).and_then(|v| v.strip_prefix(' ')).filter(|v| !v.is_empty()) else {
        return Err(Failure::Refused(format!("{} isn't installed with pacman: nothing to update", verify::PACKAGE)));
    };
    if verify::vercmp(version, installed) != std::cmp::Ordering::Greater {
        return Err(Failure::Refused(format!("{version} isn't newer than the installed {installed}")));
    }
    let out = pacman(env, &[OsStr::new("-U"), OsStr::new("--noconfirm"), OsStr::new("--noprogressbar"), OsStr::new("--"), staged.as_os_str()])?;
    if out.status.success() {
        return Ok(format!("installed {} {version} (was {installed})", verify::PACKAGE));
    }
    // pacman reports on stderr; fall back to stdout when it said nothing there
    let said = if out.stderr.iter().all(u8::is_ascii_whitespace) { &out.stdout } else { &out.stderr };
    let said = String::from_utf8_lossy(said);
    if said.contains("unable to lock database") {
        return Err(Failure::Busy);
    }
    Err(Failure::Failed(tail(&said)))
}

/// `Key : value` of pacman's `-Qi` output.
fn field<'a>(info: &'a str, key: &str) -> Option<&'a str> {
    info.lines().find_map(|l| l.split_once(':').filter(|(k, _)| k.trim() == key).map(|(_, v)| v.trim()))
}

/// Run pacman: no shell, a clean environment, stdin closed, output captured.
fn pacman(env: &Env, args: &[&OsStr]) -> Result<std::process::Output, Failure> {
    clean(&env.pacman, args).output().map_err(|e| Failure::Failed(format!("could not run pacman: {e}")))
}

fn clean(program: &[OsString], args: &[&OsStr]) -> Command {
    let mut cmd = Command::new(&program[0]);
    cmd.args(&program[1..]).args(args).env_clear().env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin").env("LC_ALL", "C").stdin(Stdio::null());
    cmd
}

/// Open the folder of `path` without following a symlink anywhere: each folder is opened from
/// the one before with `O_NOFOLLOW`, belongs to root or the user and isn't writable by others
/// (unless sticky, like /tmp); the last one is the user's and private. Returns it and the
/// file name.
fn open_private_dir(path: &str, uid: u32) -> Result<(File, &str), Failure> {
    let p = Path::new(path);
    if !p.is_absolute() || path.len() > PATH_MAX {
        return Err(Failure::Bad(format!("{path:?} is not an absolute path")));
    }
    let mut parts: Vec<&OsStr> = Vec::new();
    for c in p.components() {
        match c {
            Component::RootDir => {}
            Component::Normal(n) => parts.push(n),
            _ => return Err(Failure::Bad(format!("{path:?} is not a plain path"))),
        }
    }
    let Some((name, folders)) = parts.split_last() else { return Err(Failure::Bad(format!("{path:?} names no file"))) };
    let name = name.to_str().ok_or_else(|| Failure::Bad("the file name is not UTF-8".into()))?;
    let open = |at: &File, what: &OsStr| {
        rustix::fs::openat(at, what, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC, Mode::empty())
            .map(File::from)
            .map_err(|e| Failure::Refused(format!("{}: {e} (folders on the way must not be symlinks)", what.to_string_lossy())))
    };
    let root = rustix::fs::openat(CWD, "/", OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
        .map(File::from)
        .map_err(|e| Failure::Failed(format!("/: {e}")))?;
    let mut dir = root;
    check_dir(&dir, "/", uid, folders.is_empty())?;
    for (i, folder) in folders.iter().enumerate() {
        dir = open(&dir, folder)?;
        check_dir(&dir, &folder.to_string_lossy(), uid, i + 1 == folders.len())?;
    }
    Ok((dir, name))
}

fn check_dir(dir: &File, what: &str, uid: u32, last: bool) -> Result<(), Failure> {
    let m = dir.metadata().map_err(|e| Failure::Refused(format!("{what}: {e}")))?;
    let mode = m.mode();
    let ok = if last { m.uid() == uid && mode & 0o077 == 0 } else { (m.uid() == 0 || m.uid() == uid) && (mode & 0o022 == 0 || mode & 0o1000 != 0) };
    if ok {
        Ok(())
    } else if last {
        Err(Failure::Refused(format!("{what}: the package's folder must be yours and private (0700)")))
    } else {
        Err(Failure::Refused(format!("{what}: a folder on the way is someone else's or writable by others")))
    }
}

/// The bytes of `name` in `dir`: opened with `O_NOFOLLOW`, a regular file of `uid` with one
/// link and at most `max` bytes (checked on the open file, then read from it).
fn read_owned(dir: &File, name: &str, uid: u32, max: u64) -> Result<Vec<u8>, Failure> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC;
    let file = rustix::fs::openat(dir, name, flags, Mode::empty()).map(File::from).map_err(|e| {
        if e == rustix::io::Errno::NOENT { Failure::Refused(format!("{name} is missing")) } else { Failure::Refused(format!("{name}: {e}")) }
    })?;
    let m = file.metadata().map_err(|e| Failure::Refused(format!("{name}: {e}")))?;
    if !m.file_type().is_file() || m.uid() != uid || m.nlink() != 1 {
        return Err(Failure::Refused(format!("{name} must be a plain file of yours")));
    }
    if m.len() > max {
        return Err(Failure::Refused(format!("{name} is larger than {max} bytes")));
    }
    let mut bytes = Vec::with_capacity(m.len() as usize);
    file.take(max + 1).read_to_end(&mut bytes).map_err(|e| Failure::Refused(format!("{name}: {e}")))?;
    if bytes.len() as u64 > max {
        return Err(Failure::Refused(format!("{name} is larger than {max} bytes")));
    }
    Ok(bytes)
}

/// Write the verified bytes to root's private folder (made 0700 if missing; refused when it is
/// anything else), as `name`; returns the copy's path.
fn stage(env: &Env, name: &str, bytes: &[u8]) -> Result<PathBuf, Failure> {
    let failed = |what: &Path, e: std::io::Error| Failure::Failed(format!("{}: {e}", what.display()));
    match std::fs::DirBuilder::new().mode(0o700).create(&env.stage) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(failed(&env.stage, e)),
    }
    let m = std::fs::symlink_metadata(&env.stage).map_err(|e| failed(&env.stage, e))?;
    if !m.file_type().is_dir() || m.uid() != env.stage_owner || m.mode() & 0o777 != 0o700 {
        return Err(Failure::Failed(format!("{} isn't a private folder of root's", env.stage.display())));
    }
    let dest = env.stage.join(name);
    let part = env.stage.join(format!(".{name}.part"));
    for stale in [&part, &dest] {
        match std::fs::remove_file(stale) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(failed(stale, e)),
        }
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&part).map_err(|e| failed(&part, e))?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(|e| failed(&part, e))?;
    std::fs::rename(&part, &dest).map_err(|e| failed(&dest, e))?;
    Ok(dest)
}

// ----------------------------------------------------------------------------- passwordless

fn passwordless(env: &Env, uid: u32, on: bool) -> Result<String, Failure> {
    let path = env.rules.join(format!("{RULE_PREFIX}{uid}.rules"));
    if !on {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(format!("removed {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("no rule to remove".into()),
            Err(e) => Err(Failure::Failed(format!("{}: {e}", path.display()))),
        };
    }
    let user = user_name(env, uid)?;
    if !env.rules.is_dir() {
        return Err(Failure::Failed(format!("polkit's rules folder {} is missing", env.rules.display())));
    }
    let tmp = env.rules.join(format!(".{RULE_PREFIX}{uid}.rules.tmp"));
    let failed = |e: std::io::Error| Failure::Failed(format!("{}: {e}", path.display()));
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(e)),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).open(&tmp).map_err(failed)?;
    // polkitd reads it through its group on the folder: readable by all, whatever the umask
    f.set_permissions(std::fs::Permissions::from_mode(0o644))
        .and_then(|()| f.write_all(rule(&user, uid).as_bytes()))
        .and_then(|()| f.sync_all())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(failed)?;
    Ok(format!("wrote {}", path.display()))
}

/// The rule: `INSTALL_ACTION` for `user`, local and active; nothing else.
fn rule(user: &str, uid: u32) -> String {
    format!(
        "/* Tuner (omatuner) updates: written by /usr/lib/tuner/tuner-update for {user} (uid {uid}) when\n\
         \x20* they turned on Settings > About & logs > \"Install updates without asking for my password\".\n\
         \x20* It lets {user}, at this computer, run the update helper's install without a password. The\n\
         \x20* helper installs only an omatuner package signed with Tuner's release key and newer than\n\
         \x20* the installed one. Turn the switch off in Tuner, or delete this file. */\n\
         polkit.addRule(function (action, subject) {{\n\
         \x20   if (action.id == \"{INSTALL_ACTION}\" &&\n\
         \x20       subject.user == \"{user}\" && subject.local && subject.active) {{\n\
         \x20       return polkit.Result.YES;\n\
         \x20   }}\n\
         }});\n"
    )
}

/// The login name of `uid` (`getent passwd <uid>`), safe to quote in the rule: letters,
/// digits, `.`, `_` and `-`, not starting with `.` or `-`, at most 32, an optional final `$`.
fn user_name(env: &Env, uid: u32) -> Result<String, Failure> {
    let id = uid.to_string();
    let out =
        clean(&env.getent, &[OsStr::new("passwd"), OsStr::new(&id)]).output().map_err(|e| Failure::Failed(format!("could not run getent: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.lines().next().unwrap_or("").split(':').collect();
    let name = match fields.as_slice() {
        [name, _, u, ..] if out.status.success() && *u == id => *name,
        _ => return Err(Failure::Failed(format!("no user with uid {uid}"))),
    };
    let core = name.strip_suffix('$').unwrap_or(name);
    let ok = (1..=32).contains(&core.len())
        && core.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !core.starts_with(['.', '-']);
    if ok { Ok(name.to_owned()) } else { Err(Failure::Refused(format!("the user name {name:?} can't go in a polkit rule"))) }
}

/// The last [`TAIL_CAP`] bytes of `text`, on a character boundary.
fn tail(text: &str) -> String {
    let text = text.trim();
    let mut start = text.len().saturating_sub(TAIL_CAP);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_signer::TestKey;

    const ARCH: &str = std::env::consts::ARCH;

    fn me() -> u32 {
        rustix::process::getuid().as_raw()
    }

    /// A user folder with the package and its signature, root's stage folder and polkit's rules
    /// folder, all in a temp dir; pacman and getent are shell scripts run by `/bin/sh` (no
    /// freshly written file is ever exec'd, so tests that spawn can run in parallel).
    struct Rig {
        dir: tempfile::TempDir,
        env: Env,
        key: TestKey,
    }

    /// What the fake pacman answers.
    struct Pacman<'a> {
        name: &'a str,
        version: &'a str,
        arch: &'a str,
        installed: Option<&'a str>,
        /// `-U`'s exit status and stderr.
        install: (u8, &'a str),
    }

    impl Default for Pacman<'_> {
        fn default() -> Self {
            Pacman { name: "omatuner", version: "0.2.6-1", arch: ARCH, installed: Some("0.2.5-1"), install: (0, "") }
        }
    }

    impl Rig {
        fn new() -> Rig {
            let dir = tempfile::tempdir().unwrap();
            std::fs::create_dir(dir.path().join("rules")).unwrap();
            let update = dir.path().join("cache/update");
            std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&update).unwrap();
            let key = TestKey::new(1);
            let env = Env {
                euid: Some(0),
                pkexec_uid: Some(me().to_string()),
                key: key.public_file(),
                arch: ARCH,
                pacman: vec!["/bin/sh".into(), dir.path().join("pacman.sh").into()],
                getent: vec!["/bin/sh".into(), dir.path().join("getent.sh").into()],
                db_lock: dir.path().join("db.lck"),
                stage: dir.path().join("stage"),
                stage_owner: me(),
                rules: dir.path().join("rules"),
            };
            let rig = Rig { dir, env, key };
            rig.pacman(&Pacman::default());
            rig.getent(&format!("tv:x:{}:{}::/home/tv:/bin/bash", me(), me()));
            rig
        }

        fn update_dir(&self) -> PathBuf {
            self.dir.path().join("cache/update")
        }

        /// The fake pacman: `-Qip` and `-Q` answer as told; `-U` records its arguments and a copy
        /// of the file it got.
        fn pacman(&self, p: &Pacman) {
            let d = self.dir.path().display();
            let installed = match p.installed {
                Some(v) => format!("echo 'omatuner {v}'"),
                None => "echo \"error: package 'omatuner' was not found\" >&2; exit 1".into(),
            };
            let script = format!(
                "case \"$1\" in\n\
                 -Qip) printf 'Name            : %s\\nVersion         : %s\\nArchitecture    : %s\\n' '{}' '{}' '{}' ;;\n\
                 -Q) {installed} ;;\n\
                 -U) printf '%s\\n' \"$@\" > '{d}/argv'; cp \"$5\" '{d}/installed'; echo '{}' >&2; exit {} ;;\n\
                 esac\n",
                p.name, p.version, p.arch, p.install.1, p.install.0
            );
            std::fs::write(self.dir.path().join("pacman.sh"), script).unwrap();
        }

        fn getent(&self, line: &str) {
            std::fs::write(self.dir.path().join("getent.sh"), format!("echo '{line}'\n")).unwrap();
        }

        /// A package (its bytes) with its signature by `key`, in the user's folder.
        fn package(&self, name: &str, bytes: &[u8], signature: &str) -> String {
            let path = self.update_dir().join(name);
            std::fs::write(&path, bytes).unwrap();
            std::fs::write(self.update_dir().join(format!("{name}.minisig")), signature).unwrap();
            path.display().to_string()
        }

        fn signed(&self, version: &str) -> (String, Vec<u8>) {
            let bytes = format!("pretend zstd tarball of omatuner {version}").into_bytes();
            let name = format!("omatuner-{version}-{ARCH}.pkg.tar.zst");
            (self.package(&name, &bytes, &self.key.sign(&bytes)), bytes)
        }

        fn run(&self, args: &[&str]) -> Result<String, Failure> {
            run(Some(args.iter().map(|s| s.to_string()).collect()), &self.env)
        }

        fn installed(&self) -> Option<Vec<u8>> {
            std::fs::read(self.dir.path().join("installed")).ok()
        }
    }

    fn refused(r: Result<String, Failure>, has: &str) {
        match r {
            Err(Failure::Refused(m)) => assert!(m.contains(has), "{m:?} should say {has:?}"),
            other => panic!("expected a refusal saying {has:?}, got {other:?}"),
        }
    }

    #[test]
    fn installs_a_signed_newer_package_from_a_root_owned_copy() {
        let rig = Rig::new();
        let (path, bytes) = rig.signed("0.2.6-1");
        let done = rig.run(&["install", path.as_str()]).unwrap();
        assert_eq!(done, "installed omatuner 0.2.6-1 (was 0.2.5-1)");
        let staged = rig.env.stage.join(format!("omatuner-0.2.6-1-{ARCH}.pkg.tar.zst"));
        let argv = std::fs::read_to_string(rig.dir.path().join("argv")).unwrap();
        assert_eq!(argv, format!("-U\n--noconfirm\n--noprogressbar\n--\n{}\n", staged.display()));
        assert_eq!(rig.installed(), Some(bytes), "pacman got the verified bytes");
        assert!(!staged.exists(), "the copy is removed afterwards");
        let m = std::fs::metadata(&rig.env.stage).unwrap();
        assert_eq!(m.mode() & 0o777, 0o700);
    }

    /// Only the release key's pre-hashed signature over these very bytes passes; nothing
    /// reaches pacman otherwise.
    #[test]
    fn refuses_tampered_foreign_legacy_or_missing_signatures() {
        let rig = Rig::new();
        let name = format!("omatuner-0.2.6-1-{ARCH}.pkg.tar.zst");
        let bytes = b"the real package".to_vec();
        let mut tampered = bytes.clone();
        tampered[3] ^= 1;
        let path = rig.package(&name, &tampered, &rig.key.sign(&bytes));
        refused(rig.run(&["install", path.as_str()]), "doesn't match");
        rig.package(&name, &bytes, &TestKey::new(2).sign(&bytes));
        refused(rig.run(&["install", path.as_str()]), "another key");
        rig.package(&name, &bytes, &rig.key.sign_legacy(&bytes));
        refused(rig.run(&["install", path.as_str()]), "legacy");
        rig.package(&name, &bytes, "untrusted comment: no\nnot base64\n");
        refused(rig.run(&["install", path.as_str()]), "isn't a minisign signature");
        std::fs::remove_file(rig.update_dir().join(format!("{name}.minisig"))).unwrap();
        refused(rig.run(&["install", path.as_str()]), "is missing");
        assert_eq!(rig.installed(), None);
        assert!(!rig.env.stage.exists(), "nothing was copied for root");
    }

    /// The release key file still the placeholder: nothing installs, whatever is signed.
    #[test]
    fn a_build_without_a_release_key_installs_nothing() {
        let mut rig = Rig::new();
        let (path, _) = rig.signed("0.2.6-1");
        rig.env.key = verify::PLACEHOLDER.to_owned();
        let r = rig.run(&["install", path.as_str()]);
        assert!(matches!(r, Err(Failure::NoKey)), "{r:?}");
        assert_eq!(Failure::NoKey.code(), EXIT_NO_KEY);
        rig.env.key = "untrusted comment: x\nnot a key\n".into();
        assert!(matches!(rig.run(&["install", path.as_str()]), Err(Failure::NoKey)));
        assert_eq!(rig.installed(), None);
    }

    /// pacman's own reading of the signed copy must say omatuner, this architecture, the file
    /// name's version, and newer than what is installed.
    #[test]
    fn refuses_another_package_architecture_or_a_version_not_newer() {
        let rig = Rig::new();
        let (path, _) = rig.signed("0.2.6-1");
        rig.pacman(&Pacman { name: "omatuner-evil", ..Default::default() });
        refused(rig.run(&["install", path.as_str()]), "not omatuner");
        rig.pacman(&Pacman { arch: "riscv64", ..Default::default() });
        refused(rig.run(&["install", path.as_str()]), "for riscv64");
        rig.pacman(&Pacman { version: "0.2.7-1", ..Default::default() });
        refused(rig.run(&["install", path.as_str()]), "isn't its file name's");
        for installed in ["0.2.6-1", "0.2.7-1", "1:0.1.0-1"] {
            rig.pacman(&Pacman { installed: Some(installed), ..Default::default() });
            refused(rig.run(&["install", path.as_str()]), "isn't newer");
        }
        rig.pacman(&Pacman { installed: None, ..Default::default() });
        refused(rig.run(&["install", path.as_str()]), "isn't installed");
        assert_eq!(rig.installed(), None);
        // a pre-release installed: the release with its version is newer
        rig.pacman(&Pacman { installed: Some("0.2.6rc1-1"), ..Default::default() });
        assert!(rig.run(&["install", path.as_str()]).is_ok());
    }

    #[test]
    fn a_busy_or_failing_pacman_is_reported() {
        let rig = Rig::new();
        let (path, _) = rig.signed("0.2.6-1");
        std::fs::write(&rig.env.db_lock, "").unwrap();
        assert!(matches!(rig.run(&["install", path.as_str()]), Err(Failure::Busy)));
        assert_eq!(rig.installed(), None);
        std::fs::remove_file(&rig.env.db_lock).unwrap();
        rig.pacman(&Pacman { install: (1, "error: failed to commit transaction (conflicting files)"), ..Default::default() });
        match rig.run(&["install", path.as_str()]) {
            Err(Failure::Failed(m)) => assert_eq!(m, "error: failed to commit transaction (conflicting files)"),
            other => panic!("{other:?}"),
        }
        rig.pacman(&Pacman { install: (1, "error: failed to init transaction (unable to lock database)"), ..Default::default() });
        assert!(matches!(rig.run(&["install", path.as_str()]), Err(Failure::Busy)));
        assert!(!rig.env.stage.join(format!("omatuner-0.2.6-1-{ARCH}.pkg.tar.zst")).exists());
    }

    /// The path is reached without following a symlink, through folders nobody else can
    /// write, to a private folder and a plain file of the user's.
    #[test]
    fn refuses_unsafe_paths_and_files() {
        use std::os::unix::fs::symlink;
        let rig = Rig::new();
        let (path, bytes) = rig.signed("0.2.6-1");
        let name = format!("omatuner-0.2.6-1-{ARCH}.pkg.tar.zst");
        let dotdot = format!("{}/../update/{name}", rig.update_dir().display());
        for bad in ["relative/omatuner-0.2.6-1-x86_64.pkg.tar.zst", "/", dotdot.as_str()] {
            assert!(matches!(rig.run(&["install", bad]), Err(Failure::Bad(_))), "{bad}");
        }
        let odd = rig.package(&format!("omatuner-0.2.6-1-{ARCH}.tar.gz"), &bytes, &rig.key.sign(&bytes));
        assert!(matches!(rig.run(&["install", odd.as_str()]), Err(Failure::Bad(_))));
        // a symlinked folder on the way
        let elsewhere = rig.dir.path().join("elsewhere");
        symlink(rig.update_dir(), &elsewhere).unwrap();
        refused(rig.run(&["install", format!("{}/{name}", elsewhere.display()).as_str()]), "symlinks");
        // a symlinked file
        let real = rig.update_dir().join("real.pkg");
        std::fs::rename(&path, &real).unwrap();
        symlink(&real, &path).unwrap();
        refused(rig.run(&["install", path.as_str()]), &name);
        std::fs::remove_file(&path).unwrap();
        // a second hard link
        std::fs::rename(&real, &path).unwrap();
        std::fs::hard_link(&path, &real).unwrap();
        refused(rig.run(&["install", path.as_str()]), "plain file");
        std::fs::remove_file(&real).unwrap();
        // a folder others can read, or write
        for mode in [0o755, 0o770] {
            std::fs::set_permissions(rig.update_dir(), std::fs::Permissions::from_mode(mode)).unwrap();
            refused(rig.run(&["install", path.as_str()]), "private");
        }
        std::fs::set_permissions(rig.update_dir(), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(rig.dir.path().join("cache"), std::fs::Permissions::from_mode(0o777)).unwrap();
        refused(rig.run(&["install", path.as_str()]), "writable by others");
        std::fs::set_permissions(rig.dir.path().join("cache"), std::fs::Permissions::from_mode(0o755)).unwrap();
        // larger than the cap (a sparse file: nothing is read)
        std::fs::File::options().write(true).open(&path).unwrap().set_len(verify::MAX_PACKAGE + 1).unwrap();
        refused(rig.run(&["install", path.as_str()]), "larger than");
        assert_eq!(rig.installed(), None);
    }

    #[test]
    fn refuses_unless_root_started_by_pkexec() {
        let mut rig = Rig::new();
        let (path, _) = rig.signed("0.2.6-1");
        rig.env.euid = Some(1000);
        assert!(matches!(rig.run(&["install", path.as_str()]), Err(Failure::Bad(_))));
        rig.env.euid = Some(0);
        for uid in [None, Some(""), Some("root"), Some("1000;id"), Some("99999999999")] {
            rig.env.pkexec_uid = uid.map(String::from);
            assert!(matches!(rig.run(&["install", path.as_str()]), Err(Failure::Bad(_))), "PKEXEC_UID {uid:?}");
        }
        rig.env.pkexec_uid = Some(me().to_string());
        let path = path.as_str();
        for args in [&["install"][..], &["install", path, "extra"], &["Install", path], &["passwordless", "yes"], &["passwordless"], &[]] {
            assert!(matches!(rig.run(args), Err(Failure::Bad(_))), "{args:?}");
        }
        assert!(matches!(run(None, &rig.env), Err(Failure::Bad(_))));
        assert_eq!(rig.installed(), None);
    }

    /// `passwordless on` writes one rule naming this user and only the install action; `off`
    /// removes it (and is fine when there is none).
    #[test]
    fn the_passwordless_rule_allows_only_install_for_this_user() {
        let rig = Rig::new();
        let file = rig.env.rules.join(format!("49-omatuner-update-{}.rules", me()));
        rig.run(&["passwordless", "on"]).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("action.id == \"io.github.rthoms334.omatuner.update\" &&"), "{text}");
        assert!(text.contains("subject.user == \"tv\" && subject.local && subject.active"), "{text}");
        assert_eq!(text.matches("polkit.Result.YES").count(), 1);
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o644);
        assert_eq!(std::fs::read_dir(&rig.env.rules).unwrap().count(), 1, "no temp file left");
        rig.run(&["passwordless", "off"]).unwrap();
        assert!(!file.exists());
        assert_eq!(rig.run(&["passwordless", "off"]).unwrap(), "no rule to remove");
        // names that can't be quoted safely, or no such user
        for line in [format!("ev\"il:x:{}:1::/:/bin/sh", me()), format!("-x:x:{}:1::/:/bin/sh", me()), "tv:x:4242:1::/:/bin/sh".into()] {
            rig.getent(&line);
            assert!(rig.run(&["passwordless", "on"]).is_err(), "{line}");
        }
        assert!(!file.exists());
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("  error: boom \n"), "error: boom");
        let long = format!("{}{}", "é".repeat(2000), "end");
        let t = tail(&long);
        assert!(t.len() <= TAIL_CAP && t.ends_with("end"));
    }
}
