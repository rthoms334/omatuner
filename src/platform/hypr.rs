//! Hyprland helpers (best effort, never required). Owner: WP-08.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// Focus our own window. Omarchy 4 (Hyprland Lua config) first tries
/// `hyprctl dispatch 'hl.dsp.focus({ window = "address:<addr>" })'`, then falls back to the
/// legacy `hyprctl dispatch focuswindow address:<addr>` (same as `omarchy-launch-or-focus`).
/// Used by IPC handoff, MPRIS Raise and reminder "Watch"; Wayland clients cannot raise
/// themselves otherwise.
///
/// The window is found by OUR process id in `hyprctl -j clients` (not by class), so another
/// Tuner profile or a test process never focuses someone else's window; without a window
/// nothing is dispatched. Returns immediately: the `hyprctl` calls run on a short-lived thread
/// (callers include the UI thread), and overlapping requests collapse into one.
pub fn focus_self() {
    static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
    // tests (headless) never reach the live compositor
    if super::off_desktop() || !is_hyprland() || IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new().name("hypr-focus".into()).spawn(|| {
        if let Err(e) = focus_pid(std::process::id()) {
            tracing::debug!("focus_self: {e}");
        }
        IN_FLIGHT.store(false, Ordering::Release);
    });
    if let Err(e) = spawned {
        IN_FLIGHT.store(false, Ordering::Release);
        tracing::debug!("focus_self: {e}");
    }
}

/// True when running under Hyprland (`HYPRLAND_INSTANCE_SIGNATURE` set).
pub fn is_hyprland() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
}

fn focus_pid(pid: u32) -> Result<(), String> {
    let out = Command::new("hyprctl").args(["-j", "clients"]).output().map_err(|e| format!("hyprctl: {e}"))?;
    if !out.status.success() {
        return Err(format!("hyprctl clients exited with {}", out.status));
    }
    let Some(addr) = window_address_for_pid(&out.stdout, pid) else {
        return Ok(()); // no window yet (or headless): nothing to focus
    };
    let lua = format!("hl.dsp.focus({{ window = \"address:{addr}\" }})");
    if dispatch_ok(&[lua.as_str()]) {
        return Ok(());
    }
    let legacy = format!("address:{addr}");
    if dispatch_ok(&["focuswindow", legacy.as_str()]) { Ok(()) } else { Err(format!("could not focus {addr}")) }
}

/// `hyprctl dispatch …` succeeded: exit status 0 and the reply `ok` (Hyprland 0.56 exits
/// non-zero with an error text for unknown Lua dispatchers; older versions print the error
/// with status 0).
fn dispatch_ok(args: &[&str]) -> bool {
    Command::new("hyprctl")
        .arg("dispatch")
        .args(args)
        .output()
        .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok")
        .unwrap_or(false)
}

