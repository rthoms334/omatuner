//! Streaming XMLTV parser (quick-xml 0.42). Owner: WP-05a. Spec: protocols.md §3.
//!
//! quick-xml 0.42 notes (verified by the protocols prototype):
//! * names are `&str` (`"programme"`), attributes via `normalized_value(XmlVersion::Implicit1_0)`;
//! * entities arrive as separate `Event::GeneralRef` → resolve `amp lt gt quot apos` + char refs
//!   yourself; NEVER enable `trim_text` (it eats spaces around entities); trim after joining;
//! * reuse one `Vec<u8>` buffer (`buf.clear()` per event).
//!
//! Measured parse: 415k programmes in 0.61 s; the DB insert dominates (see jobs::import_epg).
//!
//! Multi-language: the first `<title>` whose `lang` matches the preferred list wins, else the
//! first title (same for `sub-title` and `desc`; categories keep one language). Programmes
//! with an unparsable start or without a `channel` are skipped (counted in
//! [`XmltvStats::skipped`]). A missing, unparsable or non-positive stop is filled here with the
//! next later programme's start on the same channel (one held-back programme per channel; a
//! duplicate start while it waits is skipped and counted, first in file order wins), or
//! `start + 1 h` when the channel has no later programme. Display-name norms
//! (`identity::normalize_name`) are computed here so the store never calls into `sources::`.
//! Times are UTC seconds with the zone of the timestamp applied; the EPG source's own offset
//! is added by the store at write time.

use std::collections::HashMap;
use std::io::{BufRead, Read};
use std::ops::ControlFlow;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};
use crate::model::{EpisodeNum, Timestamp};
use crate::sources::identity::normalize_name;
use crate::store::epg::{ImportEpgChannel, ImportProgramme};

#[derive(Debug, Clone, PartialEq)]
pub enum XmltvItem {
    Channel(ImportEpgChannel),
    Programme(ImportProgramme),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XmltvStats {
    pub channels: u64,
    pub programmes: u64,
    pub skipped: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct XmltvOptions {
    /// ISO-639-1 codes in preference order for multi-language titles/descriptions.
    pub preferred_langs: Vec<String>,
    /// Skip programmes ending before this (saves DB work on huge archives).
    pub min_stop: Option<Timestamp>,
    /// Skip programmes starting after this (e.g. now + 14 days).
    pub max_start: Option<Timestamp>,
}

/// Stop used when neither the programme nor a later one on its channel gives one.
const DEFAULT_DURATION_S: i64 = 3600;
const PROGRESS_EVERY: u64 = 1 << 20;

/// Parse `reader`, calling `sink` for each item; `sink` may return `Break` to cancel (then
/// `Err(Error::Cancelled)`). `progress(bytes)` is called every ~1 MiB of input.
///
/// Programmes dropped by `min_stop`/`max_start` are neither emitted nor counted. Malformed
/// XML is a `Parse` error and a document whose root is not `<tv>` (an HTML error page) or
/// that has no root element at all (an empty body, plain text, JSON) is `BadPayload`; either way the caller keeps the previous guide generation. A non-UTF-8
/// `encoding` in the XML declaration (e.g. ISO-8859-1) is transcoded on the fly; `bytes`
/// then counts the UTF-8 text.
pub fn parse<R: BufRead>(
    mut reader: R,
    opts: &XmltvOptions,
    sink: &mut dyn FnMut(XmltvItem) -> ControlFlow<()>,
    progress: &mut dyn FnMut(u64),
) -> Result<XmltvStats> {
    let head = read_head(&mut reader).map_err(|e| Error::parse(None, format!("XMLTV read error: {e}")))?;
    match declared_encoding(&head) {
        Some(enc) => {
            let src = std::io::Cursor::new(head).chain(reader);
            parse_utf8(Transcode::new(src, enc), opts, sink, progress)
        }
        None => parse_utf8(std::io::Cursor::new(head).chain(reader), opts, sink, progress),
    }
}

/// First bytes of the input (enough for the XML declaration), consumed from `r`.
fn read_head<R: BufRead>(r: &mut R) -> std::io::Result<Vec<u8>> {
    const HEAD: usize = 512;
    let mut head = Vec::with_capacity(HEAD);
    while head.len() < HEAD && !head.windows(2).any(|w| w == b"?>") {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        let n = chunk.len().min(HEAD - head.len());
        head.extend_from_slice(&chunk[..n]);
        r.consume(n);
    }
    Ok(head)
}

/// The declaration's `encoding` when it is a known non-UTF-8 encoding. Labels that cannot
/// describe bytes in which we just found `<?xml` as ASCII (UTF-16, ISO-2022-*, `replacement`)
/// are mislabels and ignored.
fn declared_encoding(head: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let decl_end = head.windows(2).position(|w| w == b"?>")?;
    let decl = &head[..decl_end];
    let start = decl.windows(5).position(|w| w == b"<?xml")?;
    let decl = &decl[start..];
    let at = decl.windows(8).position(|w| w.eq_ignore_ascii_case(b"encoding"))? + 8;
    let rest = decl[at..].trim_ascii_start().strip_prefix(b"=")?.trim_ascii_start();
    let (&quote, rest) = rest.split_first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let label = &rest[..rest.iter().position(|&c| c == quote)?];
    let enc = encoding_rs::Encoding::for_label(label)?;
    (enc != encoding_rs::UTF_8 && enc.is_ascii_compatible()).then_some(enc)
}

/// `BufRead` adapter decoding `inner` from `enc` to UTF-8.
struct Transcode<R> {
    inner: R,
    dec: encoding_rs::Decoder,
    out: Vec<u8>,
    pos: usize,
    done: bool,
}

impl<R: BufRead> Transcode<R> {
    fn new(inner: R, enc: &'static encoding_rs::Encoding) -> Self {
        Transcode { inner, dec: enc.new_decoder_without_bom_handling(), out: Vec::new(), pos: 0, done: false }
    }
}

impl<R: BufRead> std::io::Read for Transcode<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let avail = self.fill_buf()?;
        let n = avail.len().min(buf.len());
        buf[..n].copy_from_slice(&avail[..n]);
        self.consume(n);
        Ok(n)
    }
}

