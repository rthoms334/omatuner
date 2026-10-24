//! UPnP AV control (DESIGN §40 §15.3, Phase 3): the SOAP envelopes of AVTransport:1 and
//! RenderingControl:1, the DIDL-Lite item a renderer is given, and the replies Tuner reads. Pure
//! (no I/O): `super::Client` posts them with reqwest. Every reply is device data: parsed by local
//! element name, capped, never trusted for more than the fields read here.

use quick_xml::escape::escape;
use quick_xml::events::Event;

pub const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
pub const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";

/// The MIME type of the relay's stream, in the DIDL `protocolInfo` and on the HTTP response (VLC's
/// and go2tv's for MPEG-TS; the two must match).
pub const STREAM_MIME: &str = "video/mpeg";
/// `contentFeatures.dlna.org` (and the 4th field of `protocolInfo`): no `DLNA.ORG_PN` (the
/// stream's profile varies with the source), no seeking (`OP=00`, Gerbera's for a resource
/// without a size), not converted by the server (`CI=0`), and `01700000…`: streaming transfer
/// mode, background and connection-stall allowed, DLNA 1.5.
pub const CONTENT_FEATURES: &str = "DLNA.ORG_OP=00;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// The `SOAPACTION` header of `action` on `service`.
pub fn soap_action(service: &str, action: &str) -> String {
    format!("\"{service}#{action}\"")
}

/// A SOAP request body: `args` are (name, value) in order, values escaped here.
pub fn envelope(service: &str, action: &str, args: &[(&str, &str)]) -> String {
    let mut body = String::with_capacity(400);
    body.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    body.push_str(
        r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/"><s:Body>"#,
    );
    body.push_str(&format!(r#"<u:{action} xmlns:u="{service}">"#));
    for (k, v) in args {
        body.push_str(&format!("<{k}>{}</{k}>", escape(*v)));
    }
    body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));
    body
}

/// The DIDL-Lite item for the relay's stream (escaped again by [`envelope`] as the
/// `CurrentURIMetaData` argument). Samsung and LG want an item with a title, a class and a `res`
/// whose `protocolInfo` matches the HTTP response's MIME type; the title is Tuner's own (no
/// programme text reaches the TV).
pub fn didl(url: &str) -> String {
    format!(
        concat!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" "#,
            r#"xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#,
            r#"<item id="f-0" parentID="0" restricted="0"><dc:title>Tuner</dc:title>"#,
            r#"<upnp:class>object.item.videoItem</upnp:class>"#,
            r#"<res protocolInfo="http-get:*:{mime}:{features}">{url}</res></item></DIDL-Lite>"#
        ),
        mime = STREAM_MIME,
        features = CONTENT_FEATURES,
        url = escape(url),
    )
}

