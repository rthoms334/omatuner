//! Action-shaped tools (batch plan §2.5): one tool per feature with an `action` verb
//! (`queue {action: "add", …}`), instead of a tool per verb — the tool list stays near 28.
//! Whether a call counts as an action (rate-limited, logged at INFO) is decided per verb
//! ([`ToolAction::mutates`]), not per tool. OWNER: agent track.
//!
//! [`tool_actions!`] declares a verb enum (snake_case on the wire) with its names and which
//! verbs change something; [`action_schema`] is its JSON schema.

use serde_json::{Value, json};

/// The verbs of an action-shaped tool.
pub trait ToolAction: Copy + 'static {
    /// Every verb, in schema order.
    const ALL: &'static [Self];
    /// The wire name (`"add"`).
    fn name(self) -> &'static str;
    /// Changes something (playback, reminders, rules, guide data): rate-limited as an action.
    fn mutates(self) -> bool;
}

/// Declare a verb enum: `tool_actions!(pub enum QueueAction { List => ("list", false), … })`.
macro_rules! tool_actions {
    ($(#[$meta:meta])* pub enum $name:ident { $($(#[$vmeta:meta])* $var:ident => ($s:literal, $mutates:expr)),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$vmeta])* $var),+
        }

        impl $crate::agent::actions::ToolAction for $name {
            const ALL: &'static [Self] = &[$($name::$var),+];
            fn name(self) -> &'static str {
                match self {
                    $($name::$var => $s),+
                }
            }
            fn mutates(self) -> bool {
                match self {
                    $($name::$var => $mutates),+
                }
            }
        }
    };
}
pub(crate) use tool_actions;

/// The `action` property of a tool's input schema.
pub fn action_schema<A: ToolAction>(description: &str) -> Value {
    json!({ "type": "string", "enum": A::ALL.iter().map(|a| a.name()).collect::<Vec<_>>(), "description": description })
}

/// The verbs that change something (a tool's description lists them as needing confirmation).
pub fn mutating<A: ToolAction>() -> Vec<&'static str> {
    A::ALL.iter().filter(|a| a.mutates()).map(|a| a.name()).collect()
}

/// Every verb's wire name is its `name()` (serde and the schema agree).
#[cfg(test)]
pub(crate) fn assert_wire_names<A: ToolAction + serde::Serialize>() {
    for a in A::ALL {
        assert_eq!(serde_json::to_value(a).unwrap(), Value::String(a.name().to_owned()));
    }
}
