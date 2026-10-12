//! AirPlay against the fake receiver (DESIGN §40 §14.4; CAST-3): the client in `cast::airplay`
//! (CAST-1's pairing, CAST-2's session) through pairing, pair-verify, the §7.3 session, the event
//! channel, state, replace, stop and the third-party profile. Every scenario runs on tvOS 26 and
//! on tvOS 27 (`v26::…`, `v27::…`): one code path, nothing branches on the version (§7.6).
//!
//! The fake (`support/fake_airplay.rs`) and the client are written by different items from the
//! design, so a misreading fails here instead of agreeing; the pyatv interop tests at the end
//! (ignored, `TUNER_CAST_PYATV`) anchor both sides against pyatv 0.18.0 itself (§14.3).
//!
//! The relay is stood in by a static server with a token URL (`/c/<tok>/index.m3u8`) and tiny
//! segments: the fake TV fetches, it never decodes. Waits are bounded polls, never fixed sleeps.
//! The firewall-blocked check (§14.4's 10 s watchdog → `Firewall` / `ReceiverFailed`) belongs to
//! the session orchestrator (`cast::session`) and is tested with [`fake_airplay::Fetch::Never`]
//! there.

#[path = "support/fake_airplay.rs"]
mod fake_airplay;
#[path = "support/static_http.rs"]
mod static_http;

use std::collections::BTreeSet;
use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use fake_airplay::{
    AllowAccess, By, Channel, Entry, FakeAirPlay, FakeConfig, Forge, Pairing, PlaybackInfo, Request, State, StreamHeader, Tvos, command_label,
    number, text,
};
use static_http::StaticServer;
use tuner::cast::airplay::bplist::Value;
use tuner::cast::airplay::{self, CLIENT_NAME, Client, PairingSession, Profile};
use tuner::cast::receiver::{LoadReq, ReceiverEvent};
use tuner::cast::{CastErrorKind, Credentials, Identity, PairError, PairVariant, PeerCredentials, Pin};
use tuner::redact::Secret;

/// The longest any one step may take (the client's own timeouts are 3 s connect, 5 s request,
/// 3 s for a `playbackInfo` answer).
const STEP: Duration = Duration::from_secs(20);

/// The OPACK `{"com.apple.ScreenCapture": true}` HKP 5 puts in M5 as TLV 0x12 (foxCast).
const SCREEN_CAPTURE_ACL: &[u8] = b"\xe1\x57com.apple.ScreenCapture\x01";

/// The type-130 `clientTypeUUID` ("UrlFling"; PR 2899, foxCast, LIVI, DiPlay).
const URL_FLING: &str = "A6B27562-B43A-4F2D-B75F-82391E250194";

macro_rules! on_tvos_26_and_27 {
    ($($name:ident),+ $(,)?) => {
        mod v26 {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $name() {
                    super::$name(super::Tvos::V26).await;
                }
            )+
        }
        mod v27 {
            $(
                #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
                async fn $name() {
                    super::$name(super::Tvos::V27).await;
                }
            )+
        }
    };
}

on_tvos_26_and_27!(
    pairs_with_hkp3,
    pairing_requests_carry_the_client_name_on_one_connection,
    ladders_to_hkp5,
    a_wrong_pin_then_the_new_code,
    backoff_is_reported,
    refused_on_both_variants,
    pin_start_453_is_success,
    pin_start_403_is_not_supported,
    m6_with_a_type_17_tlv_pairs,
    a_forged_m6_signature_is_refused,
    a_bad_m4_proof_is_refused,
    a_changed_tv_key_is_pairing_lost,
    home_only_is_pairing_refused,
    probe_reads_the_receiver,
    the_session_sequence,
    skip_record_is_honoured,
    a_record_error_is_tolerated,
    feedback_never_overlaps_a_command,
    a_late_feedback_reply_is_dropped,
    every_event_is_acked,
    a_split_event_is_reassembled_and_acked_once,
    a_big_event_frame_is_read,
    a_bad_tag_is_lost,
    played_to_end_is_ended,
    stopped_from_the_remote,
    position_from_playback_info,
    position_estimate_when_playback_info_fails,
    replace_on_the_same_session,
    a_failed_replace_restarts_the_session,
    stop_closes_everything,
    third_party_profile,
);

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// This install's identity (`cast_identity`): a pairing UUID, an Ed25519 seed, a random locally
/// administered MAC.
fn identity() -> Identity {
    Identity {
        pairing_id: "3E1F2A4B-5C6D-4E7F-8A9B-0C1D2E3F4A5B".into(),
        seed: Secret::new([0x17; 32]),
        sender_mac: [0x02, 0x5e, 0x10, 0x20, 0x30, 0x40],
    }
}

/// `02:5E:10:20:30:40`.
fn mac_text(mac: &[u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, fut).await.unwrap_or_else(|_| panic!("{what} didn't finish within {STEP:?}"))
}

/// The relay's LAN server, stood in: a token URL serving a media playlist of 1-second segments.
/// `endlist`: a finished EVENT playlist (VOD); else it stays open like a live window.
struct Media {
    server: StaticServer,
    token: &'static str,
    endlist: bool,
}