impl<R: BufRead> BufRead for Transcode<R> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        while self.pos == self.out.len() && !self.done {
            let src = self.inner.fill_buf()?;
            let last = src.is_empty();
            let cap = self.dec.max_utf8_buffer_length(src.len()).unwrap_or(src.len() * 3 + 16).max(16);
            self.out.resize(cap, 0);
            let (_, read, written, _) = self.dec.decode_to_utf8(src, &mut self.out, last);
            self.out.truncate(written);
            self.pos = 0;
            self.inner.consume(read);
            self.done = last;
        }
        Ok(&self.out[self.pos..])
    }

    fn consume(&mut self, amt: usize) {
        self.pos = (self.pos + amt).min(self.out.len());
    }
}

fn parse_utf8<R: BufRead>(
    reader: R,
    opts: &XmltvOptions,
    sink: &mut dyn FnMut(XmltvItem) -> ControlFlow<()>,
    progress: &mut dyn FnMut(u64),
) -> Result<XmltvStats> {
    let mut xml = Reader::from_reader(reader);
    // Real feeds contain bare `&` ("Tom & Jerry", unescaped query strings): keep it as text.
    xml.config_mut().allow_dangling_amp = true;
    let mut p = Parser {
        opts,
        sink,
        stats: XmltvStats::default(),
        pending: HashMap::new(),
        text: String::with_capacity(256),
        lang: None,
        system: None,
        channel: None,
        prog: None,
    };
    let mut buf = Vec::with_capacity(4096);
    let mut stack: Vec<Tag> = Vec::with_capacity(8);
    let mut next_progress = PROGRESS_EVERY;
    let mut seen_root = false;

    loop {
        let ev = xml.read_event_into(&mut buf).map_err(|e| syntax_error(xml.error_position(), &e))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = e.name().0;
                if !seen_root {
                    seen_root = true;
                    if name != "tv" {
                        let shown: String = name.chars().take(32).collect();
                        return Err(Error::BadPayload(format!("not an XMLTV document (root element <{shown}>)")));
                    }
                }
                let parent = stack.last().copied();
                let tag = classify(name, parent, stack.len());
                match tag {
                    Tag::Channel => {
                        p.channel = Some(ChannelBuilder { id: attr(e, "id").unwrap_or_default(), ..Default::default() });
                    }
                    Tag::Programme => {
                        p.prog = Some(ProgBuilder {
                            start: attr(e, "start"),
                            stop: attr(e, "stop"),
                            channel: attr(e, "channel"),
                            catchup_id: attr(e, "catchup-id").filter(|s| !s.trim().is_empty()),
                            ..Default::default()
                        });
                    }
                    Tag::DisplayName | Tag::Title | Tag::SubTitle | Tag::Desc | Tag::Category => {
                        p.lang = attr(e, "lang");
                        p.text.clear();
                    }
                    Tag::EpisodeNum => {
                        p.system = attr(e, "system");
                        p.text.clear();
                    }
                    Tag::Value => p.text.clear(),
                    Tag::Rating | Tag::StarRating | Tag::Other => {}
                }
                // Leaf markers that carry their data in attributes or by presence.
                match (parent, name) {
                    (Some(Tag::Channel), "icon") => {
                        if let Some(c) = p.channel.as_mut().filter(|c| c.icon.is_none()) {
                            c.icon = attr(e, "src").filter(|s| !s.is_empty());
                        }
                    }
                    (Some(Tag::Programme), "icon") => {
                        if let Some(pb) = p.prog.as_mut().filter(|pb| pb.icon.is_none()) {
                            pb.icon = attr(e, "src").filter(|s| !s.is_empty());
                        }
                    }
                    (Some(Tag::Programme), "new") => p.prog.as_mut().into_iter().for_each(|pb| pb.is_new = true),
                    (Some(Tag::Programme), "live") => p.prog.as_mut().into_iter().for_each(|pb| pb.is_live = true),
                    (Some(Tag::Programme), "previously-shown") => p.prog.as_mut().into_iter().for_each(|pb| pb.previously_shown = true),
                    _ => {}
                }
                if empty {
                    p.end_tag(tag, stack.last().copied())?;
                } else {
                    stack.push(tag);
                }
            }
            Event::End(_) => {
                if let Some(tag) = stack.pop() {
                    p.end_tag(tag, stack.last().copied())?;
                }
            }
            Event::Text(t) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    p.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    p.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                if stack.last().is_some_and(|t| t.captures_text()) {
                    match r.resolve_char_ref() {
                        Ok(Some(c)) => p.text.push(c),
                        Ok(None) => match predefined_entity(&r) {
                            Some(c) => p.text.push(c),
                            // Unknown (e.g. HTML `&nbsp;` without a DTD): keep it verbatim.
                            None => {
                                p.text.push('&');
                                p.text.push_str(&r);
                                p.text.push(';');
                            }
                        },
                        Err(_) => {}
                    }
                }
            }
            Event::Eof if !stack.is_empty() => {
                // Cut-off download: never let it pass as a (short) complete guide.
                return Err(Error::parse(None, format!("XMLTV truncated: input ended with {} unclosed element(s)", stack.len())));
            }
            Event::Eof if !seen_root => {
                // An empty 200 (failed Xtream login), plain text or JSON is not an empty guide:
                // accepting it would replace the user's previous generation with nothing.
                return Err(Error::BadPayload("empty response or not an XMLTV document".into()));
            }
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
        buf.clear();
        let pos = xml.buffer_position();
        if pos >= next_progress {
            progress(pos);
            next_progress = pos + PROGRESS_EVERY;
        }
    }

    // Flush programmes still waiting for a stop time, in a deterministic order.
    let mut rest: Vec<ImportProgramme> = p.pending.drain().map(|(_, v)| v).collect();
    rest.sort_by(|a, b| (&a.xmltv_id, a.start).cmp(&(&b.xmltv_id, b.start)));
    for mut pr in rest {
        pr.stop = pr.start + DEFAULT_DURATION_S;
        p.emit_programme(pr)?;
    }
    p.stats.bytes = xml.buffer_position();
    progress(p.stats.bytes);
    Ok(p.stats)
}

