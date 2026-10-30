//! DLNA against a fake MediaRenderer (DESIGN §40 Appendix E): the client in `cast::dlna` through
//! SetAVTransportURI + Play, the transport-state poll, positions, pause, volume, the end of an
//! item, a stop from the TV and another sender taking over. The fake is an axum SOAP server
//! answering AVTransport:1 and RenderingControl:1 as VLC's and go2tv's renderers expect; its
//! transport state is scripted by the test.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tuner::cast::CastErrorKind;
use tuner::cast::dlna::soap::{self, text_of};
use tuner::cast::dlna::{Client, Control};
use tuner::cast::receiver::{LoadReq, ReceiverEvent};
use tuner::redact::Secret;

const STEP: Duration = Duration::from_secs(10);
const URL: &str = "http://192.0.2.1:47800/c/abcdefghijklmnopqrstuvwxyz/stream.ts";

#[derive(Default)]
struct Renderer {
    /// `(SOAPACTION, body)` received, in order.
    calls: Vec<(String, String)>,
    state: &'static str,
    uri: String,
    rel_time: &'static str,
    volume: Option<u32>,
    /// SetAVTransportURI answers a fault (UPnP error 714: illegal MIME type).
    refuse: bool,
    /// Every request fails (the TV went away).
    dead: bool,
}

type Shared = Arc<Mutex<Renderer>>;

fn reply(action: &str, service: &str, fields: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:{action}Response xmlns:u="{service}">{fields}</u:{action}Response></s:Body></s:Envelope>"#
    )
}

async fn control(State(r): State<Shared>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    let action = headers.get("SOAPACTION").and_then(|v| v.to_str().ok()).unwrap_or_default().to_owned();
    let body = String::from_utf8_lossy(&body).into_owned();
    let mut r = r.lock().unwrap();
    if r.dead {
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    r.calls.push((action.clone(), body.clone()));
    let name = action.trim_matches('"').rsplit('#').next().unwrap_or_default().to_owned();
    let av = soap::AV_TRANSPORT;
    let out = match name.as_str() {
        "SetAVTransportURI" if r.refuse => {
            let fault = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><detail><UPnPError><errorCode>714</errorCode></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
            return (StatusCode::INTERNAL_SERVER_ERROR, fault.to_owned());
        }
        "SetAVTransportURI" => {
            r.uri = text_of(&body, "CurrentURI").unwrap_or_default();
            r.state = "STOPPED";
            reply(&name, av, "")
        }
        "Play" => {
            r.state = "TRANSITIONING";
            reply(&name, av, "")
        }
        "Pause" => {
            r.state = "PAUSED_PLAYBACK";
            reply(&name, av, "")
        }
        "Stop" => {
            r.state = "STOPPED";
            reply(&name, av, "")
        }
        "GetTransportInfo" => reply(
            &name,
            av,
            &format!(
                "<CurrentTransportState>{}</CurrentTransportState><CurrentTransportStatus>OK</CurrentTransportStatus><CurrentSpeed>1</CurrentSpeed>",
                r.state
            ),
        ),
        "GetPositionInfo" => reply(
            &name,
            av,
            &format!("<Track>1</Track><TrackDuration>0:00:00</TrackDuration><RelTime>{}</RelTime><AbsTime>NOT_IMPLEMENTED</AbsTime>", r.rel_time),
        ),
        "GetMediaInfo" => reply(&name, av, &format!("<NrTracks>1</NrTracks><CurrentURI>{}</CurrentURI>", quick_xml::escape::escape(r.uri.as_str()))),
        "SetVolume" => {
            r.volume = text_of(&body, "DesiredVolume").and_then(|v| v.parse().ok());
            reply(&name, soap::RENDERING_CONTROL, "")
        }
        _ => return (StatusCode::INTERNAL_SERVER_ERROR, String::new()),
    };
    (StatusCode::OK, out)
}

async fn renderer() -> (Shared, Control, SocketAddr) {
    let shared: Shared = Arc::new(Mutex::new(Renderer { state: "NO_MEDIA_PRESENT", rel_time: "0:00:00", ..Renderer::default() }));
    let app = axum::Router::new()
        .route("/AVTransport/control", axum::routing::post(control))
        .route("/RenderingControl/control", axum::routing::post(control))
        .with_state(shared.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let base = url::Url::parse(&format!("http://{addr}/")).unwrap();
    let control =
        Control { av_transport: base.join("AVTransport/control").unwrap(), rendering: Some(base.join("RenderingControl/control").unwrap()) };
    (shared, control, addr)
}

fn req(live: bool) -> LoadReq {
    LoadReq { url: Secret::new(URL.into()), live, offset_s: 0.0, fmp4: false }
}

fn set(r: &Shared, f: impl FnOnce(&mut Renderer)) {
    f(&mut r.lock().unwrap());
}

fn actions(r: &Shared) -> Vec<String> {
    r.lock().unwrap().calls.iter().map(|(a, _)| a.trim_matches('"').rsplit('#').next().unwrap_or_default().to_owned()).collect()
}

/// The next event that isn't a position report.
async fn next_state(c: &mut Client) -> ReceiverEvent {
    loop {
        match tokio::time::timeout(STEP, c.next_event()).await.expect("an event in time") {
            ReceiverEvent::Position { .. } => {}
            ev => return ev,
        }
    }
}

async fn playing() -> (Shared, Client) {
    let (r, control, addr) = renderer().await;
    let mut c = Client::connect(addr, control, Some("QE55Q60R".into())).await.expect("connects");
    c.load(&req(false)).await.expect("loads");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Loading);
    set(&r, |r| r.state = "PLAYING");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    (r, c)
}

/// Load is SetAVTransportURI (the URL, and a DIDL-Lite item whose `protocolInfo` names the
/// stream's MIME type, escaped as text) then Play at speed 1; the poll turns TRANSITIONING into
/// `Loading` and PLAYING into `Playing`; SOAPACTION is quoted.
#[tokio::test]
async fn load_and_play() {
    let (r, _c) = playing().await;
    let calls = r.lock().unwrap().calls.clone();
    let (action, body) = calls.iter().find(|(a, _)| a.contains("#SetAVTransportURI")).unwrap();
    assert_eq!(action, "\"urn:schemas-upnp-org:service:AVTransport:1#SetAVTransportURI\"");
    assert_eq!(text_of(body, "CurrentURI").as_deref(), Some(URL));
    let didl = text_of(body, "CurrentURIMetaData").unwrap();
    assert!(didl.starts_with("<DIDL-Lite") && didl.contains(&format!("<res protocolInfo=\"http-get:*:{}:", soap::STREAM_MIME)), "{didl}");
    let play = calls.iter().find(|(a, _)| a.ends_with("#Play\"")).unwrap();
    assert_eq!(text_of(&play.1, "Speed").as_deref(), Some("1"));
    assert_eq!(actions(&r)[..3], ["GetTransportInfo", "GetTransportInfo", "SetAVTransportURI"], "connect checks, load stops nothing idle");
}

/// Pause and resume; a renderer that says 0:00:00 for a stream gives way to the time played; a
/// real `RelTime` is taken as it is.
#[tokio::test]
async fn pause_resume_and_positions() {
    let (r, mut c) = playing().await;
    c.set_paused(true).await.unwrap();
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Paused);
    c.set_paused(false).await.unwrap();
    set(&r, |r| r.state = "PLAYING");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Playing);
    assert_eq!(c.position().await, Some(0.0), "0 so soon after the first picture is believed");
    set(&r, |r| r.rel_time = "0:10:00.5");
    assert_eq!(c.position().await, Some(600.5));
    set(&r, |r| r.rel_time = "NOT_IMPLEMENTED");
    let estimate = c.position().await.unwrap();
    assert!((0.0..5.0).contains(&estimate), "the time played: {estimate}");
}

