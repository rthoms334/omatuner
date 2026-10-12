//! `tuner claude-setup`: how to connect Claude Code to Tuner (the MCP server `tuner mcp` and the
//! omatuner plugin with its skills). It PRINTS the commands; with `--apply` it shows them, asks
//! on the terminal, and runs them only after a "y". Nothing is written to the home directory by
//! the package itself: the Arch package ships the plugin under [`SHARE_DIR`] and the user (or
//! this helper, with consent) adds it to Claude Code. OWNER: agent track.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

/// Where the Arch package installs the marketplace (`.claude-plugin/marketplace.json`) and the
/// plugin (`plugins/omatuner/`).
pub const SHARE_DIR: &str = "/usr/share/tuner/claude";
/// The GitHub marketplace (a checkout-free install).
pub const GITHUB_MARKETPLACE: &str = "rthoms334/omatuner";
/// `<plugin>@<marketplace>`.
pub const PLUGIN: &str = "omatuner@omatuner";
/// The marketplace's name in Claude Code.
pub const MARKETPLACE: &str = "omatuner";

/// One step: what it does and the command (program + arguments, run without a shell).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub what: &'static str,
    pub argv: Vec<String>,
}

impl Step {
    fn new(what: &'static str, argv: &[&str]) -> Self {
        Self { what, argv: argv.iter().map(|s| (*s).to_owned()).collect() }
    }

    /// The command as a user would type it.
    pub fn shell(&self) -> String {
        self.argv
            .iter()
            .map(|a| if a.contains([' ', '\'', '"']) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.clone() })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The steps: the plugin from the packaged marketplace when it is installed (`local`), else from
/// GitHub. The plugin brings the MCP server (`tuner mcp`) and the seven skills.
pub fn steps(local: Option<&Path>) -> Vec<Step> {
    let source = local.map_or_else(|| GITHUB_MARKETPLACE.to_owned(), |p| p.display().to_string());
    vec![
        Step { what: "Add Tuner's plugin marketplace", argv: vec!["claude".into(), "plugin".into(), "marketplace".into(), "add".into(), source] },
        Step::new("Install the omatuner plugin (Tuner's MCP server and its skills)", &["claude", "plugin", "install", PLUGIN, "--scope", "user"]),
    ]
}

/// Bring the plugin up to date after a Tuner upgrade. `github`: refresh the marketplace first
/// (a GitHub marketplace is a clone; the packaged folder loads in place, so it needs only the
/// plugin's record updated — the refresh is harmless there).
pub fn update_steps(github: bool) -> Vec<Step> {
    let mut v = Vec::new();
    if github {
        v.push(Step::new("Refresh Tuner's plugin marketplace", &["claude", "plugin", "marketplace", "update", MARKETPLACE]));
    }
    v.push(Step::new("Update the omatuner plugin", &["claude", "plugin", "update", PLUGIN]));
    v
}

/// Take Tuner's plugin and its marketplace out of Claude Code (Settings ▸ AI agents' Disconnect).
pub fn remove_steps() -> Vec<Step> {
    vec![
        Step::new("Uninstall the omatuner plugin", &["claude", "plugin", "uninstall", PLUGIN, "--scope", "user"]),
        Step::new("Remove Tuner's plugin marketplace", &["claude", "plugin", "marketplace", "remove", MARKETPLACE]),
    ]
}

/// The packaged marketplace ([`SHARE_DIR`]) when the package installed it.
pub fn local_marketplace() -> Option<&'static Path> {
    let share = Path::new(SHARE_DIR);
    share.join(".claude-plugin/marketplace.json").is_file().then_some(share)
}

/// The setup without plugins: the MCP server only. The skills need the plugin: they read its
/// shared `RULES.md`, which a copied skill folder leaves behind.
pub fn manual_steps() -> Vec<Step> {
    vec![Step::new(
        "Register Tuner's MCP server",
        &["claude", "mcp", "add", "--scope", "user", "--transport", "stdio", "tuner", "--", "tuner", "mcp"],
    )]
}

