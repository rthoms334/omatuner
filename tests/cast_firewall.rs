//! Casting firewall (DESIGN §40 §9.4, §14.7): the `user.rules` parser on fixture files, the state
//! mapping through `detect` with an injected `systemctl`, both argv forms, and the Allow / Remove
//! runner against a fake `pkexec`. The helper's own validation is unit-tested in
//! `src/bin/tuner-firewall.rs`. Nothing here touches the real ufw, pkexec or systemctl.

use std::ffi::OsString;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tuner::cast::firewall::allow::run_with;
use tuner::cast::firewall::ufw::{self, Action, Flow, Proto, Verdict};
use tuner::cast::firewall::{
    AllowOutcome, Detection, FirewallEnv, FirewallState, LanNet, Rule, UnknownWhy, Verb, argv, assess, copy_text, detect, input_policy_blocks,
    lan_net, run,
};

const PORT: u16 = 47800;
const TV: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 5);

/// The hex of "omatuner cast", as ufw stores it.
const TAG_HEX: &str = "6f6d6174756e65722063617374";

/// The scripts these tests write are executed at once from several tests, which can race a write
/// against an exec (ETXTBSY); every test that writes or runs one holds this for its whole length.
static EXEC: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cast/ufw").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn wlan0() -> LanNet {
    LanNet { iface: "wlan0".into(), addr: Ipv4Addr::new(192, 168, 1, 20), prefix: 24 }
}

fn rule() -> Rule {
    wlan0().rule(PORT)
}

fn verdict(name: &str) -> Verdict {
    let net = wlan0();
    ufw::evaluate(&ufw::parse(&fixture(name)), &Flow { port: PORT, iface: &net.iface, local: net.addr, tv: TV })
}

// ---- the parser -----------------------------------------------------------------------------

#[test]
fn this_machines_rules_leave_the_port_closed() {
    let tuples = ufw::parse(&fixture("user.rules.machine"));
    assert_eq!(tuples.len(), 4);
    // LocalSend's two, then docker-dns's two (tagged with someone else's comment).
    assert_eq!(tuples[0].proto, Proto::Udp);
    assert_eq!(tuples[2].comment.as_deref(), Some("allow-docker-dns"));
    assert_eq!(tuples[2].ports, Some(vec![(53, 53)]));
    assert!(tuples.iter().all(|t| !t.is_ours()));
    assert_eq!(verdict("user.rules.machine"), Verdict::Unmatched);
    let (state, tagged) = assess(&fixture("user.rules.machine"), true, &wlan0(), TV, PORT);
    assert_eq!((state, tagged.len()), (FirewallState::Blocked { shadowed: false }, 0));
}

#[test]
fn a_tagged_rule_is_found_and_allows() {
    assert_eq!(verdict("tagged.rules"), Verdict::Allowed { ours: true });
    let (state, tagged) = assess(&fixture("tagged.rules"), true, &wlan0(), TV, PORT);
    assert_eq!(state, FirewallState::Allowed { ours: true });
    assert_eq!(tagged, vec![rule()]);
    assert_eq!(tagged[0].cidr(), "192.168.1.0/24");
}

#[test]
fn a_rule_for_another_interface_is_tagged_but_does_not_allow() {
    let (state, tagged) = assess(&fixture("tagged_other_net.rules"), true, &wlan0(), TV, PORT);
    assert_eq!(state, FirewallState::Blocked { shadowed: false });
    // Listed once although the file has it twice: Remove would otherwise run it twice.
    assert_eq!(tagged, vec![Rule { iface: "enp3s0".into(), net: Ipv4Addr::new(10, 0, 5, 0), prefix: 24, port: PORT }]);
}

#[test]
fn an_earlier_deny_shadows_the_allow() {
    let (state, tagged) = assess(&fixture("shadowed.rules"), true, &wlan0(), TV, PORT);
    assert_eq!(state, FirewallState::Blocked { shadowed: true });
    assert_eq!(tagged, vec![rule()]);
    // A `_log-all` reject for the TV's own address, on this interface, before the allow.
    assert_eq!(verdict("shadowed_by_reject_log.rules"), Verdict::Denied);
    let tuples = ufw::parse(&fixture("shadowed_by_reject_log.rules"));
    assert_eq!(tuples[1].action, Action::Reject);
    // The shadow is a policy-independent fact: ACCEPT policy doesn't open a denied port.
    let (state, _) = assess(&fixture("shadowed.rules"), false, &wlan0(), TV, PORT);
    assert_eq!(state, FirewallState::Blocked { shadowed: true });
}