impl Media {
    fn new(token: &'static str, segments: usize, endlist: bool) -> Self {
        let server = StaticServer::start();
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:0\n");
        if endlist {
            playlist.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        for i in 0..segments {
            playlist.push_str(&format!("#EXTINF:1.000,\nseg{i}.ts\n"));
            server.put(&format!("/c/{token}/seg{i}.ts"), vec![0x47u8; 188 * 8], "video/mp2t");
        }
        if endlist {
            playlist.push_str("#EXT-X-ENDLIST\n");
        }
        server.put(&format!("/c/{token}/index.m3u8"), playlist, "application/vnd.apple.mpegurl");
        Self { server, token, endlist }
    }

    fn url(&self) -> String {
        self.server.url(&format!("/c/{}/index.m3u8", self.token))
    }

    fn path(&self, file: &str) -> String {
        format!("/c/{}/{file}", self.token)
    }

    fn req(&self) -> LoadReq {
        LoadReq { url: Secret::new(self.url()), live: !self.endlist, offset_s: 0.0 }
    }
}

/// A TV paired with HKP 3 (no PIN dance) and a verified session on it.
async fn session(fake: &FakeAirPlay, profile: Profile) -> Client {
    let creds = fake.pre_pair(&identity(), PairVariant::Hkp3);
    match within("Client::connect", Client::connect(fake.addr(), &creds, profile)).await {
        Ok(client) => client,
        Err(e) => panic!("connect: {e:?}\n{}", fake.dump()),
    }
}

/// The next event `want` accepts; an unexpected end of the session fails the test.
async fn expect_event(fake: &FakeAirPlay, client: &mut Client, what: &str, want: impl Fn(&ReceiverEvent) -> bool) -> ReceiverEvent {
    let wait = async {
        loop {
            let ev = client.next_event().await;
            if want(&ev) {
                return ev;
            }
            if matches!(ev, ReceiverEvent::Lost(_) | ReceiverEvent::StoppedOnTv | ReceiverEvent::Ended) {
                panic!("waiting for {what}, the session ended: {ev:?}\n{}", fake.dump());
            }
        }
    };
    match tokio::time::timeout(STEP, wait).await {
        Ok(ev) => ev,
        Err(_) => panic!("no {what} within {STEP:?}\n{}", fake.dump()),
    }
}

/// Load `media` and wait for the first picture.
async fn play(fake: &FakeAirPlay, client: &mut Client, media: &Media) {
    if let Err(e) = within("load", client.load(&media.req())).await {
        panic!("load: {e:?}\n{}", fake.dump());
    }
    expect_event(fake, client, "Playing", |e| *e == ReceiverEvent::Playing).await;
}

async fn start_pairing(fake: &FakeAirPlay, variant: PairVariant) -> PairingSession {
    match within("pin-start", PairingSession::start(fake.addr(), &identity(), variant)).await {
        Ok(p) => p,
        Err(e) => panic!("pin-start: {e:?}\n{}", fake.dump()),
    }
}

/// The code the TV shows.
fn shown(fake: &FakeAirPlay) -> Pin {
    Pin::new(&fake.shown_pin().expect("a code on the TV")).expect("four digits")
}

/// §7.2's ladder over the client's API: HKP 3 first; a 470 there (at M2–M4) → HKP 5, once, with
/// the code the TV then shows. A session may ladder by itself (it asks for the HKP 5 code on the
/// same connection and reports `WrongPin` or `Refused` with `variant() == Hkp5`) or leave it to
/// the caller (`Refused` on HKP 3): both are followed.
async fn pair_with_ladder(fake: &FakeAirPlay) -> Result<PeerCredentials, PairError> {
    {
        // HKP 3's connection closes at the end of this block unless the session moved on itself.
        let mut pairing = start_pairing(fake, PairVariant::Hkp3).await;
        match within("pair-setup (HKP 3)", pairing.finish(&shown(fake))).await {
            Err(PairError::Refused { .. } | PairError::WrongPin) if pairing.variant() == PairVariant::Hkp5 => {
                return within("pair-setup (HKP 5)", pairing.finish(&shown(fake))).await;
            }
            Err(PairError::Refused { .. }) => {}
            other => return other,
        }
    }
    let mut next = start_pairing(fake, PairVariant::Hkp3.next()).await;
    within("pair-setup (HKP 5)", next.finish(&shown(fake))).await
}

fn pairing_requests(fake: &FakeAirPlay) -> Vec<Request> {
    fake.requests().into_iter().filter(|r| r.uri.starts_with("/pair-")).collect()
}

/// The steps (`Request::step`) of every request so far, minus feedback and position requests.
fn control_steps(fake: &FakeAirPlay) -> Vec<String> {
    fake.requests().iter().map(Request::step).filter(|s| s != "POST /feedback" && s != "/command playbackInfo").collect()
}

fn feedbacks(fake: &FakeAirPlay) -> usize {
    fake.with(|s| s.requests().filter(|r| r.uri == "/feedback").count())
}

/// The events the TV sent with this description, their CSeq and frames.
fn sent(fake: &FakeAirPlay, what: &str) -> Vec<(u32, Vec<usize>)> {
    fake.with(|s| {
        s.log
            .iter()
            .filter_map(|e| match e {
                Entry::EventSent { cseq, what: w, frames } if w == what => Some((*cseq, frames.clone())),
                _ => None,
            })
            .collect()
    })
}

/// How many times the sender acked the event with `cseq`.
fn acked(s: &State, cseq: u32) -> usize {
    s.log.iter().filter(|e| matches!(e, Entry::EventAcked { cseq: c } if *c == cseq)).count()
}

fn first_conn(fake: &FakeAirPlay) -> usize {
    fake.requests().first().map(|r| r.conn).expect("a request")
}

fn no_violations(fake: &FakeAirPlay) {
    let v = fake.violations();
    assert!(v.is_empty(), "the TV saw {v:?}\n{}", fake.dump());
}

/// Every string anywhere in a plist (for "never NTP").
fn strings(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => a.iter().for_each(|x| strings(x, out)),
        Value::Dict(d) => d.iter().for_each(|(k, x)| {
            out.push(k.clone());
            strings(x, out);
        }),
        _ => {}
    }
}

fn is_upper_uuid(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_digit() || ('A'..='F').contains(&c),
        })
}

// ---------------------------------------------------------------------------------------------
// Pairing (§7.2)
// ---------------------------------------------------------------------------------------------

/// HKP 3: the code on the TV, M1–M6 with the proof and signature checked; the client stores the
/// TV's id and key, and the TV trusts ours (M5 carries our pairing id and LTPK, no ACL).
async fn pairs_with_hkp3(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    assert_eq!(pairing.variant(), PairVariant::Hkp3);
    let peer = within("pair-setup", pairing.finish(&shown(&fake))).await.expect("paired");
    let (tv_id, tv_ltpk) = fake.accessory();
    assert_eq!(peer, PeerCredentials { peer_id: tv_id, peer_ltpk: tv_ltpk, variant: PairVariant::Hkp3 });

    let me = identity();
    let our_ltpk = fake_airplay::ed25519_public(me.seed.expose());
    assert!(
        fake.controllers().iter().any(|c| c.peer_id == me.pairing_id && c.peer_ltpk == our_ltpk),
        "the TV doesn't trust our key: {:?}",
        fake.controllers()
    );
    let m5 = fake.with(|s| s.m5.clone());
    assert_eq!(m5.len(), 1, "one M5");
    assert_eq!(m5[0].identifier, me.pairing_id.as_bytes());
    assert_eq!(m5[0].ltpk, our_ltpk);
    assert_eq!(m5[0].acl, None, "the ScreenCapture ACL is HKP 5's");
    no_violations(&fake);
}

/// PR 2894's regression and castcli's rule: `X-Apple-Client-Name` on pin-start and all three
/// pair-setup requests (tvOS 27 shows no code without it), HKP 3's headers, and everything on the
/// one connection that asked for the code (a new connection per request gets 470).
async fn pairing_requests_carry_the_client_name_on_one_connection(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    within("pair-setup", pairing.finish(&shown(&fake))).await.expect("paired");

    let reqs = pairing_requests(&fake);
    let uris: Vec<&str> = reqs.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(uris, ["/pair-pin-start", "/pair-setup", "/pair-setup", "/pair-setup"]);
    for r in &reqs {
        assert_eq!(r.header("X-Apple-Client-Name"), Some(CLIENT_NAME), "{} without the client name", r.uri);
        assert_eq!(r.header("X-Apple-HKP"), Some("3"), "{}", r.uri);
        assert_eq!(r.header("Content-Type"), Some("application/octet-stream"), "{}", r.uri);
        assert_eq!(r.header("User-Agent"), Some("AirPlay/320.20"), "{}", r.uri);
        assert_eq!(r.header("X-Apple-PD"), None, "{}: X-Apple-PD is pair-verify's (HKP 5)", r.uri);
        assert!(!r.encrypted);
    }
    assert!(reqs.iter().all(|r| r.conn == reqs[0].conn), "pin-start and pair-setup on one connection: {uris:?}");
}