/// The text `tuner claude-setup` prints.
pub fn instructions(local: Option<&Path>, claude_found: bool) -> String {
    let mut s = String::from("Connect Claude Code to Tuner\n\n");
    if !claude_found {
        s.push_str("Claude Code (`claude`) isn't on your PATH: install it first (https://code.claude.com), then run these.\n\n");
    }
    s.push_str("With the plugin (the MCP server and the skills /omatuner:watch, pick, series, plan, kids, fix, discover):\n");
    for st in steps(local) {
        s.push_str(&format!("  # {}\n  {}\n", st.what, st.shell()));
    }
    s.push_str(
        "\nOr only the MCP server, without the skills (they need the plugin: a copied skill folder leaves out\nthe shared rules they follow):\n",
    );
    for st in manual_steps() {
        s.push_str(&format!("  # {}\n  {}\n", st.what, st.shell()));
    }
    let update: Vec<String> = update_steps(local.is_none()).iter().map(Step::shell).collect();
    s.push_str(&format!("\nUpdate later with: {}\n", update.join(" && ")));
    s.push_str("Run `tuner claude-setup --apply` to run the plugin steps now (it asks first).\n");
    s.push_str("Or open Tuner ▸ Settings ▸ AI agents (Tuner also offers this on its first start).\n");
    s
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(program)).find(|p| p.is_file())
}

/// `tuner claude-setup [--apply]`.
pub fn main(apply: bool) -> anyhow::Result<()> {
    let local = local_marketplace();
    let claude = on_path("claude");
    let mut out = std::io::stdout().lock();
    write!(out, "{}", instructions(local, claude.is_some()))?;
    if !apply {
        return Ok(());
    }
    if claude.is_none() {
        anyhow::bail!("`claude` isn't on your PATH; install Claude Code first");
    }
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        anyhow::bail!("--apply asks before it changes Claude Code's settings: run it in a terminal");
    }
    write!(out, "\nRun the plugin steps now? This changes your Claude Code settings. [y/N] ")?;
    out.flush()?;
    let mut answer = String::new();
    stdin.lock().read_line(&mut answer)?;
    if !matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        writeln!(out, "Nothing changed.")?;
        return Ok(());
    }
    for st in steps(local) {
        writeln!(out, "→ {}", st.shell())?;
        let status = std::process::Command::new(&st.argv[0]).args(&st.argv[1..]).status()?;
        if !status.success() {
            anyhow::bail!("`{}` failed ({status}); nothing after it ran", st.shell());
        }
    }
    writeln!(out, "Done. Restart Claude Code; /mcp lists plugin:omatuner:tuner.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The packaged marketplace is used when it is installed, GitHub otherwise; the commands are
    /// argv lists (no shell), so a path with spaces stays one argument.
    #[test]
    fn steps_use_the_packaged_marketplace_when_present() {
        let local = steps(Some(Path::new("/usr/share/tuner/claude")));
        assert_eq!(local[0].argv, ["claude", "plugin", "marketplace", "add", "/usr/share/tuner/claude"]);
        assert_eq!(local[1].argv, ["claude", "plugin", "install", "omatuner@omatuner", "--scope", "user"]);
        assert_eq!(steps(None)[0].argv.last().map(String::as_str), Some(GITHUB_MARKETPLACE));
        let spaced = steps(Some(Path::new("/opt/my apps/claude")));
        assert_eq!(spaced[0].argv.len(), 5);
        assert!(spaced[0].shell().ends_with("'/opt/my apps/claude'"));
        assert!(instructions(None, false).contains("isn't on your PATH"));
        // the GitHub marketplace is a clone: refreshed before the plugin's update
        assert_eq!(
            update_steps(true).iter().map(Step::shell).collect::<Vec<_>>(),
            ["claude plugin marketplace update omatuner", "claude plugin update omatuner@omatuner"]
        );
        assert_eq!(update_steps(false).len(), 1);
        assert!(instructions(None, true).contains("Update later with: claude plugin marketplace update omatuner && claude plugin update"));
    }
}
