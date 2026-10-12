//! Reading agents' settings files that allow comments (Gemini's `settings.json`, opencode's
//! `opencode.json[c]`): `//` and `/* */` comments and trailing commas are dropped outside
//! strings, then it's plain JSON. Tuner only reads these files; it never writes them.

use std::io::Read;
use std::path::Path;

/// Biggest settings file read.
const MAX_FILE: u64 = 32 << 20;

/// JSON with comments and trailing commas → a value; `None` when it still doesn't parse.
pub(crate) fn parse(text: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&strip(text)).ok()
}

/// What a settings file says: absent, unreadable, or its text (at most [`MAX_FILE`]).
pub(crate) enum FileText {
    Missing,
    Unreadable,
    Text(String),
}

pub(crate) fn read(path: &Path) -> FileText {
    match std::fs::File::open(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileText::Missing,
        Err(_) => FileText::Unreadable,
        Ok(f) => {
            let mut s = String::new();
            match f.take(MAX_FILE).read_to_string(&mut s) {
                Ok(_) => FileText::Text(s),
                Err(_) => FileText::Unreadable,
            }
        }
    }
}

/// The text without comments and trailing commas (string contents, escapes included, kept).
fn strip(text: &str) -> String {
    let b = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                out.extend_from_slice(&b[start..i]);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out.push(b' ');
            }
            b'}' | b']' => {
                // a comma before the closing bracket (whitespace between) goes
                if let Some(p) = out.iter().rposition(|c| !c.is_ascii_whitespace())
                    && out[p] == b','
                {
                    out[p] = b' ';
                }
                out.push(b[i]);
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // only ASCII was removed or replaced, so this is still UTF-8
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_trailing_commas_go_strings_stay() {
        let text = r#"{
  // the user's note
  "theme": "Default", /* block
  comment */
  "url": "https://x//y/*not*/", "esc": "a\"//b",
  "mcpServers": { "tuner": { "command": "tuner", "args": ["mcp",], }, },
}"#;
        let v = parse(text).unwrap();
        assert_eq!(v["url"], "https://x//y/*not*/");
        assert_eq!(v["esc"], "a\"//b");
        assert_eq!(v["mcpServers"]["tuner"]["args"][0], "mcp");
        assert_eq!(parse("{\"a\": 1 // end").map(|v| v["a"].clone()), None, "unclosed: no value");
        assert!(parse("not json").is_none());
        assert_eq!(parse("{\"é\": \"ü\"}").unwrap()["é"], "ü");
    }
}
