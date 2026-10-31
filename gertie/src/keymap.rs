//! The user-editable keymap: `keymap.toml` next to the other settings.
//!
//! The file is a list of `{key, command, when}` rules. Last match wins
//! (rules bind in file order, and gpui resolves a tie by the rule
//! added later). The file is created with the defaults on first run;
//! on later startups new defaults are appended, never replacing a
//! rule you wrote. Invalid rules (an unknown command, a key gpui can't
//! parse, an unknown `when`) are ignored at runtime and left in the
//! file for you to fix; an unparseable file falls back to the
//! defaults entirely, also without touching the file.

use gpui_kit::{App, KeyBinding};
use serde::{Deserialize, Serialize};

/// The keymap file's rules, as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The keystroke, gpui syntax, "ctrl-k", "shift-f6".
    key: String,
    /// A command name (see [`COMMANDS`]) or "none" to claim the key
    /// and bind nothing to it.
    command: String,
    /// Optional context: [`WHEN_CONTEXTS`]. Omitted matches everywhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    when: Option<String>,
}

/// The `when` names a rule may use, and the gpui key contexts they
/// map to. A context matches when it appears anywhere along the
/// dispatch path from the focused element up.
const WHEN_CONTEXTS: &[(&str, &str)] = &[
    // Focus is in the composer's input (or a control inside the
    // composer column).
    ("composerFocus", "Composer"),
    // Focus is in any editable field, the composer, a rename field,
    // a settings input.
    ("editableFocus", "Input"),
    // A turn is driving (the context renders only while streaming).
    ("turnRunning", "TurnRunning"),
    // Focus is inside the turn-diff modal.
    ("turnDiffFocus", "TurnDiffPanel"),
];

/// Every command a rule may name, with its default binding. `none`
/// is not here, it is valid in a rule but binds nothing.
pub const COMMANDS: &[&str] = &[
    "palette",
    "close-turn-diff",
    "undo-delete-session",
    "recall-up",
    "recall-down",
    "cycle-focus",
    "cycle-focus-back",
];

/// The default rules, in binding order, the file is created with
/// these, and later startups append any the file is missing.
pub fn default_rules() -> Vec<Rule> {
    [
        ("ctrl-k", "palette", None),
        ("escape", "close-turn-diff", Some("turnDiffFocus")),
        ("ctrl-z", "undo-delete-session", None),
        ("up", "recall-up", Some("editableFocus")),
        ("down", "recall-down", Some("editableFocus")),
        ("f6", "cycle-focus", None),
        ("shift-f6", "cycle-focus-back", None),
    ]
    .into_iter()
    .map(|(key, command, when)| Rule {
        key: key.to_string(),
        command: command.to_string(),
        when: when.map(str::to_string),
    })
    .collect()
}

/// A rule's validity: a known command (or "none"), a parsable key,
/// a known `when`. Invalid rules are ignored, never fatal.
fn rule_is_valid(rule: &Rule) -> bool {
    let command_ok = rule.command == "none" || COMMANDS.contains(&rule.command.as_str());
    let when_ok = rule
        .when
        .as_deref()
        .map(|when| WHEN_CONTEXTS.iter().any(|(name, _)| *name == when))
        .unwrap_or(true);
    command_ok && when_ok && gpui_kit::Keystroke::parse(&rule.key).is_ok()
}

/// The binding a rule produces, None for a rule that binds nothing
/// ("none") or one that failed validation (ignored).
fn rule_to_binding(rule: &Rule) -> Option<KeyBinding> {
    if !rule_is_valid(rule) || rule.command == "none" {
        return None;
    }
    let context = rule
        .when
        .as_deref()
        .and_then(|when| WHEN_CONTEXTS.iter().find(|(name, _)| *name == when))
        .map(|(_, context)| *context);
    let key = rule.key.as_str();
    Some(match rule.command.as_str() {
        "palette" => KeyBinding::new(key, crate::palette::OpenPalette, context),
        "close-turn-diff" => KeyBinding::new(key, crate::chat::CloseTurnDiff, context),
        "undo-delete-session" => KeyBinding::new(key, crate::chat::UndoDeleteSession, context),
        "recall-up" => KeyBinding::new(key, crate::chat::RecallUp, context),
        "recall-down" => KeyBinding::new(key, crate::chat::RecallDown, context),
        "cycle-focus" => KeyBinding::new(key, crate::chat::CycleFocus, context),
        "cycle-focus-back" => KeyBinding::new(key, crate::chat::CycleFocusBack, context),
        _ => return None,
    })
}