/// The ladder: HKP 3 gets 470 at M3 → HKP 5 with the client id, `X-Apple-SupportedPINLengths: 4`
/// on pin-start and the ScreenCapture ACL in M5; the variant stored is HKP 5, and pair-verify then
/// sends HKP 5's headers with `X-Apple-PD: 1`.
async fn ladders_to_hkp5(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { pairing: Pairing::Hkp5Only, ..FakeConfig::new(tvos) });
    let peer = pair_with_ladder(&fake).await.unwrap_or_else(|e| panic!("HKP 5: {e:?}\n{}", fake.dump()));
    assert_eq!(peer.variant, PairVariant::Hkp5);

    let reqs = pairing_requests(&fake);
    assert!(
        reqs.iter().any(|r| r.uri == "/pair-setup" && r.header("X-Apple-HKP") == Some("3") && r.status == 470),
        "HKP 3 was tried first\n{}",
        fake.dump()
    );
    let hkp5: Vec<&Request> = reqs.iter().filter(|r| r.header("X-Apple-HKP") == Some("5")).collect();
    let uris: Vec<&str> = hkp5.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(uris, ["/pair-pin-start", "/pair-setup", "/pair-setup", "/pair-setup"]);
    let me = identity();
    for r in &hkp5 {
        assert_eq!(r.header("X-Apple-Client-Name"), Some(CLIENT_NAME), "{}", r.uri);
        assert_eq!(r.header("X-Apple-Client-ID"), Some(me.pairing_id.as_str()), "{}", r.uri);
        assert_eq!(r.header("Content-Type"), Some("application/octet-stream"), "{}", r.uri);
        assert_eq!(r.header("User-Agent"), Some("AirPlay/320.20"), "{}", r.uri);
    }
    assert_eq!(hkp5[0].header("X-Apple-SupportedPINLengths"), Some("4"));
    assert!(hkp5.iter().all(|r| r.conn == hkp5[0].conn), "HKP 5's pin-start and pair-setup on one connection");
    let m5 = fake.with(|s| s.m5.clone());
    let last = m5.last().expect("an M5");
    assert_eq!(last.hkp.as_deref(), Some("5"));
    assert_eq!(last.acl.as_deref(), Some(SCREEN_CAPTURE_ACL));

    let creds = Credentials { identity: me, peer };
    let connected = within("connect", Client::connect(fake.addr(), &creds, Profile::AppleTv)).await;
    assert!(connected.is_ok(), "verify with HKP 5: {:?}\n{}", connected.err(), fake.dump());
    let verify: Vec<Request> = fake.requests().into_iter().filter(|r| r.uri == "/pair-verify").collect();
    assert_eq!(verify.len(), 2, "pair-verify M1 and M3");
    for r in &verify {
        assert_eq!(r.header("X-Apple-HKP"), Some("5"));
        assert_eq!(r.header("X-Apple-PD"), Some("1"));
        assert_eq!(r.header("X-Apple-Client-Name"), Some(CLIENT_NAME));
    }
}

/// Error 2 at M4 is `WrongPin`; the TV shows a new code and the same session pairs with it, still
/// on one connection.
async fn a_wrong_pin_then_the_new_code(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    let first = fake.shown_pin().expect("a code");
    let wrong = Pin::new(if first == "9999" { "8888" } else { "9999" }).expect("digits");
    let result = within("pair-setup (wrong code)", pairing.finish(&wrong)).await;
    assert_eq!(result.err(), Some(PairError::WrongPin));
    let second = fake.shown_pin().expect("a new code");
    assert_ne!(first, second, "the TV shows a new code");

    let peer = within("pair-setup (new code)", pairing.finish(&shown(&fake))).await.expect("paired with the new code");
    assert_eq!(peer.peer_id, fake.accessory().0);
    let reqs = pairing_requests(&fake);
    assert!(reqs.iter().all(|r| r.conn == reqs[0].conn), "one connection throughout");
}

/// Error 3 with a BackOff (with HTTP 200, so it is checked first; PR 2920): `Backoff` with the
/// TV's seconds, not a hang.
async fn backoff_is_reported(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { pair_error: Some((3, Some(30))), ..FakeConfig::new(tvos) });
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    let result = within("pair-setup", pairing.finish(&shown(&fake))).await;
    assert_eq!(result.err(), Some(PairError::Backoff { secs: Some(30) }));
}

/// Allow Access refuses (470 on both variants): `Refused`, after HKP 3 and HKP 5 were each tried
/// once.
async fn refused_on_both_variants(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { allow_access: AllowAccess::Refuse470, ..FakeConfig::new(tvos) });
    let result = pair_with_ladder(&fake).await;
    assert!(matches!(result, Err(PairError::Refused { .. })), "{result:?}");
    let refused: Vec<String> =
        pairing_requests(&fake).iter().filter(|r| r.status == 470).map(|r| r.header("X-Apple-HKP").unwrap_or_default().to_owned()).collect();
    assert_eq!(refused, ["3", "5"], "one 470 per variant\n{}", fake.dump());
}

/// 453 on pin-start means the code is shown asynchronously: a success (foxCast).
async fn pin_start_453_is_success(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { pin_start_status: 453, ..FakeConfig::new(tvos) });
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    within("pair-setup", pairing.finish(&shown(&fake))).await.expect("paired after a 453");
}

/// 403 on pin-start is a Mac: `NotSupported`.
async fn pin_start_403_is_not_supported(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { pin_start_status: 403, ..FakeConfig::new(tvos) });
    let result = within("pin-start", PairingSession::start(fake.addr(), &identity(), PairVariant::Hkp3)).await;
    assert_eq!(result.err(), Some(PairError::NotSupported));
}

/// A 159-byte type-17 TLV in M6 (tvOS 26.6, send-airplay2 PR 8) is ignored; pairing succeeds.
async fn m6_with_a_type_17_tlv_pairs(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { m6_tlv17: true, ..FakeConfig::new(tvos) });
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    let peer = within("pair-setup", pairing.finish(&shown(&fake))).await.expect("type 17 ignored");
    assert_eq!(peer.peer_ltpk, fake.accessory().1);
}

/// An M6 signed with another key than the LTPK it carries: `Failed`, nothing to store.
async fn a_forged_m6_signature_is_refused(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { forge: Some(Forge::M6Signature), ..FakeConfig::new(tvos) });
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    let result = within("pair-setup", pairing.finish(&shown(&fake))).await;
    assert_eq!(result.err(), Some(PairError::Failed));
}

