//! Settings ▸ Cast (DESIGN §40 §3.6): casting on or off, the TVs, the network and the firewall,
//! how the picture and sound are made for the TV. OWNER: CAST-9.
//!
//! ```text
//! CASTING
//! Cast to TVs                                                        [ On  ]
//! Offer to set up new TVs                                            [ On  ]
//! TVS
//! 󰄙 Living Room      Apple TV · Paired · tvOS 26.6 · used 2 h ago · default   ⋯
//! 󰄙 Living Room TV   AirPlay TV · Not paired · experimental                   ⋯
//! ↻ Search again
//! Add by address     [ the TV's IPv4 address ]
//! + Add
//! NETWORK
//! Tuner serves the TV from this computer (wlan0), port 47800.
//! Firewall: Blocked (ufw)
//!  Allow on firewall ·  Show command · ↻ Check again          (Remove rule once allowed)
//! Port                                                             ‹ 47800 ›
//!  Test on TV…
//! PICTURE AND SOUND
//! Convert what the TV can't play             ‹ Automatically (uses the CPU) ›
//! Interlaced channels                        ‹ Deinterlace (Apple's rule) ›
//! Show experimental AirPlay TVs                                      [ On  ]
//! Keep this computer awake while casting                             [ On  ]
//! Live TV on the TV runs about 10–20 s behind the computer.
//! ```
//! The rows reuse `Row::Section`, `Row::Toggle`, `Row::Choice`, `Row::Item` + menu and
//! `Row::Act`. The device list is read from `CastHandle::devices()` every frame, and the page
//! keeps discovery running while it is open (`CastHandle::browse`, on demand only). A TV's
//! version comes from its TXT `osvers` or `/info` (§7.6): a sanitized device string, shown only
//! here. No address is shown: the TV is named by its name, this computer by its interface.
//! Enter (or `x`, the gear) on a TV opens its menu: Pair / Pair again (the PIN overlay), Test
//! on TV, Make default, Forget… (a destructive confirm).
//!
//! The firewall row shows the detected state (§9.4): **Allow on firewall** only for ufw with
//! `pkexec` when ufw blocks; **Remove rule** only when a rule tagged `omatuner cast` exists; a
//! rule for another network reads "Allowed on another network (eth0)". Allow and Remove start
//! the job and open the firewall card, which shows "Waiting for your password…" and the
//! outcome (ufw's message shows only there). **Show command** lists the commands to copy (this
//! computer's interface and subnet, never the TV's address).

use super::*;
use crate::cast::firewall::FirewallState;
use crate::cast::{CastHandle, Device, DeviceId, Pairing, ReceiverKind, TestState, Usability};
use crate::config::{CastConvert, CastInterlaced};
use crate::ui::overlays::cast_firewall::{allow_shows, command_lines, other_networks, state_text};
use crate::ui::overlays::cast_offer::{error_sentence, kind_label};

/// `Act::Feature(Feature::Cast, n)`.
const ACT_SEARCH: u16 = 0;
pub(super) const ACT_ADD: u16 = 1;
const ACT_ALLOW: u16 = 2;
const ACT_SHOW_COMMAND: u16 = 3;
const ACT_COPY_COMMAND: u16 = 4;
const ACT_REMOVE: u16 = 5;
const ACT_CHECK: u16 = 6;
const ACT_TEST: u16 = 7;

/// A TV's menu codes (`Action::Digit`; kept clear of the page's own `MENU_*` and the agents').
const MENU_PAIR: u8 = 50;
const MENU_TEST: u8 = 51;
const MENU_DEFAULT: u8 = 52;
const MENU_FORGET: u8 = 53;

/// The page's state, kept in `SettingsScreen`.
#[derive(Default)]
pub(super) struct State {
    /// "Add by address": the IPv4 address as typed.
    pub(super) address: String,
    /// Show command was pressed (the commands stay listed while the page is open).
    show_command: bool,
    /// Text to put on the clipboard in the next `ui`.
    copy: Option<String>,
    /// The TV Test on TV runs on (its state shows under the button).
    tested: Option<DeviceId>,
}

