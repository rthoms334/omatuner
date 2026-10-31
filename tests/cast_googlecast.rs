//! Google Cast against the fake device (DESIGN §40 Appendix E): the client in `cast::googlecast`
//! through TLS, LAUNCH, LOAD (and a replace on the same app), the TV's statuses, positions,
//! pause, volume and stop. The fake (`support/fake_googlecast.rs`) follows pychromecast's and
//! go-chromecast's message shapes; the real-device check is §18's manual checklist.

#[path = "support/fake_googlecast.rs"]
mod fake_googlecast;

use std::time::Duration;

use fake_googlecast::{FakeCast, SESSION, Script, TRANSPORT};
use tuner::cast::CastErrorKind;
use tuner::cast::googlecast::{Client, NS_CONNECTION, NS_MEDIA, NS_RECEIVER, PLATFORM};
use tuner::cast::receiver::{LoadReq, ReceiverEvent};
use tuner::redact::Secret;

const STEP: Duration = Duration::from_secs(10);

fn req(live: bool, fmp4: bool) -> LoadReq {
    LoadReq { url: Secret::new("http://192.0.2.1:47800/c/abcdefghijklmnopqrstuvwxyz/index.m3u8".into()), live, offset_s: 0.0, fmp4 }
}

async fn next(c: &mut Client) -> ReceiverEvent {
    tokio::time::timeout(STEP, c.next_event()).await.expect("an event in time")
}

/// The next event that isn't a position report.
async fn next_state(c: &mut Client) -> ReceiverEvent {
    loop {
        match next(c).await {
            ReceiverEvent::Position { .. } => {}
            ev => return ev,
        }
    }
}

async fn connected(fake: &FakeCast) -> Client {
    tokio::time::timeout(STEP, Client::connect(fake.addr(), Some("Chromecast".into()))).await.expect("in time").expect("connects")
}

/// Connect is CONNECT to the platform, LAUNCH of the Default Media Receiver (waiting through a
/// status that doesn't name it yet), CONNECT to its transport; LOAD names the HLS segment type and
/// the stream type; the answer's BUFFERING is `Loading`, the PLAYING after it `Playing`.
#[tokio::test]
async fn connect_launch_and_load() {
    let fake = FakeCast::start(Script { slow_launch: true, ..Script::default() }).await;
    let mut c = connected(&fake).await;
    assert_eq!(c.info().model.as_deref(), Some("Chromecast"));
    let log = fake.log();
    assert_eq!((log[0].namespace.as_str(), log[0].destination.as_str(), log[0].payload["type"].as_str()), (NS_CONNECTION, PLATFORM, Some("CONNECT")));
    assert_eq!(log[1].payload["appId"], "CC1AD845");

    c.load(&req(true, false)).await.expect("loads");
    let log = fake.log();
    let connected = log.iter().position(|r| r.namespace == NS_CONNECTION && r.destination == TRANSPORT);
    let load = log.iter().position(|r| r.payload["type"] == "LOAD");
    assert!(connected.is_some() && connected < load, "connected to the app before LOAD: {log:?}");
    let load = fake.log().into_iter().find(|r| r.payload["type"] == "LOAD").unwrap();
    assert_eq!(load.destination, TRANSPORT);
    assert_eq!(load.payload["sessionId"], SESSION);
    assert_eq!(load.payload["media"]["streamType"], "LIVE");
    assert_eq!(load.payload["media"]["hlsVideoSegmentFormat"], "mpeg2_ts");
    assert_eq!(load.payload["autoplay"], true);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
}

/// A replace is another LOAD on the same app: the old item's INTERRUPTED is ignored, the new one
/// starts from `Loading` again; fMP4 segments are named as such.
#[tokio::test]
async fn a_replace_is_another_load_on_the_same_app() {
    let fake = FakeCast::start(Script::default()).await;
    let mut c = connected(&fake).await;
    c.load(&req(true, false)).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    c.replace(&req(false, true)).await.unwrap();
    assert_eq!(fake.types(NS_RECEIVER).iter().filter(|t| *t == "LAUNCH").count(), 1, "the app stays up");
    let loads: Vec<_> = fake.log().into_iter().filter(|r| r.payload["type"] == "LOAD").collect();
    assert_eq!(loads[1].payload["media"]["hlsVideoSegmentFormat"], "fmp4");
    assert_eq!(loads[1].payload["media"]["streamType"], "BUFFERED");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading, "the INTERRUPTED of the old item doesn't stop the new one");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    assert_eq!(fake.media_session(), 2);
}