/// A server proof that doesn't verify at M4 (pyatv leaves the check as a TODO): `Failed`, and M5
/// — our identity — is never sent.
async fn a_bad_m4_proof_is_refused(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { forge: Some(Forge::M4Proof), ..FakeConfig::new(tvos) });
    let mut pairing = start_pairing(&fake, PairVariant::Hkp3).await;
    let result = within("pair-setup", pairing.finish(&shown(&fake))).await;
    assert_eq!(result.err(), Some(PairError::Failed));
    let setups = pairing_requests(&fake).iter().filter(|r| r.uri == "/pair-setup").count();
    assert_eq!(setups, 2, "M1 and M3 only\n{}", fake.dump());
    assert!(fake.with(|s| s.m5.is_empty()));
}

// ---------------------------------------------------------------------------------------------
// Pair-verify (§7.2)
// ---------------------------------------------------------------------------------------------

/// The TV was reset (a new long-term key): pair-verify's signature fails → `PairingLost`.
async fn a_changed_tv_key_is_pairing_lost(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let creds = fake.pre_pair(&identity(), PairVariant::Hkp3);
    fake.reset_accessory();
    let result = within("connect", Client::connect(fake.addr(), &creds, Profile::AppleTv)).await;
    assert_eq!(result.err(), Some(CastErrorKind::PairingLost));
}

/// "Everyone in Your Home" (tvOS 27.2, pyatv #2885): an empty 200 to pair-verify →
/// `PairingRefused`; and a 470 → `PairingRefused`.
async fn home_only_is_pairing_refused(tvos: Tvos) {
    for allow in [AllowAccess::HomeOnly, AllowAccess::Refuse470] {
        let fake = FakeAirPlay::start(FakeConfig { allow_access: allow, ..FakeConfig::new(tvos) });
        let creds = fake.pre_pair(&identity(), PairVariant::Hkp3);
        let result = within("connect", Client::connect(fake.addr(), &creds, Profile::AppleTv)).await;
        assert_eq!(result.err(), Some(CastErrorKind::PairingRefused), "{allow:?}");
    }
}

/// `GET /info` without pairing ("Add by address…" and the version line): the model, build and
/// AirPlay version of the tvOS the TV runs.
async fn probe_reads_the_receiver(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let info = within("probe", airplay::probe_info(fake.addr())).await.expect("/info");
    assert_eq!(info.model.as_deref(), Some("AppleTV14,1"));
    assert_eq!(info.os_build.as_deref(), Some(tvos.os_build()));
    assert_eq!(info.source_version.as_deref(), Some(tvos.source_version()));
    assert_eq!(info.psi.as_deref(), Some(fake_airplay::PSI));
}

// ---------------------------------------------------------------------------------------------
// The session (§7.3)
// ---------------------------------------------------------------------------------------------

