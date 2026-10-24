//! A UPnP device description (the XML at an SSDP `LOCATION`): the MediaRenderer's name, maker,
//! model and `UDN` (its stable id), and the control URLs of AVTransport:1 and RenderingControl:1,
//! made absolute against `URLBase` or the description's own URL. Pure; the XML is device data
//! (capped by the caller, strings sanitized by discovery).

use quick_xml::events::Event;
use url::Url;

use super::soap::{AV_TRANSPORT, RENDERING_CONTROL};

/// The longest description read.
pub const MAX_DESCRIPTION: usize = 256 * 1024;

/// One device of a description (embedded devices are listed after their parent).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Device {
    device_type: String,
    friendly_name: String,
    manufacturer: String,
    model_name: String,
    udn: String,
    /// `(serviceType, controlURL)`.
    services: Vec<(String, String)>,
}

/// What Tuner keeps of a renderer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renderer {
    pub name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    /// `uuid:…`.
    pub udn: String,
    pub av_transport: Url,
    pub rendering: Option<Url>,
}

/// The renderer in `xml` fetched from `location`: the first device that has an AVTransport
/// service (a MediaRenderer, or the root of a TV that nests it). `None` without one.
pub fn parse(xml: &str, location: &Url) -> Option<Renderer> {
    let (devices, base) = devices(xml)?;
    let base = base.and_then(|b| Url::parse(b.trim()).ok()).unwrap_or_else(|| location.clone());
    let d = devices
        .iter()
        .filter(|d| d.services.iter().any(|(t, _)| t.starts_with(AV_TRANSPORT.trim_end_matches('1'))))
        .min_by_key(|d| !d.device_type.contains("MediaRenderer"))?;
    let url = |prefix: &str| {
        let (_, path) = d.services.iter().find(|(t, _)| t.starts_with(prefix.trim_end_matches('1')))?;
        let u = base.join(path.trim()).ok()?;
        // a renderer is controlled over plain HTTP on the LAN, at the host that described it
        (u.scheme() == "http" && u.host_str() == location.host_str()).then_some(u)
    };
    let opt = |s: &str| Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    Some(Renderer {
        name: d.friendly_name.trim().to_owned(),
        manufacturer: opt(&d.manufacturer),
        model: opt(&d.model_name),
        udn: opt(&d.udn)?,
        av_transport: url(AV_TRANSPORT)?,
        rendering: url(RENDERING_CONTROL),
    })
}

/// Every `<device>` (flattened) and the `URLBase`.
fn devices(xml: &str) -> Option<(Vec<Device>, Option<String>)> {
    let mut r = quick_xml::Reader::from_str(xml);
    let mut out = Vec::new();
    // the devices being read (nested), and the element path below the innermost one
    let mut stack: Vec<(Device, usize)> = Vec::new();
    let mut path: Vec<String> = Vec::new();
    let mut service: Option<(String, String)> = None;
    let mut base = None;
    // the current element's text (entities arrive as their own events)
    let mut text = String::new();
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) => {
                let name = e.local_name().as_ref().to_owned();
                match name.as_str() {
                    "device" => stack.push((Device::default(), path.len() + 1)),
                    "service" if !stack.is_empty() => service = Some(Default::default()),
                    _ => {}
                }
                path.push(name);
                text.clear();
            }
            Ok(Event::Text(t)) => text.push_str(&t.xml10_content()),
            Ok(Event::GeneralRef(g)) => text.push(super::soap::entity(&g)?),
            Ok(Event::CData(t)) => text.push_str(&t.xml10_content()),
            Ok(Event::End(e)) => {
                let value = std::mem::take(&mut text).trim().to_owned();
                match e.local_name().as_ref() {
                    "device" => {
                        if let Some((d, _)) = stack.pop() {
                            out.push(d);
                        }
                    }
                    "service" => {
                        if let (Some(s), Some((d, _))) = (service.take(), stack.last_mut()) {
                            d.services.push(s);
                        }
                    }
                    "URLBase" if stack.is_empty() => base = Some(value),
                    leaf => {
                        if let Some(s) = service.as_mut() {
                            match leaf {
                                "serviceType" => s.0 = value,
                                "controlURL" => s.1 = value,
                                _ => {}
                            }
                        } else if let Some((d, _)) = stack.last_mut().filter(|(_, depth)| path.len() == depth + 1) {
                            // only the device's own fields, not those of a nested element
                            match leaf {
                                "deviceType" => d.device_type = value,
                                "friendlyName" => d.friendly_name = value,
                                "manufacturer" => d.manufacturer = value,
                                "modelName" => d.model_name = value,
                                "UDN" => d.udn = value,
                                _ => {}
                            }
                        }
                    }
                }
                path.pop();
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }
    Some((out, base))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Samsung-style description: the renderer is the root device, control URLs are relative,
    /// no `URLBase`.
    const SAMSUNG: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0" xmlns:sec="http://www.sec.co.kr/dlna">
 <specVersion><major>1</major><minor>0</minor></specVersion>
 <device>
  <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
  <friendlyName>[TV] Samsung Q60 &amp; Co</friendlyName>
  <manufacturer>Samsung Electronics</manufacturer>
  <modelName>QE55Q60R</modelName>
  <UDN>uuid:0bf4a2b1-0000-1000-8000-001122334455</UDN>
  <serviceList>
   <service><serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
    <controlURL>/upnp/control/RenderingControl1</controlURL></service>
   <service><serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
    <controlURL>/upnp/control/ConnectionManager1</controlURL></service>
   <service><serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
    <controlURL>/upnp/control/AVTransport1</controlURL></service>
  </serviceList>
 </device>
