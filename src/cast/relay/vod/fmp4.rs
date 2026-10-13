//! fMP4 pieces of a full-length run's output (ISO/IEC 14496-12 §8.8): the init section of a run's
//! first segment, and one track of a muxed segment (the surround renditions, cast design §8.4).
//!
//! * **The init section.** Each segment of a run is a whole fragmented file (the segment muxer's
//!   `-individual_header_trailer 1`, `movflags=+delay_moov`: AC-3 and E-AC-3 copied into MP4 need
//!   their first packet before the `moov` can be written), so it starts with `ftyp` + `moov`;
//!   [`split_header`] cuts it there, leaving `moof` + `mdat` pairs, an HLS fMP4 media segment.
//!   The `moov` is the same bytes in every segment of every run (`-fflags +bitexact`,
//!   `-map_metadata -1`; `tests/cast_relay.rs` checks): the first one published serves all.
//! * **One track.** The surround renditions are made as one muxed output (picture, stereo AAC,
//!   surround) so that every track is cut where the picture is: by the segment muxer at the
//!   keyframe, the sound by ffmpeg's interleaving, the same in every run (separate audio outputs
//!   would be cut relative to their own first packet, which differs between runs). [`track_init`]
//!   and [`track_media`] then take one track out: the `moov` keeps `mvhd`, that `trak` and its
//!   `trex`; each `moof` keeps `mfhd` and that track's `traf` with its `trun` data offsets moved
//!   onto a new `mdat` holding only that track's samples (ffmpeg writes a track's samples of a
//!   fragment contiguously, in `trun` order). Track IDs are kept, so `tfhd` matches the `tkhd`.
//!   A track with no samples in a segment (the sound ended before the picture: a film's silent
//!   credits) has no `traf` there (`movenc` writes none): its segment is one fragment without
//!   samples (`mfhd`, a `traf` with only its `tfhd`, an empty `mdat`), a gap in that rendition
//!   rather than a segment the TV can't have.

/// A box at the start of `b`: `(type, header length, size)`.
fn header(b: &[u8]) -> Option<([u8; 4], usize, usize)> {
    let size = u32::from_be_bytes(b.get(..4)?.try_into().ok()?);
    let kind: [u8; 4] = b.get(4..8)?.try_into().ok()?;
    let (hdr, size) = match size {
        1 => (16, usize::try_from(u64::from_be_bytes(b.get(8..16)?.try_into().ok()?)).ok()?),
        0 => (8, b.len()),
        n => (8, n as usize),
    };
    (size >= hdr && size <= b.len()).then_some((kind, hdr, size))
}

/// The boxes of `data`: `(type, whole box)`; `None` when one is damaged or cut short.
fn boxes(data: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (kind, _, size) = header(rest)?;
        out.push((kind, &rest[..size]));
        rest = &rest[size..];
    }
    Some(out)
}

