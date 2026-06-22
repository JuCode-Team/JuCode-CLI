//! Single source of truth for built-in slash commands.
//!
//! Both `command_list_event` (what clients show in menus / autocomplete) and the
//! `/help` text derive from [`COMMANDS`], and `handle_command` gates on it before
//! dispatching. This makes it structurally impossible for a handled command to go
//! unlisted: an unlisted name is rejected before dispatch, so adding a command
//! means adding it here (one place) plus its dispatch arm.

pub struct CommandSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: &'static str,
    pub description: &'static str,
    /// Hidden from the curated `/help` line and flagged `ADV` in the command list;
    /// still discoverable in autocomplete and fully runnable.
    pub advanced: bool,
}

pub const COMMANDS: &[CommandSpec] = &[
    CommandSpec { name: "/help", aliases: &["/"], args: "", description: "Show available commands", advanced: false },
    CommandSpec { name: "/login", aliases: &[], args: "[web-url] [api-url]", description: "Sign in to JuCode (OAuth)", advanced: false },
    CommandSpec { name: "/new", aliases: &[], args: "", description: "Start a new session", advanced: false },
    CommandSpec { name: "/model", aliases: &[], args: "[model] [effort]", description: "Show or switch the model", advanced: false },
    CommandSpec { name: "/tree", aliases: &[], args: "", description: "Show the conversation tree", advanced: false },
    CommandSpec { name: "/trust", aliases: &[], args: "[yes|no|repo]", description: "Trust this project's local resources", advanced: false },
    CommandSpec { name: "/resume", aliases: &[], args: "[session-id]", description: "List or resume sessions", advanced: false },
    CommandSpec { name: "/context", aliases: &[], args: "", description: "Show context usage", advanced: false },
    CommandSpec { name: "/goal", aliases: &[], args: "[objective|pause|resume|blocked|complete|clear]", description: "Manage the session goal", advanced: false },
    CommandSpec { name: "/doctor", aliases: &[], args: "", description: "Run environment diagnostics", advanced: false },
    CommandSpec { name: "/skills", aliases: &[], args: "[list|install <id>|sync]", description: "Manage skills", advanced: false },
    CommandSpec { name: "/pin", aliases: &[], args: "<skill>", description: "Pin a skill for the session", advanced: false },
    CommandSpec { name: "/compact", aliases: &[], args: "", description: "Compact context now", advanced: false },
    CommandSpec { name: "/quit", aliases: &["/exit"], args: "", description: "Quit", advanced: false },
    CommandSpec { name: "/config", aliases: &[], args: "", description: "Show current configuration", advanced: true },
    CommandSpec { name: "/checkout", aliases: &[], args: "<id>", description: "Check out a conversation node", advanced: true },
    CommandSpec { name: "/fork", aliases: &[], args: "<id>", description: "Fork a branch from a node", advanced: true },
    CommandSpec { name: "/delete", aliases: &[], args: "<id>", description: "Delete a branch", advanced: true },
    CommandSpec { name: "/extensions", aliases: &[], args: "", description: "List configured extensions", advanced: true },
    CommandSpec { name: "/stats", aliases: &[], args: "", description: "Show context statistics", advanced: true },
];

/// Whether `command` is a recognized built-in (by canonical name or alias).
pub fn is_known(command: &str) -> bool {
    COMMANDS
        .iter()
        .any(|spec| spec.name == command || spec.aliases.contains(&command))
}

/// The curated one-line `/help` listing (non-advanced commands with their args).
pub fn help_line() -> String {
    COMMANDS
        .iter()
        .filter(|spec| !spec.advanced)
        .map(|spec| {
            if spec.args.is_empty() {
                spec.name.to_string()
            } else {
                format!("{} {}", spec.name, spec.args)
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_are_unique_and_known() {
        let mut seen = std::collections::HashSet::new();
        for spec in COMMANDS {
            assert!(spec.name.starts_with('/'), "{} must start with /", spec.name);
            assert!(seen.insert(spec.name), "duplicate command {}", spec.name);
            assert!(is_known(spec.name));
            for alias in spec.aliases {
                assert!(is_known(alias), "alias {alias} not known");
            }
        }
    }

    #[test]
    fn previously_unlisted_commands_are_registered() {
        for name in [
            "/config",
            "/checkout",
            "/fork",
            "/delete",
            "/extensions",
            "/stats",
            "/exit",
        ] {
            assert!(is_known(name), "{name} must be registered");
        }
        assert!(!is_known("/bogus"));
    }
}