</root>"#;

    #[test]
    fn a_root_renderer_with_relative_urls() {
        let loc = Url::parse("http://192.168.1.40:9197/dmr").unwrap();
        let r = parse(SAMSUNG, &loc).unwrap();
        assert_eq!(r.name, "[TV] Samsung Q60 & Co");
        assert_eq!((r.manufacturer.as_deref(), r.model.as_deref()), (Some("Samsung Electronics"), Some("QE55Q60R")));
        assert_eq!(r.udn, "uuid:0bf4a2b1-0000-1000-8000-001122334455");
        assert_eq!(r.av_transport.as_str(), "http://192.168.1.40:9197/upnp/control/AVTransport1");
        assert_eq!(r.rendering.unwrap().as_str(), "http://192.168.1.40:9197/upnp/control/RenderingControl1");
    }

    /// A TV whose root is a basic device nesting the renderer, with a `URLBase` and a version 2
    /// AVTransport; the nested device's fields aren't the root's.
    #[test]
    fn a_nested_renderer_and_url_base() {
        let xml = r#"<root xmlns="urn:schemas-upnp-org:device-1-0"><URLBase>http://192.168.1.41:1400/</URLBase>
 <device><deviceType>urn:schemas-upnp-org:device:Basic:1</deviceType><friendlyName>Living room</friendlyName>
  <UDN>uuid:root</UDN>
  <deviceList><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
   <friendlyName>Living room TV</friendlyName><manufacturer>LG Electronics</manufacturer><modelName>OLED55C1</modelName>
   <UDN>uuid:renderer</UDN><serviceList><service><serviceType>urn:schemas-upnp-org:service:AVTransport:2</serviceType>
   <controlURL>MediaRenderer/AVTransport/Control</controlURL></service></serviceList></device></deviceList>
 </device></root>"#;
        let loc = Url::parse("http://192.168.1.41:1400/xml/device_description.xml").unwrap();
        let r = parse(xml, &loc).unwrap();
        assert_eq!((r.name.as_str(), r.udn.as_str()), ("Living room TV", "uuid:renderer"));
        assert_eq!(r.av_transport.as_str(), "http://192.168.1.41:1400/MediaRenderer/AVTransport/Control");
        assert_eq!(r.rendering, None);
    }

    /// No AVTransport (a media server), no UDN, a control URL on another host or scheme, or broken
    /// XML: not a renderer Tuner can drive.
    #[test]
    fn what_isnt_a_usable_renderer() {
        let loc = Url::parse("http://192.168.1.40:9197/dmr").unwrap();
        assert_eq!(parse(&SAMSUNG.replace("AVTransport:1", "ContentDirectory:1"), &loc), None);
        assert_eq!(parse(&SAMSUNG.replace("<UDN>uuid:0bf4a2b1-0000-1000-8000-001122334455</UDN>", ""), &loc), None);
        let elsewhere = SAMSUNG.replace("/upnp/control/AVTransport1", "http://10.0.0.1/control");
        assert_eq!(parse(&elsewhere, &loc), None, "control lives on the host that described it");
        assert_eq!(parse(&SAMSUNG.replace("/upnp/control/AVTransport1", "https://192.168.1.40/c"), &loc), None);
        assert_eq!(parse(&SAMSUNG[..300], &loc), None);
    }
}
