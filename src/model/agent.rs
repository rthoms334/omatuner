//! The agent that made a change, by name (DESIGN §30.12): rules, seen marks and guide-fix batches
//! made over MCP keep the name their client reported (`initialize.clientInfo`), so Tuner can say
//! "added by Codex" later, or "your agent" when it didn't say.

use serde::{Deserialize, Serialize};

/// Longest name kept (after sanitising).
pub const MAX_NAME: usize = 32;

/// What Tuner calls an agent it can't name.
const UNKNOWN: &str = "your agent";

/// Client names (or titles) → the name Tuner shows. Ordered, first match wins; a key is a run of
/// whole lower-case words matched against the client's words, so "amp" never matches "example".
/// Cursor comes before the VS Code row (it reports `cursor-vscode`); Roo Code before Cline (it
/// was a Cline fork, `roo-cline`).
const FRIENDLY: &[(&str, &str)] = &[
    ("claude", "Claude"),
    ("codex", "Codex"),
    ("gemini", "Gemini"),
    ("cursor", "Cursor"),
    ("windsurf", "Windsurf"),
    ("codeium", "Windsurf"),
    ("kiro", "Kiro"),
    ("zed", "Zed"),
    ("roo", "Roo Code"),
    ("cline", "Cline"),
    ("continue", "Continue"),
    ("goose", "Goose"),
    ("opencode", "OpenCode"),
    ("amp", "Amp"),
    ("lm studio", "LM Studio"),
    ("lmstudio", "LM Studio"),
    ("copilot", "Copilot"),
    // VS Code reports "Visual Studio Code"; its agent is Copilot
    ("visual studio", "Copilot"),
    ("vscode", "Copilot"),
];

/// Words of SDK defaults and placeholders ("mcp", "example-client", "test"): a name made only of
/// these (or digits) says nothing, so it stays unknown. So does one that would read as the user or
/// the app ("you", "Tuner"): "You added Arsenal" must only ever mean the user.
const GENERIC: &str = "mcp client server test tests example sample demo sdk app agent cli python typescript ts js node rust go java rmcp fastmcp you your yours me my i user users the tuner omatuner";

/// The agent that made a change, as Tuner names it on screen: the friendly name of the MCP client
/// that asked, `None` = unknown ("your agent"). Client-supplied text: sanitised, display only —
/// never an identity or a permission (guards check "an agent's" vs "the user's", not the name).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "Option<String>", into = "Option<String>")]
pub struct AgentName(Option<String>);

impl AgentName {
    /// From `initialize.clientInfo`: the table above, else the title, else the name (unless
    /// generic), else unknown.
    pub fn from_client(name: &str, title: Option<&str>) -> Self {
        let words = [words(name), title.map(words).unwrap_or_default()];
        if let Some((_, shown)) = FRIENDLY.iter().find(|(key, _)| {
            let key: Vec<&str> = key.split(' ').collect();
            words.iter().any(|w| w.windows(key.len()).any(|run| run == key.as_slice()))
        }) {
            return Self::named(shown);
        }
        [title.unwrap_or(""), name].into_iter().map(Self::named).find(|n| n.0.as_deref().is_some_and(|s| !generic(s))).unwrap_or_default()
    }

    /// A stored or given name, sanitised (the DB, backups, tests).
    pub fn named(s: &str) -> Self {
        Self(sanitise(s))
    }

    /// Who made agent rows from before names were kept (migration 0013, backups without
    /// `agent`): only the Claude Code plugin could.
    pub fn before_names() -> Self {
        Self::named("Claude")
    }

    /// What the `agent` column holds (`None` = unknown).
    pub fn stored(&self) -> Option<&str> {
        self.0.as_deref()
    }

    /// Mid-sentence: "Codex" | "your agent".
    pub fn name(&self) -> &str {
        self.0.as_deref().unwrap_or(UNKNOWN)
    }

    /// At the start of a sentence: "Codex" | "Your agent" (a real name keeps its own case: "iFlow").
    pub fn subject(&self) -> String {
        self.0.clone().unwrap_or_else(|| cap_first(UNKNOWN))
    }

    /// "Codex's" | "your agent's".
    pub fn possessive(&self) -> String {
        format!("{}'s", self.name())
    }

    /// At the start of a sentence: "Codex's" | "Your agent's".
    pub fn subject_possessive(&self) -> String {
        format!("{}'s", self.subject())
    }
}