#[test]
fn denies_that_do_not_match_the_traffic_do_not_shadow() {
    // Other interface, protocol, port, source net, destination address, source port, direction and
    // a route rule: each is skipped or not matched, so the final allow wins.
    assert_eq!(verdict("deny_elsewhere.rules"), Verdict::Allowed { ours: true });
}

#[test]
fn a_wide_allow_with_a_port_list_and_range_covers() {
    assert_eq!(verdict("covering.rules"), Verdict::Allowed { ours: false });
    let ports = ufw::parse(&fixture("covering.rules"))[0].ports.clone().unwrap();
    assert_eq!(ports, vec![(80, 80), (443, 443), (47790, 47810)]);
    // Not ours: Remove has nothing to offer.
    assert!(assess(&fixture("covering.rules"), true, &wlan0(), TV, PORT).1.is_empty());
}

#[test]
fn proto_any_and_logged_actions_are_read() {
    assert_eq!(verdict("any_proto.rules"), Verdict::Allowed { ours: false });
    let t = &ufw::parse(&fixture("any_proto.rules"))[0];
    assert_eq!((t.action, t.proto, t.ports.clone()), (Action::Allow, Proto::Any, None));
    assert_eq!(t.iface.as_deref(), Some("wlan0"));
}

#[test]
fn app_profile_rules_keep_their_direction_after_the_app_fields() {
    let tuples = ufw::parse(&fixture("apps.rules"));
    assert_eq!(tuples.len(), 3);
    assert_eq!((tuples[0].ports.clone(), tuples[0].iface.clone()), (Some(vec![(22, 22)]), None));
    // A tagged app rule on a port list is not in Tuner's shape, so Remove doesn't list it.
    assert_eq!((tuples[1].iface.as_deref(), tuples[1].is_ours()), (Some("wlan0"), true));
    assert!(ufw::tagged(&tuples).is_empty());
    assert_eq!(verdict("apps.rules"), Verdict::Allowed { ours: false });
}

#[test]
fn route_rules_are_ignored() {
    assert!(ufw::parse(&fixture("routes.rules")).is_empty());
}

#[test]
fn malformed_lines_and_non_hex_comments_do_not_break_the_file() {
    let tuples = ufw::parse(&fixture("odd.rules"));
    // Three survive: the two bad comments (read as no comment) and the one with trailing text.
    assert_eq!(tuples.len(), 3, "{tuples:?}");
    assert!(tuples.iter().all(|t| t.comment.is_none()), "{tuples:?}");
    assert!(ufw::tagged(&tuples).is_empty());
}

#[test]
fn a_tagged_rule_of_another_shape_is_not_offered_for_removal() {
    let tuples = ufw::parse(&fixture("tagged_wrong_shape.rules"));
    assert_eq!(tuples.len(), 5);
    assert!(tuples.iter().all(|t| t.is_ours()));
    assert!(ufw::tagged(&tuples).is_empty());
}

#[test]
fn the_input_policy_is_read_from_etc_default_ufw() {
    assert!(input_policy_blocks("# c\nDEFAULT_INPUT_POLICY=\"DROP\"\n"));
    assert!(input_policy_blocks("DEFAULT_INPUT_POLICY=\"REJECT\"\n"));
    assert!(input_policy_blocks("DEFAULT_OUTPUT_POLICY=\"ACCEPT\"\n"));
    assert!(!input_policy_blocks("DEFAULT_INPUT_POLICY=\"ACCEPT\"\nDEFAULT_OUTPUT_POLICY=\"DROP\"\n"));
    assert!(!input_policy_blocks("DEFAULT_INPUT_POLICY = accept\n"));
    // With ACCEPT and nothing matching, nothing blocks the port.
    let (state, _) = assess(&fixture("user.rules.machine"), false, &wlan0(), TV, PORT);
    assert_eq!(state, FirewallState::NoFirewall);
}