/// A stop after something played: the item's end once the relay sent the whole stream, else a
/// stop from the TV; another URI playing is another sender.
#[tokio::test]
async fn the_end_a_stop_and_another_sender() {
    let (r, mut c) = playing().await;
    c.stream_ended();
    set(&r, |r| r.state = "STOPPED");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Ended);

    let (r, mut c) = playing().await;
    set(&r, |r| r.state = "STOPPED");
    assert_eq!(next_state(&mut c).await, ReceiverEvent::StoppedOnTv);

    let (r, mut c) = playing().await;
    set(&r, |r| r.uri = "http://192.168.1.9/other.mp4".into());
    assert_eq!(next_state(&mut c).await, ReceiverEvent::StoppedOnTv);
}

/// A refused item fails the load; a renderer that stops answering is lost; a replace stops what
/// plays first; volume goes to RenderingControl as 0–100; stop is Stop.
#[tokio::test]
async fn refusal_loss_replace_volume_stop() {
    let (r, control, addr) = renderer().await;
    set(&r, |r| r.refuse = true);
    let mut c = Client::connect(addr, control, None).await.unwrap();
    assert_eq!(c.load(&req(true)).await, Err(CastErrorKind::ReceiverFailed));

    let (r, mut c) = playing().await;
    c.replace(&req(true)).await.unwrap();
    let a = actions(&r);
    let tail: Vec<_> = a.iter().rev().take(3).rev().cloned().collect();
    assert_eq!(tail, ["Stop", "SetAVTransportURI", "Play"]);
    assert_eq!(c.set_volume(0.42).await, Some(()));
    assert_eq!(r.lock().unwrap().volume, Some(42));
    c.stop().await;
    assert_eq!(actions(&r).last().map(String::as_str), Some("Stop"));

    let (r, mut c) = playing().await;
    set(&r, |r| r.dead = true);
    assert_eq!(next_state(&mut c).await, ReceiverEvent::Lost(CastErrorKind::Unreachable));
}