/// quick-xml messages can quote document text, so they are redacted like any foreign error.
fn syntax_error(pos: u64, e: &quick_xml::Error) -> Error {
    Error::parse(None, crate::redact::redact(&format!("XMLTV syntax error at byte {pos}: {e}")).into_owned())
}

fn predefined_entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

/// Attribute value (entities resolved); falls back to the raw text when normalization fails
/// (e.g. an unknown entity in a URL).
fn attr(e: &BytesStart<'_>, key: &str) -> Option<String> {
    let a = e.try_get_attribute(key).ok().flatten()?;
    Some(match a.normalized_value(XmlVersion::Implicit1_0) {
        Ok(v) => v.into_owned(),
        Err(_) => a.value.into_owned(),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tag {
    Channel,
    Programme,
    DisplayName,
    Title,
    SubTitle,
    Desc,
    Category,
    EpisodeNum,
    Rating,
    StarRating,
    /// `<value>` inside `<rating>` / `<star-rating>`.
    Value,
    Other,
}

impl Tag {
    fn captures_text(self) -> bool {
        matches!(self, Tag::DisplayName | Tag::Title | Tag::SubTitle | Tag::Desc | Tag::Category | Tag::EpisodeNum | Tag::Value)
    }
}

/// `depth` = number of open ancestors (`<channel>`/`<programme>` only directly under `<tv>`).
fn classify(name: &str, parent: Option<Tag>, depth: usize) -> Tag {
    match (parent, name) {
        (Some(Tag::Channel), "display-name") => Tag::DisplayName,
        (Some(Tag::Programme), "title") => Tag::Title,
        (Some(Tag::Programme), "sub-title") => Tag::SubTitle,
        (Some(Tag::Programme), "desc") => Tag::Desc,
        (Some(Tag::Programme), "category") => Tag::Category,
        (Some(Tag::Programme), "episode-num") => Tag::EpisodeNum,
        (Some(Tag::Programme), "rating") => Tag::Rating,
        (Some(Tag::Programme), "star-rating") => Tag::StarRating,
        (Some(Tag::Rating | Tag::StarRating), "value") => Tag::Value,
        (None | Some(Tag::Other), "channel") if depth <= 1 => Tag::Channel,
        (None | Some(Tag::Other), "programme") if depth <= 1 => Tag::Programme,
        _ => Tag::Other,
    }
}

type LangText = (Option<String>, String);

#[derive(Debug, Default)]
struct ChannelBuilder {
    id: String,
    names: Vec<String>,
    icon: Option<String>,
}

#[derive(Debug, Default)]
struct ProgBuilder {
    start: Option<String>,
    stop: Option<String>,
    channel: Option<String>,
    catchup_id: Option<String>,
    titles: Vec<LangText>,
    sub_titles: Vec<LangText>,
    descs: Vec<LangText>,
    categories: Vec<LangText>,
    episode_ns: Option<EpisodeNum>,
    episode_other: Option<EpisodeNum>,
    icon: Option<String>,
    rating: Option<String>,
    star_rating: Option<String>,
    is_new: bool,
    is_live: bool,
    previously_shown: bool,
}

struct Parser<'a> {
    opts: &'a XmltvOptions,
    sink: &'a mut dyn FnMut(XmltvItem) -> ControlFlow<()>,
    stats: XmltvStats,
    /// Programmes without a usable stop, keyed by channel, waiting for the next start.
    pending: HashMap<String, ImportProgramme>,
    /// Text of the innermost capturing element (reused buffer).
    text: String,
    /// `lang` / `system` attribute of the open capturing element.
    lang: Option<String>,
    system: Option<String>,
    channel: Option<ChannelBuilder>,
    prog: Option<ProgBuilder>,
}

impl Parser<'_> {
    /// Trimmed captured text; clears the buffer.
    fn take_text(&mut self) -> String {
        let t = self.text.trim().to_owned();
        self.text.clear();
        t
    }

    /// Close `tag` (whose parent is now `parent`).
    fn end_tag(&mut self, tag: Tag, parent: Option<Tag>) -> Result<()> {
        match tag {
            Tag::DisplayName => {
                let t = self.take_text();
                self.lang = None;
                if let Some(c) = self.channel.as_mut()
                    && !t.is_empty()
                    && !c.names.contains(&t)
                {
                    c.names.push(t);
                }
            }
            Tag::Title | Tag::SubTitle | Tag::Desc | Tag::Category => {
                let t = self.take_text();
                let l = self.lang.take();
                if let Some(pb) = self.prog.as_mut()
                    && !t.is_empty()
                {
                    let list = match tag {
                        Tag::Title => &mut pb.titles,
                        Tag::SubTitle => &mut pb.sub_titles,
                        Tag::Desc => &mut pb.descs,
                        _ => &mut pb.categories,
                    };
                    list.push((l, t));
                }
            }
            Tag::EpisodeNum => {
                let t = self.take_text();
                let sys = self.system.take();
                if let Some(pb) = self.prog.as_mut()
                    && let Some(ep) = parse_episode_num(sys.as_deref(), &t)
                {
                    if sys.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("xmltv_ns")) {
                        pb.episode_ns.get_or_insert(ep);
                    } else {
                        pb.episode_other.get_or_insert(ep);
                    }
                }
            }
            Tag::Value => {
                let t = self.take_text();
                if let Some(pb) = self.prog.as_mut()
                    && !t.is_empty()
                {
                    match parent {
                        Some(Tag::Rating) => {
                            pb.rating.get_or_insert(t);
                        }
                        Some(Tag::StarRating) => {
                            pb.star_rating.get_or_insert(t);
                        }
                        _ => {}
                    }
                }
            }
            Tag::Channel => {
                if let Some(c) = self.channel.take() {
                    self.emit_channel(c)?;
                }
            }
            Tag::Programme => {
                if let Some(pb) = self.prog.take() {
                    self.finish_programme(pb)?;
                }
            }
            Tag::Rating | Tag::StarRating | Tag::Other => {}
        }
        Ok(())
    }

    fn send(&mut self, item: XmltvItem) -> Result<()> {
        match (self.sink)(item) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(Error::Cancelled),
        }
    }

    fn emit_channel(&mut self, c: ChannelBuilder) -> Result<()> {
        let id = c.id.trim();
        if id.is_empty() {
            return Ok(());
        }
        let display_names = c.names.into_iter().map(|n| {
            let norm = normalize_name(&n);
            (n, norm)
        });
        let ch = ImportEpgChannel { xmltv_id: id.to_owned(), display_names: display_names.collect(), icon: c.icon };
        self.stats.channels += 1;
        self.send(XmltvItem::Channel(ch))
    }

    fn finish_programme(&mut self, pb: ProgBuilder) -> Result<()> {
        let start = pb.start.as_deref().and_then(parse_time);
        let channel = pb.channel.as_deref().map(str::trim).filter(|c| !c.is_empty());
        let (Some(start), Some(channel)) = (start, channel) else {
            self.stats.skipped += 1;
            return Ok(());
        };
        let stop = pb.stop.as_deref().and_then(parse_time).filter(|&s| s > start);
        let langs = &self.opts.preferred_langs;
        let title = pick_lang(&pb.titles, langs).unwrap_or_default().to_owned();
        let prog = ImportProgramme {
            xmltv_id: channel.to_owned(),
            start,
            stop: stop.unwrap_or(0),
            title,
            sub_title: pick_lang(&pb.sub_titles, langs).map(str::to_owned),
            description: pick_lang(&pb.descs, langs).map(str::to_owned),
            categories: pick_categories(pb.categories, langs),
            icon: pb.icon,
            episode: pb.episode_ns.or(pb.episode_other),
            rating: pb.rating,
            star_rating: pb.star_rating,
            catchup_id: pb.catchup_id,
            is_new: pb.is_new,
            is_live: pb.is_live,
            previously_shown: pb.previously_shown,
        };
        let mut prog = prog;
        if let Some(prev_start) = self.pending.get(channel).map(|p| p.start) {
            if start > prev_start {
                if let Some(mut prev) = self.pending.remove(channel) {
                    prev.stop = start;
                    self.emit_programme(prev)?;
                }
            } else if start == prev_start {
                // Same (channel, start) as the waiting programme: first in file order wins,
                // like the store's `ON CONFLICT DO NOTHING`.
                self.stats.skipped += 1;
                return Ok(());
            } else if stop.is_none() {
                // Out of order and stop-less: it ends where the waiting programme starts.
                prog.stop = prev_start;
                return self.emit_programme(prog);
            }
        }
        if stop.is_some() {
            self.emit_programme(prog)
        } else {
            self.pending.insert(prog.xmltv_id.clone(), prog);
            Ok(())
        }
    }

    fn emit_programme(&mut self, pr: ImportProgramme) -> Result<()> {
        if self.opts.min_stop.is_some_and(|m| pr.stop < m) || self.opts.max_start.is_some_and(|m| pr.start > m) {
            return Ok(());
        }
        self.stats.programmes += 1;
        self.send(XmltvItem::Programme(pr))
    }
}