#[test]
fn the_lan_net_and_its_rule_are_scoped_to_the_subnet() {
    let net = LanNet { iface: "wlan0".into(), addr: Ipv4Addr::new(192, 168, 1, 20), prefix: 24 };
    assert_eq!(net.network(), Ipv4Addr::new(192, 168, 1, 0));
    assert_eq!(net.rule(PORT), Rule { iface: "wlan0".into(), net: Ipv4Addr::new(192, 168, 1, 0), prefix: 24, port: PORT });
    let lo = lan_net(Ipv4Addr::LOCALHOST).expect("the loopback has an address");
    assert_eq!((lo.iface.as_str(), lo.addr), ("lo", Ipv4Addr::LOCALHOST));
}

// ---- detection ------------------------------------------------------------------------------

/// A temp directory laid out like the machine: `etc/ufw/{ufw.conf,user.rules}`, `default_ufw`, a
/// `ufw` file, a `systemctl` that answers from `active.<unit>` files, and a fake `pkexec`.
struct Sandbox {
    dir: tempfile::TempDir,
    env: FirewallEnv,
}

impl Sandbox {
    fn new() -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("etc/ufw")).unwrap();
        std::fs::write(p.join("etc/ufw/ufw.conf"), "# c\nENABLED=yes\nLOGLEVEL=low\n").unwrap();
        std::fs::write(p.join("etc/ufw/user.rules"), "### RULES ###\n### END RULES ###\n").unwrap();
        std::fs::write(p.join("default_ufw"), "DEFAULT_INPUT_POLICY=\"DROP\"\n").unwrap();
        std::fs::write(p.join("ufw"), "").unwrap();
        script(&p.join("systemctl"), "#!/bin/sh\n[ -f \"$(dirname \"$0\")/active.$3\" ]\n");
        // Records its argv and pid, then behaves as `code`, `stderr`, `sleep`, `append` and
        // `strip` say; `append` / `strip` stand in for the helper editing user.rules.
        script(
            &p.join("pkexec"),
            "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$d/argv\"\necho $$ > \"$d/pid\"\n\
             [ -f \"$d/sleep\" ] && exec sleep 60\n\
             [ -f \"$d/append\" ] && cat \"$d/append\" >> \"$d/etc/ufw/user.rules\"\n\
             [ -f \"$d/strip\" ] && : > \"$d/etc/ufw/user.rules\"\n\
             [ -f \"$d/stderr\" ] && cat \"$d/stderr\" >&2\n\
             exit \"$(cat \"$d/code\" 2>/dev/null || echo 0)\"\n",
        );
        let env = FirewallEnv {
            etc_ufw: p.join("etc/ufw"),
            default_ufw: p.join("default_ufw"),
            ufw: p.join("ufw"),
            pkexec: p.join("pkexec"),
            systemctl: p.join("systemctl"),
            helper: p.join("tuner-firewall"),
            policy: p.join("the.policy"),
        };
        Sandbox { dir, env }
    }

    fn put(&self, name: &str, text: &str) {
        std::fs::write(self.dir.path().join(name), text).unwrap();
    }

    fn active(&self, unit: &str) {
        self.put(&format!("active.{unit}"), "");
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap()
    }

    /// Install the packaged helper and its policy (the files only have to exist).
    fn packaged(&mut self) {
        self.put("tuner-firewall", "");
        self.put("the.policy", "");
    }

    async fn detect(&self) -> Detection {
        detect(&self.env, PORT, Ipv4Addr::LOCALHOST).await
    }
}