static TOGGLES: [CfgToggle; 2] = [
    CfgToggle { label: "Cast to TVs", get: |c| c.cast.enabled, set: |c, v| c.cast.enabled = v },
    CfgToggle { label: "Offer to set up new TVs", get: |c| c.cast.offer_setup, set: |c, v| c.cast.offer_setup = v },
];

static PICTURE: [CfgToggle; 2] = [
    CfgToggle { label: "Show experimental AirPlay TVs", get: |c| c.cast.experimental, set: |c, v| c.cast.experimental = v },
    CfgToggle { label: "Keep this computer awake while casting", get: |c| c.cast.keep_awake, set: |c, v| c.cast.keep_awake = v },
];

static CONVERT: CfgChoice = CfgChoice {
    label: "Convert what the TV can't play",
    options: &["Automatically (uses the CPU)", "Never"],
    get: |c| usize::from(c.cast.convert == CastConvert::Never),
    set: |c, i| c.cast.convert = [CastConvert::Auto, CastConvert::Never][i],
};

static INTERLACED: CfgChoice = CfgChoice {
    label: "Interlaced channels",
    options: &["Deinterlace (Apple's rule)", "Send as is"],
    get: |c| usize::from(c.cast.interlaced == CastInterlaced::AsIs),
    set: |c, i| c.cast.interlaced = [CastInterlaced::Auto, CastInterlaced::AsIs][i],
};

/// The one port the TV fetches from (the firewall opens exactly it; a change moves Tuner's
/// rule on the next Allow on firewall).
static PORT: CfgNumber = CfgNumber {
    label: "Port",
    unit: "",
    min: 1024.0,
    max: 65535.0,
    step: 1.0,
    get: |c| f64::from(c.cast.media_port),
    set: |c, v| c.cast.media_port = v.round().clamp(1024.0, 65535.0) as u16,
};

