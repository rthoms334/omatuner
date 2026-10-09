//! Helpers shared by the repository modules. Owner: WP-02 (moved out of the frozen
//! `store/mod.rs` so WP-02 can grow it freely).
//!
//! Write rules (DESIGN §6.3/§7.2): upserts are `INSERT … ON CONFLICT(…) DO UPDATE`; never
//! `INSERT OR REPLACE` on a table referenced by a foreign key.

use crate::model::{Fingerprint, ItemKind};

pub fn fp_to_json(fp: &Fingerprint) -> String { serde_json::to_string(fp).unwrap_or_else(|_| "{}".into()) }
pub fn fp_from_json(s: &str) -> Fingerprint { serde_json::from_str(s).unwrap_or_default() }
pub fn kind_str(k: ItemKind) -> &'static str { k.as_str() }