fn script(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The line ufw writes for Tuner's rule on the loopback (where `detect` runs here).
fn lo_rule_line() -> String {
    format!("### tuple ### allow tcp {PORT} 0.0.0.0/0 any 127.0.0.0/8 in_lo comment={TAG_HEX}\n")
}

#[tokio::test]
async fn detect_maps_the_files_and_units_to_a_state() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();

    // Nothing active: no firewall, whatever the files say.
    let d = sb.detect().await;
    assert_eq!((d.state, d.ufw, d.pkexec), (FirewallState::NoFirewall, false, true));
    assert!(!d.can_allow() && !d.can_remove());

    // The unit is active but ENABLED=no.
    sb.active("ufw");
    sb.put("etc/ufw/ufw.conf", "ENABLED=no\n");
    let d = sb.detect().await;
    assert_eq!((d.state, d.ufw), (FirewallState::NoFirewall, false));

    // ENABLED=yes and the unit active: the default DROP policy blocks.
    sb.put("etc/ufw/ufw.conf", "ENABLED=yes\n");
    let d = sb.detect().await;
    assert_eq!((d.state, d.ufw), (FirewallState::Blocked { shadowed: false }, true));
    assert_eq!(d.net.as_ref().map(|n| n.iface.as_str()), Some("lo"));
    assert_eq!(d.port, PORT);
    assert!(d.can_allow());

    // ENABLED=yes but the unit is down: not in force.
    std::fs::remove_file(sb.dir.path().join("active.ufw")).unwrap();
    assert_eq!(sb.detect().await.state, FirewallState::NoFirewall);
    sb.active("ufw");

    // Policy ACCEPT with no matching rule: nothing blocks.
    sb.put("default_ufw", "DEFAULT_INPUT_POLICY=\"ACCEPT\"\n");
    assert_eq!(sb.detect().await.state, FirewallState::NoFirewall);
    sb.put("default_ufw", "DEFAULT_INPUT_POLICY=\"DROP\"\n");

    // Tuner's rule for this interface and subnet: allowed, ours, and removable.
    sb.put("etc/ufw/user.rules", &lo_rule_line());
    let d = sb.detect().await;
    assert_eq!(d.state, FirewallState::Allowed { ours: true });
    assert_eq!(d.tagged, vec![Rule { iface: "lo".into(), net: Ipv4Addr::new(127, 0, 0, 0), prefix: 8, port: PORT }]);
    assert!(d.can_remove() && !d.can_allow());

    // A deny ahead of it: blocked by an earlier rule.
    sb.put("etc/ufw/user.rules", &format!("### tuple ### deny tcp {PORT} 0.0.0.0/0 any 0.0.0.0/0 in\n{}", lo_rule_line()));
    let d = sb.detect().await;
    assert_eq!(d.state, FirewallState::Blocked { shadowed: true });
    assert!(d.can_allow() && d.can_remove());

    // No pkexec: nothing to run, copy only.
    std::fs::remove_file(sb.dir.path().join("pkexec")).unwrap();
    let d = sb.detect().await;
    assert!(!d.pkexec && !d.can_allow() && !d.can_remove());
}

#[tokio::test]
async fn detect_reports_what_it_cannot_tell() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();

    sb.active("firewalld");
    assert_eq!(sb.detect().await.state, FirewallState::Unknown { why: UnknownWhy::Firewalld });
    std::fs::remove_file(sb.dir.path().join("active.firewalld")).unwrap();

    sb.active("nftables");
    assert_eq!(sb.detect().await.state, FirewallState::Unknown { why: UnknownWhy::Nftables });
    std::fs::remove_file(sb.dir.path().join("active.nftables")).unwrap();

    // ufw is in force but its files can't be read.
    sb.active("ufw");
    std::fs::remove_file(sb.dir.path().join("etc/ufw/user.rules")).unwrap();
    let d = sb.detect().await;
    assert_eq!(d.state, FirewallState::Unknown { why: UnknownWhy::Unreadable });
    assert!(d.ufw && !d.can_allow());
    std::fs::remove_file(sb.dir.path().join("etc/ufw/ufw.conf")).unwrap();
    assert_eq!(sb.detect().await.state, FirewallState::Unknown { why: UnknownWhy::Unreadable });

    // ufw not installed: with an active unit it is still no ufw (a stale unit name).
    let sb = Sandbox::new();
    std::fs::remove_file(sb.dir.path().join("ufw")).unwrap();
    sb.active("ufw");
    assert_eq!(sb.detect().await.state, FirewallState::NoFirewall);
}

// ---- argv and the copy text -----------------------------------------------------------------

fn strings(argv: Vec<OsString>) -> Vec<String> {
    argv.into_iter().map(|a| a.into_string().unwrap()).collect()
}

