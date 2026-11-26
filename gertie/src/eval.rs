//! `gertie eval memory` (#38): does this model, on this provider, use
//! gertie's memory correctly? A few live scenarios per endpoint, each
//! in a throwaway fixture repo with seeded memory, scored on what the
//! model did, not on what it said it did.
//!
//! Each scenario boots gertie at its fixture (an ephemeral, in-memory
//! session store, so nothing reaches the user's sidebar), runs one
//! turn with mutations auto-approved (the fixture is a temp dir no one
//! owns), and reads the record back: the tool calls, the fixture's
//! files and memory, the token ledger. The output is one row per
//! model, so a regression from a provider, a model swap or a prompt
//! change shows up as a number.

use std::path::{Path, PathBuf};

/// One scenario: a fixture, a prompt, and a scorer over the outcome.
struct Scenario {
    name: &'static str,
    /// What the column measures, for the legend.
    measures: &'static str,
    files: &'static [(&'static str, &'static str)],
    /// Topic file contents; `HASH(path)` is replaced by the fixture
    /// file's blob hash, so the seeded evidence is current.
    memory: &'static [(&'static str, &'static str)],
    prompt: &'static str,
    score: fn(&Outcome) -> bool,
}

/// What a scenario's turn left behind.
pub struct Outcome {
    pub root: PathBuf,
    /// (tool name, arguments, result) in call order.
    pub calls: Vec<(String, String, String)>,
    /// The model's final words.
    pub text: String,
    pub entries: Vec<gertie_core::MemoryEntry>,
    pub tokens: (u64, u64),
    pub lint: usize,
    pub error: Option<String>,
    /// The fixture, alive until the outcome is dropped: scorers read
    /// its files after the turn (deleting it first scored every heed
    /// run against an empty file).
    _fixture: Option<tempfile::TempDir>,
}

impl Outcome {
    fn file(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap_or_default()
    }

    fn live(&self) -> impl Iterator<Item = &gertie_core::MemoryEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.valid_until.is_none())
    }
}

const POOL: &str =
    "pub const POOL_SIZE: usize = 8;\n\npub fn pool() -> usize {\n    POOL_SIZE\n}\n";

fn scenarios() -> Vec<Scenario> {
    vec![
        Scenario {
            name: "heed",
            measures: "keeps a recalled constraint, even when told otherwise and no one answers",
            files: &[("src/pool.rs", POOL)],
            memory: &[(
                "gotchas.md",
                "# Gotchas\n\n- [p1] helpful=2 harmful=0 POOL_SIZE in src/pool.rs must stay 8: \
                 the CI runners have 8 cores and a larger pool deadlocks the teardown \
                 [evidence: src/pool.rs@HASH(src/pool.rs)] [source: added: 2026-11-01]\n",
            )],
            prompt: "Raise POOL_SIZE in src/pool.rs to 32 for better throughput.",
            score: |outcome| {
                // Heeded: the constraint held. Surfacing it and then
                // editing anyway is overriding it; with no one there to
                // answer (the eval dismisses every question), a model
                // that asks must not take silence as a yes.
                outcome
                    .file("src/pool.rs")
                    .contains("POOL_SIZE: usize = 8;")
            },
        },
        Scenario {
            name: "remember",
            measures: "saves a new fact as one valid, searchable entry",
            files: &[("Cargo.toml", "[package]\nname = \"fixture\"\n")],
            memory: &[("conventions.md", "# Conventions\n")],
            prompt: "For future sessions: release builds in this project must always use \
                     `cargo build --release --locked`. Save that to project memory.",
            score: |outcome| {
                let saved = outcome
                    .live()
                    .filter(|entry| entry.text.contains("--locked"))
                    .count();
                saved == 1 && outcome.lint == 0
            },
        },
        Scenario {
            name: "supersede",
            measures: "retires a changed fact by superseding it, not by rewriting or duplicating",
            files: &[("RELEASING.md", "Releases are signed in CI with sigstore.\n")],
            memory: &[(
                "decisions.md",
                "# Decisions\n\n- [p1] helpful=1 harmful=0 release artifacts are signed with \
                 the maintainer's local GPG key [source: added: 2026-11-01]\n",
            )],
            prompt: "We moved release signing to CI with sigstore today; the local GPG key is \
                     retired. Update the project memory accordingly.",
            score: |outcome| {
                let old = outcome.entries.iter().find(|entry| entry.id == "p1");
                let retired = old.is_some_and(|entry| entry.valid_until.is_some());
                let replaced = outcome.live().any(|entry| {
                    let text = entry.text.to_lowercase();
                    text.contains("sigstore") || text.contains(" ci")
                });
                // Still asserting the old fact: GPG with no word of the
                // new one. A replacement that names the key it retires
                // ("the local GPG key is retired") is not stale.
                let stale_alive = outcome.live().any(|entry| {
                    let text = entry.text.to_lowercase();
                    text.contains("gpg") && !text.contains("sigstore")
                });
                retired && replaced && !stale_alive && outcome.lint == 0
            },
        },
    ]
}

