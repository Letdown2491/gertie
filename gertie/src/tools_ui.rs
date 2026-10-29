//! Human-facing tool naming: what the model is *doing*, not the wire name.
//!
//! The transcript is for people. A card that says `⚙ read_file` and dumps
//! twelve raw lines tells the user what the model sees; one that says
//! "Reading `src/chat.rs`" with a one-line outcome tells them what is
//! happening. The raw output stays available behind the expander — this
//! module only picks the friendly face.

/// One tool call's friendly label. `running` picks the tense: gerund
/// while it executes ("Reading…"), past tense once it's finished ("Read").
pub fn action_label(name: &str, arguments: &str, running: bool) -> String {
    let args: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let field = |key: &str| {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .unwrap_or_default()
    };
    let tense = |doing: String, done: String| {
        if running { doing } else { done }
    };

    // Whittle a command down for inline display; the full command stays
    // in the raw output.
    let short = |text: &str, cap: usize| {
        let one_line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("");
        let mut short: String = one_line.trim().chars().take(cap).collect();
        if one_line.trim().chars().count() > cap {
            short.push('…');
        }
        short
    };

    let label = if let Some(agent) = name.strip_prefix("agent_") {
        tense(
            format!("Running the {agent} agent"),
            format!("Ran the {agent} agent"),
        )
    } else {
        match name {
            "read_file" => {
                let path = field("path");
                tense(format!("Reading `{path}`"), format!("Read `{path}`"))
            }
            "write_file" => {
                let path = field("path");
                tense(format!("Writing `{path}`"), format!("Wrote `{path}`"))
            }
            "edit_file" => {
                let path = field("path");
                tense(format!("Editing `{path}`"), format!("Edited `{path}`"))
            }
            "bash" => {
                let command = short(&field("command"), 60);
                tense(format!("Running `{command}`"), format!("Ran `{command}`"))
            }
            "glob" => {
                let pattern = short(&field("pattern"), 50);
                tense(
                    format!("Finding files matching `{pattern}`"),
                    format!("Found files matching `{pattern}`"),
                )
            }
            "grep" => {
                let pattern = short(&field("pattern"), 50);
                tense(
                    format!("Searching for `{pattern}`"),
                    format!("Searched for `{pattern}`"),
                )
            }
            "web_search" => {
                let query = short(&field("query"), 60);
                tense(
                    format!("Searching the web for \u{201c}{query}\u{201d}"),
                    format!("Searched the web for \u{201c}{query}\u{201d}"),
                )
            }
            "web_fetch" => {
                let url = short(&field("url"), 60);
                tense(format!("Fetching {url}"), format!("Fetched {url}"))
            }
            "search_history" => {
                let query = short(&field("query"), 60);
                tense(
                    format!("Searching past chats for `{query}`"),
                    format!("Searched past chats for `{query}`"),
                )
            }
            "load_skill" => {
                let skill = field("skill");
                tense(
                    format!("Loading the `{skill}` skill"),
                    format!("Loaded the `{skill}` skill"),
                )
            }
            other => {
                let humanized = humanize_name(other);
                tense(format!("Running {humanized}"), format!("Ran {humanized}"))
            }
        }
    };

    // A missing argument leaves an empty slot in the label ("Reading ``",
    // "Fetching " with nothing after) — usually because the arguments
    // haven't streamed in yet. Read as a plain verb instead of a hole.
    let label =
        if label.contains("``") || label.contains("\u{201c}\u{201d}") || label.ends_with(' ') {
            let humanized = humanize_name(name);
            tense(format!("Running {humanized}"), format!("Ran {humanized}"))
        } else {
            label
        };
    if running {
        format!("{label}…")
    } else {
        label
    }
}

/// MCP tools arrive as `mcp_{server}_{tool}`; everything else is
/// snake_case. Either way the card should read like words, not wiring.
fn humanize_name(name: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp_") {
        return format!("the {} MCP tool", rest.replace('_', " "));
    }
    name.replace('_', " ")
}