/// The text of the first element named `name` (local name, any namespace), unescaped; `None`
/// when absent or the XML breaks first.
pub fn text_of(xml: &str, name: &str) -> Option<String> {
    let mut r = quick_xml::Reader::from_str(xml);
    let mut inside = false;
    let mut out = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) if e.local_name().as_ref() == name => inside = true,
            Ok(Event::Empty(e)) if e.local_name().as_ref() == name => return Some(String::new()),
            Ok(Event::Text(t)) if inside => out.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(g)) if inside => out.push(entity(&g)?),
            Ok(Event::CData(t)) if inside => out.push_str(&t.xml10_content()),
            Ok(Event::End(e)) if inside && e.local_name().as_ref() == name => return Some(out),
            Ok(Event::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// An entity reference's character: a character reference, or one of XML's five predefined
/// entities (a renderer's XML declares no others Tuner reads).
pub fn entity(r: &quick_xml::events::BytesRef<'_>) -> Option<char> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c);
    }
    Some(match r.xml10_content().as_ref() {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

/// A SOAP fault's `UPnPError` code (`errorCode`), e.g. 701 (transition not available).
pub fn fault_code(xml: &str) -> Option<u32> {
    text_of(xml, "errorCode")?.trim().parse().ok()
}

/// `H+:MM:SS[.F+]` (UPnP AV `RelTime`, `TrackDuration`) in seconds; `None` for
/// `NOT_IMPLEMENTED`, empty, or anything else.
pub fn parse_time(s: &str) -> Option<f64> {
    let mut parts = s.trim().split(':');
    let (h, m, sec) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let h: u64 = h.trim_start_matches('+').parse().ok()?;
    let m: u64 = m.parse().ok().filter(|m| *m < 60)?;
    let sec: f64 = sec.parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0 && *s < 61.0)?;
    Some((h * 3600 + m * 60) as f64 + sec)
}

/// The renderer's `CurrentTransportState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Stopped,
    Playing,
    Paused,
    Transitioning,
    NoMedia,
    Other,
}

impl Transport {
    pub fn parse(s: &str) -> Self {
        match s.trim() {
            "STOPPED" => Transport::Stopped,
            "PLAYING" => Transport::Playing,
            "PAUSED_PLAYBACK" | "PAUSED_RECORDING" => Transport::Paused,
            "TRANSITIONING" => Transport::Transitioning,
            "NO_MEDIA_PRESENT" => Transport::NoMedia,
            _ => Transport::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelopes_escape_their_arguments_and_the_didl_twice() {
        let url = "http://192.168.1.88:47800/c/abc/stream.ts?a=1&b=2";
        let didl = didl(url);
        assert!(didl.contains("http-get:*:video/mpeg:DLNA.ORG_OP=00;"));
        assert!(didl.contains("stream.ts?a=1&amp;b=2</res>"), "{didl}");
        let env = envelope(AV_TRANSPORT, "SetAVTransportURI", &[("InstanceID", "0"), ("CurrentURI", url), ("CurrentURIMetaData", &didl)]);
        assert!(env.contains(r#"<u:SetAVTransportURI xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">"#));
        assert!(env.contains("<CurrentURI>http://192.168.1.88:47800/c/abc/stream.ts?a=1&amp;b=2</CurrentURI>"));
        assert!(env.contains("<CurrentURIMetaData>&lt;DIDL-Lite "), "the DIDL is text inside the argument");
        assert_eq!(text_of(&env, "CurrentURIMetaData").as_deref(), Some(didl.as_str()), "and reads back whole");
        assert_eq!(soap_action(AV_TRANSPORT, "Play"), "\"urn:schemas-upnp-org:service:AVTransport:1#Play\"");
    }

    #[test]
    fn replies_are_read_by_local_name() {
        let reply = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
            <Track>1</Track><TrackDuration>0:00:00</TrackDuration><RelTime>1:02:03.500</RelTime><AbsTime>NOT_IMPLEMENTED</AbsTime>
            <TrackURI/></u:GetPositionInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(text_of(reply, "RelTime").and_then(|t| parse_time(&t)), Some(3723.5));
        assert_eq!(text_of(reply, "AbsTime").and_then(|t| parse_time(&t)), None);
        assert_eq!(text_of(reply, "TrackURI").as_deref(), Some(""));
        assert_eq!(text_of(reply, "Missing"), None);
        assert_eq!(text_of("<a><b>broken", "b"), None);
        let fault = r#"<s:Envelope><s:Body><s:Fault><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0">
            <errorCode>701</errorCode><errorDescription>Transition not available</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(fault_code(fault), Some(701));
    }

    #[test]
    fn times_and_states() {
        assert_eq!(parse_time("0:00:05"), Some(5.0));
        assert_eq!(parse_time("10:00:00"), Some(36000.0));
        assert_eq!(parse_time("+0:01:00.25"), Some(60.25));
        for bad in ["NOT_IMPLEMENTED", "", "1:2", "0:61:00", "a:b:c", "0:00:-1", "1:00:00:00"] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(Transport::parse("PAUSED_PLAYBACK"), Transport::Paused);
        assert_eq!(Transport::parse(" PLAYING "), Transport::Playing);
        assert_eq!(Transport::parse("CUSTOM"), Transport::Other);
    }
}