/// §7.3 exactly: pair-verify, SETUP with declared PTP (and never NTP anywhere), the event
/// channel, RECORD, `GET /info`, the type-130 SETUP, then insert, `isInterestedInDateRange`,
/// `actionAtItemEnd 1` and `setRate 1`; feedback only after them. The item is foxCast's key set
/// pointing at the relay's token URL, `/command` carries exactly `X-Apple-StreamID`, and the TV
/// fetches like AVPlayer.
async fn the_session_sequence(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokA1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    let info = client.info();
    assert_eq!(info.model.as_deref(), Some("AppleTV14,1"));
    assert_eq!(info.source_version.as_deref(), Some(tvos.source_version()));
    assert_eq!(info.pairing, Some(PairVariant::Hkp3));
    if let Some(v) = &info.os_version {
        assert!(v.contains(tvos.os_build()), "{v}");
    }
    play(&fake, &mut client, &media).await;
    fake.until("the first /feedback", |s| s.requests().any(|r| r.uri == "/feedback").then_some(())).await;

    let steps: Vec<String> = fake
        .log()
        .iter()
        .filter_map(|e| match e {
            Entry::Request(r) => Some(r.step()),
            Entry::EventsConnected => Some("<event channel>".into()),
            _ => None,
        })
        .take_while(|s| s != "POST /feedback")
        .collect();
    assert_eq!(
        steps,
        [
            "POST /pair-verify",
            "POST /pair-verify",
            "SETUP",
            "<event channel>",
            "RECORD",
            "GET /info",
            "SETUP 130",
            "/command insertPlayQueueItem",
            "/command setProperty isInterestedInDateRange=true",
            "/command setProperty actionAtItemEnd=1",
            "/command setRate 1",
        ],
        "\n{}",
        fake.dump()
    );

    let reqs = fake.requests();
    let conn = reqs[0].conn;
    assert!(reqs.iter().all(|r| r.conn == conn), "one control connection");
    assert!(reqs.iter().skip(2).all(|r| r.encrypted), "encrypted right after pair-verify");

    // SETUP: declared PTP, no timing port, no NTP anywhere (§7.3 "Timing").
    let setup = reqs.iter().find(|r| r.step() == "SETUP").expect("SETUP");
    let body = setup.plist().expect("a plist body");
    let me = identity();
    assert_eq!(text(body, "timingProtocol"), Some("PTP"));
    assert!(body.get("timingPort").is_none(), "no timing port with PTP");
    let peer = body.get("timingPeerInfo").expect("timingPeerInfo");
    assert!(text(peer, "ID").is_some_and(is_upper_uuid), "{peer:?}");
    assert_eq!(peer.get("Addresses"), Some(&Value::Array(vec![Value::String("127.0.0.1".into())])));
    assert_eq!(number(peer, "DeviceType"), Some(0.0));
    assert_eq!(peer.get("SupportsClockPortMatchingOverride"), Some(&Value::Bool(true)));
    assert_eq!(body.get("timingPeerList"), Some(&Value::Array(vec![peer.clone()])));
    assert!(text(body, "sessionUUID").is_some() && text(body, "sessionCorrelationUUID").is_some());
    for key in ["updateSessionRequest", "statsCollectionEnabled", "isMultiSelectAirPlay"] {
        assert_eq!(body.get(key), Some(&Value::Bool(false)), "{key}");
    }
    for key in ["deviceID", "macAddress"] {
        assert!(text(body, key).is_some_and(|m| m.eq_ignore_ascii_case(&mac_text(&me.sender_mac))), "{key}: {body:?}");
    }
    for (key, want) in [
        ("name", "Tuner"),
        ("model", "iPhone14,3"),
        ("osName", "iPhone OS"),
        ("osVersion", "16.5"),
        ("osBuildVersion", "20F66"),
        ("sourceVersion", "690.7.1"),
    ] {
        assert_eq!(text(body, key), Some(want), "{key}");
    }
    let mut words = Vec::new();
    for r in &reqs {
        if let Some(p) = r.plist() {
            strings(p, &mut words);
        }
    }
    assert!(!words.iter().any(|w| w == "NTP" || w == "timingPort"), "never NTP");

    // The session URI: rtsp://<our LAN address>/<random u32>, the same for RECORD.
    let id = setup.uri.strip_prefix("rtsp://127.0.0.1/").expect("rtsp://<lan-ip>/<id>");
    assert!(id.parse::<u32>().is_ok(), "{}", setup.uri);
    let record = reqs.iter().find(|r| r.method == "RECORD").expect("RECORD");
    assert_eq!(record.uri, setup.uri);

    // RTSP requests carry pyatv's headers; their CSeq only grows.
    let rtsp: Vec<&Request> = reqs.iter().filter(|r| r.protocol == "RTSP/1.0").collect();
    assert!(rtsp.len() >= 4, "SETUP, RECORD, SETUP 130, feedback");
    let mut last = None;
    for r in &rtsp {
        let cseq: u64 = r.header("CSeq").and_then(|c| c.parse().ok()).unwrap_or_else(|| panic!("{}: CSeq", r.step()));
        assert!(last.is_none_or(|l| cseq > l), "CSeq goes up: {}", r.step());
        last = Some(cseq);
        for h in ["DACP-ID", "Active-Remote", "Client-Instance"] {
            assert!(r.header(h).is_some(), "{}: {h}", r.step());
        }
        assert_eq!(r.header("User-Agent"), Some("AirPlay/550.10"), "{}", r.step());
    }
    for r in reqs.iter().filter(|r| r.uri == "/info" || r.uri == "/command") {
        assert_eq!(r.protocol, "HTTP/1.1", "{}", r.step());
    }

    // The type-130 SETUP: the UrlFling remote-control stream on `<psi>-RCS-1`.
    let rcs = reqs.iter().find(|r| r.step() == "SETUP 130").and_then(Request::plist).expect("SETUP 130");
    let Some(Value::Array(streams)) = rcs.get("streams") else { panic!("streams: {rcs:?}") };
    assert_eq!(streams.len(), 1);
    assert_eq!(number(&streams[0], "controlType"), Some(1.0));
    assert_eq!(text(&streams[0], "channelID"), Some(format!("{}-RCS-1", fake_airplay::PSI).as_str()));
    assert!(text(&streams[0], "clientUUID").is_some_and(is_upper_uuid));
    assert_eq!(text(&streams[0], "clientTypeUUID"), Some(URL_FLING));

    // `/command`: Content-Type and X-Apple-StreamID exactly; no session headers.
    for r in reqs.iter().filter(|r| r.uri == "/command") {
        assert_eq!(r.header("Content-Type"), Some("application/x-apple-binary-plist"), "{}", r.step());
        assert_eq!(r.header("X-Apple-StreamID"), Some("1"), "{}", r.step());
        for h in ["X-Apple-Stream-ID", "X-Apple-ProtocolVersion", "X-Apple-Session-ID"] {
            assert!(r.header(h).is_none(), "{}: {h}", r.step());
        }
    }

    // The item: foxCast's key set, the relay's token URL, start position 0.
    let commands = fake.commands();
    let item = commands[0].get("item").expect("item");
    let Value::Dict(fields) = item else { panic!("{item:?}") };
    let keys: BTreeSet<&str> = fields.iter().map(|(k, _)| k.as_str()).collect();
    let want: BTreeSet<&str> = [
        "uuid",
        "Content-Location",
        "mediaType",
        "IsTLSEnabled",
        "playbackRestrictions",
        "referenceRestrictions",
        "supportsIntegratedTimeline",
        "snapTimeToPausePlayback",
        "clientBundleID",
        "clientProcName",
        "playerLoggingID",
        "playerItemLoggingID",
        "Start-Position",
    ]
    .into();
    assert_eq!(keys, want);
    let uuid = text(item, "uuid").expect("uuid");
    assert!(is_upper_uuid(uuid), "{uuid}");
    assert_eq!(text(item, "Content-Location"), Some(media.url().as_str()), "the relay's token URL, nothing else");
    assert_eq!(text(item, "mediaType"), Some("file"));
    for key in ["IsTLSEnabled", "supportsIntegratedTimeline", "snapTimeToPausePlayback"] {
        assert_eq!(item.get(key), Some(&Value::Bool(false)), "{key}");
    }
    assert_eq!(number(item, "playbackRestrictions"), Some(0.0));
    assert_eq!(number(item, "referenceRestrictions"), Some(2.0));
    assert_eq!(text(item, "clientBundleID"), Some("io.github.rthoms334.tuner"));
    assert_eq!(text(item, "clientProcName"), Some("Tuner"));
    assert_eq!(text(item, "playerLoggingID"), Some("TUNER"));
    assert_eq!(text(item, "playerItemLoggingID"), Some("I/TUNER.01"));
    let start = item.get("Start-Position").expect("Start-Position");
    for (key, want) in [("flags", 1.0), ("value", 0.0), ("epoch", 0.0), ("timescale", 1.0)] {
        assert_eq!(number(start, key), Some(want), "Start-Position.{key}");
    }
    let interest = &commands[1];
    assert_eq!(interest.get("item").and_then(|i| text(i, "uuid")), Some(uuid));

    // The TV fetched like AVPlayer: a 2-byte range on the playlist, the playlist, the segments.
    let fetches: Vec<_> = fake.with(|s| s.fetches().cloned().collect());
    assert_eq!(fetches[0].path, media.path("index.m3u8"));
    assert_eq!(fetches[0].range.as_deref(), Some("bytes=0-1"));
    assert_eq!(fetches[1].path, media.path("index.m3u8"));
    assert_eq!(fetches[2].path, media.path("seg0.ts"));
    assert!(fetches[0].user_agent.contains(tvos.os_build()));
    no_violations(&fake);
}

/// `skipRecord` in SETUP: no RECORD, and `updateInfo` still comes (after SETUP) and is acked.
async fn skip_record_is_honoured(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { skip_record: Some(true), ..FakeConfig::new(tvos) });
    let media = Media::new("tokS1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    assert!(!control_steps(&fake).iter().any(|s| s == "RECORD"), "RECORD skipped\n{}", fake.dump());
    let (cseq, _) = sent(&fake, "updateInfo").first().cloned().expect("updateInfo");
    fake.until("the updateInfo ack", |s| (acked(s, cseq) == 1).then_some(())).await;
    no_violations(&fake);
}

/// A RECORD error is logged, not fatal (§7.3 step 4).
async fn a_record_error_is_tolerated(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { record_status: 500, ..FakeConfig::new(tvos) });
    let media = Media::new("tokR1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    assert!(fake.requests().iter().any(|r| r.method == "RECORD" && r.status == 500));
}

