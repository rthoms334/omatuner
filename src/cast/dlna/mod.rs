//! The DLNA / UPnP AV client (DESIGN §40 §15.3, Phase 3): MediaRenderers (Samsung, LG, Sony,
//! Panasonic and Philips TVs, Kodi, AV receivers) found by SSDP ([`ssdp`]) and driven by SOAP
//! over reqwest ([`soap`]). A renderer rarely plays HLS, so the relay hands it one continuous
//! MPEG-TS stream (`/c/<tok>/stream.ts`: the TS segments ffmpeg makes, back to back) and the
//! pipeline is otherwise AirPlay's, with a DLNA profile (`relay::plan`).
//!
//! * **Load**: `Stop` (when something plays; errors ignored), `SetAVTransportURI` with a DIDL-Lite
//!   item, `Play`. A replace is the same: a renderer has no queue Tuner relies on.
//! * **Events**: position is never evented [DL corr. 5] and GENA needs a callback server, so the
//!   client polls `GetTransportInfo` every second: `TRANSITIONING` → `Loading` before the first
//!   picture, `PLAYING` → `Playing`, `PAUSED_PLAYBACK` → `Paused`; `STOPPED` / `NO_MEDIA_PRESENT`
//!   after something played → `Ended` when the relay's stream reached its end
//!   ([`Client::stream_ended`]), else `StoppedOnTv`. A renderer playing another URI (another
//!   sender) is `StoppedOnTv` too (`GetMediaInfo` every 5 s). Three polls in a row unanswered:
//!   `Lost(Unreachable)`.
//! * **Positions**: `GetPositionInfo`'s `RelTime` (films and catch-up, every 2 s); many renderers
//!   say `0:00:00` for a stream without a length, so a `RelTime` of 0 after 5 s of playing gives
//!   way to the time played since the first picture.
//! * **Volume**: RenderingControl `SetVolume` (0–100, channel Master), when the renderer has it.

pub mod description;
pub mod soap;
pub mod ssdp;

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;
use url::Url;

use super::CastErrorKind;
use super::receiver::{LoadReq, ReceiverEvent, ReceiverInfo};
use soap::{AV_TRANSPORT, RENDERING_CONTROL, Transport};

/// Where a renderer is controlled (from its description; crate only, like its address).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Control {
    pub av_transport: Url,
    pub rendering: Option<Url>,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest SOAP reply read.
const MAX_REPLY: usize = 64 * 1024;
const POLL: Duration = Duration::from_secs(1);
const POSITION_EVERY: u32 = 2;
const MEDIA_EVERY: u32 = 5;
/// Polls unanswered in a row before the renderer counts as gone.
const MISSES: u32 = 3;
/// A `RelTime` still 0 this long after the first picture: the estimate instead.
const ZERO_GRACE: Duration = Duration::from_secs(5);
/// After a `Play`, a `STOPPED` this soon is the renderer still taking the item, not a stop.
const SETTLE: Duration = Duration::from_secs(8);

/// The item the renderer has now.
#[derive(Debug, Clone)]
struct Item {
    uri: String,
    live: bool,
    state: Option<Transport>,
    played: bool,
    /// The relay served the whole stream (a stop now is the end of the item).
    complete: bool,
    started: Instant,
    /// Time played before the current run, and since when it runs (the position estimate).
    played_s: f64,
    since: Option<Instant>,
}

impl Item {
    fn estimate(&self) -> f64 {
        self.played_s + self.since.map_or(0.0, |t| t.elapsed().as_secs_f64())
    }
}

/// One renderer.
pub struct Client(Box<State>);

struct State {
    http: reqwest::Client,
    control: Control,
    info: ReceiverInfo,
    item: Option<Item>,
    pending: VecDeque<ReceiverEvent>,
    done: bool,
    next_poll: Option<Instant>,
    polls: u32,
    misses: u32,
}

