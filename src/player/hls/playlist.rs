//! Provider metadata is kept in memory only. Local playlists are built from an allowlist:
//! no EXTINF titles, comments, provider URIs or unknown attributes reach disk or mpv.

use crate::cast::relay::upstream::attributes;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Resource {
    pub uri: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub resource: Resource,
    pub iv: Option<String>,
}

#[derive(Clone)]
pub(super) struct Segment {
    pub seq: u64,
    pub duration: f64,
    pub discontinuity: u64,
    pub resource: Resource,
    pub key: Option<Key>,
    pub map: Option<Resource>,
    pub gap: bool,
}

pub(super) struct Media {
    pub segments: Vec<Segment>,
    pub target: f64,
    pub ended: bool,
}

pub(super) fn attr<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    attributes(text).into_iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

fn resource(base: &url::Url, uri: &str, range: Option<(u64, u64)>) -> Result<Resource, &'static str> {
    let url = base.join(uri).map_err(|_| "invalid HLS resource")?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("unsupported HLS resource scheme");
    }
    Ok(Resource { uri: url.into(), range })
}

fn byte_range(value: &str, previous_end: u64) -> Result<(u64, u64), &'static str> {
    let (len, offset) = value.split_once('@').map_or((value, None), |(a, b)| (a, Some(b)));
    let len: u64 = len.parse().map_err(|_| "invalid HLS byte range")?;
    let start = offset.map_or(Ok(previous_end), str::parse).map_err(|_| "invalid HLS byte range")?;
    if len == 0 || start.checked_add(len).is_none() {
        return Err("invalid HLS byte range");
    }
    Ok((start, len))
}

pub(super) fn parse(text: &str, base: &url::Url) -> Result<Media, &'static str> {
    if !text.trim_start_matches('\u{feff}').starts_with("#EXTM3U") {
        return Err("invalid HLS playlist");
    }
    let mut out = Media { segments: Vec::new(), target: 2.0, ended: false };
    let (mut seq, mut disc) = (0_u64, 0_u64);
    let (mut key, mut map, mut duration, mut range) = (None, None, None, None);
    let (mut range_end, mut gap) = (0, false);
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.parse().map_err(|_| "invalid HLS sequence")?;
        } else if let Some(v) = line.strip_prefix("#EXT-X-DISCONTINUITY-SEQUENCE:") {
            disc = v.parse().map_err(|_| "invalid HLS discontinuity")?;
        } else if line == "#EXT-X-DISCONTINUITY" {
            disc = disc.checked_add(1).ok_or("invalid HLS discontinuity")?;
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            out.target = v.parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0).ok_or("invalid HLS duration")?;
        } else if let Some(v) = line.strip_prefix("#EXTINF:") {
            duration =
                Some(v.split(',').next().unwrap_or("").parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0).ok_or("invalid HLS duration")?);
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = Some(byte_range(v, range_end)?);
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            key = match attr(v, "METHOD") {
                Some("NONE") => None,
                Some("AES-128") if attr(v, "KEYFORMAT").is_none_or(|f| f == "identity") => {
                    let iv = attr(v, "IV").map(str::to_owned);
                    if iv.as_ref().is_some_and(|iv| {
                        !iv.strip_prefix("0x").is_some_and(|n| n.len() <= 32 && !n.is_empty() && n.bytes().all(|c| c.is_ascii_hexdigit()))
                    }) {
                        return Err("invalid HLS IV");
                    }
                    Some(Key { resource: resource(base, attr(v, "URI").ok_or("missing HLS key")?, None)?, iv })
                }
                _ => return Err("unsupported HLS encryption"),
            };
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let range = attr(v, "BYTERANGE").map(|r| byte_range(r, 0)).transpose()?;
            map = Some(resource(base, attr(v, "URI").ok_or("missing HLS map")?, range)?);
        } else if line == "#EXT-X-GAP" {
            gap = true;
        } else if line == "#EXT-X-ENDLIST" {
            out.ended = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            let duration = duration.take().ok_or("missing HLS segment duration")?;
            let range = range.take();
            range_end = range.map_or(0, |(start, len)| start + len);
            out.segments.push(Segment {
                seq,
                duration,
                discontinuity: disc,
                resource: resource(base, line, range)?,
                key: key.clone(),
                map: map.clone(),
                gap,
            });
            seq = seq.checked_add(1).ok_or("invalid HLS sequence")?;
            gap = false;
        }
    }
    if out.segments.is_empty() {
        return Err("empty HLS media playlist");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_keys_ranges_gaps_and_discontinuities() {
        let p = parse("#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:42\n#EXT-X-DISCONTINUITY-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"key?token=secret\"\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"20@2\"\n#EXTINF:2,title\n#EXT-X-BYTERANGE:100@10\na.ts\n#EXT-X-DISCONTINUITY\n#EXT-X-GAP\n#EXTINF:2,\n#EXT-X-BYTERANGE:90\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,\nb.ts\n", &url::Url::parse("https://example.test/path/index.m3u8").unwrap()).unwrap();
        assert_eq!(p.segments.len(), 3);
        let s = &p.segments[1];
        assert_eq!((s.seq, s.discontinuity, s.gap, s.resource.range), (43, 8, true, Some((110, 90))));
        assert!(s.key.as_ref().unwrap().resource.uri.ends_with("/path/key?token=secret"));
        assert_eq!(s.map.as_ref().unwrap().range, Some((2, 20)));
        assert!(p.segments[2].key.is_none());
    }
}
