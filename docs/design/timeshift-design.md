# Rolling live rewind

Implemented in `player/hls/` and the local player controller on `feat/netflix-player`.
The HTTP acceptance tests still need an outside-sandbox run. The sandbox prohibits
localhost sockets; a filesystem test exercises the actual store/playlist and real mpv.

## Decision and transport gate

The user's outside-sandbox HTTP measurements of the original transport experiment were
**+0.022–0.038 seconds for live HLS**, versus about **+1.45 seconds for paced raw TS**.
Those measurements selected the split implementation; they are not measurements of the
new production fetcher. `zap_gate_http` remains as the original remux experiment.
`zap_gate_http_provider_hls` measures the production recorder, including listener/reader
startup, against direct HTTP playback, with a strict **0.15-second added-delay limit**,
three alternating-order pairs and no extra scheduling allowance.

* HTTP(S) live URLs ending in `.m3u8` use Tuner's rolling disk recorder. Query parameters
  do not affect routing. Xtream `live_format=auto` supplies these URLs.
* Raw TS and other live inputs retain the mpv-cache implementation from 523a89f: up to
  30 minutes of append-only disk cache, then a rolling two-minute memory window after
  the 2 GiB write limit or the free-space reserve. No TS remux has been introduced.
* VOD, catch-up and casting retain their transports.

HLS bytes are downloaded unchanged: no ffmpeg process, probe, transcoder or segmenter.
The recorder starts with the newest complete provider segment, then polls at half the
target duration (100 ms–5 s). Startup does not wait for several segments to accumulate.
mpv sees only a token-protected loopback URL. After initial probing, an unchanged local
playlist request waits for the next publication, avoiding a whole extra HLS polling
interval when the decoder races the fetcher. Recording is independent of pause,
playback position, demuxer read-ahead and video selection.

## Ownership and shutdown

Every picture has its own recorder, local listener, store and ordinary connection lease.
The fetcher serializes **all** upstream requests: playlists, segments, AES keys, init maps
and an external audio rendition. It reuses the cast upstream's `Net` runtime and actual
socket-close acknowledgement, header construction and variant selection. A finished
response is not sufficient permission to open another socket. Idle pooling is disabled.

The controller acquires its shared budget lease before starting the recorder. mpv's
END_FILE closes a local reader and does not release that provider lease. On stop, zap,
cast handover or exit, the recorder is cancelled and joined after its provider socket
closes; only then is its lease released. Normal upstream retries and local seeks retain
that lease. Before the first successful play, upstream errors also reach the existing
controller failure/copy-fallback and connection-limit-learning policy. Casting itself
has no transport changes; its upstream helpers only gained crate-level visibility.

## Storage and the bound

Each recorder uses a private **0700** directory under `cache/live-rewind/`. Media file
names are numeric counters with fixed format extensions. Keys and init maps use counters
too. URLs, credentials, provider titles, comments and arbitrary provider tags are neither
logged nor used in names. Local playlists are rendered in memory from allowed metadata;
EXTINF titles are discarded. Requests and errors never log raw reqwest error strings.

Before allocating a new media segment, the store removes segments ending at or before
`new_edge - 1800`. The segment straddling that cutoff stays. Thus media storage covers
30 minutes and is bounded by **bitrate × (1800 seconds + one source segment) / 8**, using
actual segment durations, plus referenced init maps and 16-byte AES keys. There is no
extra deletion backlog. An in-flight response is bounded in RAM (128 MiB); it does not
create another unbounded disk spool. Unreferenced keys/maps are pruned with segments.
The memory and request caps can reject exceptionally large provider resources.

A local HTTP response reads a segment into bounded memory before sending it. A slow
reader cannot keep an unlinked disk file alive. The local listener caps concurrent
clients. A new channel starts empty, stop/zap/exit remove the old directory, and startup
removes folders belonging to processes that no longer exist. Pictures do not delete
each other's active folders.

Below **3,000,000,000 free bytes**, or after a disk write failure, the recorder logs once
and permanently switches to memory for that channel. It unlinks the disk recording and
keeps only a short, byte-bounded local segment supply; the *same provider reader* keeps
feeding mpv. The exposed rewind range then comes from mpv's two-minute packet cache
(128 MiB forward and 128 MiB backward; bitrate may shorten it). Regaining free space does
not restart disk allocation mid-channel. An unavailable directory uses the same fetcher
with memory-only storage. If the loopback listener cannot start, playback reports an
error; it never hands the provider URL to mpv.

## Local playlist and real mpv seeking

The private media playlist has advancing `EXT-X-MEDIA-SEQUENCE`, target duration,
discontinuity sequence, numeric resource URIs, explicit AES IVs, and **no ENDLIST**
unless the source ends or the view reaches a timestamp-discontinuity boundary. It carries `EXT-X-PLAYLIST-TYPE:EVENT` to enable libavformat's HLS
seek path. This is deliberately a **private mpv interface**, not an RFC-compliant public
EVENT publication: real EVENT playlists must not remove old entries.