/// A TV row's id: the first 8 bytes of its `DeviceId` (stable while the list changes under an
/// open menu).
fn row_id(id: DeviceId) -> i64 {
    let b = id.as_bytes();
    i64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn device_of(cast: &CastHandle, id: i64) -> Option<Device> {
    cast.devices().into_iter().find(|d| row_id(d.id) == id)
}

fn is_default(cfg: &Config, d: &Device) -> bool {
    cfg.cast.default_device.as_deref() == Some(d.id.to_string().as_str())
}

/// The TV Test on TV uses: the default if it is paired, else the most recently used paired
/// one (`devices()` lists the most recent first).
fn test_device(cfg: &Config, devices: &[Device]) -> Option<DeviceId> {
    let ready = |d: &&Device| d.selectable() && matches!(d.pairing, Pairing::Paired(_) | Pairing::NotNeeded);
    devices.iter().filter(ready).find(|d| is_default(cfg, d)).or_else(|| devices.iter().find(ready)).map(|d| d.id)
}

/// "Apple TV · Paired · tvOS 26.6 · used 2 h ago · default".
fn detail(d: &Device, cfg: &Config, now: crate::model::Timestamp) -> String {
    let mut parts: Vec<String> = vec![kind_label(d.kind).to_owned()];
    match d.usability {
        Usability::Mac => parts.push("Mac · Can't cast to Macs".into()),
        Usability::HomeOnly => parts.push("Same Home only".into()),
        Usability::Usable => match d.pairing {
            Pairing::Paired(_) => parts.push("Paired".into()),
            Pairing::Required => parts.push("Not paired".into()),
            Pairing::NotNeeded => {}
        },
    }
    if let Some(v) = &d.os_version {
        parts.push(if d.kind == ReceiverKind::AppleTv { format!("tvOS {v}") } else { v.clone() });
    }
    if let Some(at) = d.last_used {
        parts.push(format!("used {}", super::super::live::age_label(now, at)));
    }
    if !d.available {
        parts.push("not found now".into());
    }
    if d.experimental {
        parts.push("experimental".into());
    }
    if is_default(cfg, d) {
        parts.push("default".into());
    }
    parts.join(" · ")
}

/// The Cast page's rows.
pub(super) fn rows(s: &SettingsScreen, ctx: &UiCtx<'_>, v: &mut Vec<Row>) {
    let cfg = ctx.config;
    let cast = ctx.services.agent.cast();
    v.push(Row::Section("Casting".into()));
    v.push(Row::Note(
        "Send what plays to a TV on your network (C in the player), and bring it back with Play here. Tuner fetches the stream and serves the TV itself: your playlists' addresses and logins never reach the TV.".into(),
        false,
    ));
    v.extend(TOGGLES.iter().map(Row::Toggle));
    if !cfg.cast.enabled {
        return;
    }

    let devices = cast.devices();
    v.push(Row::Section(if cast.searching() { "TVs · searching…".into() } else { "TVs".into() }));
    let shown = devices.iter().filter(|d| cfg.cast.experimental || !d.experimental);
    let casting = ctx.player.cast.as_ref().map(|c| c.device);
    for d in shown {
        v.push(Row::Item {
            item: Item::Feature(Feature::Cast, row_id(d.id)),
            icon: if casting == Some(d.id) { glyphs::CAST_CONNECTED } else { glyphs::CAST }.to_owned(),
            color: None,
            label: d.name.clone(),
            detail: Some(detail(d, cfg, ctx.now)),
            reorder: false,
        });
    }
    if devices.is_empty() && !cast.searching() {
        v.push(Row::Note("No TVs found. Is the TV on, and on the same network? Firewall help is under Network.".into(), false));
    }
    v.push(Row::Act { act: Act::Feature(Feature::Cast, ACT_SEARCH), icon: glyphs::REFRESH, label: "Search again".into(), danger: false });
    v.push(Row::Local { field: Local::CastAddress, label: "Add by address" });
    v.push(Row::Act { act: Act::Feature(Feature::Cast, ACT_ADD), icon: glyphs::ADD, label: "Add".into(), danger: false });

    v.push(Row::Section("Network".into()));
    let port = cfg.cast.media_port;
    let d = cast.firewall();
    let iface = d.as_ref().and_then(|d| d.net.as_ref()).map(|n| format!(" ({})", n.iface)).unwrap_or_default();
    v.push(Row::Note(format!("Tuner serves the TV from this computer{iface}, port {port}."), false));
    match &d {
        None => v.push(Row::Note("Firewall: checking…".into(), false)),
        Some(d) => {
            let blocked = matches!(d.state, FirewallState::Blocked { .. });
            v.push(Row::Note(format!("Firewall: {}", state_text(d.state)), blocked));
            for r in other_networks(d) {
                v.push(Row::Note(format!("Allowed on another network ({})", r.iface), false));
            }
            if let Some(old) = d.tagged.iter().map(|r| r.port).find(|p| *p != port) {
                v.push(Row::Note(format!("Tuner's firewall rule opens port {old}: Allow on firewall moves it to {port}."), false));
            }
            if allow_shows(d, false) || (d.ufw && d.pkexec && d.tagged.iter().any(|r| r.port != port)) {
                v.push(Row::Act {
                    act: Act::Feature(Feature::Cast, ACT_ALLOW),
                    icon: glyphs::CHECK,
                    label: "Allow on firewall".into(),
                    danger: false,
                });
            }
            let lines = command_lines(d, None);
            if blocked || matches!(d.state, FirewallState::Unknown { .. }) {
                if s.cast.show_command && !lines.is_empty() {
                    v.extend(lines.iter().map(|l| Row::Note(l.clone(), false)));
                    v.push(Row::Act {
                        act: Act::Feature(Feature::Cast, ACT_COPY_COMMAND),
                        icon: glyphs::COPY,
                        label: "Copy the command".into(),
                        danger: false,
                    });
                } else if !lines.is_empty() {
                    v.push(Row::Act {
                        act: Act::Feature(Feature::Cast, ACT_SHOW_COMMAND),
                        icon: glyphs::COPY,
                        label: "Show command".into(),
                        danger: false,
                    });
                }
            }
            if d.can_remove() {
                v.push(Row::Act { act: Act::Feature(Feature::Cast, ACT_REMOVE), icon: glyphs::CLOSE, label: "Remove rule".into(), danger: true });
            }
        }
    }
    if let crate::cast::FirewallJob::Waiting { .. } = cast.firewall_job() {
        v.push(Row::Note("Waiting for your password…".into(), false));
    }
    v.push(Row::Act { act: Act::Feature(Feature::Cast, ACT_CHECK), icon: glyphs::REFRESH, label: "Check again".into(), danger: false });
    v.push(Row::Number(&PORT));
    if test_device(cfg, &devices).is_some() {
        v.push(Row::Act { act: Act::Feature(Feature::Cast, ACT_TEST), icon: glyphs::CAST, label: "Test on TV…".into(), danger: false });
    }
    if let Some(tv) = s.cast.tested.and_then(|id| devices.iter().find(|d| d.id == id)) {
        match cast.test_state() {
            TestState::Running => v.push(Row::Note(format!("Playing Tuner's test card on {} for 15 seconds…", tv.name), false)),
            TestState::Passed => v.push(Row::Note(format!("{} played the test card: ready to cast.", tv.name), false)),
            TestState::Failed(kind) => v.push(Row::Note(error_sentence(kind, &tv.name, true, cfg.cast.convert == CastConvert::Never), true)),
            TestState::Idle => {}
        }
    }

    v.push(Row::Section("Picture and sound".into()));
    v.push(Row::Choice(&CONVERT));
    v.push(Row::Choice(&INTERLACED));
    v.extend(PICTURE.iter().map(Row::Toggle));
    v.push(Row::Note("Live TV on the TV runs about 10–20 s behind the computer.".into(), false));
}

/// Start Test on TV on `device`.
fn test(s: &mut SettingsScreen, cast: &CastHandle, device: DeviceId) {
    s.cast.tested = Some(device);
    cast.test_on_tv(device);
}

/// Its `Act::Feature(Feature::Cast, n)` buttons.
pub(super) fn act(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, n: u16) {
    let cast = ctx.services.agent.cast();
    match n {
        ACT_SEARCH => cast.browse(),
        ACT_ADD => match s.cast.address.trim().parse::<std::net::Ipv4Addr>() {
            Ok(ip) => {
                cast.add_by_address(ip);
                s.cast.address.clear();
                s.error = None;
            }
            Err(_) => s.error = Some((Local::CastAddress, "Enter the TV's IPv4 address (four numbers with dots)".into())),
        },
        // the card shows "Waiting for your password…" and the outcome
        ACT_ALLOW => {
            cast.firewall_allow();
            ctx.open(OverlayRequest::CastFirewall);
        }
        ACT_REMOVE => {
            cast.firewall_remove();
            ctx.open(OverlayRequest::CastFirewall);
        }
        ACT_SHOW_COMMAND => s.cast.show_command = true,
        ACT_COPY_COMMAND => {
            if let Some(d) = cast.firewall() {
                s.cast.copy = Some(command_lines(&d, None).join("\n"));
                ctx.toast(Toast::info("Copied the command"));
            }
        }
        ACT_CHECK => cast.firewall_check(),
        ACT_TEST => {
            if let Some(id) = test_device(ctx.config, &cast.devices()) {
                test(s, &cast, id);
            }
        }
        _ => {}
    }
}

/// Enter on a TV's row: its menu.
pub(super) fn item(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64) {
    s.item_menu(ctx, Item::Feature(Feature::Cast, id));
}

/// A TV's menu (title, items).
pub(super) fn menu(ctx: &UiCtx<'_>, id: i64) -> Option<(String, Vec<MenuItem>)> {
    let d = device_of(&ctx.services.agent.cast(), id)?;
    let mi = |icon: &'static str, label: &str, code: u8| MenuItem {
        icon,
        label: label.into(),
        action: Action::Digit(code),
        enabled: true,
        shortcut: None,
    };
    let mut items = Vec::new();
    if d.selectable() {
        match d.pairing {
            Pairing::Required => items.push(mi(glyphs::LOCK, "Pair", MENU_PAIR)),
            Pairing::Paired(_) => items.push(mi(glyphs::REFRESH, "Pair again", MENU_PAIR)),
            Pairing::NotNeeded => {}
        }
        if !matches!(d.pairing, Pairing::Required) {
            items.push(mi(glyphs::CAST, "Test on TV", MENU_TEST));
        }
        if !is_default(ctx.config, &d) {
            items.push(mi(glyphs::CHECK, "Make default", MENU_DEFAULT));
        }
    }
    items.push(mi(glyphs::CLOSE, "Forget…", MENU_FORGET));
    Some((d.name, items))
}