/// `en` matches `en`, `EN`, `en-GB`, `en_US` (primary subtags compared case-insensitively).
fn lang_matches(lang: &str, pref: &str) -> bool {
    let primary = |s: &str| s.split(['-', '_']).next().unwrap_or("").trim().to_owned();
    let (a, b) = (primary(lang), primary(pref));
    !a.is_empty() && a.eq_ignore_ascii_case(&b)
}

/// First text whose lang matches the preference list (in preference order), else the first.
fn pick_lang<'a>(items: &'a [LangText], prefs: &[String]) -> Option<&'a str> {
    prefs
        .iter()
        .find_map(|p| items.iter().find(|(l, _)| l.as_deref().is_some_and(|l| lang_matches(l, p))))
        .or_else(|| items.first())
        .map(|(_, t)| t.as_str())
}

/// Categories of one language (the preferred one if present, else the first category's) plus
/// language-less ones; duplicates dropped, file order kept.
fn pick_categories(items: Vec<LangText>, prefs: &[String]) -> Vec<String> {
    let chosen: Option<String> = prefs
        .iter()
        .find_map(|p| items.iter().find_map(|(l, _)| l.as_ref().filter(|l| lang_matches(l, p)).cloned()))
        .or_else(|| items.first().and_then(|(l, _)| l.clone()));
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for (l, t) in items {
        let keep = match (&l, &chosen) {
            (None, _) | (_, None) => true,
            (Some(l), Some(c)) => lang_matches(l, c),
        };
        if keep && !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// `YYYYMMDDhhmmss ±hhmm` or any leading prefix (`YYYYMMDDhhmm`, `YYYYMMDD`), optional space,
/// zone `±hhmm` / `Z` / `UTC` / `GMT` / `BST` (+0100); no zone → UTC. Hand-written, no alloc.
///
/// Also accepted: `±hh:mm` / `±hh`, and the common unambiguous abbreviations CET CEST WET WEST
/// EET EEST MSK EST EDT CDT MST MDT PST PDT. Out-of-range fields or unknown trailing text → `None`.
pub fn parse_time(s: &str) -> Option<Timestamp> {
    let b = s.trim().as_bytes();
    let n = b.iter().take_while(|c| c.is_ascii_digit()).count();
    if !(n == 4 || (6..=14).contains(&n) && n % 2 == 0) {
        return None;
    }
    let num = |i: usize, len: usize| b[i..i + len].iter().fold(0i64, |a, c| a * 10 + i64::from(c - b'0'));
    let field = |i: usize, default: i64| if n >= i + 2 { num(i, 2) } else { default };
    let (year, month, day) = (num(0, 4), field(4, 1), field(6, 1));
    let (hour, min, sec) = (field(8, 0), field(10, 0), field(12, 0));
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    let offset_s = parse_zone(&b[n..])?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + min * 60 + sec - offset_s)
}

/// Zone suffix → offset east of UTC in seconds. Empty → 0.
fn parse_zone(z: &[u8]) -> Option<i64> {
    let z = z.trim_ascii();
    if z.is_empty() {
        return Some(0);
    }
    if let [sign @ (b'+' | b'-'), rest @ ..] = z {
        let digits: [u8; 4] = match rest {
            [a, b, c, d] => [*a, *b, *c, *d],
            [a, b, b':', c, d] => [*a, *b, *c, *d],
            [a, b] => [*a, *b, b'0', b'0'],
            _ => return None,
        };
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let d = |i: usize| i64::from(digits[i] - b'0');
        let (h, m) = (d(0) * 10 + d(1), d(2) * 10 + d(3));
        if h > 14 || m > 59 {
            return None;
        }
        let off = h * 3600 + m * 60;
        return Some(if *sign == b'-' { -off } else { off });
    }
    const NAMED: &[(&str, i64)] = &[
        ("Z", 0),
        ("UT", 0),
        ("UTC", 0),
        ("GMT", 0),
        ("WET", 0),
        ("BST", 60),
        ("CET", 60),
        ("WEST", 60),
        ("CEST", 120),
        ("EET", 120),
        ("EEST", 180),
        ("MSK", 180),
        ("EST", -300),
        ("EDT", -240),
        ("CDT", -300),
        ("MST", -420),
        ("MDT", -360),
        ("PST", -480),
        ("PDT", -420),
    ];
    NAMED.iter().find(|(name, _)| name.as_bytes().eq_ignore_ascii_case(z)).map(|(_, min)| min * 60)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `xmltv_ns` (`s.e.p`, zero-based, each optionally `x/y`) or `onscreen` (`S03E14`, `3x14`).
///
/// `system` = None tries `xmltv_ns` when the value looks dotted, else the on-screen forms; other
/// systems (`dd_progid`, `crid`, …) carry no season/episode → `None`. Result is 1-based.
pub fn parse_episode_num(system: Option<&str>, value: &str) -> Option<EpisodeNum> {
    let v = value.trim();
    match system.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("xmltv_ns") => parse_xmltv_ns(v),
        Some(s) if s.eq_ignore_ascii_case("onscreen") || s.eq_ignore_ascii_case("SxxExx") => parse_onscreen(v),
        Some(_) => None,
        None if v.contains('.') => parse_xmltv_ns(v).or_else(|| parse_onscreen(v)),
        None => parse_onscreen(v),
    }
}

fn parse_xmltv_ns(v: &str) -> Option<EpisodeNum> {
    let mut parts = v.split('.');
    let mut part = || -> Option<Option<u32>> {
        let p = parts.next().unwrap_or("");
        let n = p.split('/').next().unwrap_or("").trim();
        if n.is_empty() {
            return Some(None);
        }
        n.parse::<u32>().ok().map(|x| x.checked_add(1))
    };
    let season = part()?;
    let episode = part()?;
    (season.is_some() || episode.is_some()).then_some(EpisodeNum { season, episode })
}

fn parse_onscreen(v: &str) -> Option<EpisodeNum> {
    use std::sync::LazyLock;

    use regex::Regex;
    static SE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bs(?:eason)?\s*\.?\s*(\d{1,4})\s*[-_.,/ ]?\s*e(?:p(?:isode)?)?\s*\.?\s*(\d{1,5})\b").expect("static regex")
    });
    static X: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b(\d{1,3})\s*[xX]\s*(\d{1,4})\b").expect("static regex"));
    static S: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bs(?:eason)?\s*\.?\s*(\d{1,4})\b").expect("static regex"));
    static E: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\be(?:p(?:isode)?)?\s*\.?\s*(\d{1,5})\b").expect("static regex"));
    let n = |c: &regex::Captures<'_>, i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    if let Some(c) = SE.captures(v).or_else(|| X.captures(v)) {
        return Some(EpisodeNum { season: n(&c, 1), episode: n(&c, 2) });
    }
    let season = S.captures(v).and_then(|c| n(&c, 1));
    let episode = E.captures(v).and_then(|c| n(&c, 1));
    (season.is_some() || episode.is_some()).then_some(EpisodeNum { season, episode })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Timestamp {
        days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s
    }

    #[test]
    fn civil_days_match_jiff() {
        for (y, m, d) in [(1970, 1, 1), (2000, 2, 29), (2026, 10, 9), (1969, 12, 31), (2100, 3, 1), (1600, 1, 1)] {
            let date = jiff::civil::date(y as i16, m as i8, d as i8);
            let want = date.to_zoned(jiff::tz::TimeZone::UTC).unwrap().timestamp().as_second();
            assert_eq!(days_from_civil(y, m, d) * 86_400, want, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn parse_time_table() {
        let base = t(2026, 10, 9, 20, 0, 0);
        assert_eq!(base, 1_791_576_000);
        let cases: &[(&str, Option<Timestamp>)] = &[
            ("20261009200000 +0000", Some(base)),
            ("20261009200000 +0100", Some(base - 3600)),
            ("20261009200000 -0530", Some(base + 5 * 3600 + 1800)),
            ("20261009200000 +01:00", Some(base - 3600)),
            ("20261009200000 +02", Some(base - 7200)),
            ("20261009200000+0100", Some(base - 3600)),
            ("20261009200000", Some(base)),
            ("  20261009200000  ", Some(base)),
            ("20261009200000Z", Some(base)),
            ("20261009200000 Z", Some(base)),
            ("20261009200000 UTC", Some(base)),
            ("20261009200000 gmt", Some(base)),
            ("20261009200000 BST", Some(base - 3600)),
            ("20261009200000 CEST", Some(base - 7200)),
            ("20261009200000 EST", Some(base + 5 * 3600)),
            ("202610092000", Some(base)),
            ("202610092000 Z", Some(base)),
            ("2026100920", Some(base)),
            ("20261009", Some(t(2026, 10, 9, 0, 0, 0))),
            ("202610", Some(t(2026, 10, 1, 0, 0, 0))),
            ("2026", Some(t(2026, 1, 1, 0, 0, 0))),
            ("20240229120000", Some(t(2024, 2, 29, 12, 0, 0))),
            ("20261231235959 +1400", Some(t(2026, 12, 31, 9, 59, 59))),
            ("19691231230000", Some(-3600)),
            // invalid
            ("", None),
            ("garbage", None),
            ("202", None),
            ("20261", None),
            ("2026100920000", None),
            ("202610092000000", None),
            ("20261309200000", None),
            ("20261000200000", None),
            ("20230229120000", None),
            ("20261009240000", None),
            ("20261009206000", None),
            ("20261009200000 +1500", None),
            ("20261009200000 +01x0", None),
            ("20261009200000 XYZ", None),
            ("20261009200000 +0100 junk", None),
        ];
        for (input, want) in cases {
            assert_eq!(parse_time(input), *want, "{input:?}");
        }
    }

    #[test]
    fn episode_numbers() {
        let e = |s: Option<u32>, ep: Option<u32>| Some(EpisodeNum { season: s, episode: ep });
        let cases: &[(Option<&str>, &str, Option<EpisodeNum>)] = &[
            (Some("xmltv_ns"), "1.4.0/1", e(Some(2), Some(5))),
            (Some("xmltv_ns"), "0.0.", e(Some(1), Some(1))),
            (Some("xmltv_ns"), "2/5.11/24.", e(Some(3), Some(12))),
            (Some("xmltv_ns"), " . 6 . ", e(None, Some(7))),
            (Some("xmltv_ns"), "3..", e(Some(4), None)),
            (Some("xmltv_ns"), "..0/2", None),
            (Some("xmltv_ns"), "", None),
            (Some("xmltv_ns"), "a.b.c", None),
            (Some("XMLTV_NS"), "0.1.0", e(Some(1), Some(2))),
            (Some("onscreen"), "S03E14", e(Some(3), Some(14))),
            (Some("onscreen"), "s3e4", e(Some(3), Some(4))),
            (Some("onscreen"), "S03 E14", e(Some(3), Some(14))),
            (Some("onscreen"), "Season 2 Episode 7", e(Some(2), Some(7))),
            (Some("onscreen"), "3x14", e(Some(3), Some(14))),
            (Some("onscreen"), "E12", e(None, Some(12))),
            (Some("onscreen"), "Ep. 5", e(None, Some(5))),
            (Some("onscreen"), "S2", e(Some(2), None)),
            (Some("onscreen"), "S00E01", e(Some(0), Some(1))),
            (Some("onscreen"), "Pilot", None),
            (Some("SxxExx"), "S01E02", e(Some(1), Some(2))),
            (Some("dd_progid"), "EP01234567.0012", None),
            (Some("crid"), "crid://x/1", None),
            (None, "1.4.0/1", e(Some(2), Some(5))),
            (None, "S01E02", e(Some(1), Some(2))),
        ];
        for (sys, v, want) in cases {
            assert_eq!(parse_episode_num(*sys, v), *want, "{sys:?} {v:?}");
        }
    }

    #[test]
    fn language_selection() {
        let items: Vec<LangText> = vec![(Some("cy".into()), "Newyddion".into()), (Some("en-GB".into()), "News".into())];
        assert_eq!(pick_lang(&items, &["en".into()]), Some("News"));
        assert_eq!(pick_lang(&items, &["de".into(), "cy".into()]), Some("Newyddion"));
        assert_eq!(pick_lang(&items, &[]), Some("Newyddion"));
        assert_eq!(pick_lang(&[], &["en".into()]), None);
        let cats: Vec<LangText> = vec![
            (Some("fr".into()), "Sport".into()),
            (Some("nl".into()), "Sport".into()),
            (Some("nl".into()), "Voetbal".into()),
            (Some("fr".into()), "Football".into()),
            (None, "Live".into()),
        ];
        assert_eq!(pick_categories(cats.clone(), &["nl".into()]), ["Sport", "Voetbal", "Live"]);
        assert_eq!(pick_categories(cats, &[]), ["Sport", "Football", "Live"]);
    }

    fn parse_str(xml: &str, langs: &[&str]) -> (Vec<XmltvItem>, XmltvStats) {
        let opts = XmltvOptions { preferred_langs: langs.iter().map(|s| s.to_string()).collect(), min_stop: None, max_start: None };
        let mut items = Vec::new();
        let stats = parse(
            xml.as_bytes(),
            &opts,
            &mut |i| {
                items.push(i);
                ControlFlow::Continue(())
            },
            &mut |_| {},
        )
        .expect("parse");
        (items, stats)
    }

    fn programmes(items: &[XmltvItem]) -> Vec<&ImportProgramme> {
        items.iter().filter_map(|i| if let XmltvItem::Programme(p) = i { Some(p) } else { None }).collect()
    }

    #[test]
    fn entities_spaces_and_unknown_refs() {
        let (items, _) = parse_str(
            r#"<tv><programme start="20261009200000" stop="20261009210000" channel="a">
                <title>  Barney &amp; Friends </title>
                <desc>&#72;&#x69; &lt;b&gt; &quot;q&quot; &apos;a&apos; &nbsp;x<![CDATA[ <raw> ]]></desc>
                <credits><actor>Not &amp; captured</actor></credits>
            </programme></tv>"#,
            &[],
        );
        let p = programmes(&items)[0];
        assert_eq!(p.title, "Barney & Friends");
        assert_eq!(p.description.as_deref(), Some("Hi <b> \"q\" 'a' &nbsp;x <raw>"));
    }

    #[test]
    fn missing_stop_filled_from_next_programme_of_same_channel() {
        let (items, stats) = parse_str(
            r#"<tv>
            <programme start="20261009200000" channel="a"><title>A1</title></programme>
            <programme start="20261009200000" stop="20261009203000" channel="b"><title>B1</title></programme>
            <programme start="20261009204500" channel="a"><title>A2</title></programme>
            <programme start="20261009210000" stop="20261009200000" channel="a"><title>A3 bad stop</title></programme>
            </tv>"#,
            &[],
        );
        let ps = programmes(&items);
        let get = |t: &str| ps.iter().find(|p| p.title == t).copied().expect(t);
        let base = t(2026, 10, 9, 20, 0, 0);
        assert_eq!(get("A1").stop, base + 45 * 60);
        assert_eq!(get("A2").stop, base + 3600);
        assert_eq!(get("A3 bad stop").stop, base + 7200, "stop <= start → treated as missing (EOF default 1 h)");
        assert_eq!(get("B1").stop, base + 1800);
        assert_eq!(stats.programmes, 4);
    }

    #[test]
    fn empty_and_self_closing_elements() {
        let (items, stats) = parse_str(
            r#"<tv><channel id="x"/><channel id=""><display-name>No id</display-name></channel>
            <programme start="20261009200000" stop="20261009210000" channel="x"/>
            <programme start="20261009210000" stop="20261009220000"><title>no channel</title></programme></tv>"#,
            &[],
        );
        assert_eq!(stats.channels, 1);
        assert_eq!(stats.programmes, 1);
        assert_eq!(stats.skipped, 1);
        assert!(matches!(&items[0], XmltvItem::Channel(c) if c.xmltv_id == "x" && c.display_names.is_empty()));
        assert_eq!(programmes(&items)[0].title, "");
    }

    #[test]
    fn malformed_xml_is_a_parse_error() {
        let opts = XmltvOptions { preferred_langs: vec![], min_stop: None, max_start: None };
        let r = parse(
            &b"<tv><programme start=\"2026\" channel=\"a\"><title>x</titl></programme></tv>"[..],
            &opts,
            &mut |_| ControlFlow::Continue(()),
            &mut |_| {},
        );
        assert!(matches!(r, Err(Error::Parse { .. })), "{r:?}");
    }
}