impl Client {
    /// Check the renderer answers (`GetTransportInfo`).
    pub async fn connect(addr: SocketAddr, control: Control, model: Option<String>) -> Result<Self, CastErrorKind> {
        let _ = addr;
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| CastErrorKind::Unreachable)?;
        let s = State {
            http,
            control,
            info: ReceiverInfo { model, ..ReceiverInfo::default() },
            item: None,
            pending: VecDeque::new(),
            done: false,
            next_poll: None,
            polls: 0,
            misses: 0,
        };
        s.transport_state().await?;
        Ok(Self(Box::new(s)))
    }

    pub async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.load(req).await
    }

    pub async fn replace(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        self.0.load(req).await
    }

    pub async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        self.0.set_paused(paused).await
    }

    /// The renderer fetched part of the stream (nothing to do: positions are polled).
    pub fn fetched(&mut self, _from_s: f64, _upto_s: f64) {}

    /// The relay served the stream to its end: the next stop is the item's end.
    pub fn stream_ended(&mut self) {
        if let Some(i) = self.0.item.as_mut() {
            i.complete = true;
        }
    }

    pub async fn set_volume(&mut self, level: f64) -> Option<()> {
        self.0.set_volume(level).await
    }

    pub async fn position(&mut self) -> Option<f64> {
        self.0.position().await
    }

    /// Cancel-safe (the waits are the event queue and the poll timer; a poll cut short is simply
    /// not taken).
    pub async fn next_event(&mut self) -> ReceiverEvent {
        self.0.next_event().await
    }

    pub async fn stop(&mut self) {
        self.0.stop().await
    }

    pub fn info(&self) -> ReceiverInfo {
        self.0.info.clone()
    }
}