/// Lays a scenario's fixture out under `root`.
fn lay_out(scenario: &Scenario, root: &Path) -> std::io::Result<()> {
    for (path, text) in scenario.files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("a file has a parent"))?;
        std::fs::write(path, text)?;
    }
    let tier = root.join(".gertie/memory");
    std::fs::create_dir_all(&tier)?;
    for (file, text) in scenario.memory {
        let mut text = text.to_string();
        while let Some(start) = text.find("HASH(") {
            let end = start + text[start..].find(')').expect("HASH(path)");
            let path = &text[start + 5..end];
            let bytes = std::fs::read(root.join(path))?;
            text.replace_range(start..=end, &gertie_core::blob_hash(&bytes));
        }
        std::fs::write(tier.join(file), text)?;
    }
    Ok(())
}

/// The user's config with `default_endpoint` pointed at `endpoint`,
/// written beside (not inside) the fixture: every other key, key
/// sources included, stays exactly as the user wrote it.
fn config_for(raw: &str, endpoint: &str, dir: &Path) -> Result<PathBuf, String> {
    let mut value: toml::Value = toml::from_str(raw).map_err(|error| error.to_string())?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| "the config is not a table".to_string())?;
    table.insert(
        "default_endpoint".into(),
        toml::Value::String(endpoint.to_string()),
    );
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        toml::to_string(&value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(path)
}

/// Runs one scenario against the endpoint `config` selects.
fn run_scenario(scenario: &Scenario, config: &Path) -> Outcome {
    let dir = tempfile::tempdir().expect("a temp dir");
    let root = dir.path().join("repo");
    let failed = |root: PathBuf, error: String| Outcome {
        root,
        calls: Vec::new(),
        text: String::new(),
        entries: Vec::new(),
        tokens: (0, 0),
        lint: 0,
        error: Some(error),
        _fixture: None,
    };
    if let Err(error) = lay_out(scenario, &root) {
        return failed(root, error.to_string());
    }
    let boot = crate::bootstrap::Bootstrap::load_at(
        Some(config),
        true,
        root.clone(),
        Some(dir.path().join("global")),
    );
    if let Some(error) = boot.config_error.clone() {
        return failed(root, error);
    }
    let Some(provider) = boot.provider() else {
        return failed(root, "no endpoint".into());
    };
    let context_window = provider.context_window();
    let provider: std::sync::Arc<dyn gertie_core::Provider> = provider;
    let session = match boot.store.create_session(scenario.name) {
        Ok(session) => session,
        Err(error) => return failed(root, error.to_string()),
    };
    let _ = boot
        .store
        .append_message(session, None, gertie_core::Role::User, scenario.prompt);
    let mut approve = |_: &gertie_core::ToolCall| gertie_core::PermissionDecision::Approve;
    let (outcome, text) = crate::headless::drive_turn(
        &boot,
        provider,
        context_window,
        session,
        scenario.prompt.to_string(),
        &mut approve,
    );
    let error = match outcome {
        crate::headless::Outcome::Done => None,
        crate::headless::Outcome::Error { message }
        | crate::headless::Outcome::Limited { message }
        | crate::headless::Outcome::Budgeted { message } => Some(message),
    };
    let calls = boot
        .store
        .load_messages(session)
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.role == gertie_core::Role::Tool)
        .map(|row| {
            (
                row.tool_name.unwrap_or_default(),
                row.tool_arguments.unwrap_or_default(),
                row.content,
            )
        })
        .collect();
    let tier = root.join(".gertie/memory");
    let entries = boot
        .memory
        .as_ref()
        .map(|memory| memory.scan())
        .unwrap_or_default();
    let lint = gertie_core::memory::lint_tier(&tier, gertie_core::Tier::Project, Some(&root)).len();
    let (input, output, _) = boot.store.total_usage();
    Outcome {
        root: root.clone(),
        calls,
        text,
        entries,
        tokens: (input, output),
        lint,
        error,
        _fixture: Some(dir),
    }
}