#[test]
fn argv_is_the_helper_form_when_the_helper_and_its_policy_are_installed() {
    let mut sb = Sandbox::new();
    sb.packaged();
    let p = |s: &str| sb.dir.path().join(s).display().to_string();
    let head = [p("pkexec"), "--disable-internal-agent".into(), p("tuner-firewall")];
    let tail = |words: &[&str]| head.iter().cloned().chain(words.iter().map(|w| w.to_string())).collect::<Vec<_>>();
    assert_eq!(strings(argv(&sb.env, Verb::Allow { first: false }, &rule())), tail(&["allow", "wlan0", "192.168.1.0/24", "47800"]));
    assert_eq!(strings(argv(&sb.env, Verb::Allow { first: true }, &rule())), tail(&["allow", "wlan0", "192.168.1.0/24", "47800", "--first"]));
    assert_eq!(strings(argv(&sb.env, Verb::Remove, &rule())), tail(&["remove", "wlan0", "192.168.1.0/24", "47800"]));
}

#[test]
fn argv_is_the_ufw_form_from_a_checkout() {
    let sb = Sandbox::new();
    let ufw = sb.env.ufw.display().to_string();
    let a = strings(argv(&sb.env, Verb::Allow { first: false }, &rule()));
    assert_eq!(a[..3], [sb.env.pkexec.display().to_string(), "--disable-internal-agent".into(), ufw.clone()]);
    // The comment is one argument, with no quotes: there is no shell.
    assert_eq!(a[3..].join("|"), "allow|in|on|wlan0|from|192.168.1.0/24|to|any|port|47800|proto|tcp|comment|omatuner cast");
    let first = strings(argv(&sb.env, Verb::Allow { first: true }, &rule()));
    assert_eq!(first[3..5], ["prepend", "allow"]);
    let remove = strings(argv(&sb.env, Verb::Remove, &rule()));
    assert_eq!(remove[3..].join("|"), "delete|allow|in|on|wlan0|from|192.168.1.0/24|to|any|port|47800|proto|tcp");

    // The helper without its policy is a checkout too.
    let half = Sandbox::new();
    half.put("tuner-firewall", "");
    assert_eq!(strings(argv(&half.env, Verb::Remove, &rule()))[2], half.env.ufw.display().to_string());
}

#[test]
fn the_copy_text_has_this_computers_scope_and_never_the_tv() {
    let c = copy_text(&rule());
    assert_eq!(c.allow, "sudo ufw allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp comment 'omatuner cast'");
    assert_eq!(c.remove, "sudo ufw delete allow in on wlan0 from 192.168.1.0/24 to any port 47800 proto tcp");
    assert!(c.firewalld.contains("source address=192.168.1.0/24 port port=47800 protocol=tcp accept"));
    for text in [&c.allow, &c.remove, &c.firewalld] {
        assert!(!text.contains("192.168.1.5"));
    }
}

// ---- the runner -----------------------------------------------------------------------------

async fn outcome(sb: &Sandbox, verb: Verb) -> AllowOutcome {
    run(&sb.env, verb, &rule(), CancellationToken::new()).await
}

const ALLOW: Verb = Verb::Allow { first: false };

#[tokio::test]
async fn pkexec_exit_statuses_map_to_outcomes() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();

    sb.put("code", "126");
    assert_eq!(outcome(&sb, ALLOW).await, AllowOutcome::Cancelled);

    sb.put("code", "127");
    sb.put("stderr", "Error executing command as another user: No authentication agent found.\n");
    assert_eq!(outcome(&sb, ALLOW).await, AllowOutcome::NoAgent);
    sb.put("stderr", "Error executing command as another user: Not authorized\n\nThis incident has been reported.\n");
    assert_eq!(outcome(&sb, ALLOW).await, AllowOutcome::NotAuthorized);

    sb.put("code", "3");
    sb.put("stderr", "ERROR: Bad port '47800'\n");
    assert_eq!(outcome(&sb, ALLOW).await, AllowOutcome::Failed { message: "ERROR: Bad port '47800'".into() });
    sb.put("code", "2");
    sb.put("stderr", "tuner-firewall: bad network \"8.8.8.0/24\"\n");
    assert!(matches!(outcome(&sb, Verb::Remove).await, AllowOutcome::Failed { message } if message.contains("bad network")));
}

#[tokio::test]
async fn a_failure_message_is_redacted_and_capped_to_its_tail() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();
    sb.put("code", "3");
    sb.put("stderr", &format!("{}\nfetching https://user:hunter2@example.com/live/x failed\nTAIL-MARK\n", "x".repeat(10_000)));
    let AllowOutcome::Failed { message } = outcome(&sb, ALLOW).await else { panic!("not a failure") };
    assert!(message.len() <= 2048, "{}", message.len());
    assert!(message.ends_with("TAIL-MARK"));
    assert!(!message.contains("hunter2") && message.contains("***"), "{message}");
}