/// Binds the rules in order, later rules outrank earlier ones at the
/// same context depth (gpui resolves a tie by the binding added
/// later).
pub fn bind_rules(rules: &[Rule], cx: &mut App) {
    let bindings: Vec<KeyBinding> = rules.iter().filter_map(rule_to_binding).collect();
    cx.bind_keys(bindings);
}

/// Parses the file's rules. A structurally broken file (not TOML, or
/// `rules` not a list of tables) is an Err, the caller falls back to
/// the defaults. Invalid rules (unknown command, unparsable key,
/// unknown `when`, a stray field) are dropped here, ignored, never
/// fatal, and left in the file untouched for the user to fix.
fn parse_rules(source: &str) -> Result<Vec<Rule>, String> {
    let doc: toml::Table = toml::from_str(source).map_err(|error| error.to_string())?;
    let Some(rules) = doc.get("rules").and_then(|rules| rules.as_array()) else {
        // No rules at all: a valid file that binds nothing, the
        // merge decides what to add. (A `rules` key of the wrong
        // shape is structural: `as_array` already said no above only
        // for non-array; a missing key is the same "empty" case.)
        if doc.contains_key("rules") {
            return Err("`rules` is not a list of tables".to_string());
        }
        return Ok(Vec::new());
    };
    Ok(rules
        .iter()
        .filter_map(|value| Rule::deserialize(value.clone()).ok())
        .filter(rule_is_valid)
        .collect())
}

/// The startup merge: the file's rules, plus any default the file
/// doesn't cover. A default is not added when
/// - a rule already names its command (you customized that command,
///   where it lives is your business), or
/// - a rule claims the same key and `when` (defaults never steal a
///   key you've bound, even to "none").
///
/// Returns the effective rules and whether the file needs the
/// additions written to it.
fn merged(file_rules: Vec<Rule>) -> (Vec<Rule>, bool) {
    let valid: Vec<Rule> = file_rules
        .iter()
        .filter(|rule| rule_is_valid(rule))
        .cloned()
        .collect();
    let mut rules = file_rules;
    let mut dirty = false;
    for default in default_rules() {
        // The command is already yours (rebound, renamed, or
        // claimed with "none")...
        let command_taken = valid.iter().any(|rule| rule.command == default.command);
        // ...or the key is: defaults never steal a key you've bound.
        let key_claimed = valid
            .iter()
            .any(|rule| rule.key == default.key && rule.when == default.when);
        if !command_taken && !key_claimed {
            rules.push(default);
            dirty = true;
        }
    }
    (rules, dirty)
}

/// Loads the keymap and binds it. The file lives beside the other
/// settings (`ui.json` and friends); a missing file is created with
/// the defaults, a broken one falls back to them in memory without a
/// rewrite.
pub fn load_and_bind(cx: &mut App) {
    let Some(dir) = crate::config::Config::data_dir() else {
        bind_rules(&default_rules(), cx);
        return;
    };
    let path = dir.join("keymap.toml");
    let (rules, dirty) = match std::fs::read_to_string(&path) {
        Ok(source) => match parse_rules(&source) {
            // Broken file: the defaults answer this session; the file
            // waits for a fix (rewriting it would destroy the user's
            // only copy of their edits).
            Err(error) => {
                eprintln!(
                    "gertie: {error} in {}; using the default keymap",
                    path.display()
                );
                (default_rules(), false)
            }
            Ok(file_rules) => merged(file_rules),
        },
        // Missing file: create it with the defaults so the user has
        // something to edit.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (default_rules(), true),
        Err(error) => {
            eprintln!(
                "gertie: could not read {}: {error}; using the default keymap",
                path.display()
            );
            (default_rules(), false)
        }
    };
    if dirty
        && let Err(error) =
            std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&path, render_file(&rules)))
    {
        eprintln!("gertie: could not write {}: {error}", path.display());
    }
    bind_rules(&rules, cx);
}

/// The file's text: a header explaining the format, then the rules.
fn render_file(rules: &[Rule]) -> String {
    let mut out = String::from(HEADER);
    for rule in rules {
        out.push_str(&rule_to_toml(rule));
    }
    out
}

