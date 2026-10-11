//! `tuner mcp`: the MCP server over stdio (DESIGN §30.2). Owner: MCP-bridge track.
//!
//! Hand-rolled JSON-RPC 2.0, one message per line (MCP stdio transport): `initialize` (protocol
//! version negotiation over `tools::PROTOCOL_VERSIONS`, `capabilities: {tools: {listChanged:
//! false}}`, `serverInfo {name: "tuner", version}`, `instructions`), `notifications/initialized`
//! and other notifications (ignored), `ping`, `tools/list`, `tools/call`. Anything else with an
//! id → `-32601`; unparsable line → `-32700` (id null); a request line over 1 MiB is refused
//! (`-32600`) without being buffered whole. Requests are handled in order.
//!
//! `tools/call` → `proto::Call::from_tool(name, arguments)` (bad arguments → a tool error
//! result, not a JSON-RPC error, so the model can correct itself) → `client::Client::call` →
//! result `{content: [{type: "text", text: <result JSON>}], structuredContent: <result>,
//! isError: false}`; an error reply or a `ClientError` → `{content: [{type: "text", text:
//! "<code>: <message>"}], isError: true}`.
//!
//! stdout carries only protocol lines; logging goes to stderr (Claude Code keeps it in its MCP
//! log) at WARN, or DEBUG with `--debug` — never to `tuner.log`, which the running app owns.

use std::io::{BufRead, Write};

use super::client::Client;

/// `tuner mcp [--profile-dir DIR] [--debug]`: serve stdin/stdout until stdin closes.
pub fn main(paths: &crate::paths::AppPaths, profile_dir: Option<&std::path::Path>, debug: bool) -> anyhow::Result<()> {
    let _ = (paths, profile_dir, debug);
    todo!("MCP-bridge")
}

/// The protocol loop over any reader/writer (tests drive it with in-memory pipes).
pub fn serve(input: impl BufRead, output: impl Write, client: &Client) -> std::io::Result<()> {
    let _ = (input, output, client);
    todo!("MCP-bridge")
}
