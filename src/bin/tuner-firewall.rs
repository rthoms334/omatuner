//! `tuner-firewall`: the casting firewall helper (DESIGN §40 §9.4), the only code that ever runs
//! as root. Started by Tuner through `pkexec --disable-internal-agent /usr/lib/tuner/tuner-firewall
//! allow|remove <iface> <cidr> <port> [--first]` under the polkit action
//! `io.github.rthoms334.omatuner.firewall`. Std only, no dependencies. OWNER: CAST-7.
//!
//! Validates everything before acting (verb; interface `^[A-Za-z0-9_.-]{1,15}$` present in
//! `/sys/class/net`; an RFC 1918 IPv4 CIDR, /8–/32, host bits zero; port 1024–65535; euid 0 with
//! `PKEXEC_UID` set), then runs exactly one fixed `/usr/bin/ufw` command. Exit codes: 0 done,
//! 2 bad arguments, 3 ufw failed (its stderr tail passed through). Never 126/127 (pkexec's).
//!
//! The pattern follows PearPlay's `helper/firewall.py` (MIT). Stricter than the regex above in one
//! way: an interface of `.` or `..` (which would pass it and name a directory under
//! `/sys/class/net`) or one starting with `-` (which ufw's parser could read as an option) is refused.

use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};

/// Bad arguments (or not started by pkexec): nothing was run.
const EXIT_BAD_ARGS: u8 = 2;
/// ufw ran and failed; its stderr tail is on ours.
const EXIT_UFW_FAILED: u8 = 3;
/// The most of ufw's stderr that is passed back.
const TAIL_CAP: usize = 2048;
/// The comment on the rule, everywhere (Tuner's `firewall::TAG`).
const TAG: &str = "omatuner cast";

/// What the helper touches, injectable so the unit tests never run the real ufw or ask for root.
struct Env {
    ufw: PathBuf,
    sys_class_net: PathBuf,
    /// The effective uid.
    euid: Option<u32>,
    /// `PKEXEC_UID`, set by pkexec for the program it starts.
    pkexec_uid: Option<String>,
}

impl Env {
    fn real() -> Env {
        Env {
            ufw: "/usr/bin/ufw".into(),
            sys_class_net: "/sys/class/net".into(),
            euid: effective_uid(),
            pkexec_uid: std::env::var("PKEXEC_UID").ok(),
        }
    }
}

/// The effective uid (field 2 of `Uid:` in `/proc/self/status`): std has no `geteuid`.
fn effective_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|l| l.strip_prefix("Uid:"))?.split_whitespace().nth(1)?.parse().ok()
}

fn main() -> ExitCode {
    let args: Option<Vec<String>> = std::env::args_os().skip(1).map(|a| a.into_string().ok()).collect();
    match run(args, &Env::real()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Bad(why)) => {
            eprintln!("tuner-firewall: {why}");
            ExitCode::from(EXIT_BAD_ARGS)
        }
        Err(Failure::Ufw(tail)) => {
            let _ = std::io::stderr().write_all(tail.as_bytes());
            ExitCode::from(EXIT_UFW_FAILED)
        }
    }
}

/// Validate, then run the one ufw command. `args` is `None` when one was not UTF-8.
fn run(args: Option<Vec<String>>, env: &Env) -> Result<(), Failure> {
    let args = args.ok_or_else(|| Failure::Bad("arguments are not UTF-8".into()))?;
    execute(env, &plan(&args, env).map_err(Failure::Bad)?)
}

/// Why the helper stopped.
enum Failure {
    /// Exit 2: nothing was run.
    Bad(String),
    /// Exit 3: ufw's message.
    Ufw(String),
}

