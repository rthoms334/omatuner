//! `tuner-firewall`: the casting firewall helper (DESIGN §40 §9.4), the only code that ever runs
//! as root. Started by Tuner through `pkexec --disable-internal-agent /usr/lib/tuner/tuner-firewall
//! allow|remove <iface> <cidr> <port> [--first]` under the polkit action
//! `io.github.rthoms334.omatuner.firewall`. Std only, no dependencies. OWNER: CAST-7.
//!
//! Validates everything before acting (verb; interface `^[A-Za-z0-9_.-]{1,15}$` present in
//! `/sys/class/net`; an RFC 1918 IPv4 CIDR, /8–/32, host bits zero; port 1024–65535; euid 0 with
//! `PKEXEC_UID` set), then runs exactly one fixed `/usr/bin/ufw` command. Exit codes: 0 done,
//! 2 bad arguments, 3 ufw failed (its stderr tail passed through). Never 126/127 (pkexec's).

use std::process::ExitCode;

/// Bad arguments: nothing was run.
const EXIT_BAD_ARGS: u8 = 2;

fn main() -> ExitCode {
    // CAST-7: validate the arguments, then run the one fixed ufw command. Until then the helper
    // refuses everything (exit 2, no ufw call).
    eprintln!("tuner-firewall: not available in this build");
    ExitCode::from(EXIT_BAD_ARGS)
}