/// Address of the first mapped window of `pid` in `hyprctl -j clients` output.
fn window_address_for_pid(clients_json: &[u8], pid: u32) -> Option<String> {
    let clients: Vec<serde_json::Value> = serde_json::from_slice(clients_json).ok()?;
    clients
        .iter()
        .filter(|c| c.get("pid").and_then(|p| p.as_u64()) == Some(u64::from(pid)))
        .filter(|c| c.get("mapped").and_then(|m| m.as_bool()).unwrap_or(true))
        .find_map(|c| c.get("address").and_then(|a| a.as_str()))
        .filter(|a| a.starts_with("0x") && a[2..].chars().all(|ch| ch.is_ascii_hexdigit()))
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLIENTS: &str = r#"[
        {"address": "0xaaab06565860", "mapped": true, "class": "foot", "pid": 156146},
        {"address": "0xaaab07000000", "mapped": false, "class": "tuner", "pid": 4242},
        {"address": "0xaaab07400000", "mapped": true, "class": "tuner", "pid": 4242},
        {"address": "0xaaab07800000", "mapped": true, "class": "tuner", "pid": 777}
    ]"#;

    #[test]
    fn finds_our_mapped_window_by_pid_only() {
        assert_eq!(window_address_for_pid(CLIENTS.as_bytes(), 4242).as_deref(), Some("0xaaab07400000"));
        assert_eq!(window_address_for_pid(CLIENTS.as_bytes(), 777).as_deref(), Some("0xaaab07800000"));
        assert_eq!(window_address_for_pid(CLIENTS.as_bytes(), 1), None);
    }

    /// `packaging/hypr/tuner.lua` evaluated with stand-ins for Hyprland's `hl` table (and
    /// Omarchy's real `o.window` helper when installed): it must give both of Tuner's classes
    /// opacity 1 (opting out of `default-opacity`) and `render_unfocused`, must not force idle
    /// inhibition, and must map the pop-out launch's window (`crate::window_app_id`) floating,
    /// pinned and without initial focus — never the normal window. Rule names as Hyprland
    /// 0.56's Lua window rules spell them (`WINDOW_RULE_EFFECT_DESCS`: `float`, `pin`,
    /// `no_initial_focus`, like Omarchy's webcam overlay). Skipped without a `lua` interpreter.
    #[test]
    fn hyprland_snippet_rules() {
        let snippet = concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/hypr/tuner.lua");
        let helpers = "/usr/share/omarchy/default/hypr/helpers.lua";
        let script = format!(
            r#"
            hl = {{ window_rule = function(r) RULES[#RULES + 1] = r end, bind = function() end,
                   on = function() end, dsp = {{ exec_cmd = function(c) return c end }} }}
            RULES = {{}}
            local f = io.open("{helpers}", "r")
            if f then f:close(); dofile("{helpers}") else
              o = {{}}
              function o.window(match, rules) rules.match = {{ class = match }}; hl.window_rule(rules) end
            end
            dofile("{snippet}")
            for _, r in ipairs(RULES) do
              print(r.match.class, r.tag or "-", r.opacity or "-", tostring(r.render_unfocused), r.idle_inhibit or "-",
                    tostring(r.float), tostring(r.pin), tostring(r.no_initial_focus))
            end
            "#
        );
        let out = match Command::new("lua").arg("-e").arg(&script).output() {
            Ok(o) => o,
            Err(_) => {
                eprintln!("skipped: no lua interpreter");
                return;
            }
        };
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let rules = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<Vec<&str>> = rules.lines().map(|l| l.split('\t').collect()).collect();
        let both = "^(tuner|tuner-popout)$";
        let nil = "nil";
        assert_eq!(
            lines,
            [
                [both, "-default-opacity", "-", nil, "-", nil, nil, nil],
                [both, "-", "1 1", nil, "-", nil, nil, nil],
                [both, "-", "-", "true", "-", nil, nil, nil],
                ["^(tuner-popout)$", "-", "-", nil, "-", "true", "true", "true"],
            ],
            "{rules}"
        );
        let popout = crate::window_app_id(true, true);
        assert_eq!(crate::window_app_id(true, false), crate::APP_ID, "no pop-out (nor its rule) outside Hyprland");
        assert_eq!(crate::window_app_id(false, true), crate::APP_ID);
        for line in &lines {
            let class = regex::Regex::new(line[0]).unwrap();
            assert!(class.is_match(popout), "{line:?}");
            assert_eq!(class.is_match(crate::APP_ID), line[7] != "true", "the normal window keeps its focus: {line:?}");
        }
    }

    /// The shipped desktop entry passes `desktop-file-validate` (skipped when not installed).
    #[test]
    fn desktop_entry_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/packaging/tuner.desktop");
        match Command::new("desktop-file-validate").arg(path).output() {
            Ok(out) => assert!(
                out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ),
            Err(_) => eprintln!("skipped: desktop-file-validate not installed"),
        }
        let entry = std::fs::read_to_string(path).unwrap();
        assert!(entry.contains("\nStartupWMClass=tuner\n") && entry.contains("\nIcon=tuner\n") && entry.contains("\nExec=tuner %U\n"));
    }

    #[test]
    fn rejects_garbage_and_suspicious_addresses() {
        assert_eq!(window_address_for_pid(b"not json", 1), None);
        assert_eq!(window_address_for_pid(br#"{"pid": 1}"#, 1), None);
        let injected = r#"[{"address": "0x1\" }) hl.dsp.exit() --", "mapped": true, "pid": 9}]"#;
        assert_eq!(window_address_for_pid(injected.as_bytes(), 9), None);
    }
}