/// Validate `args` (everything after the program name) and the process, and return the one ufw
/// argument list to run. Pure apart from reading `sys_class_net`.
fn plan(args: &[String], env: &Env) -> Result<Vec<String>, String> {
    if env.euid != Some(0) {
        return Err("must run as root".into());
    }
    if !env.pkexec_uid.as_deref().is_some_and(|u| !u.is_empty() && u.bytes().all(|b| b.is_ascii_digit())) {
        return Err("must be started by pkexec".into());
    }
    let (verb, iface, cidr, port, first) = match args {
        [verb, iface, cidr, port] => (verb, iface, cidr, port, false),
        [verb, iface, cidr, port, flag] if flag == "--first" && verb == "allow" => (verb, iface, cidr, port, true),
        _ => return Err("usage: allow|remove <iface> <cidr> <port> [--first]".into()),
    };
    if verb != "allow" && verb != "remove" {
        return Err(format!("unknown verb {verb:?}"));
    }
    check_iface(iface, env)?;
    check_cidr(cidr)?;
    check_port(port)?;
    let mut ufw: Vec<String> = Vec::new();
    match (verb.as_str(), first) {
        ("allow", true) => ufw.extend(["prepend".into(), "allow".into()]),
        ("allow", false) => ufw.push("allow".into()),
        _ => ufw.extend(["delete".into(), "allow".into()]),
    }
    ufw.extend(["in", "on", iface, "from", cidr, "to", "any", "port", port, "proto", "tcp"].map(String::from));
    if verb == "allow" {
        ufw.extend(["comment".into(), TAG.into()]);
    }
    Ok(ufw)
}

/// `^[A-Za-z0-9_.-]{1,15}$`, not `.`, `..` or starting `-`, and a real interface.
fn check_iface(iface: &str, env: &Env) -> Result<(), String> {
    let ok = (1..=15).contains(&iface.len())
        && iface.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        && iface != "."
        && iface != ".."
        && !iface.starts_with('-');
    if !ok {
        return Err(format!("bad interface {iface:?}"));
    }
    if !env.sys_class_net.join(iface).exists() {
        return Err(format!("no such interface {iface:?}"));
    }
    Ok(())
}

/// A dotted-quad RFC 1918 network, `/8` to `/32`, inside its private block, host bits zero.
fn check_cidr(cidr: &str) -> Result<(), String> {
    let bad = || format!("bad network {cidr:?}");
    let (ip, prefix) = cidr.split_once('/').ok_or_else(bad)?;
    if prefix.is_empty() || prefix.len() > 2 || !prefix.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let prefix: u32 = prefix.parse().map_err(|_| bad())?;
    let ip: Ipv4Addr = ip.parse().map_err(|_| bad())?;
    if !(8..=32).contains(&prefix) {
        return Err(bad());
    }
    let addr = u32::from(ip);
    // The private blocks: (network, prefix). The network must lie wholly inside one.
    let blocks = [(Ipv4Addr::new(10, 0, 0, 0), 8), (Ipv4Addr::new(172, 16, 0, 0), 12), (Ipv4Addr::new(192, 168, 0, 0), 16)];
    let inside = blocks.iter().any(|&(net, bits)| prefix >= bits && addr >> (32 - bits) == u32::from(net) >> (32 - bits));
    if !inside || (prefix < 32 && addr << prefix != 0) {
        return Err(bad());
    }
    Ok(())
}

/// Decimal digits only, 1024–65535.
fn check_port(port: &str) -> Result<(), String> {
    let ok = !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|p| (1024..=65535).contains(&p));
    if ok { Ok(()) } else { Err(format!("bad port {port:?}")) }
}

