//! Nerd Font (Material Design, `nf-md-*`) glyphs rendered from the primary monospace font —
//! Omarchy uses the same font for text and icons (no separate icon set). All code points were
//! verified present in JetBrainsMono Nerd Font via `fc-query` charset. Implemented.

pub const HOME: &str = "\u{f02dc}";
pub const LIVE_TV: &str = "\u{f0502}";
pub const GUIDE: &str = "\u{f00f0}";
pub const MOVIES: &str = "\u{f0381}";
pub const SERIES: &str = "\u{f07f4}";
pub const FILMSTRIP: &str = "\u{f0231}";
pub const SEARCH: &str = "\u{f0349}";
pub const SETTINGS: &str = "\u{f0493}";
pub const FAVOURITE: &str = "\u{f04ce}";
pub const FAVOURITE_OFF: &str = "\u{f04d2}";
pub const FAV_LIST: &str = "\u{f0857}";
pub const HEART: &str = "\u{f02d1}";
pub const LOCK: &str = "\u{f033e}";
pub const UNLOCK: &str = "\u{f033f}";
pub const HIDDEN: &str = "\u{f0209}";
/// Explicit content (parental control hides it): the eye-off of hiding.
pub const EXPLICIT: &str = HIDDEN;
pub const PLAY: &str = "\u{f040a}";
pub const PAUSE: &str = "\u{f03e4}";
pub const STOP: &str = "\u{f04db}";
pub const NEXT: &str = "\u{f04ad}";
pub const PREVIOUS: &str = "\u{f04ae}";
pub const SKIP_BACK: &str = "\u{f0d6a}";
pub const SKIP_FORWARD: &str = "\u{f0d3d}";
pub const VOLUME: &str = "\u{f057e}";
/// Speaker by level (nf-md-volume_low / nf-md-volume_medium; `VOLUME` is volume_high).
pub const VOLUME_LOW: &str = "\u{f057f}";
pub const VOLUME_MEDIUM: &str = "\u{f0580}";
pub const MUTED: &str = "\u{f0581}";
pub const AUDIO_TRACK: &str = "\u{f0597}";
pub const SUBTITLES: &str = "\u{f0a16}";
pub const ASPECT: &str = "\u{f0e5c}";
pub const FULLSCREEN: &str = "\u{f0293}";
pub const MINI_PLAYER: &str = "\u{f0e57}";
/// Pop the player out (nf-md-open_in_new) / back in to the full window (nf-md-arrow_collapse).
pub const POP_OUT: &str = "\u{f03cc}";
pub const POP_IN: &str = "\u{f0615}";
pub const HISTORY: &str = "\u{f02da}";
pub const CATCHUP: &str = "\u{f0459}";
pub const LIVE_DOT: &str = "\u{f0003}";
pub const REMINDER: &str = "\u{f009a}";
pub const REFRESH: &str = "\u{f0450}";
pub const SYNC: &str = "\u{f04e6}";
pub const WARNING: &str = "\u{f0026}";
pub const OFFLINE: &str = "\u{f05aa}";
pub const NO_VIDEO: &str = "\u{f0568}";
pub const PLAYLIST: &str = "\u{f0411}";
pub const INFO: &str = "\u{f02fd}";
pub const CHEVRON_LEFT: &str = "\u{f0141}";
/// Leave the player (nf-md-arrow_left).
pub const ARROW_LEFT: &str = "\u{f004d}";
pub const CHEVRON_RIGHT: &str = "\u{f0142}";
/// An expanded row (nf-md-chevron_down).
pub const CHEVRON_DOWN: &str = "\u{f0140}";
/// "Always play from" a copy (DESIGN §29.14).
pub const PIN: &str = "\u{f0403}";
pub const CHECK: &str = "\u{f012c}";
/// Tick-list boxes (nf-md-checkbox_marked / nf-md-checkbox_blank_outline).
pub const CHECKBOX_ON: &str = "\u{f0132}";
pub const CHECKBOX_OFF: &str = "\u{f0131}";
pub const CLOSE: &str = "\u{f0156}";
pub const ADD: &str = "\u{f0415}";
pub const FOLDER: &str = "\u{f024b}";
pub const SOURCE_URL: &str = "\u{f01e7}";
/// Countries and languages (the category filter): nf-md-earth, as `SOURCE_URL`.
pub const EARTH: &str = "\u{f01e7}";
pub const SOURCE_XTREAM: &str = "\u{f048b}";
pub const SOURCE_FILE: &str = "\u{f0214}";
pub const KEYBOARD: &str = "\u{f030c}";
pub const RADIO: &str = "\u{f0439}";
pub const MUSIC: &str = "\u{f075a}";
pub const SORT: &str = "\u{f04ba}";
pub const TAG: &str = "\u{f04f9}";
pub const BUSY: &str = "\u{f051f}";
/// nf-md-backup_restore.
pub const BACKUP: &str = "\u{f006f}";
pub const DISPLAY: &str = "\u{f0379}";
pub const THEME: &str = "\u{f03d8}";
pub const REORDER: &str = "\u{f01dd}";
pub const ACCOUNT: &str = "\u{f0306}";
/// A playlist's colour dot (U+25CF, in JetBrains Mono): scope tabs, the playlist picker.
pub const DOT: &str = "\u{25cf}";
/// A page's menu (nf-md-dots_horizontal).
pub const MORE: &str = "\u{f01d8}";

/// Icons offered when creating a playlist source (UHF "choose an icon").
// The batch features' glyphs (base-B placeholders on existing glyphs; each owning UI track may
// point its own at a better one: queue = Q-ui, follow = F-C, spoiler = S-ui, multi-view = MV-2,
// sleep = rui, guide fixes = G-ui).
pub const UP_NEXT: &str = PLAYLIST;
/// Up next's next item in the player (nf-md-skip_next_circle): not the next channel's `NEXT`.
pub const QUEUE_NEXT: &str = "\u{f0661}";
pub const FOLLOW: &str = REMINDER;
pub const SPOILER: &str = HIDDEN;
pub const MULTI_VIEW: &str = MINI_PLAYER;
pub const SLEEP: &str = REMINDER;
pub const GUIDE_FIX: &str = GUIDE;
/// An AI agent (Settings ▸ AI agents; nf-md-robot).
pub const AGENT: &str = "\u{f06a9}";
/// Copy to the clipboard (nf-md-content_copy).
pub const COPY: &str = "\u{f018f}";

pub const SOURCE_ICONS: &[&str] = &[PLAYLIST, LIVE_TV, SOURCE_XTREAM, SOURCE_URL, MOVIES, SERIES, RADIO, MUSIC, FAVOURITE, HEART, FOLDER, TAG];
