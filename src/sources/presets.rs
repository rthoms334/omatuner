//! Free-channel playlists offered by onboarding ("Try free channels") and Settings ▸ Playlists
//! ("Add free channels"). DESIGN §27.8. Nothing is fetched until the user picks one, and none
//! is ever added on its own.
//!
//! The user's country comes from the system timezone (via the tz database's `zone.tab`) and,
//! failing that, the locale's territory: many systems keep `en_US` whatever the country, while
//! the timezone is set for where the machine is.

use std::collections::HashMap;
use std::sync::OnceLock;

/// iptv-org: every country in one playlist (~11 000 channels).
pub const IPTV_ORG_ALL: &str = "https://iptv-org.github.io/iptv/index.m3u";
/// Free-TV/IPTV (~2 000 channels; its `x-tvg-url` lists epgshare01 guides).
pub const FREE_TV: &str = "https://raw.githubusercontent.com/Free-TV/IPTV/master/playlist.m3u8";

const ZONE_TAB: &str = "/usr/share/zoneinfo/zone.tab";
const ISO3166_TAB: &str = "/usr/share/zoneinfo/iso3166.tab";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preset {
    /// iptv-org playlist of one country (ISO 3166-1 alpha-2, uppercase: `"GB"`).
    IptvOrgCountry(String),
    IptvOrgAll,
    FreeTv,
}

impl Preset {
    pub fn url(&self) -> String {
        match self {
            Preset::IptvOrgCountry(cc) => format!("https://iptv-org.github.io/iptv/countries/{}.m3u", iptv_org_code(cc)),
            Preset::IptvOrgAll => IPTV_ORG_ALL.to_owned(),
            Preset::FreeTv => FREE_TV.to_owned(),
        }
    }

    /// Name the source is added under.
    pub fn source_name(&self) -> String {
        match self {
            Preset::IptvOrgCountry(cc) => format!("iptv-org · {}", country_name(cc).unwrap_or_else(|| cc.clone())),
            Preset::IptvOrgAll => "iptv-org · All countries".to_owned(),
            Preset::FreeTv => "Free-TV".to_owned(),
        }
    }

    /// One line for the choice list.
    pub fn description(&self) -> &'static str {
        match self {
            Preset::IptvOrgCountry(_) => "Free channels from your country, collected by iptv-org",
            Preset::IptvOrgAll => "About 11 000 free channels from every country",
            Preset::FreeTv => "About 2 000 hand-picked free channels, with a TV guide",
        }
    }
}

/// The presets offered, in order: the user's country (when known), all countries, Free-TV.
pub fn presets(country: Option<&str>) -> Vec<Preset> {
    let mut out: Vec<Preset> = country.map(|cc| Preset::IptvOrgCountry(cc.to_ascii_uppercase())).into_iter().collect();
    out.extend([Preset::IptvOrgAll, Preset::FreeTv]);
    out
}

/// iptv-org's file name for a country: the ISO code in lowercase, except the United Kingdom,
/// which iptv-org calls `uk`.
pub fn iptv_org_code(iso: &str) -> String {
    match iso.to_ascii_lowercase().as_str() {
        "gb" => "uk".to_owned(),
        cc => cc.to_owned(),
    }
}

/// The user's country (uppercase ISO code), detected once per process (two small local file
/// reads at most).
pub fn detect_country() -> Option<String> {
    static COUNTRY: OnceLock<Option<String>> = OnceLock::new();
    COUNTRY
        .get_or_init(|| {
            let tz = jiff::tz::TimeZone::system();
            let zone_tab = std::fs::read_to_string(ZONE_TAB).ok();
            let locale = ["LC_ALL", "LANG"].into_iter().filter_map(|k| std::env::var(k).ok()).find(|v| !v.trim().is_empty());
            country_from(tz.iana_name(), zone_tab.as_deref(), locale.as_deref())
        })
        .clone()
}

/// [`detect_country`] without the I/O: the timezone's country in `zone_tab` (lines
/// `CC<TAB>coordinates<TAB>Area/City…`), else the territory of a locale like `en_GB.UTF-8`.
pub fn country_from(tz: Option<&str>, zone_tab: Option<&str>, locale: Option<&str>) -> Option<String> {
    let valid = |cc: &str| (cc.len() == 2 && cc.bytes().all(|b| b.is_ascii_alphabetic())).then(|| cc.to_ascii_uppercase());
    let by_tz = tz.zip(zone_tab).and_then(|(tz, tab)| {
        tab.lines().filter(|l| !l.starts_with('#')).find_map(|l| {
            let mut cols = l.split('\t');
            let cc = cols.next()?;
            (cols.nth(1)? == tz).then(|| valid(cc)).flatten()
        })
    });
    by_tz.or_else(|| {
        let territory = locale?.split(['.', '@']).next()?.split_once('_')?.1;
        valid(territory)
    })
}

/// Display name of a country from the tz database's `iso3166.tab`, except where its short form
/// reads oddly as a playlist name (tzdata's "GB" is "Britain (UK)").
pub fn country_name(iso: &str) -> Option<String> {
    if iso.eq_ignore_ascii_case("GB") {
        return Some("United Kingdom".to_owned());
    }
    static NAMES: OnceLock<HashMap<String, String>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        let tab = std::fs::read_to_string(ISO3166_TAB).unwrap_or_default();
        tab.lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.split_once('\t'))
            .map(|(cc, name)| (cc.trim().to_ascii_uppercase(), name.trim().to_owned()))
            .collect()
    });
    names.get(&iso.to_ascii_uppercase()).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_comes_from_the_timezone_then_the_locale() {
        let tab = "# comment\nGB\t+513030-0000731\tEurope/London\nUS\t+404251-0740023\tAmerica/New_York\tEastern (most areas)\n";
        // the timezone wins over a default en_US locale
        assert_eq!(country_from(Some("Europe/London"), Some(tab), Some("en_US.UTF-8")).as_deref(), Some("GB"));
        assert_eq!(country_from(Some("America/New_York"), Some(tab), None).as_deref(), Some("US"));
        // no country for UTC: the locale's territory
        assert_eq!(country_from(Some("Etc/UTC"), Some(tab), Some("de_AT.UTF-8@euro")).as_deref(), Some("AT"));
        assert_eq!(country_from(None, None, Some("C.UTF-8")), None);
        assert_eq!(country_from(None, None, Some("POSIX")), None);
        assert_eq!(Preset::IptvOrgCountry("GB".into()).url(), "https://iptv-org.github.io/iptv/countries/uk.m3u");
        assert_eq!(Preset::IptvOrgCountry("FR".into()).url(), "https://iptv-org.github.io/iptv/countries/fr.m3u");
    }
}