impl From<Option<String>> for AgentName {
    fn from(s: Option<String>) -> Self {
        s.map_or_else(Self::default, |s| Self::named(&s))
    }
}

impl From<AgentName> for Option<String> {
    fn from(a: AgentName) -> Self {
        a.0
    }
}

/// Plain text only: letters, digits, spaces and `-_.+'()` (no markup, no "://", no control or
/// direction characters, no newlines, no blank "letters" such as the Hangul fillers), spaces
/// collapsed, trimmed, at most [`MAX_NAME`] chars; `None` when nothing is left.
fn sanitise(s: &str) -> Option<String> {
    let mut out = String::new();
    for c in s.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        if c == ' ' {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else if (c.is_alphanumeric() && !invisible(c)) || "-_.+'()".contains(c) {
            out.push(c);
        }
    }
    let trim = |s: &str| s.trim_matches(|c: char| " -_.+".contains(c)).to_owned();
    let out = trim(&trim(&out).chars().take(MAX_NAME).collect::<String>());
    (!out.is_empty()).then_some(out)
}

/// Lower-case words (runs of letters and digits).
fn words(s: &str) -> Vec<String> {
    s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_owned).collect()
}

fn generic(s: &str) -> bool {
    words(s).iter().all(|w| GENERIC.split(' ').any(|g| g == w) || w.chars().all(|c| c.is_ascii_digit()))
}

/// Default-ignorable characters that still count as letters: they'd show as nothing at all.
fn invisible(c: char) -> bool {
    matches!(c, '\u{115F}' | '\u{1160}' | '\u{3164}' | '\u{FFA0}' | '\u{17B4}'..='\u{17B5}' | '\u{180B}'..='\u{180F}')
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_friendly_and_plain_text() {
        for (name, title, shown) in [
            ("claude-code", None, "Claude"),
            ("claude-ai", None, "Claude"),
            ("codex-mcp-client", Some("Codex"), "Codex"),
            ("x", Some("Codex"), "Codex"),
            ("gemini-cli-mcp-client", None, "Gemini"),
            ("cursor-vscode", None, "Cursor"),
            ("Visual Studio Code", None, "Copilot"),
            ("roo-cline", None, "Roo Code"),
            ("LM Studio", None, "LM Studio"),
            // whole words only: "example" isn't Amp
            ("example", Some("My Helper"), "My Helper"),
            ("my-helper", None, "my-helper"),
            // SDK defaults say nothing
            ("mcp", None, "your agent"),
            ("test", None, "your agent"),
            ("example-client", Some("MCP Client 2"), "your agent"),
            // nor can a client pass itself off as the user or the app
            ("You", None, "your agent"),
            ("tuner", None, "your agent"),
            ("client", Some("Your Agent"), "your agent"),
            ("", None, "your agent"),
        ] {
            assert_eq!(AgentName::from_client(name, title).name(), shown, "{name} / {title:?}");
        }
        let unknown = AgentName::default();
        assert_eq!((unknown.name(), unknown.subject().as_str(), unknown.possessive().as_str()), ("your agent", "Your agent", "your agent's"));
        assert_eq!(unknown.subject_possessive(), "Your agent's");
        let codex = AgentName::named("Codex");
        assert_eq!((AgentName::named("iFlow").subject(), AgentName::named("iFlow").subject_possessive()), ("iFlow".into(), "iFlow's".into()));
        assert_eq!((codex.subject().as_str(), codex.possessive().as_str(), codex.stored()), ("Codex", "Codex's", Some("Codex")));

        // client-supplied text can't carry markup, an address, a newline or a direction override
        let evil = AgentName::from_client("Evil\u{202E}<b>bot</b>\nhttp://x/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", None);
        let s = evil.name();
        assert!(!s.contains(['<', '>', '\n', '\u{202E}']) && !s.contains("://") && s.chars().count() <= MAX_NAME, "{s:?}");
        assert_eq!(AgentName::named(" \u{200B}--  \t"), AgentName::default());
        assert_eq!(AgentName::from_client("\u{3164}\u{3164}", Some("\u{FFA0}")), AgentName::default());
        // stored and backed up as a plain optional string; edited values are sanitised on the way in
        assert_eq!(serde_json::to_string(&codex).unwrap(), r#""Codex""#);
        assert_eq!(serde_json::to_string(&unknown).unwrap(), "null");
        assert_eq!(serde_json::from_str::<AgentName>(r#""<Codex>""#).unwrap(), codex);
    }
}