/// A choice in a TV's menu.
pub(super) fn menu_choice(s: &mut SettingsScreen, ctx: &mut UiCtx<'_>, id: i64, code: u8) {
    let cast = ctx.services.agent.cast();
    let Some(d) = device_of(&cast, id) else { return };
    match code {
        MENU_PAIR => ctx.open(OverlayRequest::CastPair { device: d.id, then_cast: false }),
        MENU_TEST => test(s, &cast, d.id),
        MENU_DEFAULT => {
            let hex = d.id.to_string();
            ctx.edit_config(move |c| c.cast.default_device = Some(hex));
        }
        MENU_FORGET => {
            let token = s.token(Pending::Feature(Feature::Cast, id));
            ctx.open(OverlayRequest::Confirm(ConfirmSpec {
                title: format!("Forget {}?", d.name),
                body: "Tuner forgets its pairing and what it learned about this TV's channels. It shows up again when it's found; casting to it needs pairing again.".into(),
                confirm_label: "Forget".into(),
                destructive: true,
                token,
            }));
        }
        _ => {}
    }
}

/// The Forget confirm answered Forget.
pub(super) fn confirmed(ctx: &mut UiCtx<'_>, id: i64) {
    let cast = ctx.services.agent.cast();
    if let Some(d) = device_of(&cast, id) {
        cast.forget(d.id);
        if is_default(ctx.config, &d) {
            ctx.edit_config(|c| c.cast.default_device = None);
        }
    }
}