/// One request in flight (PR 2899: an RTSP `/feedback` racing an HTTP `/command` drops the
/// connection): a `/command` answered slowly across a feedback tick makes feedback wait.
async fn feedback_never_overlaps_a_command(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokF1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    fake.until("the first /feedback", |s| s.requests().any(|r| r.uri == "/feedback").then_some(())).await;

    // Longer than the 2 s feedback interval.
    fake.set_command_delay(Duration::from_millis(2500));
    within("pause (slow)", client.set_paused(true)).await.expect("paused");
    fake.set_command_delay(Duration::ZERO);
    within("resume", client.set_paused(false)).await.expect("resumed");
    let after = feedbacks(&fake);
    fake.until("feedback going on", |s| (s.requests().filter(|r| r.uri == "/feedback").count() > after).then_some(())).await;

    no_violations(&fake);
    let conn = first_conn(&fake);
    assert!(!fake.with(|s| s.closed(Channel::Control(conn))));
    let labels: Vec<String> = fake.commands().iter().map(command_label).filter(|l| l != "playbackInfo").collect();
    assert!(labels.ends_with(&["setRate 0".into(), "setRate 1".into()]), "{labels:?}");
}

/// A `/feedback` answered late (pyatv #2893): the stray CSeq is dropped and the following replies
/// still match their requests; the session goes on.
async fn a_late_feedback_reply_is_dropped(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { late_feedback_reply: true, ..FakeConfig::new(tvos) });
    let media = Media::new("tokL1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    fake.until("the held /feedback", |s| s.requests().any(|r| r.uri == "/feedback").then_some(())).await;

    within("pause behind the held feedback", client.set_paused(true)).await.expect("paused");
    within("resume", client.set_paused(false)).await.expect("resumed");
    let after = feedbacks(&fake);
    fake.until("feedback going on", |s| (s.requests().filter(|r| r.uri == "/feedback").count() > after).then_some(())).await;
    no_violations(&fake);
    let conn = first_conn(&fake);
    assert!(!fake.with(|s| s.closed(Channel::Control(conn))), "\n{}", fake.dump());
}

// ---------------------------------------------------------------------------------------------
// The event channel (§7.3 step 3)
// ---------------------------------------------------------------------------------------------

/// Every event-channel request (`updateInfo`, states, notifications) is acked `RTSP/1.0 200 OK`
/// with its CSeq, so the TV never times the session out (`ack_timeout` 1 s here).
async fn every_event_is_acked(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokE1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    within("pause", client.set_paused(true)).await.expect("paused");
    expect_event(&fake, &mut client, "Paused", |e| *e == ReceiverEvent::Paused).await;

    fake.until("every event acked", |s| {
        let sent: Vec<u32> = s.log.iter().filter_map(|e| if let Entry::EventSent { cseq, .. } = e { Some(*cseq) } else { None }).collect();
        let all = sent.iter().all(|c| s.log.iter().any(|e| matches!(e, Entry::EventAcked { cseq } if cseq == c)));
        (sent.len() >= 5 && all).then_some(())
    })
    .await;
    assert!(!fake.log().iter().any(|e| matches!(e, Entry::AckTimeout { .. })));
    no_violations(&fake);
}

/// castcli #35 (AppleTV14,1, 26.6): an event request split 1024 + 1259 bytes across HAP frames is
/// reassembled up to its Content-Length and acked once.
async fn a_split_event_is_reassembled_and_acked_once(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { split_events: true, ..FakeConfig::new(tvos) });
    let media = Media::new("tokE2", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    let (cseq, frames) = sent(&fake, "updateInfo").first().cloned().expect("updateInfo");
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0], 1024);
    fake.until("the split request's ack", |s| (acked(s, cseq) > 0).then_some(())).await;
    // A second ack would come with the next frame's events; the first one after it settles it.
    let later = fake.with(|s| s.log.iter().filter(|e| matches!(e, Entry::EventSent { .. })).count());
    fake.until("the later events acked", |s| {
        let acked = s.log.iter().filter(|e| matches!(e, Entry::EventAcked { .. })).count();
        (acked >= later).then_some(())
    })
    .await;
    assert_eq!(fake.with(|s| acked(s, cseq)), 1, "acked once\n{}", fake.dump());
    no_violations(&fake);
}

/// One 8931-byte event frame (claimed for tvOS 27; unverified): read, since inbound frames may be
/// up to 65 535 bytes.
async fn a_big_event_frame_is_read(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig { big_event_frame: true, split_events: false, ..FakeConfig::new(tvos) });
    let media = Media::new("tokE3", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    let (cseq, frames) = sent(&fake, "updateInfo").first().cloned().expect("updateInfo");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert!(frames[0] >= 8931, "{frames:?}");
    fake.until("the big frame's ack", |s| (acked(s, cseq) == 1).then_some(())).await;
    no_violations(&fake);
}

/// A frame whose tag doesn't verify loses the session.
async fn a_bad_tag_is_lost(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokE4", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    fake.send_bad_frame();
    expect_event(&fake, &mut client, "Lost", |e| matches!(e, ReceiverEvent::Lost(_))).await;
}

// ---------------------------------------------------------------------------------------------
// State (§7.3 "Events", "Position")
// ---------------------------------------------------------------------------------------------

/// `itemPlayedToEnd` after the last segment of a finished playlist → `Ended` (then Up next).
async fn played_to_end_is_ended(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokV1", 3, true);
    let mut client = session(&fake, Profile::AppleTv).await;
    within("load", client.load(&media.req())).await.expect("load");
    let wait = async {
        loop {
            match client.next_event().await {
                ReceiverEvent::Ended => return,
                ev @ (ReceiverEvent::Lost(_) | ReceiverEvent::StoppedOnTv) => panic!("{ev:?}\n{}", fake.dump()),
                _ => {}
            }
        }
    };
    within("Ended", wait).await;
}

/// The remote's Menu: the TV closes the connection after `playing` and sends no `stopped` →
/// `StoppedOnTv` (foxCast `waitQueue`).
async fn stopped_from_the_remote(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokM1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    fake.stop_from_remote();
    let wait = async {
        loop {
            match client.next_event().await {
                ReceiverEvent::StoppedOnTv => return,
                ReceiverEvent::Lost(kind) => panic!("Lost({kind:?}) instead of StoppedOnTv"),
                _ => {}
            }
        }
    };
    within("StoppedOnTv", wait).await;
}

/// A `playbackInfo` request answered on the event channel gives the TV's position (PR 2846).
async fn position_from_playback_info(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokP1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    fake.set_position(42.5, Some(600.0));
    let pos = within("position", client.position()).await;
    assert_eq!(pos, Some(42.5), "\n{}", fake.dump());
    let ask = fake.commands().into_iter().find(|c| text(c, "type") == Some("playbackInfo")).expect("a playbackInfo request");
    assert_eq!(text(&ask, "kind"), Some("request"));
    assert!(number(&ask, "messageID").is_some());
}

/// A 500 or silence to `playbackInfo` → the estimate for the rest of the session: a position,
/// never past the media the TV fetched.
async fn position_estimate_when_playback_info_fails(tvos: Tvos) {
    for mode in [PlaybackInfo::Status500, PlaybackInfo::Silent] {
        let fake = FakeAirPlay::start(FakeConfig { playback_info: mode, ..FakeConfig::new(tvos) });
        let media = Media::new("tokP2", 3, false);
        let mut client = session(&fake, Profile::AppleTv).await;
        play(&fake, &mut client, &media).await;
        within("fetched", client.fetched(2.0)).await;
        let pos = within("position", client.position()).await.unwrap_or_else(|| panic!("{mode:?}: an estimate"));
        assert!((0.0..=2.0).contains(&pos), "{mode:?}: {pos} past what the TV fetched");
        let again = within("position again", client.position()).await.expect("still estimating");
        assert!((pos..=2.0).contains(&again), "{mode:?}: {again} after {pos}");
    }
}