[FFmpeg's HLS demuxer](https://github.com/FFmpeg/FFmpeg/blob/n9.0/libavformat/hls.c)
marks ordinary sliding live playlists unseekable. A short real-mpv probe could seek in a
trimmed EVENT playlist. An initial long probe mistakenly requested an already-pruned
point: its six-second initial lookahead left less than 15 seconds behind playback in a
20-second store. The corrected fixture starts with one segment, as production does.
The implementation uses the store's retained index and local views rather than depending
on a long-distance FFmpeg seek in a possibly stale decoder playlist.

For every disk-buffer seek, Tuner clamps the requested position to its recorded range,
opens a fresh **localhost-only view beginning at the containing segment**, and supplies
`live_start_index=0` and `start=<offset within that segment>`. The view continues to grow
and roll as recording advances; it is not a frozen VOD snapshot. mpv seeks only within
the first segment of that view. The controller translates its relative `time-pos` back
to the recording timeline using the origin captured when the first playlist is served.
This handles old decoder playlists after long pauses and seeks on either side of a
provider timestamp discontinuity. It performs no provider reopen, recording restart,
lease transfer, history push or user-visible channel-generation change.

Targeted views answer every ready playlist request immediately, including unchanged
reloads. FFmpeg's `reload_playlist` refreshes an unfinished playlist before opening the
next segment when its reload interval expires, **even if that segment is already listed**.
Holding those requests until a new publication can therefore freeze recorded playback
during an upstream pause. The 250 ms initial-probe grace is only used to pace the original
live reader; it is not evidence that a seek reader has consumed the recording. Opening
a replacement view also wakes pending reloads so requests for the old view can finish.
Debug logs under `tuner::player::hls` record numeric view/resource IDs, timeline/sequence,
waits, response sizes and handler elapsed time, never URLs or tokens.

A real-mpv probe also found that decoding straight across a provider timestamp reset
can stall/reset `time-pos`. Each local view therefore stops before the next discontinuity
with ENDLIST. The controller hands off to the next local view at that boundary (on its
tick or EOF), preserving the recorded timeline, pause state, history and connection.
Both sides remain available for rewind. This boundary is remembered even if the old
segments expire while a reader is paused.

The oldest point, Live / lag label, −10/+10, oldest-point toast and Back to live use the
recorded timeline. The exposed edge has a half-second guard so exact seeks still find
video. Pause preserves the point while retained. When it expires, the controller opens
a view at the oldest retained point and leaves mpv paused. In memory mode the original
cache-seek and byte-full paused-reader maintenance apply.

## Playlist variations and reconnects

* Masters choose the highest-bandwidth variant through the cast relay's default mpv-like
  selector. A selected variant's separate default audio rendition (or first if none is
  default) is also cached, through the same serialized reader. The local master contains
  only the chosen video and audio. This does not implement adaptive switching or retain
  every alternative audio/subtitle rendition.
* Relative paths resolve against the final redirected playlist URL. `User-Agent`,
  Referer, Origin and extra headers, including Cookie, use the existing header handling
  on playlists, segments, maps and keys.
* AES-128 identity keys are cached locally. Implicit provider-sequence IVs become explicit
  IVs before local sequence numbers are changed. Unsupported encryption is refused.
* Byte-range segments and init maps are materialized as complete local resources. A
  provider ignoring Range is sliced after a bounded download. GAP segments are skipped;
  sequence holes and source discontinuities are marked in the local timeline.
* Every retry obtains a fresh media playlist rather than retrying a captured expired
  segment URL forever. A 403/404 also refreshes the master/variant selection, preserving
  the sequence cursor if the selected rendition count is unchanged.
* Retry timing reuses the player's sticky reconnect policy. Continuing sequences append
  to the existing buffer. A backwards provider media sequence starts a new empty epoch;
  the controller reopens its local reader at the new edge. Old files are removed.

## Hidden windows and multi-view

A hidden picture playing at live suspends decoding (`vid=no`) after the existing render
watchdog delay. Paused or rewound pictures retain/restore video selection. The HLS
recorder continues in every case. Raw TS retains the old cache transport with the same
hidden-window policy. Missing video until a keyframe after rewinding through a stretch
watched with decoding suspended is accepted. Each picture applies the policy separately.

## Validation and outside commands

Unit tests cover playlist parsing, sequences, key IV metadata, byte ranges, gaps,
discontinuities, pruning and the duration bound, numeric/private output, low-space
transition, routing TS away from the recorder and actual mpv `vid` selection by the
controller. The existing real paced-TS pipe test remains applicable without sockets.

Opt-in filesystem tests use real mpv with backward packet caching disabled:

```sh
cargo test --lib rolling_playlist_real_mpv --locked -- --ignored --nocapture
cargo test --lib local_view_real_mpv --locked -- --ignored --nocapture
```

The first runs a 20-second store with two-second segments for 64 seconds and checks
rolling files and a decoded 15-second rewind. The second checks local seek views on
both sides of a timestamp reset and while paused. Neither substitutes for HTTP latency
or provider-connection acceptance.

Run these **outside the sandbox**, with mpv, ffmpeg, openssl and at least 3 GB free:

```sh
cargo test --test timeshift_transport zap_gate_http_provider_hls --locked -- --ignored --nocapture
RUST_LOG=tuner::player::hls=debug cargo test --test timeshift_transport recorded_hls_http_mpv --locked -- --ignored --nocapture
cargo test --test timeshift_transport rolling_hls_http_mpv --locked -- --ignored --nocapture
cargo test --test timeshift_transport provider_hls_master_keys_headers_and_expiring_urls --locked -- --ignored --nocapture
cargo test --test player_mpv live_rewind_cache_seek_pause_clamp_and_channel_change --locked -- --ignored --nocapture
cargo test --test player_mpv live_hls_hidden_playing_suspends_but_paused_keeps_decoding --locked -- --ignored --nocapture
cargo test --test player_mpv live_hls_rolling_controller_pause_expiry_and_zap --locked -- --ignored --nocapture
cargo test --test player_mpv live_hls_discontinuity_keeps_the_recording_and_timeline --locked -- --ignored --nocapture
```

The rolling HTTP test runs more than 60 seconds, injects a midstream outage, checks real
video after −15/−10/+10/live seeks with mpv's backward cache disabled, pauses beyond the
20-second window, and checks file/byte bounds, actual provider socket overlap and zap
cleanup. The encrypted master test checks AES decoding, credential headers and replacing
an expired segment URL without discarding history. Additional controller tests use the
20-second window to check automatic pause expiry, the actual UI commands and lease-safe
zap, and continuing playback/rewind across a timestamp reset. The original `zap_gate_http` remux
comparison is retained, including its historical 0.5 s + 0.1 s allowance; the production
HLS gate above has the new strict 0.15 s limit.

The short `recorded_hls_http_mpv` regression freezes an unfinished provider playlist after
recording 24 seconds. Two seeks, including one close to a segment boundary, must each
advance six seconds with output-picture changes, unchanged recording bounds and no
additional provider segment requests. Backward packet caching remains disabled. This
proves old playback does not need the next live publication or an artificial ENDLIST.

Seek assertions wait for the new FILE_LOADED before accepting PLAYBACK_RESTART, to exclude
a restart still queued by the replaced reader. The target tolerance remains 0.6 seconds.
Old-media seeks require multiple seconds of playback; edge seeks allow a bounded cache
refill rather than assuming 300 ms is sufficient. A real-mpv filesystem probe reproduced
the original timing failure: a 19.5-second target in a 20-second recording decoded at
19.52, then stayed there for 300 ms with `paused-for-cache=true` and zero cache duration.
An old target of 3.3 decoded at 3.32 and advanced to 3.64 in the same interval. Segment
duration is not HTTP transfer time: the fixtures are unthrottled and local blobs are
served from already-recorded bytes. After publishing the next filesystem segment, the
edge probe advanced 0.2 seconds in 2.27 seconds; old seeks advanced six seconds in about
six seconds, with changing output pictures. The socket-free handler regression failed
before the fix and passed after it. These results do not substitute for rerunning the
HTTP tests outside the sandbox.

Local results:

* `cargo check --locked` and `cargo check --all-targets --locked`: passed; HTTP tests also
  built with `cargo test --test timeshift_transport --locked --no-run`.
* Seven HLS unit tests passed, including the memory-only fallback and waiting for a new
  playlist publication without delaying initial probes.
* Real mpv 0.41, 64-second filesystem run: a 15-second rewind decoded video at the requested
  point (62.08 → 47.08 seconds), oldest retained point 46 seconds, **10 segment files**.
* Real mpv local views decoded 7.320/3.320/15.320/5.520 seconds for requested
  7.300/3.300/15.300/5.500; the last remained paused. The discontinuity handoff passed.
* The controller's actual `vid` property test and the existing real paced-TS pipe
  seek/pause/clamp/zap test passed.
* `cargo fmt --check`: passed. Exactly one
  `cargo clippy --all-targets --locked -- -D warnings` ran and passed. The final review's
  memory-only directory fallback and playlist-polling changes followed that run; they
  passed check/unit/fmt verification. Clippy was not repeated, per the one-run instruction.

HTTP acceptance, including the strict 0.15-second gate and the complete controller matrix,
is **not claimed** from this sandbox. Run the commands above outside it.