#[tokio::test]
async fn exit_zero_is_confirmed_by_the_rule_in_user_rules() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();
    let line = format!("### tuple ### allow tcp {PORT} 0.0.0.0/0 any 192.168.1.0/24 in_wlan0 comment={TAG_HEX}\n");

    // The helper added it: Added. The argv was the checkout form through pkexec's generic action.
    sb.put("append", &line);
    assert_eq!(outcome(&sb, ALLOW).await, AllowOutcome::Added);
    assert!(sb.read("argv").starts_with("--disable-internal-agent\n"));
    assert!(sb.read("argv").ends_with("comment\nomatuner cast\n"));

    // Exit 0 but the file shows nothing new (another network, or an unloaded ruleset): unverified.
    let sb2 = Sandbox::new();
    assert_eq!(outcome(&sb2, ALLOW).await, AllowOutcome::AddedUnverified);

    // Remove: the rule is gone afterwards, or still there.
    let sb3 = Sandbox::new();
    sb3.put("etc/ufw/user.rules", &line);
    assert_eq!(outcome(&sb3, Verb::Remove).await, AllowOutcome::AddedUnverified);
    sb3.put("strip", "");
    assert_eq!(outcome(&sb3, Verb::Remove).await, AllowOutcome::Added);

    // No readable user.rules after exit 0: the 10 s fetch check decides.
    std::fs::remove_file(sb3.dir.path().join("etc/ufw/user.rules")).unwrap();
    std::fs::remove_file(sb3.dir.path().join("strip")).unwrap();
    assert_eq!(outcome(&sb3, ALLOW).await, AllowOutcome::AddedUnverified);
}

#[tokio::test]
async fn the_packaged_form_is_what_pkexec_is_given() {
    let _g = EXEC.lock().await;
    let mut sb = Sandbox::new();
    sb.packaged();
    let _ = run(&sb.env, Verb::Allow { first: true }, &rule(), CancellationToken::new()).await;
    let helper = sb.dir.path().join("tuner-firewall").display().to_string();
    assert_eq!(sb.read("argv"), format!("--disable-internal-agent\n{helper}\nallow\nwlan0\n192.168.1.0/24\n47800\n--first\n"));
}

#[tokio::test]
async fn a_missing_pkexec_is_a_failure_not_a_panic() {
    let sb = Sandbox::new();
    std::fs::remove_file(sb.dir.path().join("pkexec")).unwrap();
    assert!(matches!(outcome(&sb, ALLOW).await, AllowOutcome::Failed { message } if message.contains("pkexec")));
}

/// Wait (to a deadline) for the fake pkexec to say it is running.
async fn running_pid(sb: &Sandbox) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(sb.dir.path().join("pid"))
            && let Ok(pid) = text.trim().parse()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "pkexec never started");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).exists()
}

#[tokio::test]
async fn cancel_kills_pkexec() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();
    sb.put("sleep", "");
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let (env, cancel) = (sb.env.clone(), cancel.clone());
        async move { run(&env, ALLOW, &rule(), cancel).await }
    });
    let pid = running_pid(&sb).await;
    assert!(alive(pid));
    cancel.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(10), task).await.expect("Cancel did not stop the runner").unwrap();
    assert_eq!(outcome, AllowOutcome::Cancelled);
    assert!(!alive(pid), "pkexec was left running");
}

/// The timeout outlasts the fake's start by seconds, and its pid is read before anything is
/// killed: no race between the shell writing its pid and the timeout.
#[tokio::test]
async fn the_timeout_kills_pkexec() {
    let _g = EXEC.lock().await;
    let sb = Sandbox::new();
    sb.put("sleep", "");
    let task = tokio::spawn({
        let env = sb.env.clone();
        async move { run_with(&env, ALLOW, &rule(), CancellationToken::new(), Duration::from_secs(3)).await }
    });
    let pid = running_pid(&sb).await;
    assert!(alive(pid));
    let outcome = tokio::time::timeout(Duration::from_secs(15), task).await.expect("the timeout did not stop the runner").unwrap();
    assert_eq!(outcome, AllowOutcome::TimedOut);
    assert!(!alive(pid), "pkexec was left running");
}