/// Run the one fixed ufw command: no shell, a clean environment, stdin null, output captured.
fn execute(env: &Env, ufw_args: &[String]) -> Result<(), Failure> {
    let out = Command::new(&env.ufw)
        .args(ufw_args)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Failure::Ufw(format!("could not run {}: {e}", env.ufw.display())))?;
    if out.status.success() {
        return Ok(());
    }
    // ufw reports its errors on stderr; fall back to stdout if it said nothing there.
    let said = if out.stderr.iter().all(u8::is_ascii_whitespace) { &out.stdout } else { &out.stderr };
    Err(Failure::Ufw(tail(&String::from_utf8_lossy(said))))
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

    /// An environment that is root-under-pkexec with these interfaces present.
    fn env(dir: &tempfile::TempDir, ifaces: &[&str]) -> Env {
        for i in ifaces {
            std::fs::create_dir_all(dir.path().join("net").join(i)).unwrap();
        }
        Env { ufw: dir.path().join("ufw"), sys_class_net: dir.path().join("net"), euid: Some(0), pkexec_uid: Some("1000".into()) }
    }

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn joined(a: &[&str], env: &Env) -> Result<String, String> {
        plan(&args(a), env).map(|v| v.join(" "))
    }

    #[test]
    fn builds_the_one_fixed_command() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir, &["wlan0", "br-lan.1"]);
        assert_eq!(
            joined(&["allow", "wlan0", "192.168.1.0/24", "47800"], &env).unwrap(),
            "allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment omatuner cast"
        );
        assert_eq!(
            joined(&["allow", "wlan0", "192.168.1.0/24", "47800", "--first"], &env).unwrap(),
            "prepend allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment omatuner cast"
        );
        assert_eq!(
            joined(&["remove", "br-lan.1", "10.0.0.0/8", "1024"], &env).unwrap(),
            "delete allow in on br-lan.1 from 10.0.0.0/8 to any port 1024 proto tcp"
        );
        // The arguments are passed as given, one each: the comment is a single argument.
        let ufw = plan(&args(&["allow", "wlan0", "172.16.0.0/12", "65535"]), &env).unwrap();
        assert_eq!(ufw.last().map(String::as_str), Some("omatuner cast"));
    }

    #[test]
    fn refuses_bad_arguments_without_running_anything() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir, &["wlan0"]);
        let ok = ["allow", "wlan0", "192.168.1.0/24", "47800"];
        assert!(plan(&args(&ok), &env).is_ok());
        let cases: &[&[&str]] = &[
            &[],
            &["allow"],
            &["add", "wlan0", "192.168.1.0/24", "47800"],
            &["ALLOW", "wlan0", "192.168.1.0/24", "47800"],
            &["remove", "wlan0", "192.168.1.0/24", "47800", "--first"],
            &["allow", "wlan0", "192.168.1.0/24", "47800", "--second"],
            &["allow", "wlan0", "192.168.1.0/24", "47800", "--first", "x"],
            // interface
            &["allow", "eth9", "192.168.1.0/24", "47800"],
            &["allow", "", "192.168.1.0/24", "47800"],
            &["allow", "..", "192.168.1.0/24", "47800"],
            &["allow", ".", "192.168.1.0/24", "47800"],
            &["allow", "wlan0 ", "192.168.1.0/24", "47800"],
            &["allow", "../wlan0", "192.168.1.0/24", "47800"],
            &["allow", "-wlan0", "192.168.1.0/24", "47800"],
            &["allow", "abcdefghijklmnop", "192.168.1.0/24", "47800"],
            // network
            &["allow", "wlan0", "8.8.8.0/24", "47800"],
            &["allow", "wlan0", "0.0.0.0/0", "47800"],
            &["allow", "wlan0", "192.168.0.0/15", "47800"],
            &["allow", "wlan0", "192.0.0.0/8", "47800"],
            &["allow", "wlan0", "172.32.0.0/16", "47800"],
            &["allow", "wlan0", "172.16.0.0/11", "47800"],
            &["allow", "wlan0", "10.0.0.0/7", "47800"],
            &["allow", "wlan0", "192.168.1.5/24", "47800"],
            &["allow", "wlan0", "192.168.1.0", "47800"],
            &["allow", "wlan0", "192.168.1.0/", "47800"],
            &["allow", "wlan0", "192.168.1.0/+24", "47800"],
            &["allow", "wlan0", "192.168.1.0/024", "47800"],
            &["allow", "wlan0", "192.168.1.0/33", "47800"],
            &["allow", "wlan0", "192.168.1.0/24;ls", "47800"],
            &["allow", "wlan0", "::1/128", "47800"],
            &["allow", "wlan0", "192.168.001.0/24", "47800"],
            // port
            &["allow", "wlan0", "192.168.1.0/24", "1023"],
            &["allow", "wlan0", "192.168.1.0/24", "22"],
            &["allow", "wlan0", "192.168.1.0/24", "65536"],
            &["allow", "wlan0", "192.168.1.0/24", "+47800"],
            &["allow", "wlan0", "192.168.1.0/24", "47800:47900"],
            &["allow", "wlan0", "192.168.1.0/24", "47800,80"],
            &["allow", "wlan0", "192.168.1.0/24", ""],
            &["allow", "wlan0", "192.168.1.0/24", "0x1f90"],
        ];
        for case in cases {
            assert!(plan(&args(case), &env).is_err(), "accepted {case:?}");
        }
        // Edges that are fine.
        for case in
            [["allow", "wlan0", "192.168.1.7/32", "1024"], ["allow", "wlan0", "172.31.255.0/24", "65535"], ["allow", "wlan0", "10.1.0.0/16", "47800"]]
        {
            assert!(plan(&args(&case), &env).is_ok(), "refused {case:?}");
        }
    }

    #[test]
    fn refuses_unless_root_started_by_pkexec() {
        let dir = tempfile::tempdir().unwrap();
        let mut env = env(&dir, &["wlan0"]);
        let ok = args(&["allow", "wlan0", "192.168.1.0/24", "47800"]);
        assert!(plan(&ok, &env).is_ok());
        env.euid = Some(1000);
        assert!(plan(&ok, &env).is_err());
        env.euid = None;
        assert!(plan(&ok, &env).is_err());
        env.euid = Some(0);
        for uid in [None, Some(""), Some("root"), Some("1000;id")] {
            env.pkexec_uid = uid.map(String::from);
            assert!(plan(&ok, &env).is_err(), "accepted PKEXEC_UID {uid:?}");
        }
    }

    #[test]
    fn tail_keeps_the_end_on_a_char_boundary() {
        assert_eq!(tail("  ERROR: boom \n"), "ERROR: boom");
        let long = format!("{}{}", "é".repeat(2000), "end");
        let t = tail(&long);
        assert!(t.len() <= TAIL_CAP && t.ends_with("end"));
    }

    /// One test runs the stub ufw (a script just written; with parallel tests that fork, the
    /// write could race an exec, so nothing else here spawns): ufw's failure comes back as exit 3
    /// with its message, success records the exact argv.
    #[test]
    fn runs_ufw_and_passes_its_failure_through() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let env = env(&dir, &["wlan0"]);
        let log = dir.path().join("argv");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nif [ -f '{}' ]; then echo 'ERROR: Bad port' >&2; exit 1; fi\n",
            log.display(),
            dir.path().join("fail").display()
        );
        std::fs::write(&env.ufw, script).unwrap();
        std::fs::set_permissions(&env.ufw, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ufw = plan(&args(&["allow", "wlan0", "192.168.1.0/24", "47800"]), &env).unwrap();
        assert!(execute(&env, &ufw).is_ok());
        let recorded: Vec<String> = std::fs::read_to_string(&log).unwrap().lines().map(String::from).collect();
        assert_eq!(recorded.last().map(String::as_str), Some("omatuner cast"));
        assert_eq!(recorded.len(), 14);
        std::fs::write(dir.path().join("fail"), "").unwrap();
        match execute(&env, &ufw) {
            Err(Failure::Ufw(msg)) => assert_eq!(msg, "ERROR: Bad port"),
            _ => panic!("ufw's failure was not reported"),
        }
    }
}