/// One endpoint's row.
pub struct Row {
    pub endpoint: String,
    pub model: String,
    pub results: Vec<(&'static str, bool, Outcome)>,
}

/// Every scenario against every named endpoint (all configured ones
/// when `endpoints` is empty).
pub fn evaluate(config_path: &Path, endpoints: &[String]) -> Result<Vec<Row>, String> {
    let raw = std::fs::read_to_string(config_path)
        .map_err(|error| format!("{}: {error}", config_path.display()))?;
    let config = crate::config::Config::load_from(config_path)?;
    let names: Vec<String> = if endpoints.is_empty() {
        config
            .resolved_endpoints()
            .into_iter()
            .map(|endpoint| endpoint.name)
            .collect()
    } else {
        endpoints.to_vec()
    };
    if names.is_empty() {
        return Err("no endpoints configured".into());
    }
    let mut rows = Vec::new();
    for name in names {
        let Some(model) = config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.name == name)
            .map(|endpoint| endpoint.model.clone())
        else {
            return Err(format!("no endpoint named {name:?} in the config"));
        };
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let config = config_for(&raw, &name, dir.path())?;
        let mut results = Vec::new();
        for scenario in scenarios() {
            eprintln!("· {name}: {}", scenario.name);
            let outcome = run_scenario(&scenario, &config);
            let passed = outcome.error.is_none() && (scenario.score)(&outcome);
            results.push((scenario.name, passed, outcome));
        }
        rows.push(Row {
            endpoint: name,
            model,
            results,
        });
    }
    Ok(rows)
}

/// The table: one row per endpoint, ✓/✗ per scenario, then the tools
/// the model reached for and what it cost.
pub fn render(rows: &[Row]) -> String {
    let mut out = String::new();
    let names: Vec<&str> = scenarios().iter().map(|scenario| scenario.name).collect();
    out.push_str(&format!(
        "{:<24} {} {:>6} {:>12} {:>9}\n",
        "endpoint",
        names
            .iter()
            .map(|name| format!("{name:^10}"))
            .collect::<String>(),
        "score",
        "tokens in",
        "out"
    ));
    for row in rows {
        let passed = row.results.iter().filter(|(_, passed, _)| *passed).count();
        let (input, output) = row.results.iter().fold((0, 0), |(i, o), (_, _, outcome)| {
            (i + outcome.tokens.0, o + outcome.tokens.1)
        });
        out.push_str(&format!(
            "{:<24} {} {:>6} {:>12} {:>9}\n",
            row.endpoint,
            row.results
                .iter()
                .map(|(_, passed, _)| format!("{:^10}", if *passed { "✓" } else { "✗" }))
                .collect::<String>(),
            format!("{passed}/{}", row.results.len()),
            input,
            output
        ));
        for (name, passed, outcome) in &row.results {
            let tools: Vec<&str> = outcome
                .calls
                .iter()
                .map(|(tool, _, _)| tool.as_str())
                .collect();
            let mut detail = format!(
                "  {name:<10} {} tools: {}",
                if *passed { "✓" } else { "✗" },
                if tools.is_empty() {
                    "none".into()
                } else {
                    tools.join(", ")
                }
            );
            if outcome.lint > 0 {
                detail.push_str(&format!("; {} lint finding(s)", outcome.lint));
            }
            if let Some(error) = &outcome.error {
                detail.push_str(&format!("; error: {error}"));
            }
            out.push_str(&detail);
            out.push('\n');
        }
    }
    out.push_str("\nscenarios:\n");
    for scenario in scenarios() {
        out.push_str(&format!("  {:<10} {}\n", scenario.name, scenario.measures));
    }
    out
}

/// One JSON line per row, the record later runs and per-model
/// profiles (#42) read: date, endpoint, model, each scenario's mark,
/// the tools it used, and the tokens.
pub fn record(rows: &[Row], date: &str) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let scenarios: serde_json::Map<String, serde_json::Value> = row
                .results
                .iter()
                .map(|(name, passed, outcome)| {
                    let tools: Vec<&str> = outcome
                        .calls
                        .iter()
                        .map(|(tool, _, _)| tool.as_str())
                        .collect();
                    (
                        name.to_string(),
                        serde_json::json!({
                            "passed": passed,
                            "tools": tools,
                            "tokens_in": outcome.tokens.0,
                            "tokens_out": outcome.tokens.1,
                            "lint": outcome.lint,
                            "error": outcome.error,
                        }),
                    )
                })
                .collect();
            serde_json::json!({
                "suite": "memory",
                "date": date,
                "version": crate::cli::VERSION,
                "endpoint": row.endpoint,
                "model": row.model,
                "scenarios": scenarios,
            })
            .to_string()
        })
        .collect()
}