// ---------------------------------------------------------------------------------------------
// Replace, stop (§7.3)
// ---------------------------------------------------------------------------------------------

/// VioletRelay's replace on the same session: `setRate 0`, `actionAtItemEnd 0`, insert after the
/// old item, remove it, `setRate 1`; `actionAtItemEnd 1` again only once the TV fetched the new
/// item. No new pair-verify.
async fn replace_on_the_same_session(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let (a, b) = (Media::new("tokA2", 3, false), Media::new("tokB2", 3, false));
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &a).await;
    let old = fake.with(|s| s.current_item().map(str::to_owned)).expect("an item playing");
    let before = fake.commands().len();

    within("replace", client.replace(&b.req())).await.expect("replaced");
    let cmds: Vec<Value> = fake.commands()[before..].to_vec();
    let labels: Vec<String> = cmds.iter().map(command_label).filter(|l| l != "playbackInfo").collect();
    assert_eq!(
        labels[..5],
        ["setRate 0", "setProperty actionAtItemEnd=0", "insertPlayQueueItem", "removePlayQueueItem", "setRate 1"],
        "\n{}",
        fake.dump()
    );
    let insert = cmds.iter().find(|c| text(c, "type") == Some("insertPlayQueueItem")).expect("insert");
    let item = insert.get("item").expect("item");
    assert_eq!(text(item, "Content-Location"), Some(b.url().as_str()));
    let new = text(item, "uuid").expect("uuid").to_owned();
    assert_ne!(new, old);
    assert_eq!(insert.get("itemAfter").and_then(|i| text(i, "uuid")), Some(old.as_str()));
    let remove = cmds.iter().find(|c| text(c, "type") == Some("removePlayQueueItem")).expect("remove");
    assert_eq!(remove.get("item").and_then(|i| text(i, "uuid")), Some(old.as_str()));

    let first = b.path("seg0.ts");
    fake.until("the new item's first segment", |s| s.fetches().any(|f| f.path == first && f.status == 200).then_some(())).await;
    let rearmed = |s: &State| s.requests().filter_map(Request::command).skip(before).map(command_label).any(|l| l == "setProperty actionAtItemEnd=1");
    assert!(!fake.with(rearmed), "actionAtItemEnd 1 before the TV fetched the new item");
    within("fetched", client.fetched(1.0)).await;
    fake.until("actionAtItemEnd 1 after the fetch", |s| rearmed(s).then_some(())).await;

    let reqs = fake.requests();
    assert!(reqs.iter().all(|r| r.conn == reqs[0].conn), "the session outlives the zap");
    assert_eq!(reqs.iter().filter(|r| r.uri == "/pair-verify").count(), 2, "no new pair-verify");
    no_violations(&fake);
}

/// Any error in the replace sequence → TEARDOWN and the whole start path again (pair-verify …
/// insert) with the new item; `replace` still succeeds.
async fn a_failed_replace_restarts_the_session(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let (a, b) = (Media::new("tokA3", 3, false), Media::new("tokB3", 3, false));
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &a).await;
    let first = first_conn(&fake);
    fake.fail_next_commands(1);

    within("replace", client.replace(&b.req())).await.unwrap_or_else(|e| panic!("replace: {e:?}\n{}", fake.dump()));
    expect_event(&fake, &mut client, "Playing (new session)", |e| *e == ReceiverEvent::Playing).await;

    let reqs = fake.requests();
    let teardown = reqs.iter().position(|r| r.conn == first && r.method == "TEARDOWN").expect("TEARDOWN of the old session");
    let second: Vec<String> = reqs[teardown..].iter().filter(|r| r.conn != first).map(Request::step).filter(|s| s != "POST /feedback").collect();
    assert!(second.len() >= 10, "{second:?}\n{}", fake.dump());
    assert_eq!(
        second[..10],
        [
            "POST /pair-verify",
            "POST /pair-verify",
            "SETUP",
            "RECORD",
            "GET /info",
            "SETUP 130",
            "/command insertPlayQueueItem",
            "/command setProperty isInterestedInDateRange=true",
            "/command setProperty actionAtItemEnd=1",
            "/command setRate 1",
        ],
        "\n{}",
        fake.dump()
    );
    let insert = reqs[teardown..]
        .iter()
        .filter(|r| r.conn != first)
        .find_map(|r| r.command().filter(|c| text(c, "type") == Some("insertPlayQueueItem")).cloned());
    let insert = insert.expect("insert on the new session");
    assert_eq!(insert.get("item").and_then(|i| text(i, "Content-Location")), Some(b.url().as_str()));
    let seg = b.path("seg0.ts");
    assert!(fake.with(|s| s.fetches().any(|f| f.path == seg)), "the TV plays the new item");
}

/// Stop: `setRate 0`, TEARDOWN, then the event channel and the control connection close.
async fn stop_closes_everything(tvos: Tvos) {
    let fake = FakeAirPlay::start(FakeConfig::new(tvos));
    let media = Media::new("tokX1", 3, false);
    let mut client = session(&fake, Profile::AppleTv).await;
    play(&fake, &mut client, &media).await;
    let conn = first_conn(&fake);
    within("stop", client.stop()).await;

    fake.until("the event channel closed", |s| s.closed(Channel::Events).then_some(())).await;
    fake.until("the control connection closed", |s| s.closed(Channel::Control(conn)).then_some(())).await;
    let steps = control_steps(&fake);
    assert_eq!(steps[steps.len() - 2..], ["/command setRate 0", "TEARDOWN"], "\n{}", fake.dump());
    let by_sender = fake.log().iter().any(|e| matches!(e, Entry::Closed { channel: Channel::Control(c), by: By::Sender } if *c == conn));
    assert!(by_sender, "Tuner closes the control connection");
    no_violations(&fake);
}

// ---------------------------------------------------------------------------------------------
// Third-party AirPlay TVs (§7.7)
// ---------------------------------------------------------------------------------------------