/// The page became current: detect the firewall again (discovery runs from `frame`).
pub(super) fn enter(_s: &mut SettingsScreen, ctx: &UiCtx<'_>) {
    if ctx.config.cast.enabled {
        ctx.services.agent.cast().firewall_check();
    }
}

/// Every frame the page shows: keep discovery running, pick up "Add by address", hand a Copy
/// to the clipboard, and repaint while things change off the UI thread.
pub(super) fn frame(s: &mut SettingsScreen, ui: &egui::Ui, ctx: &mut UiCtx<'_>) {
    if let Some(text) = s.cast.copy.take() {
        ui.ctx().copy_text(text);
    }
    if s.page != SettingsPage::Cast || !ctx.config.cast.enabled {
        return;
    }
    let cast = ctx.services.agent.cast();
    cast.browse();
    match cast.take_add_outcome() {
        Some(Ok(id)) => {
            let name = cast.devices().into_iter().find(|d| d.id == id).map_or_else(|| "The TV".to_owned(), |d| d.name);
            ctx.toast(Toast::info(format!("Added {name}")));
        }
        Some(Err(kind)) => ctx.toast(Toast::error(error_sentence(kind, "The TV at that address", true, false))),
        None => {}
    }
    let busy = cast.searching() || matches!(cast.test_state(), TestState::Running) || cast.firewall().is_none();
    ui.ctx().request_repaint_after(std::time::Duration::from_millis(if busy { 250 } else { 1000 }));
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;

    use egui_kittest::kittest::Queryable;

    use super::*;
    use crate::cast::firewall::{Detection, LanNet, Rule};
    use crate::cast::testing::{FakeCall, FakeCast};
    use crate::cast::{PairVariant, Protocol};
    use crate::ui::screens::home::testkit::{harness, has_label, rig};

    fn detection(state: FirewallState, tagged: Vec<Rule>) -> Detection {
        Detection {
            state,
            ufw: true,
            pkexec: true,
            net: Some(LanNet { iface: "wlan0".into(), addr: Ipv4Addr::new(192, 168, 1, 88), prefix: 24 }),
            tagged,
            port: 47800,
        }
    }

    /// The TVs with their state lines and versions; the firewall row offers Allow while ufw
    /// blocks and Remove once Tuner's rule is there; no TV address anywhere on the page.
    #[test]
    fn tvs_firewall_and_versions_show_by_state() {
        let (t, one) = rig();
        let fake = Arc::new(FakeCast::default());
        let apple = DeviceId::derive("airplay", "apple");
        let hisense = DeviceId::derive("airplay", "hisense");
        fake.script(|sc| {
            let mut a = Device::new(apple, "Living Room", ReceiverKind::AppleTv, Protocol::AirPlay).at("192.168.1.5:7000".parse().unwrap());
            a.pairing = Pairing::Paired(PairVariant::Hkp3);
            a.os_version = Some("26.6 (23L773)".into());
            let h = Device::new(hisense, "Living Room TV", ReceiverKind::AirPlayTv, Protocol::AirPlay);
            sc.devices = vec![a, h];
            sc.firewall = Some(detection(FirewallState::Blocked { shadowed: false }, Vec::new()));
        });
        t.services().agent.set_cast(Some(CastHandle::new(fake.clone())));
        let mut h = harness(t, one, SettingsScreen::default(), egui::vec2(1600.0, 1400.0));
        h.state_mut().enter(Route::Settings(SettingsPage::Cast));
        h.run_steps(2);
        assert!(has_label(&h, "Apple TV · Paired · tvOS 26.6 (23L773)"));
        assert!(has_label(&h, "AirPlay TV · Not paired · experimental"));
        assert!(has_label(&h, "Firewall: Blocked (ufw)"));
        assert!(has_label(&h, "Tuner serves the TV from this computer (wlan0), port 47800."));
        assert!(has_label(&h, "Allow on firewall") && !has_label(&h, "Remove rule"));
        assert!(has_label(&h, "Test on TV…"));
        let calls = fake.take_calls();
        assert!(calls.contains(&FakeCall::FirewallCheck) && calls.contains(&FakeCall::Browse), "{calls:?}");
        {
            let app = h.state_mut();
            let rows = app.t.with_ctx(|ctx| app.s.rows(ctx));
            app.s.row = rows.iter().position(|r| matches!(r, Row::Act { label, .. } if label == "Allow on firewall")).unwrap();
            app.s.pane = Pane::Content;
            app.act(Action::Activate);
        }
        h.run_steps(2);
        assert_eq!(fake.take_calls().into_iter().filter(|c| *c != FakeCall::Browse).collect::<Vec<_>>(), [FakeCall::FirewallAllow]);
        assert!(
            h.state_mut().nav().iter().any(|n| matches!(n, crate::ui::NavRequest::Open(OverlayRequest::CastFirewall))),
            "the card shows how it goes"
        );

        let ours = Rule { iface: "wlan0".into(), net: Ipv4Addr::new(192, 168, 1, 0), prefix: 24, port: 47800 };
        let other = Rule { iface: "eth0".into(), net: Ipv4Addr::new(10, 0, 0, 0), prefix: 8, port: 47800 };
        fake.script(|sc| sc.firewall = Some(detection(FirewallState::Allowed { ours: true }, vec![ours, other])));
        h.run_steps(2);
        assert!(has_label(&h, "Firewall: Allowed (Tuner's rule)") && has_label(&h, "Allowed on another network (eth0)"));
        assert!(has_label(&h, "Remove rule") && !has_label(&h, "Allow on firewall") && !has_label(&h, "Show command"));
        for never in ["192.168.1.5", "://"] {
            assert!(h.query_all_by_label_contains(never).next().is_none(), "{never} painted");
        }
    }
}