/// A friendly one-line outcome for a finished tool call — the summary
/// line under the label. Raw output stays behind the expander.
pub fn result_summary(name: &str, result: &str) -> String {
    let failed = result.starts_with("error:")
        || result.starts_with("exit ")
        || result.starts_with("blocked by policy")
        || result.ends_with("denied by user");
    let line_count = result.lines().count();

    if failed {
        let first = result.lines().next().unwrap_or("");
        let mut summary: String = first.chars().take(120).collect();
        if first.chars().count() > 120 {
            summary.push('…');
        }
        return summary;
    }

    if let Some(agent) = name.strip_prefix("agent_") {
        let status = result
            .lines()
            .find(|line| line.trim_start().starts_with("STATUS:"))
            .and_then(|line| line.split_once(':').map(|(_, rest)| rest.trim()))
            .unwrap_or("done");
        return format!("{agent} agent finished ({status})");
    }

    match name {
        "read_file" => format!(
            "{line_count} line{}",
            if line_count == 1 { "" } else { "s" }
        ),
        "grep" | "search_history" => {
            if result.trim() == "(no matches)" {
                "no matches".into()
            } else {
                format!(
                    "{line_count} match{}",
                    if line_count == 1 { "" } else { "es" }
                )
            }
        }
        "glob" => {
            if result.trim() == "(no matches)" {
                "no matches".into()
            } else {
                format!(
                    "{line_count} file{}",
                    if line_count == 1 { "" } else { "s" }
                )
            }
        }
        "bash" => {
            if result.trim() == "(no output)" {
                "done, no output".into()
            } else {
                let first = result.lines().next().unwrap_or("");
                let mut summary: String = first.chars().take(120).collect();
                if first.chars().count() > 120 {
                    summary.push('…');
                }
                summary
            }
        }
        _ => {
            // write_file, edit_file, web_fetch, load_skill, MCP — the tool
            // already speaks in outcomes ("edited src/x.rs (1 occurrence)").
            let first = result.lines().next().unwrap_or("");
            let mut summary: String = first.chars().take(120).collect();
            if first.chars().count() > 120 {
                summary.push('…');
            }
            summary
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_use_tense_and_paths() {
        assert_eq!(
            action_label("read_file", r#"{"path": "src/main.rs"}"#, true),
            "Reading `src/main.rs`…"
        );
        assert_eq!(
            action_label("read_file", r#"{"path": "src/main.rs"}"#, false),
            "Read `src/main.rs`"
        );
        assert_eq!(
            action_label("edit_file", r#"{"path": "a.rs"}"#, false),
            "Edited `a.rs`"
        );
    }

    #[test]
    fn bash_labels_show_the_command_not_the_wiring() {
        let label = action_label("bash", r#"{"command": "cargo test\n--all"}"#, true);
        assert_eq!(label, "Running `cargo test`…");
    }

    #[test]
    fn agent_tools_read_as_agents() {
        assert_eq!(
            action_label("agent_explore", r#"{"brief_path": "b.md"}"#, true),
            "Running the explore agent…"
        );
    }

    #[test]
    fn unknown_and_mcp_tools_get_words() {
        assert_eq!(
            action_label("frobnicate_widgets", "{}", false),
            "Ran frobnicate widgets"
        );
        assert_eq!(
            action_label("mcp_github_create_issue", "{}", true),
            "Running the github create issue MCP tool…"
        );
    }

    #[test]
    fn missing_arguments_degrade_to_the_generic_verb() {
        // Arguments that haven't arrived (or don't parse) leave an empty
        // slot — the card should read as a plain verb, not a hole.
        assert_eq!(action_label("read_file", "", true), "Running read file…");
        assert_eq!(action_label("bash", "not json", false), "Ran bash");
        assert_eq!(action_label("web_fetch", "{}", false), "Ran web fetch");
        assert_eq!(
            action_label("web_search", r#"{"query": ""}"#, true),
            "Running web search…"
        );
    }

    #[test]
    fn result_summaries_count_what_mattered() {
        assert_eq!(result_summary("read_file", "1  a\n2  b\n3  c"), "3 lines");
        assert_eq!(result_summary("grep", "a:1: x"), "1 match");
        assert_eq!(result_summary("grep", "(no matches)"), "no matches");
        assert_eq!(result_summary("glob", "one.rs\ntwo.rs"), "2 files");
        assert_eq!(result_summary("bash", "(no output)"), "done, no output");
    }

    #[test]
    fn failures_surface_the_reason() {
        let summary = result_summary("bash", "exit exit code: 3:\nboom");
        assert!(summary.starts_with("exit"));
        assert_eq!(
            result_summary("edit_file", "error: denied by user"),
            "error: denied by user"
        );
    }

    #[test]
    fn agent_summaries_read_the_status_line() {
        let report = "STATUS: done\nEVIDENCE: ran tests\nSUMMARY: green";
        assert_eq!(
            result_summary("agent_test", report),
            "test agent finished (done)"
        );
    }
}
