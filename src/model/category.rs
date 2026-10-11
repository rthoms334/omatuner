//! Category filter by country / language (DESIGN §28). Plain data: the tag detection and the
//! display tables live in `sources::category_tags`, the rule is applied by `store::library`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A country, language or region tag detected in a provider's category name (`"UK | Sports"`
/// → country `GB`). Canonical text (also its serialized form):
/// * country: ISO 3166-1 alpha-2, upper case — `"GB"` (never `"UK"`), `"US"`, `"FR"`;
/// * language: `"lang:"` + ISO 639-1 code, lower case — `"lang:en"`, `"lang:ar"`;
/// * region: `"region:"` + a fixed id, lower case — `"region:latam"`, `"region:exyu"`.
///
/// Only `sources::category_tags` creates tags from names; the constructors keep the form.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CategoryTag(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TagKind {
    Country,
    Language,
    Region,
}

impl CategoryTag {
    pub fn country(iso: &str) -> Self {
        Self(iso.to_ascii_uppercase())
    }
    pub fn language(code: &str) -> Self {
        Self(format!("lang:{}", code.to_ascii_lowercase()))
    }
    pub fn region(id: &str) -> Self {
        Self(format!("region:{}", id.to_ascii_lowercase()))
    }

    pub fn kind(&self) -> TagKind {
        if self.0.starts_with("lang:") {
            TagKind::Language
        } else if self.0.starts_with("region:") {
            TagKind::Region
        } else {
            TagKind::Country
        }
    }

    /// The code without its kind prefix: `"GB"`, `"en"`, `"latam"`.
    pub fn code(&self) -> &str {
        self.0.split_once(':').map_or(self.0.as_str(), |(_, c)| c)
    }
}

/// The per-source rule (`SourceSettings::category_filter`): which tags' categories are shown.
/// Applied on every library build, so categories a provider adds later follow it (DESIGN §28.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CategoryFilter {
    /// Tags whose categories are shown. Every other tag is hidden — also tags that first appear
    /// after the rule was saved.
    pub keep: BTreeSet<CategoryTag>,
    /// Show categories without a tag ("No country tag").
    pub keep_untagged: bool,
    /// The tags the playlist had when the rule was last saved: the picker marks the others "new".
    pub known: BTreeSet<CategoryTag>,
}

impl Default for CategoryFilter {
    fn default() -> Self {
        Self { keep: BTreeSet::new(), keep_untagged: true, known: BTreeSet::new() }
    }
}

impl CategoryFilter {
    /// Does the rule show a category with this tag (`None` = untagged)?
    pub fn keeps(&self, tag: Option<&CategoryTag>) -> bool {
        match tag {
            Some(t) => self.keep.contains(t),
            None => self.keep_untagged,
        }
    }
}

/// How the source's category filter treats one group (`Group::filter`). `Group::hidden` is the
/// effective flag and already includes the rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum GroupFilter {
    /// No rule, or the rule keeps this group's tag.
    #[default]
    Kept,
    /// The rule hides this group (and the user has not shown it).
    Hidden,
    /// The rule would hide it, but the user showed this group (`GroupOverride::shown`).
    Exempt,
}