/// VioletRelay's profile: the type-120 SETUP before the type-130 one; the stream-id header
/// negotiated in the order `X-Apple-Stream-ID`, none, `X-Apple-StreamID` and kept; never the
/// session headers; `HLS-Content-Location`, `isAudioOnly`, `streamType 1`, a millisecond
/// Start-Position; `channelID` from our MAC (no `psi`).
async fn third_party_profile(tvos: Tvos) {
    let cfg = FakeConfig { profile: fake_airplay::Profile::ThirdParty, third_party_header: StreamHeader::Joined, ..FakeConfig::new(tvos) };
    let fake = FakeAirPlay::start(cfg);
    let media = Media::new("tokH1", 3, false);
    let mut client = session(&fake, Profile::ThirdParty).await;
    play(&fake, &mut client, &media).await;

    let steps = control_steps(&fake);
    let at = |s: &str| steps.iter().position(|x| x == s).unwrap_or_else(|| panic!("no {s}: {steps:?}"));
    assert!(at("SETUP 120") < at("SETUP 130"), "{steps:?}");

    let reqs = fake.requests();
    let commands: Vec<&Request> = reqs.iter().filter(|r| r.uri == "/command").collect();
    let spelling = |r: &Request| match (r.header("X-Apple-Stream-ID"), r.header("X-Apple-StreamID")) {
        (Some(_), None) => "hyphenated",
        (None, Some(_)) => "joined",
        (None, None) => "none",
        _ => "both",
    };
    let tried: Vec<(&str, u16)> = commands.iter().take(3).map(|r| (spelling(r), r.status)).collect();
    assert_eq!(tried, [("hyphenated", 400), ("none", 400), ("joined", 200)], "\n{}", fake.dump());
    assert!(commands[3..].iter().all(|r| spelling(r) == "joined"), "the spelling that worked is kept");
    for r in &reqs {
        assert!(r.header("X-Apple-ProtocolVersion").is_none() && r.header("X-Apple-Session-ID").is_none(), "{}", r.step());
    }

    let insert = fake.commands().into_iter().find(|c| text(c, "type") == Some("insertPlayQueueItem")).expect("insert");
    let item = insert.get("item").expect("item");
    assert_eq!(text(item, "Content-Location"), Some(media.url().as_str()));
    assert_eq!(text(item, "HLS-Content-Location"), Some(media.url().as_str()));
    assert_eq!(item.get("isAudioOnly"), Some(&Value::Bool(false)));
    assert_eq!(number(item, "streamType"), Some(1.0));
    let start = item.get("Start-Position").expect("Start-Position");
    assert_eq!(number(start, "timescale"), Some(1000.0));
    assert_eq!(number(start, "value"), Some(0.0));

    let setup = reqs.iter().find(|r| r.step() == "SETUP").and_then(Request::plist).expect("SETUP");
    let device = text(setup, "deviceID").expect("deviceID");
    let rcs = reqs.iter().find(|r| r.step() == "SETUP 130").and_then(Request::plist).expect("SETUP 130");
    let Some(Value::Array(streams)) = rcs.get("streams") else { panic!("{rcs:?}") };
    assert_eq!(text(&streams[0], "channelID"), Some(format!("{device}-RCS-1").as_str()));
    no_violations(&fake);
}

// ---------------------------------------------------------------------------------------------
// pyatv interop (dev only, §14.3 anchor 3): run once in CAST-3 and again at the gate.
// ---------------------------------------------------------------------------------------------

const INTEROP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/cast/pyatv_interop.py");

/// The venv's interpreter (`TUNER_CAST_PYATV=<venv>/bin/python` with pyatv 0.18.0), or `None`.
fn pyatv_python() -> Option<std::ffi::OsString> {
    let python = std::env::var_os("TUNER_CAST_PYATV");
    if python.is_none() {
        eprintln!("skipped: set TUNER_CAST_PYATV=<venv>/bin/python (pyatv 0.18.0, see {INTEROP})");
    }
    python
}

/// The next JSON line from the script whose `event` is `event`.
async fn next_json(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    what: &str,
    want: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    within(what, async {
        loop {
            let line = lines.next_line().await.expect("script output").unwrap_or_else(|| panic!("the script ended before {what}"));
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && want(&v)
            {
                return v;
            }
        }
    })
    .await
}

/// Our client against pyatv's own receiver (`AirPlayServerAuth`, PIN 1111): pin-start and
/// pair-setup on one connection, then pair-verify, after which pyatv reads our next request with
/// the control keys. (pyatv's receiver has no session routes: `connect` then fails at SETUP.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "dev-only: needs pyatv 0.18.0 in a venv, TUNER_CAST_PYATV=<venv>/bin/python"]
async fn interop_our_client_pairs_and_verifies_with_pyatv() {
    use tokio::io::AsyncBufReadExt;
    let Some(python) = pyatv_python() else { return };
    let mut child = tokio::process::Command::new(python)
        .arg("-I")
        .arg(INTEROP)
        .arg("server")
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("run the interop script");
    let mut lines = tokio::io::BufReader::new(child.stdout.take().expect("stdout")).lines();
    let ready = next_json(&mut lines, "ready", |v| v["event"] == "ready").await;
    let port = u16::try_from(ready["port"].as_u64().expect("port")).expect("a port");
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let pin = Pin::new(ready["pin"].as_str().expect("pin")).expect("four digits");

    let peer = {
        let mut pairing = match within("pin-start", PairingSession::start(addr, &identity(), PairVariant::Hkp3)).await {
            Ok(p) => p,
            Err(e) => panic!("pin-start against pyatv: {e:?}"),
        };
        within("pair-setup", pairing.finish(&pin)).await.expect("paired with pyatv")
    };
    assert_eq!(peer.peer_id, ready["identifier"].as_str().expect("identifier"));
    let creds = Credentials { identity: identity(), peer };
    let _ = within("connect", Client::connect(addr, &creds, Profile::AppleTv)).await;
    next_json(&mut lines, "verified", |v| v["event"] == "verified").await;
    let next = next_json(&mut lines, "an encrypted request", |v| v["event"] == "request" && v["encrypted"] == true).await;
    assert_eq!(next["method"], "SETUP", "{next}");
}

/// pyatv's client against our fake (tvOS 26 mode, PIN 1111): pair-setup and pair-verify succeed,
/// and `GET /info` over the encrypted connection is answered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "dev-only: needs pyatv 0.18.0 in a venv, TUNER_CAST_PYATV=<venv>/bin/python"]
async fn interop_pyatv_pairs_and_verifies_with_the_fake() {
    let Some(python) = pyatv_python() else { return };
    let fake = FakeAirPlay::start(FakeConfig::new(Tvos::V26));
    let port = fake.addr().port().to_string();
    let run =
        tokio::process::Command::new(python).arg("-I").arg(INTEROP).args(["client", "--port", &port, "--pin", "1111"]).kill_on_drop(true).output();
    let out = within("the pyatv client", run).await.expect("run the interop script");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}\n{}\n{}", String::from_utf8_lossy(&out.stderr), fake.dump());
    let done: serde_json::Value =
        stdout.lines().filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok()).find(|v| v["event"] == "done").expect("a done line");
    assert_eq!(done["model"], "AppleTV14,1", "{done}");
    assert_eq!(fake.controllers().len(), 1, "pyatv's controller is trusted");
    let info = fake.requests().into_iter().any(|r| r.uri == "/info" && r.encrypted && r.status == 200);
    assert!(info, "an encrypted GET /info\n{}", fake.dump());
    assert!(fake.violations().is_empty(), "{:?}", fake.violations());
}