fn body(whole: &[u8]) -> &[u8] {
    header(whole).map_or(&[][..], |(_, hdr, size)| &whole[hdr..size])
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn wrap(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
    let len: usize = 8 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut v = Vec::with_capacity(len);
    v.extend_from_slice(&(len as u32).to_be_bytes());
    v.extend_from_slice(kind);
    for p in parts {
        v.extend_from_slice(p);
    }
    v
}

/// Where a segment's media starts: after its `ftyp` and `moov` (`None` when it doesn't start
/// with them).
pub fn split_header(seg: &[u8]) -> Option<usize> {
    let (kind, _, ftyp) = header(seg)?;
    if &kind != b"ftyp" {
        return None;
    }
    let (kind, _, moov) = header(&seg[ftyp..])?;
    (&kind == b"moov").then_some(ftyp + moov)
}

/// The track IDs of an init section, in `trak` order.
pub fn tracks(init: &[u8]) -> Option<Vec<u32>> {
    let moov = boxes(init)?.into_iter().find(|(k, _)| k == b"moov")?.1;
    boxes(body(moov))?.into_iter().filter(|(k, _)| k == b"trak").map(|(_, trak)| track_id(trak)).collect()
}

/// A `trak`'s ID from its `tkhd` (version 1: after two 64-bit times).
fn track_id(trak: &[u8]) -> Option<u32> {
    let tkhd = boxes(body(trak))?.into_iter().find(|(k, _)| k == b"tkhd")?.1;
    let b = body(tkhd);
    be32(b, if *b.first()? == 1 { 20 } else { 12 })
}

/// The init section of `track` alone: `ftyp`, and a `moov` with `mvhd`, its `trak` and an `mvex`
/// with `mehd` and its `trex`.
pub fn track_init(init: &[u8], track: u32) -> Option<Vec<u8>> {
    let top = boxes(init)?;
    let ftyp = top.iter().find(|(k, _)| k == b"ftyp")?.1;
    let moov = top.iter().find(|(k, _)| k == b"moov")?.1;
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut found = false;
    for (kind, b) in boxes(body(moov))? {
        match &kind {
            b"mvhd" => parts.push(b.to_vec()),
            b"trak" if track_id(b) == Some(track) => {
                found = true;
                parts.push(b.to_vec());
            }
            b"mvex" => {
                let mut mvex: Vec<&[u8]> = Vec::new();
                for (kind, b) in boxes(body(b))? {
                    // trex: version/flags, then track_ID
                    if &kind == b"mehd" || (&kind == b"trex" && be32(body(b), 4) == Some(track)) {
                        mvex.push(b);
                    }
                }
                parts.push(wrap(b"mvex", &mvex));
            }
            _ => {}
        }
    }
    if !found {
        return None;
    }
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    let mut out = ftyp.to_vec();
    out.extend(wrap(b"moov", &refs));
    Some(out)
}

/// The `trex` default sample size of `track` in an init section (what a `traf` without sizes
/// falls back on).
pub fn trex_size(init: &[u8], track: u32) -> Option<u32> {
    let moov = boxes(init)?.into_iter().find(|(k, _)| k == b"moov")?.1;
    let mvex = boxes(body(moov))?.into_iter().find(|(k, _)| k == b"mvex")?.1;
    let trex = boxes(body(mvex))?.into_iter().find(|(k, b)| k == b"trex" && be32(body(b), 4) == Some(track))?.1;
    // version/flags, track_ID, default_sample_description_index, duration, size
    be32(body(trex), 16)
}

/// One `trun`: where its samples are (relative to the `moof`'s start) and how long they are.
struct Run {
    /// Offset of the `data_offset` field inside the `traf` body.
    field: usize,
    data_offset: i32,
    bytes: usize,
}

/// The `trun`s of a `traf` body; `None` for layouts this doesn't rewrite (an explicit base data
/// offset, a `trun` without a data offset).
fn runs(traf: &[u8], trex_size: Option<u32>) -> Option<Vec<Run>> {
    let kids = boxes(traf)?;
    let tfhd = body(kids.iter().find(|(k, _)| k == b"tfhd")?.1);
    let flags = be32(tfhd, 0)? & 0xff_ffff;
    if flags & 0x1 != 0 {
        return None; // base-data-offset-present: offsets into the old file
    }
    let mut at = 8;
    for (bit, len) in [(0x2, 4), (0x8, 4)] {
        if flags & bit != 0 {
            at += len;
        }
    }
    let default_size = if flags & 0x10 != 0 { be32(tfhd, at) } else { trex_size };
    let mut out = Vec::new();
    let mut pos = 0usize;
    for (kind, whole) in &kids {
        if kind == b"trun" {
            let (_, hdr, _) = header(whole)?;
            let b = body(whole);
            let tflags = be32(b, 0)? & 0xff_ffff;
            let n = be32(b, 4)? as usize;
            if tflags & 0x1 == 0 {
                return None;
            }
            let data_offset = i32::from_be_bytes(b.get(8..12)?.try_into().ok()?);
            // each sample's record: duration (0x100), size (0x200), flags (0x400), offset (0x800)
            let mut j = 12 + if tflags & 0x4 != 0 { 4 } else { 0 };
            let per = [0x100, 0x200, 0x400, 0x800].iter().filter(|f| tflags & **f != 0).count() * 4;
            let size_at = if tflags & 0x100 != 0 { 4 } else { 0 };
            let mut bytes = 0usize;
            for _ in 0..n {
                let size = if tflags & 0x200 != 0 { be32(b, j + size_at)? } else { default_size? };
                bytes = bytes.checked_add(size as usize)?;
                j += per;
            }
            out.push(Run { field: pos + hdr + 8, data_offset, bytes });
        }
        pos += whole.len();
    }
    Some(out)
}

/// `track`'s fragments of a media segment (`moof` + `mdat` pairs, as the module docs say);
/// `trex_size`: the init's default sample size for it. `None` when the segment can't be read
/// (or holds no fragment at all); one fragment without samples when it has none of `track`'s.
pub fn track_media(seg: &[u8], track: u32, trex_size: Option<u32>) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(seg.len() / 2);
    let mut first_mfhd: Option<&[u8]> = None;
    let mut at = 0usize;
    for (kind, whole) in boxes(seg)? {
        let here = at;
        at += whole.len();
        if &kind != b"moof" {
            continue; // mdat (read through the moof), styp, sidx, prft…
        }
        let mut mfhd: Option<&[u8]> = None;
        let mut traf: Option<&[u8]> = None;
        for (kind, b) in boxes(body(whole))? {
            match &kind {
                b"mfhd" => mfhd = Some(b),
                b"traf" if boxes(body(b))?.iter().any(|(k, t)| k == b"tfhd" && be32(body(t), 4) == Some(track)) => traf = Some(b),
                _ => {}
            }
        }
        first_mfhd = first_mfhd.or(mfhd);
        let (Some(mfhd), Some(traf)) = (mfhd, traf) else { continue };
        let runs = runs(body(traf), trex_size)?;
        let mut data = Vec::new();
        let mut new_traf = traf.to_vec();
        // the new moof is mfhd + this traf: its mdat's payload starts right after both
        let moof_len = 8 + mfhd.len() + traf.len();
        let (_, traf_hdr, _) = header(traf)?;
        for r in &runs {
            let from = here.checked_add_signed(r.data_offset as isize)?;
            let bytes = seg.get(from..from.checked_add(r.bytes)?)?;
            let offset = i32::try_from(moof_len + 8 + data.len()).ok()?;
            new_traf[traf_hdr + r.field..traf_hdr + r.field + 4].copy_from_slice(&offset.to_be_bytes());
            data.extend_from_slice(bytes);
        }
        out.extend(wrap(b"moof", &[mfhd, &new_traf]));
        out.extend(wrap(b"mdat", &[&data]));
    }
    if out.is_empty() {
        // none of the track's samples here: a fragment without any (default-base-is-moof)
        let mut tfhd = 0x0002_0000u32.to_be_bytes().to_vec();
        tfhd.extend_from_slice(&track.to_be_bytes());
        out.extend(wrap(b"moof", &[first_mfhd?, &wrap(b"traf", &[&wrap(b"tfhd", &[&tfhd])])]));
        out.extend(wrap(b"mdat", &[]));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(kind: &[u8; 4], parts: &[&[u8]]) -> Vec<u8> {
        wrap(kind, parts)
    }

    fn tfhd(track: u32, default_size: Option<u32>) -> Vec<u8> {
        let flags: u32 = 0x20000 | if default_size.is_some() { 0x10 } else { 0 };
        let mut b = flags.to_be_bytes().to_vec();
        b.extend_from_slice(&track.to_be_bytes());
        if let Some(s) = default_size {
            b.extend_from_slice(&s.to_be_bytes());
        }
        bx(b"tfhd", &[&b])
    }

    /// A trun with a data offset and, unless `None`, per-sample sizes.
    fn trun(offset: i32, sizes: &[u32], explicit: bool) -> Vec<u8> {
        let flags: u32 = 0x1 | if explicit { 0x200 } else { 0 };
        let mut b = flags.to_be_bytes().to_vec();
        b.extend_from_slice(&(sizes.len() as u32).to_be_bytes());
        b.extend_from_slice(&offset.to_be_bytes());
        if explicit {
            for s in sizes {
                b.extend_from_slice(&s.to_be_bytes());
            }
        }
        bx(b"trun", &[&b])
    }

    /// One fragment of three tracks as ffmpeg lays it out: track 1 (sizes in trun), 2 (sizes in
    /// trun), 3 (tfhd's default size), their samples back to back in one mdat.
    fn fragment(seq: u32) -> Vec<u8> {
        let mfhd = bx(b"mfhd", &[&[0, 0, 0, 0], &seq.to_be_bytes()]);
        let samples: [(u32, &[u32], bool, u8); 3] = [(1, &[5, 3], true, b'v'), (2, &[2, 2, 2], true, b'a'), (3, &[4, 4], false, b's')];
        // offsets need the moof's size: build once with zeros to measure
        let build = |offsets: [i32; 3]| {
            let mut trafs = Vec::new();
            for (i, (track, sizes, explicit, _)) in samples.iter().enumerate() {
                let t = tfhd(*track, (!explicit).then_some(sizes[0]));
                let r = trun(offsets[i], sizes, *explicit);
                trafs.push(bx(b"traf", &[&t, &r]));
            }
            let refs: Vec<&[u8]> = std::iter::once(mfhd.as_slice()).chain(trafs.iter().map(Vec::as_slice)).collect();
            bx(b"moof", &refs)
        };
        let len = build([0; 3]).len() as i32;
        let (mut offsets, mut at) = ([0i32; 3], len + 8);
        let mut payload = Vec::new();
        for (i, (_, sizes, _, tag)) in samples.iter().enumerate() {
            offsets[i] = at;
            let n: u32 = sizes.iter().sum();
            payload.extend(std::iter::repeat_n(*tag, n as usize));
            at += n as i32;
        }
        let mut out = build(offsets);
        out.extend(bx(b"mdat", &[&payload]));
        out
    }

    fn samples_of(seg: &[u8]) -> Vec<u8> {
        boxes(seg).unwrap().into_iter().filter(|(k, _)| k == b"mdat").flat_map(|(_, b)| body(b).to_vec()).collect()
    }

    /// Each track's split keeps its own samples (and only those) with data offsets that point at
    /// them, fragment after fragment; the rest of the segment is dropped.
    #[test]
    fn a_muxed_segment_splits_into_its_tracks() {
        let mut seg = bx(b"styp", &[b"msdh"]);
        seg.extend(fragment(7));
        seg.extend(fragment(8));
        for (track, tag, n) in [(1u32, b'v', 16usize), (2, b'a', 12), (3, b's', 16)] {
            let one = track_media(&seg, track, None).unwrap();
            assert_eq!(samples_of(&one), vec![tag; n], "track {track}");
            // every trun's data offset lands on the first sample of its mdat
            let parts = boxes(&one).unwrap();
            assert_eq!(parts.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [*b"moof", *b"mdat", *b"moof", *b"mdat"]);
            let moof = parts[0].1;
            let traf = boxes(body(moof)).unwrap().into_iter().find(|(k, _)| k == b"traf").unwrap().1;
            let r = runs(body(traf), None).unwrap();
            assert_eq!(r[0].data_offset as usize, moof.len() + 8, "track {track}");
            assert_eq!(boxes(body(moof)).unwrap().iter().filter(|(k, _)| k == b"traf").count(), 1);
        }
        // a track without samples in the segment (the sound ended): one empty fragment
        let none = track_media(&seg, 9, None).unwrap();
        let parts = boxes(&none).unwrap();
        assert_eq!(parts.iter().map(|(k, _)| *k).collect::<Vec<_>>(), [*b"moof", *b"mdat"]);
        assert!(samples_of(&none).is_empty());
        let traf = boxes(body(parts[0].1)).unwrap().into_iter().find(|(k, _)| k == b"traf").unwrap().1;
        assert_eq!(runs(body(traf), None).map(|r| r.len()), Some(0));
        let tfhd = boxes(body(traf)).unwrap()[0];
        assert_eq!((tfhd.0, be32(body(tfhd.1), 4)), (*b"tfhd", Some(9)));
        assert_eq!(track_media(&seg[..seg.len() - 3], 1, None), None, "cut short");
        assert_eq!(track_media(&bx(b"styp", &[b"msdh"]), 1, None), None, "no fragment at all");
    }

    #[test]
    fn the_init_keeps_one_track_and_the_header_is_found() {
        let tkhd = |id: u32| bx(b"tkhd", &[&[0, 0, 0, 3], &[0; 8], &id.to_be_bytes(), &[0; 4]]);
        let trex =
            |id: u32, size: u32| bx(b"trex", &[&[0; 4], &id.to_be_bytes(), &1u32.to_be_bytes(), &0u32.to_be_bytes(), &size.to_be_bytes(), &[0; 4]]);
        let traks: Vec<Vec<u8>> = (1..=3).map(|id| bx(b"trak", &[&tkhd(id), &bx(b"mdia", &[])])).collect();
        let mvex = bx(b"mvex", &[&bx(b"mehd", &[&[0; 8]]), &trex(1, 0), &trex(2, 0), &trex(3, 1792)]);
        let moov = bx(b"moov", &[&bx(b"mvhd", &[&[0; 100]]), &traks[0], &traks[1], &traks[2], &mvex, &bx(b"udta", &[])]);
        let mut init = bx(b"ftyp", &[b"iso6"]);
        init.extend(&moov);
        assert_eq!(tracks(&init), Some(vec![1, 2, 3]));
        assert_eq!(trex_size(&init, 3), Some(1792));
        let one = track_init(&init, 3).unwrap();
        assert_eq!(tracks(&one), Some(vec![3]));
        let moov = boxes(&one).unwrap()[1].1;
        let kinds: Vec<[u8; 4]> = boxes(body(moov)).unwrap().iter().map(|(k, _)| *k).collect();
        assert_eq!(kinds, [*b"mvhd", *b"trak", *b"mvex"]);
        assert_eq!(trex_size(&one, 3), Some(1792));
        assert_eq!(trex_size(&one, 2), None);
        assert_eq!(track_init(&init, 4), None);

        let mut first = init.clone();
        first.extend(fragment(1));
        assert_eq!(split_header(&first), Some(init.len()));
        assert_eq!(split_header(&fragment(2)), None, "no header");
    }
}
