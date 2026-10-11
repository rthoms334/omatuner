//! Country / language tags of category names (DESIGN §28.2): detection, display names, the
//! defaults of the category filter, and per-tag summaries for the "Choose what to show" picker.
//!
//! Detection reads the provider's RAW category name (`grp.raw_name`): smart-name cleanup drops
//! the prefix from the display name and the group key squashes the separators away.

use std::collections::BTreeSet;

use crate::model::{CategoryTag, ItemKind, TagKind};

/// Countries whose categories count as English-language by default (with `lang:en`).
pub const ENGLISH_COUNTRIES: &[&str] = &["GB", "US", "CA", "AU", "IE", "NZ"];

/// The tag of a raw category name, if it has one (grammar, synonyms and non-tags: DESIGN
/// §28.2). `"UK | Sports"`, `"|UK| Movies"`, `"[UK] Kids"`, `"UK: News"`, `"🇬🇧 Sports"` →
/// `GB`; `"EN ▎Movies"`, `"ENGLISH"` → `lang:en`; `"4K | Sports"`, `"VIP"`, `"CNN: Live"` → none.
pub fn detect_tag(raw: &str) -> Option<CategoryTag> {
    let _ = raw;
    todo!("P6-core")
}

/// Is this one of the English-language tags (pre-ticked by default)?
pub fn is_english(tag: &CategoryTag) -> bool {
    match tag.kind() {
        TagKind::Country => ENGLISH_COUNTRIES.contains(&tag.code()),
        TagKind::Language => tag.code() == "en",
        TagKind::Region => false,
    }
}

/// Default keep-list (DESIGN §28.6): the user's country (ISO code from
/// `presets::detect_country`) and its main language, plus every English-language tag — whether
/// or not the playlist has them yet, so English categories added later are shown.
pub fn default_keep(country: Option<&str>) -> BTreeSet<CategoryTag> {
    let mut keep: BTreeSet<CategoryTag> = ENGLISH_COUNTRIES.iter().map(|cc| CategoryTag::country(cc)).collect();
    keep.insert(CategoryTag::language("en"));
    if let Some(cc) = country.filter(|cc| cc.len() == 2) {
        keep.insert(CategoryTag::country(cc));
        if let Some(lang) = country_language(cc) {
            keep.insert(CategoryTag::language(lang));
        }
    }
    keep
}

/// Main language of a country, for [`default_keep`] (`"FR"` → `"fr"`); `None` when unknown.
pub fn country_language(iso: &str) -> Option<&'static str> {
    let _ = iso;
    None // P6-core: small table (DESIGN §28.6)
}

/// Short code for chips and lists: `GB` → `"UK"`, `lang:en` → `"EN"`, `region:latam` →
/// `"LATAM"`.
pub fn tag_short(tag: &CategoryTag) -> String {
    match tag.kind() {
        TagKind::Country if tag.code() == "GB" => "UK".to_owned(),
        _ => tag.code().to_ascii_uppercase(),
    }
}

/// Display name: `GB` → "United Kingdom", `lang:ar` → "Arabic", `region:exyu` →
/// "Ex-Yugoslavia". Falls back to [`tag_short`].
pub fn tag_label(tag: &CategoryTag) -> String {
    match tag.kind() {
        TagKind::Country => crate::sources::presets::country_name(tag.code()).unwrap_or_else(|| tag_short(tag)),
        // P6-core: language and region names (DESIGN §28.2 tables)
        TagKind::Language | TagKind::Region => tag_short(tag),
    }
}

/// One entry of the picker: a tag (or the untagged categories) with what it covers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagSummary {
    /// `None` = the categories without a tag ("No country tag").
    pub tag: Option<CategoryTag>,
    pub live_groups: u32,
    pub movie_groups: u32,
    pub series_groups: u32,
    /// Sums of the groups' `item_count` (an item in two groups counts twice).
    pub channels: u32,
    pub movies: u32,
    pub series: u32,
}

impl TagSummary {
    pub fn items(&self) -> u32 {
        self.channels + self.movies + self.series
    }
    pub fn groups(&self) -> u32 {
        self.live_groups + self.movie_groups + self.series_groups
    }
}

/// Per-tag summary of one source's categories.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CategorySummary {
    /// Tagged entries by `items()` descending (ties: label), then the untagged entry (present
    /// only when the source has untagged categories).
    pub tags: Vec<TagSummary>,
    /// Some categories were imported before raw names were stored (smart names on): their
    /// tags are unknown until the playlist is refreshed.
    pub needs_refresh: bool,
}

impl CategorySummary {
    /// Number of distinct tags (the untagged entry not counted). The onboarding step is shown
    /// when this is ≥ 2.
    pub fn tag_count(&self) -> usize {
        self.tags.iter().filter(|t| t.tag.is_some()).count()
    }
    /// Every tag present (what `CategoryFilter::known` is set to on save).
    pub fn present(&self) -> BTreeSet<CategoryTag> {
        self.tags.iter().filter_map(|t| t.tag.clone()).collect()
    }
}

/// Summarize categories given as `(kind, raw name, item count)` (pure; the store feeds it).
pub fn summarize<'a>(groups: impl IntoIterator<Item = (ItemKind, &'a str, u32)>) -> Vec<TagSummary> {
    let _ = groups.into_iter();
    todo!("P6-core")
}
