//! The guide-channel catalogue (norms, trigram postings), moved out of the mapping screen: one
//! shared, lazily built `Arc<Catalog>` keyed by the guides' active generations and dropped
//! after 2 min unused (screen, job and agent share it). OWNER: G-core.

/// The shared catalogue (G-core fills it).
#[derive(Debug, Default)]
pub struct Catalog {
    /// The guides' active generations it was built from.
    pub generations: Vec<(crate::model::EpgSourceId, i64)>,
}