const HEADER: &str = r#"# gertie keymap. Rules are {key, command, when}; the last matching
# rule wins. `when` is one of: composerFocus, editableFocus,
# turnRunning, turnDiffFocus, omit it to match everywhere.
# gertie adds new defaults here on upgrade; rules you write are
# never replaced. Set command = "none" to claim a key and bind
# nothing to it.
"#;

fn rule_to_toml(rule: &Rule) -> String {
    let mut out = format!(
        "\n[[rules]]\nkey = \"{}\"\ncommand = \"{}\"\n",
        rule.key, rule.command
    );
    if let Some(when) = &rule.when {
        out.push_str(&format!("when = \"{when}\"\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_merges_to_the_defaults() {
        let (rules, dirty) = merged(Vec::new());
        assert_eq!(rules, default_rules());
        assert!(dirty, "the defaults land in a fresh file");
    }

    #[test]
    fn an_unparseable_file_is_a_parse_error() {
        assert!(parse_rules("this is not toml [").is_err());
        // A `rules` key of the wrong shape is structural too.
        assert!(parse_rules("rules = 3").is_err());
    }

    #[test]
    fn invalid_rules_are_ignored_but_valid_ones_bind() {
        let source = r#"
[[rules]]
key = "ctrl-g"
command = "no such command"

[[rules]]
key = "a-b"
command = "palette"

[[rules]]
key = "ctrl-h"
command = "palette"
when = "noSuchContext"

[[rules]]
key = "ctrl-j"
command = "palette"
what = "typo"

[[rules]]
key = "ctrl-p"
command = "palette"
"#;
        let rules = parse_rules(source).expect("structurally fine");
        assert_eq!(rules.len(), 1, "only the valid rule survives parsing");
        assert_eq!(rules[0].key, "ctrl-p");
        assert!(rule_to_binding(&rules[0]).is_some(), "and it binds");
    }

    #[test]
    fn none_rules_bind_nothing() {
        let rule = Rule {
            key: "ctrl-k".into(),
            command: "none".into(),
            when: None,
        };
        assert!(rule_is_valid(&rule), "none is a valid command");
        assert!(rule_to_binding(&rule).is_none(), "but it binds nothing");
    }

    #[test]
    fn new_defaults_are_added_but_customized_commands_are_not_touched() {
        // An older gertie's file: the palette default as it was, the
        // f6 cycle not yet invented, and the user has moved
        // undo-delete to a key of their own.
        let file_rules = vec![
            Rule {
                key: "ctrl-k".into(),
                command: "palette".into(),
                when: None,
            },
            Rule {
                key: "ctrl-alt-z".into(),
                command: "undo-delete-session".into(),
                when: None,
            },
        ];
        let (rules, dirty) = merged(file_rules);
        assert!(dirty, "the new defaults land in the file");
        // The brand-new defaults are appended...
        assert!(rules.iter().any(|rule| rule.command == "cycle-focus"));
        assert!(rules.iter().any(|rule| rule.command == "cycle-focus-back"));
        // ...the user's customized command keeps its key (no ctrl-z
        // default re-added behind it)...
        assert!(
            !rules
                .iter()
                .any(|rule| rule.command == "undo-delete-session" && rule.key == "ctrl-z")
        );
        // ...and the palette default was not duplicated (the file's
        // own rule covers the command).
        assert_eq!(
            rules
                .iter()
                .filter(|rule| rule.command == "palette")
                .count(),
            1
        );
    }

    #[test]
    fn a_default_never_steals_a_key_a_user_rule_holds() {
        // The user claimed the f6 slot with "none", the cycle-focus
        // default must not come back.
        let file_rules = vec![Rule {
            key: "f6".into(),
            command: "none".into(),
            when: None,
        }];
        let (rules, _) = merged(file_rules);
        assert!(!rules.iter().any(|rule| rule.command == "cycle-focus"));
        // Rebinding counts too: the user's own command on the same
        // key and when.
        let file_rules = vec![Rule {
            key: "ctrl-k".into(),
            command: "cycle-focus".into(),
            when: None,
        }];
        let (rules, _) = merged(file_rules);
        assert!(
            !rules
                .iter()
                .any(|rule| rule.command == "palette" && rule.key == "ctrl-k")
        );
    }

    #[test]
    fn the_written_file_round_trips_through_the_parser() {
        let rules = default_rules();
        let parsed = parse_rules(&render_file(&rules)).expect("gertie's own output parses");
        assert_eq!(parsed, rules);
    }
}
