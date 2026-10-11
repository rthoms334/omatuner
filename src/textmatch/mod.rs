//! One folding and matching stack (B3) shared by Following, the spoiler guard and Guide fixes:
//! each file has one owner.
//!
//! | File | Contents | Owner |
//! |---|---|---|
//! | `fold.rs` | `fold`: ASCII fast path, `deunicode` ≥ 0x80, "&" → "and", "vs"/"vs." → "v" | F-AB |
//! | `terms.rs` | `TermSet`: aho-corasick LeftmostLongest, whole-token check | F-AB |
//! | `fixture.rs` | `parse_fixture` ("A v B", team-suffix exclusions) | F-AB |
//! | `airing.rs` | `airing_kind`: Live / New / Repeat / Highlights / Unknown | F-AB |
//! | `event.rs` | `event_key` (Following's ledger and `spoiler_seen`) | F-AB |
//! | `scores.rs` | `neutralise_scores`, the result-verb list | S1 |
//! | `timeshift.rs` | `marker`: "+1" / "+2" channel names | G-core |
//!
//! Batch base: signatures with safe placeholder bodies and the designs' test tables marked
//! `#[ignore]` — the owner un-ignores them with the real bodies.

pub mod airing;
pub mod event;
pub mod fixture;
pub mod fold;
pub mod scores;
pub mod terms;
pub mod timeshift;