impl State {
    /// One SOAP action; the reply's body (`Err` on a fault, a non-2xx, no answer).
    async fn call(&self, service: &str, url: &Url, action: &str, args: &[(&str, &str)]) -> Result<String, CastErrorKind> {
        let body = soap::envelope(service, action, args);
        let resp = self
            .http
            .post(url.clone())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPACTION", soap::soap_action(service, action))
            .body(body)
            .send()
            .await
            .map_err(|_| CastErrorKind::Unreachable)?;
        let status = resp.status();
        let mut text = Vec::new();
        let mut resp = resp;
        while let Ok(Some(chunk)) = resp.chunk().await {
            if text.len() + chunk.len() > MAX_REPLY {
                return Err(CastErrorKind::ReceiverFailed);
            }
            text.extend_from_slice(&chunk);
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        if status.is_success() {
            Ok(text)
        } else {
            let code = soap::fault_code(&text).map_or_else(|| "?".to_owned(), |c| c.to_string());
            tracing::debug!("cast: DLNA {action} answered {} (UPnP error {code})", status.as_u16());
            Err(CastErrorKind::ReceiverFailed)
        }
    }

    async fn av(&self, action: &str, args: &[(&str, &str)]) -> Result<String, CastErrorKind> {
        let mut all = vec![("InstanceID", "0")];
        all.extend_from_slice(args);
        self.call(AV_TRANSPORT, &self.control.av_transport, action, &all).await
    }

    async fn transport_state(&self) -> Result<Transport, CastErrorKind> {
        let r = self.av("GetTransportInfo", &[]).await?;
        Ok(soap::text_of(&r, "CurrentTransportState").map_or(Transport::Other, |s| Transport::parse(&s)))
    }

    async fn load(&mut self, req: &LoadReq) -> Result<(), CastErrorKind> {
        let uri = req.url.expose().clone();
        if self.item.take().is_some() || matches!(self.transport_state().await, Ok(Transport::Playing | Transport::Paused)) {
            let _ = self.av("Stop", &[]).await;
        }
        self.pending.clear();
        let didl = soap::didl(&uri);
        if let Err(e) = self.av("SetAVTransportURI", &[("CurrentURI", &uri), ("CurrentURIMetaData", &didl)]).await {
            tracing::info!("cast: the DLNA TV refused the item (SetAVTransportURI)");
            return Err(if e == CastErrorKind::Unreachable { e } else { CastErrorKind::ReceiverFailed });
        }
        if let Err(e) = self.av("Play", &[("Speed", "1")]).await {
            tracing::info!("cast: the DLNA TV didn't start playing (Play)");
            return Err(e);
        }
        self.item =
            Some(Item { uri, live: req.live, state: None, played: false, complete: false, started: Instant::now(), played_s: 0.0, since: None });
        self.done = false;
        (self.polls, self.misses) = (0, 0);
        self.next_poll = Some(Instant::now() + POLL);
        Ok(())
    }

    async fn set_paused(&mut self, paused: bool) -> Result<(), CastErrorKind> {
        if paused {
            self.av("Pause", &[]).await?;
        } else {
            self.av("Play", &[("Speed", "1")]).await?;
        }
        if let Some(i) = self.item.as_mut() {
            if paused {
                i.played_s = i.estimate();
                i.since = None;
            } else if i.played && i.since.is_none() {
                i.since = Some(Instant::now());
            }
        }
        Ok(())
    }

    async fn set_volume(&mut self, level: f64) -> Option<()> {
        let url = self.control.rendering.clone()?;
        let v = ((level.clamp(0.0, 1.0) * 100.0).round() as u32).to_string();
        self.call(RENDERING_CONTROL, &url, "SetVolume", &[("InstanceID", "0"), ("Channel", "Master"), ("DesiredVolume", &v)]).await.ok().map(|_| ())
    }

    async fn position(&mut self) -> Option<f64> {
        let item = self.item.as_ref()?;
        let estimate = item.estimate();
        let since_play = item.since.map(|_| item.started.elapsed());
        let rel = match self.av("GetPositionInfo", &[]).await {
            Ok(r) => soap::text_of(&r, "RelTime").and_then(|t| soap::parse_time(&t)),
            Err(_) => None,
        };
        Some(match rel {
            Some(t) if t > 0.0 => t,
            // a renderer that keeps 0 (or can't say) for a stream without a length
            _ if since_play.is_some_and(|d| d >= ZERO_GRACE) || rel.is_none() => estimate,
            _ => 0.0,
        })
    }

    async fn next_event(&mut self) -> ReceiverEvent {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return ev;
            }
            match self.next_poll {
                Some(at) if !self.done => {
                    tokio::time::sleep_until(at).await;
                    self.poll().await;
                }
                _ => return std::future::pending().await,
            }
        }
    }

    async fn poll(&mut self) {
        self.next_poll = Some(Instant::now() + POLL);
        self.polls += 1;
        let state = match self.transport_state().await {
            Ok(s) => {
                self.misses = 0;
                s
            }
            Err(_) => {
                self.misses += 1;
                if self.misses >= MISSES {
                    tracing::info!("cast: the DLNA TV stopped answering");
                    self.finish(ReceiverEvent::Lost(CastErrorKind::Unreachable));
                }
                return;
            }
        };
        self.transport(state);
        let Some(item) = self.item.as_ref() else { return };
        let (played, live) = (item.played, item.live);
        if played && self.polls.is_multiple_of(MEDIA_EVERY) && !self.done {
            let uri = item.uri.clone();
            if let Ok(r) = self.av("GetMediaInfo", &[]).await
                && let Some(current) = soap::text_of(&r, "CurrentURI").filter(|u| !u.trim().is_empty())
                && current.trim() != uri
            {
                tracing::info!("cast: the DLNA TV plays something else now");
                self.finish(ReceiverEvent::StoppedOnTv);
                return;
            }
        }
        if played
            && !live
            && self.polls.is_multiple_of(POSITION_EVERY)
            && !self.done
            && let Some(pos) = self.position().await
        {
            self.pending.push_back(ReceiverEvent::Position { pos, dur: None });
        }
    }

    /// A transport state polled (repeats are dropped).
    fn transport(&mut self, state: Transport) {
        let Some(item) = self.item.as_mut() else { return };
        if item.state == Some(state) {
            return;
        }
        item.state = Some(state);
        match state {
            Transport::Transitioning if !item.played => self.pending.push_back(ReceiverEvent::Loading),
            Transport::Transitioning => self.pending.push_back(ReceiverEvent::Buffering(true)),
            Transport::Playing => {
                if item.since.is_none() {
                    item.since = Some(Instant::now());
                }
                item.played = true;
                self.pending.push_back(ReceiverEvent::Playing);
            }
            Transport::Paused if item.played => {
                item.played_s = item.estimate();
                item.since = None;
                self.pending.push_back(ReceiverEvent::Paused);
            }
            Transport::Stopped | Transport::NoMedia if item.played => {
                if item.complete && !item.live {
                    tracing::info!("cast: the DLNA TV played the item to its end");
                    let ev = ReceiverEvent::Ended;
                    item.since = None;
                    self.next_poll = None;
                    self.pending.push_back(ev);
                } else {
                    self.finish(ReceiverEvent::StoppedOnTv);
                }
            }
            Transport::Stopped | Transport::NoMedia if item.started.elapsed() >= SETTLE => {
                // it took the item and let it go without playing: the format, most likely
                tracing::info!("cast: the DLNA TV stopped before it played");
                self.finish(ReceiverEvent::Lost(CastErrorKind::ReceiverFailed));
            }
            _ => {}
        }
    }

    fn finish(&mut self, ev: ReceiverEvent) {
        self.done = true;
        self.next_poll = None;
        self.pending.push_back(ev);
    }

    async fn stop(&mut self) {
        self.next_poll = None;
        if self.item.take().is_some() && !self.done {
            let _ = self.av("Stop", &[]).await;
        }
        self.done = true;
    }
}