/// Films report the TV's own clock: asked now (GET_STATUS) and with every status; pause and
/// resume go to the media session; the end of the film is `Ended`.
#[tokio::test]
async fn positions_pause_and_the_end() {
    let fake = FakeCast::start(Script { position: 1234.5, ..Script::default() }).await;
    let mut c = connected(&fake).await;
    c.load(&req(false, false)).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    assert_eq!(c.position().await, Some(1234.5));
    c.set_paused(true).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Paused);
    c.set_paused(false).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    assert!(fake.types(NS_MEDIA).ends_with(&["PAUSE".to_owned(), "PLAY".to_owned()]));
    fake.media_status("PLAYING", None, 2000.0);
    // the pause's and the resume's answers carry positions of their own first
    while next(&mut c).await != (ReceiverEvent::Position { pos: 2000.0, dur: None }) {}
    fake.media_status("IDLE", Some("FINISHED"), 5400.0);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Ended);
}

/// Stopped from the TV (its remote's stop: CANCELLED; its Home or another app: the app gone;
/// the app's transport closing) is `StoppedOnTv` once something played.
#[tokio::test]
async fn stopped_on_the_tv() {
    for how in 0..3 {
        let fake = FakeCast::start(Script::default()).await;
        let mut c = connected(&fake).await;
        c.load(&req(true, false)).await.unwrap();
        assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
        assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
        match how {
            0 => fake.media_status("IDLE", Some("CANCELLED"), 0.0),
            1 => fake.app_gone(),
            _ => fake.close_transport(),
        }
        assert_eq!(next_state(&mut c).await, ReceiverEvent::StoppedOnTv, "case {how}");
    }
}

/// A TV that can't play the item: LOAD_FAILED fails the load (the session escalates); a player
/// ERROR before the first picture is the receiver failing it.
#[tokio::test]
async fn the_tv_failing_the_item() {
    let fake = FakeCast::start(Script { load_failed: true, ..Script::default() }).await;
    let mut c = connected(&fake).await;
    assert_eq!(c.load(&req(true, false)).await, Err(CastErrorKind::ReceiverFailed));

    let fake = FakeCast::start(Script { play_after_load: false, ..Script::default() }).await;
    let mut c = connected(&fake).await;
    c.load(&req(true, false)).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    fake.media_status("IDLE", Some("ERROR"), 0.0);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Lost(CastErrorKind::ReceiverFailed));
}

/// A refused LAUNCH is `Unsupported`; nothing listening is `Unreachable`; a connection cut
/// mid-session is `Lost(Unreachable)`, and the next load connects again.
#[tokio::test]
async fn connection_failures() {
    let fake = FakeCast::start(Script { launch_error: true, ..Script::default() }).await;
    assert_eq!(Client::connect(fake.addr(), None).await.err(), Some(CastErrorKind::Unsupported));
    let nobody = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    assert_eq!(Client::connect(nobody, None).await.err(), Some(CastErrorKind::Unreachable));

    let fake = FakeCast::start(Script::default()).await;
    let mut c = connected(&fake).await;
    c.load(&req(true, false)).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    fake.drop_connections();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Lost(CastErrorKind::Unreachable));
    c.load(&req(true, false)).await.expect("connects again");
    assert_eq!(fake.connections(), 2);
}

/// Volume is the receiver's SET_VOLUME (0–1); stop STOPs the app (the TV goes home).
#[tokio::test]
async fn volume_and_stop() {
    let fake = FakeCast::start(Script::default()).await;
    let mut c = connected(&fake).await;
    assert_eq!(c.set_volume(0.3).await, Some(()));
    assert_eq!(fake.volume(), Some(0.3));
    c.load(&req(true, false)).await.unwrap();
    c.stop().await;
    let stop = fake.log().into_iter().find(|r| r.namespace == NS_RECEIVER && r.payload["type"] == "STOP").expect("STOP");
    assert_eq!(stop.payload["sessionId"], SESSION);
}
