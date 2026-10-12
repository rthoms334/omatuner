# Library load performance (perf track)

The field catalogue's library build went from 1.2 s to 3.1 s after the shared category rule and
title-language hiding (PR #15). That database was deleted for privacy, so the work here runs on
a synthetic catalogue of the same shape.

## The fixture

`benches/support/catalogue.rs` builds the catalogue through the importer (`store::import`, in
2 000-row batches; `finish_catalogue` writes the title match columns):

- two Xtream-like playlists with 80k channels, 300k movies and 40k series between them, plus
  800k episodes, all listed as an M3U playlist imports them;
- 2 500 categories:
  - tagged with every separator style (`UK | …`, `DE: …`, `AR ✪ …`, `EX-YU ❖ …`);
  - untagged Latin names;
  - untagged names in Arabic, Greek, Hebrew, Hangul, Thai, Cyrillic and Han script, so script
    detection runs;
- titles carrying their own market prefix (`EN - …`, `DE ★ …`, `AR-SUBS - …`, `MULTI - …`,
  `SW| …`) or none, with the same titles on both playlists so they stack;
- 300 favourites, about 300 movies and 3 800 episodes with progress;
- the English-only shared rule, which hides titles in other languages automatically.

The generation is deterministic.

To run it:

- `cargo bench --bench startup -- catalogue` runs only this part.
- `TUNER_BENCH_CATALOGUE=<file>` seeds the database once and reuses it.

The bench measures three variants on a fresh reader each run, in rotating rounds:

- no rule;
- the rule with titles kept, which is the pre-#15 behaviour;
- the default rule.

For each variant it prints the best and median total, and the best time of each step from
`library::load_profiled`.

## Where the time went (batch/base, this machine)

The default rule took about 650 ms (best of 5, quiet machine). Under load the median reached 2.4 s.

| Step | ms |
|---|---|
| movies (`load_vod`) | 520 |
| channels | 186 |
| series | 64 |
| title languages | 13–31 |
| stacks | 11 |
| groups, including script detection on 2.5k categories | 5 |

The new #15 code is cheap: script detection, title-language checks and the per-category
recounts together cost under 40 ms.

The cost was in reading the item tables:

- Each query sorted every row with every selected column through SQLite's sorter
  (`ORDER BY s.sort_index, …, o.user_order, provider_order`). That sorter holds about 100 MB in
  `temp_store=MEMORY` at this size, and the rows of categories the rule hides were sorted too.
- Each query probed `item_override` once per row.
- Group links went through a `HashMap<item, Vec<(group, primary)>>`, which means 300k small
  vectors and hash lookups per link.
- Hidden rows still had their keys copied, and `match_lang` was interned through `to_vec()`,
  which allocates once per row.

The median under load (2.4 s against a best of 0.8 s) points to the sorter's memory as the
likely amplifier on a busy machine.

## What changed (`src/store/library.rs`)

- **Item tables are read in id order, without a sort or override join in SQL.** Rows of disabled
  playlists are skipped in Rust (`SourceOrder`). Display order (playlist, user order first,
  provider order, id) is restored in memory by sorting a small key per item (`display_order` and
  `permute`). The sort is skipped when the keys are already in order, which is the usual case
  after an import.
- **Channels keep their SQL select.** The B8 `COALESCE(o.shift_min, ce.shift_min,
  c.tvg_shift_min)` and the override join stay as they were. Only the `JOIN source` and the
  `ORDER BY` changed.
- **`Links`.** Memberships are loaded once, each with its group's flags resolved, and sorted by
  item id. They are merged with the id-ordered scan through a cursor, so `effective_groups` needs
  no hash lookups and has a single-group fast path.
- **`Overrides`.** The user's movie and series overrides (few rows) are looked up by playlist and
  borrowed key, with no join.
- **Hidden titles are dropped before the rest of the row is read.** The key is borrowed
  (`text()`), `match_lang` interning no longer allocates per row, the split, pin and join sets are
  skipped when empty, and `ExplicitIndex::clean` is no longer hashed for every movie and series row.
- **Title languages.** The protected keys (favourites and progress) are looked up by `&str`
  without cloning each key. Market detection is cached per interned `match_lang` allocation, so
  it costs a pointer hash and no string hash.
- **`load_profiled` / `LoadProfile`.** These time the steps. `load_with` logs the steps at `info`
  when a build takes 1 s or more, so the next field report shows where the time goes.

The output is unchanged. A canonical digest of the snapshot was identical before and after on
three databases and both visibilities:

- channels, hidden channels, members, VOD vectors, category items, both stacks' membership, tops
  and metadata, explicit refs and counts;
- the databases were the fixture; the fixture with 2 342 overrides (renames, hidden, locked,
  user order), explicit categories and items, out-of-order provider order, swapped playlist
  order and 2 079 title links; and the same with one playlist disabled.

## Results

Interleaved A/B on the same database, best of 5 on fresh readers, three rounds:

| Variant | batch/base | perf | Change |
|---|---|---|---|
| default English rule | 650 ms | 344 ms | −47 % |
| no rule | 735 ms | 417 ms | −43 % |

Bench steps after the change (default rule):

| Step | ms |
|---|---|
| channels | 101 |
| movies | 179 |
| series | 22 |
| title languages | 27 |
| stacks | 10 |

New gates in `benches/startup.rs`:

- the catalogue load with the rule takes under 550 ms (best of 7);
- the default rule costs at most 1.25 × the rule with titles kept.

## Follow-up: channels beside the VOD (gaps/perf)

The channels are now read on a second reader, on their own thread, while the first reader
reads the movies and series (DESIGN §5.4). Both readers read one snapshot. The build opens a
read transaction on its connection. Before that, the second connection reads
`PRAGMA data_version`, and it reads it again after it starts its own transaction. If a write
committed in between, the value differs, and the build tries again (3 times) or reads
everything on one connection. `store::open_reader(path)` is now `pub(crate)`. Interleaved A/B
on the same catalogue database, best of 7, same machine:

| Variant | before | after |
|---|---|---|
| default English rule | 355 ms | 251 ms |
| rule, titles kept | 330 ms | 229 ms |
| no rule | 430 ms | 333 ms |
| 100k-channel profile (median of 5) | 208 ms | 184 ms |

The `channels` step is now the wait for the thread after the VOD steps (0 ms on the catalogue).
On the 100k profile, the time until the library reaches the app fell from about 900 ms to
about 650 ms.

## Not done (possible follow-ups)

- **`stacks::dominant_market`** (owned by stacks) still hashes a `GroupId` for each title and
  group. It costs a few milliseconds here.

## Tests

- `store::library::tests::load_puts_items_in_display_order_with_overrides` covers the order that
  is now rebuilt in memory and the override lookup:
  - two enabled playlists by `sort_index` (not id) and one disabled playlist;
  - movie ids out of provider order, a user-ordered movie first, a renamed and locked movie, and a
    hidden movie;
  - channels with a user order, and category item order.

  The test fails if the in-memory sort is skipped.
- `tests/store_catalogue.rs::library_load_merges_overrides_and_skips_disabled_sources` is
  unchanged and still covers the channel overrides and hidden categories.