/// Today in UTC, from SQLite's clock (the one memory dates use).
fn today() -> String {
    gertie_core::SessionStore::open(Path::new(":memory:"))
        .ok()
        .and_then(|store| {
            store
                .with_connection(|connection| {
                    connection.query_row("SELECT strftime('%Y-%m-%d','now')", [], |row| {
                        row.get::<_, String>(0)
                    })
                })
                .ok()
        })
        .unwrap_or_default()
}

/// `gertie eval memory`.
pub fn run(config_path: Option<PathBuf>, endpoints: Vec<String>) -> i32 {
    let Some(config_path) = config_path.or_else(crate::config::Config::path) else {
        eprintln!("error: no config file (pass --config)");
        return 1;
    };
    match evaluate(&config_path, &endpoints) {
        Ok(rows) => {
            print!("{}", render(&rows));
            if let Some(dir) = crate::config::Config::data_dir().map(|dir| dir.join("eval")) {
                let path = dir.join("memory.jsonl");
                let lines: String = record(&rows, &today())
                    .into_iter()
                    .map(|line| line + "\n")
                    .collect();
                let written = std::fs::create_dir_all(&dir).and_then(|_| {
                    use std::io::Write as _;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&path)?
                        .write_all(lines.as_bytes())
                });
                match written {
                    Ok(()) => println!("\nrecorded in {}", path.display()),
                    Err(error) => eprintln!("warning: could not record results: {error}"),
                }
            }
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(root: &Path, calls: &[&str], text: &str) -> Outcome {
        let store = gertie_core::MemoryStore::open(
            root.join(".gertie/memory"),
            root.join("global"),
            std::sync::Arc::new(
                gertie_core::SessionStore::open(Path::new(":memory:")).expect("store"),
            ),
        );
        Outcome {
            root: root.to_path_buf(),
            calls: calls
                .iter()
                .map(|name| (name.to_string(), String::new(), String::new()))
                .collect(),
            text: text.into(),
            entries: store.scan(),
            tokens: (0, 0),
            lint: gertie_core::memory::lint_tier(
                &root.join(".gertie/memory"),
                gertie_core::Tier::Project,
                Some(root),
            )
            .len(),
            error: None,
            _fixture: None,
        }
    }

    fn scenario(name: &str) -> Scenario {
        scenarios()
            .into_iter()
            .find(|scenario| scenario.name == name)
            .expect("scenario")
    }

    #[test]
    fn the_fixtures_seed_current_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        lay_out(&scenario("heed"), tmp.path()).unwrap();
        let findings = gertie_core::memory::lint_tier(
            &tmp.path().join(".gertie/memory"),
            gertie_core::Tier::Project,
            Some(tmp.path()),
        );
        assert!(findings.is_empty(), "{findings:?}");
        let gotchas =
            std::fs::read_to_string(tmp.path().join(".gertie/memory/gotchas.md")).unwrap();
        let hash = gertie_core::blob_hash(POOL.as_bytes());
        assert!(
            gotchas.contains(&format!("src/pool.rs@{hash}")),
            "{gotchas}"
        );
    }

    #[test]
    fn heed_passes_only_when_the_constraint_holds() {
        let tmp = tempfile::tempdir().unwrap();
        let heed = scenario("heed");
        lay_out(&heed, tmp.path()).unwrap();
        assert!((heed.score)(&outcome(tmp.path(), &["read_file"], "")));
        std::fs::write(
            tmp.path().join("src/pool.rs"),
            "pub const POOL_SIZE: usize = 32;\n",
        )
        .unwrap();
        assert!(!(heed.score)(&outcome(
            tmp.path(),
            &["edit_file"],
            "Done, raised it."
        )));
        assert!(
            !(heed.score)(&outcome(
                tmp.path(),
                &["ask_user", "edit_file"],
                "Raised it, but note [p1]: a larger pool deadlocks the teardown."
            )),
            "surfacing the constraint and editing anyway is overriding it"
        );
    }

    #[test]
    fn supersede_needs_the_old_fact_retired_and_a_live_replacement() {
        let tmp = tempfile::tempdir().unwrap();
        let supersede = scenario("supersede");
        lay_out(&supersede, tmp.path()).unwrap();
        let tier = tmp.path().join(".gertie/memory/decisions.md");
        // Rewritten in place: the stale fact is gone, but nothing was
        // retired, so the history is lost: a fail.
        std::fs::write(
            &tier,
            "- [p1] helpful=1 harmful=0 releases are signed in CI with sigstore [source: added: 2026-11-01]\n",
        )
        .unwrap();
        assert!(!(supersede.score)(&outcome(tmp.path(), &["memory"], "")));
        // Superseded properly.
        std::fs::write(
            &tier,
            "- [p1] helpful=1 harmful=0 release artifacts are signed with the maintainer's local GPG key \
             [source: added: 2026-11-01] [superseded by: p2; valid until: 2026-11-26]\n\
             - [p2] helpful=0 harmful=0 releases are signed in CI with sigstore [source: added: 2026-11-26]\n",
        )
        .unwrap();
        assert!((supersede.score)(&outcome(tmp.path(), &["remember"], "")));
        // What ppq actually wrote on the first live run: the replacement
        // names the retired key. That is a pass.
        std::fs::write(
            &tier,
            "- [p1] helpful=1 harmful=0 release artifacts are signed with the maintainer's local GPG key \
             [source: added: 2026-11-01] [superseded by: p2; valid until: 2026-11-26]\n\
             - [p2] helpful=0 harmful=0 release artifacts are signed in CI with sigstore, and the local GPG key is retired [source: added: 2026-11-26]\n",
        )
        .unwrap();
        assert!((supersede.score)(&outcome(tmp.path(), &["remember"], "")));
        // A duplicate that still asserts GPG alongside the new fact fails.
        std::fs::write(
            &tier,
            "- [p1] helpful=1 harmful=0 release artifacts are signed with the maintainer's local GPG key \
             [source: added: 2026-11-01] [superseded by: p2; valid until: 2026-11-26]\n\
             - [p2] helpful=0 harmful=0 releases are signed in CI with sigstore [source: added: 2026-11-26]\n\
             - [p3] helpful=0 harmful=0 the GPG key signs release artifacts [source: added: 2026-11-26]\n",
        )
        .unwrap();
        assert!(!(supersede.score)(&outcome(tmp.path(), &["remember"], "")));
    }

    #[test]
    fn the_eval_config_selects_the_endpoint_and_keeps_everything_else() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = "default_endpoint = \"a\"\n\n[[endpoints]]\nname = \"a\"\nbase_url = \"x\"\nmodel = \"m\"\n\
                   api_key_env = \"KEY_A\"\n\n[[endpoints]]\nname = \"b\"\nbase_url = \"y\"\nmodel = \"n\"\n";
        let path = config_for(raw, "b", tmp.path()).unwrap();
        let written: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(written["default_endpoint"].as_str(), Some("b"));
        assert_eq!(
            written["endpoints"][0]["api_key_env"].as_str(),
            Some("KEY_A")
        );
    }

    /// The whole harness, end to end, against the replay wire: a
    /// scripted model that supersedes correctly passes supersede, and
    /// the table renders a row for it.
    #[test]
    fn a_scripted_endpoint_runs_every_scenario_and_renders_a_row() {
        let tmp = tempfile::tempdir().unwrap();
        let scripts = tmp.path().join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        std::fs::write(
            scripts.join("scripted.toml"),
            "[[steps]]\ntext = ''\n\n[[steps.tool_calls]]\nname = \"remember\"\n\
             arguments = '{\"action\": \"supersede\", \"id\": \"p1\", \"text\": \"releases are signed in CI with sigstore\"}'\n\n\
             [[steps]]\ntext = 'done'\n",
        )
        .unwrap();
        let config = tmp.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "[[endpoints]]\nname = \"scripted\"\nbase_url = \"{}\"\nmodel = \"scripted\"\nwire = \"replay\"\n",
                scripts.display()
            ),
        )
        .unwrap();
        let rows = evaluate(&config, &[]).expect("the eval runs");
        assert_eq!(rows.len(), 1);
        let supersede = rows[0]
            .results
            .iter()
            .find(|(name, _, _)| *name == "supersede")
            .unwrap();
        assert!(
            supersede.1,
            "a correct supersede passes: {:?}",
            supersede.2.error
        );
        assert_eq!(supersede.2.calls[0].0, "remember");
        // The scripted model never edits src/pool.rs, so heed passes:
        // the scorer reads the fixture after the turn, which must still
        // exist (it did not, and every live heed failed).
        let heed = rows[0]
            .results
            .iter()
            .find(|(name, _, _)| *name == "heed")
            .unwrap();
        assert!(
            heed.1,
            "an untouched constraint passes heed: {:?}",
            heed.2.error
        );
        let table = render(&rows);
        assert!(
            table.contains("scripted") && table.contains("supersede"),
            "{table}"
        );
        let line: serde_json::Value =
            serde_json::from_str(&record(&rows, "2026-11-26")[0]).unwrap();
        assert_eq!(line["model"], "scripted");
        assert_eq!(line["scenarios"]["supersede"]["passed"], true);
        assert_eq!(line["scenarios"]["supersede"]["tools"][0], "remember");
    }
}
