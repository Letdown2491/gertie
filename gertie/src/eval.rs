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
    /// An outcome over an existing directory, for other suites' tests.
    #[cfg(test)]
    pub(crate) fn for_test(
        root: &Path,
        calls: Vec<(String, String, String)>,
        tokens: (u64, u64),
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            calls,
            text: String::new(),
            entries: Vec::new(),
            tokens,
            lint: 0,
            error: None,
            _fixture: None,
        }
    }

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
pub(crate) fn config_for(
    raw: &str,
    endpoint: &str,
    overrides: &Overrides,
    dir: &Path,
) -> Result<PathBuf, String> {
    let mut value: toml::Value = toml::from_str(raw).map_err(|error| error.to_string())?;
    let table = value
        .as_table_mut()
        .ok_or_else(|| "the config is not a table".to_string())?;
    table.insert(
        "default_endpoint".into(),
        toml::Value::String(endpoint.to_string()),
    );
    // A profile under trial (--set) lands on the endpoint under test,
    // in this throwaway copy only.
    if let Some(endpoints) = table
        .get_mut("endpoints")
        .and_then(|value| value.as_array_mut())
        && let Some(entry) = endpoints
            .iter_mut()
            .filter_map(|value| value.as_table_mut())
            .find(|entry| entry.get("name").and_then(|name| name.as_str()) == Some(endpoint))
    {
        if let Some(writes) = overrides.memory_writes {
            entry.insert("memory_writes".into(), toml::Value::Boolean(writes));
        }
        if let Some(guidance) = overrides.tool_guidance {
            entry.insert(
                "tool_guidance".into(),
                toml::Value::String(guidance.as_str().into()),
            );
        }
    }
    if let Some(enabled) = overrides.repo_map {
        let section = table
            .entry("repo_map")
            .or_insert_with(|| toml::Value::Table(Default::default()));
        if let Some(section) = section.as_table_mut() {
            section.insert("enabled".into(), toml::Value::Boolean(enabled));
        }
    }
    let path = dir.join("config.toml");
    std::fs::write(
        &path,
        toml::to_string(&value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    Ok(path)
}

/// A profile to try without editing the config (#42): `--set
/// memory_writes=false`, `--set tool_guidance=terse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub memory_writes: Option<bool>,
    pub tool_guidance: Option<gertie_core::ToolGuidance>,
    /// `[repo_map] enabled` in the throwaway config (#41).
    pub repo_map: Option<bool>,
}

impl Overrides {
    pub fn parse(settings: &[String]) -> Result<Self, String> {
        let mut overrides = Self::default();
        for setting in settings {
            let (key, value) = setting
                .split_once('=')
                .ok_or_else(|| format!("--set {setting}: expected key=value"))?;
            match key.trim() {
                "memory_writes" => {
                    overrides.memory_writes = Some(match value.trim() {
                        "true" => true,
                        "false" => false,
                        other => {
                            return Err(format!("memory_writes takes true or false, not {other}"));
                        }
                    })
                }
                "tool_guidance" => {
                    overrides.tool_guidance =
                        Some(gertie_core::ToolGuidance::parse(value).ok_or_else(|| {
                            format!("tool_guidance takes standard or terse, not {value}")
                        })?)
                }
                "repo_map" => {
                    overrides.repo_map = Some(match value.trim() {
                        "true" | "on" => true,
                        "false" | "off" => false,
                        other => return Err(format!("repo_map takes true or false, not {other}")),
                    })
                }
                other => {
                    return Err(format!(
                        "--set {other}: the settings are memory_writes, tool_guidance and repo_map"
                    ));
                }
            }
        }
        Ok(overrides)
    }

    fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Runs one scenario against the endpoint `config` selects.
fn run_scenario(scenario: &Scenario, config: &Path) -> Outcome {
    run_fixture(
        scenario.name,
        |root| lay_out(scenario, root),
        scenario.prompt,
        config,
    )
}

/// One turn in a throwaway repo `lay` fills, against the endpoint
/// `config` selects, read back as an [`Outcome`]. Shared by the suites.
pub(crate) fn run_fixture(
    name: &str,
    lay: impl FnOnce(&Path) -> std::io::Result<()>,
    prompt: &str,
    config: &Path,
) -> Outcome {
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
    if let Err(error) = lay(&root) {
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
    let context_window = boot.compaction_window(provider.context_window());
    let provider: std::sync::Arc<dyn gertie_core::Provider> = provider;
    let session = match boot.store.create_session(name) {
        Ok(session) => session,
        Err(error) => return failed(root, error.to_string()),
    };
    let _ = boot
        .store
        .append_message(session, None, gertie_core::Role::User, prompt);
    let mut approve = |_: &gertie_core::ToolCall| gertie_core::PermissionDecision::Approve;
    let (outcome, text) = crate::headless::drive_turn(
        &boot,
        provider,
        context_window,
        session,
        prompt.to_string(),
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
    /// The memory tool's write commands were offered (#42): the profile
    /// setting a comparison varies.
    pub memory_writes: bool,
    /// How much the tool definitions coached the model.
    pub tool_guidance: gertie_core::ToolGuidance,
    /// The profile came from --set, not the config.
    pub trial: bool,
    pub results: Vec<(&'static str, bool, Outcome)>,
}

/// Every scenario against every named endpoint (all configured ones
/// when `endpoints` is empty).
pub fn evaluate(
    config_path: &Path,
    endpoints: &[String],
    overrides: &Overrides,
) -> Result<Vec<Row>, String> {
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
        let Some(endpoint) = config
            .endpoints
            .iter()
            .find(|endpoint| endpoint.name == name)
        else {
            return Err(format!("no endpoint named {name:?} in the config"));
        };
        let model = endpoint.model.clone();
        let memory_writes = overrides
            .memory_writes
            .unwrap_or_else(|| endpoint.memory_writes_in_force());
        let tool_guidance = overrides
            .tool_guidance
            .unwrap_or_else(|| endpoint.tool_guidance_in_force());
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let config = config_for(&raw, &name, overrides, dir.path())?;
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
            memory_writes,
            tool_guidance,
            trial: !overrides.is_empty(),
            results,
        });
    }
    Ok(rows)
}

/// The table: one row per endpoint, ✓/✗ per scenario, then the tools
/// the model reached for and what it cost.
pub fn render(rows: &[Row], history: &[String]) -> String {
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
        out.push_str(&format!(
            "  profile: memory writes {}; tool guidance {}{}\n",
            if row.memory_writes {
                "through the memory tool and remember"
            } else {
                "through remember only"
            },
            row.tool_guidance.as_str(),
            if row.trial {
                " (trial, from --set)"
            } else {
                ""
            }
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
    let advice = suggestions(rows, history);
    if !advice.is_empty() {
        out.push_str("\nsuggestions:\n");
        for line in advice {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str("\nscenarios:\n");
    for scenario in scenarios() {
        out.push_str(&format!("  {:<10} {}\n", scenario.name, scenario.measures));
    }
    out
}

/// Output tokens priced against input: providers bill output at
/// several times input (four is a middle value), so a run that writes
/// more costs more than its input count shows.
const OUTPUT_WEIGHT: u64 = 4;

/// A row's cost in input-token equivalents: tokens in plus weighted
/// tokens out, across its scenarios.
fn cost(row: &Row) -> u64 {
    row.results
        .iter()
        .map(|(_, _, outcome)| outcome.tokens.0 + OUTPUT_WEIGHT * outcome.tokens.1)
        .sum()
}

/// Every recorded passing run of the same endpoint, model and memory
/// setting under `guidance`, as costs. Lines are the JSON `record`
/// writes; lines from before a field existed do not match.
fn recorded_costs(history: &[String], row: &Row, guidance: &str) -> Vec<u64> {
    history
        .iter()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            if value["endpoint"] != row.endpoint.as_str()
                || value["model"] != row.model.as_str()
                || value["tool_guidance"] != guidance
                || value["memory_writes"] != row.memory_writes
            {
                return None;
            }
            let scenarios = value["scenarios"].as_object()?;
            if scenarios
                .values()
                .any(|scenario| scenario["passed"] != true)
            {
                return None;
            }
            Some(
                scenarios
                    .values()
                    .map(|scenario| {
                        scenario["tokens_in"].as_u64().unwrap_or(0)
                            + OUTPUT_WEIGHT * scenario["tokens_out"].as_u64().unwrap_or(0)
                    })
                    .sum(),
            )
        })
        .collect()
}

fn mean(costs: &[u64]) -> f64 {
    costs.iter().sum::<u64>() as f64 / costs.len().max(1) as f64
}

/// Terse is kept only when it pays: the mean cost of its passing runs
/// at least this much below standard's.
const TERSE_MIN_SAVING: f64 = 0.10;
/// Runs per side before a verdict: single runs vary by as much as the
/// effect being measured.
const RUNS_FOR_VERDICT: usize = 2;

/// What the results say to change, per endpoint (#42). Each line names
/// the setting and how to confirm it before keeping it. `history` is
/// the results file's earlier lines: a terse trial is judged against
/// the model's last passing standard run, not on its score alone,
/// because less coaching can trade cheaper requests for more of them.
pub fn suggestions(rows: &[Row], history: &[String]) -> Vec<String> {
    use gertie_core::ToolGuidance::{Standard, Terse};
    let mut out = Vec::new();
    for row in rows {
        let name = &row.endpoint;
        let failed: Vec<&str> = row
            .results
            .iter()
            .filter(|(_, passed, _)| !passed)
            .map(|(scenario, _, _)| *scenario)
            .collect();
        let errored = row
            .results
            .iter()
            .any(|(_, _, outcome)| outcome.error.is_some());
        if errored {
            out.push(format!(
                "{name}: a scenario errored (see above); fix that before reading the scores"
            ));
            continue;
        }
        let memory_failed = failed
            .iter()
            .any(|scenario| matches!(*scenario, "remember" | "supersede"));
        let wrote_through_memory = row.results.iter().any(|(_, _, outcome)| {
            outcome.calls.iter().any(|(tool, arguments, _)| {
                tool == "memory"
                    && serde_json::from_str::<serde_json::Value>(arguments)
                        .ok()
                        .and_then(|value| value["command"].as_str().map(str::to_string))
                        .is_some_and(|command| command != "view")
            })
        });
        if row.memory_writes && (memory_failed || wrote_through_memory) {
            out.push(format!(
                "{name}: wrote memory through the memory tool{}; try remember as the only \
                 write path: gertie eval memory --endpoint {name} --set memory_writes=false",
                if memory_failed {
                    " and failed a memory scenario"
                } else {
                    ""
                }
            ));
        }
        match (row.tool_guidance, failed.is_empty(), row.trial) {
            (Standard, true, _) => out.push(format!(
                "{name}: passed everything with full tool guidance; terse guidance sends about \
                 60% fewer tool-definition tokens every request: gertie eval memory --endpoint \
                 {name} --set tool_guidance=terse"
            )),
            (Terse, true, trial) => {
                let standard = recorded_costs(history, row, "standard");
                let mut terse = recorded_costs(history, row, "terse");
                terse.push(cost(row));
                if standard.is_empty() {
                    if trial {
                        out.push(format!(
                            "{name}: passed everything with terse guidance, but there is no \
                             passing standard run to compare cost against: run gertie eval \
                             memory --endpoint {name} without --set first"
                        ));
                    }
                } else if standard.len() < RUNS_FOR_VERDICT || terse.len() < RUNS_FOR_VERDICT {
                    out.push(format!(
                        "{name}: terse passed; {} terse and {} standard run(s) on record, and \
                         single runs vary as much as the effect: run {} again before deciding",
                        terse.len(),
                        standard.len(),
                        if terse.len() < RUNS_FOR_VERDICT {
                            format!(
                                "gertie eval memory --endpoint {name} --set tool_guidance=terse"
                            )
                        } else {
                            format!("gertie eval memory --endpoint {name} (standard)")
                        }
                    ));
                } else {
                    let saving = 1.0 - mean(&terse) / mean(&standard);
                    let numbers = format!(
                        "mean cost {:.0} terse over {} runs vs {:.0} standard over {} \
                         ({:+.0}%; output weighted x{OUTPUT_WEIGHT})",
                        mean(&terse),
                        terse.len(),
                        mean(&standard),
                        standard.len(),
                        -saving * 100.0
                    );
                    match (saving >= TERSE_MIN_SAVING, trial) {
                        (true, true) => out.push(format!(
                            "{name}: terse passes and pays ({numbers}); keep it by adding \
                             tool_guidance = \"terse\" to its [[endpoints]] entry"
                        )),
                        (true, false) => {}
                        (false, _) => out.push(format!(
                            "{name}: terse passes but does not pay ({numbers}): with less \
                             coaching the model takes more steps; stay on standard{}",
                            if trial {
                                ""
                            } else {
                                ": remove tool_guidance from the endpoint"
                            }
                        )),
                    }
                }
            }
            (Terse, false, _) => out.push(format!(
                "{name}: failed {} with terse guidance; it needs the full descriptions: \
                 tool_guidance = \"standard\" (or remove the setting)",
                failed.join(", ")
            )),
            _ => {}
        }
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
                "memory_writes": row.memory_writes,
                "tool_guidance": row.tool_guidance.as_str(),
                "trial": row.trial,
                "scenarios": scenarios,
            })
            .to_string()
        })
        .collect()
}

/// Today in UTC, from SQLite's clock (the one memory dates use).
pub(crate) fn today() -> String {
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
pub fn run(config_path: Option<PathBuf>, endpoints: Vec<String>, settings: Vec<String>) -> i32 {
    let overrides = match Overrides::parse(&settings) {
        Ok(overrides) => overrides,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    let Some(config_path) = config_path.or_else(crate::config::Config::path) else {
        eprintln!("error: no config file (pass --config)");
        return 1;
    };
    match evaluate(&config_path, &endpoints, &overrides) {
        Ok(rows) => {
            let path = crate::config::Config::data_dir().map(|dir| dir.join("eval/memory.jsonl"));
            let history: Vec<String> = path
                .as_ref()
                .and_then(|path| std::fs::read_to_string(path).ok())
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default();
            print!("{}", render(&rows, &history));
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
        let overrides =
            Overrides::parse(&["tool_guidance=terse".into(), "memory_writes=true".into()]).unwrap();
        let path = config_for(raw, "b", &overrides, tmp.path()).unwrap();
        let written: toml::Value = toml::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(written["default_endpoint"].as_str(), Some("b"));
        assert_eq!(
            written["endpoints"][0]["api_key_env"].as_str(),
            Some("KEY_A")
        );
        assert_eq!(
            written["endpoints"][1]["tool_guidance"].as_str(),
            Some("terse")
        );
        assert_eq!(
            written["endpoints"][1]["memory_writes"].as_bool(),
            Some(true)
        );
        assert!(
            written["endpoints"][0].get("tool_guidance").is_none(),
            "only the endpoint under test"
        );
        assert!(Overrides::parse(&["speed=fast".into()]).is_err());
        assert!(Overrides::parse(&["tool_guidance=loud".into()]).is_err());
    }

    /// A scenario's name, mark, and (tool, arguments) calls.
    type Scripted<'a> = (&'static str, bool, Vec<(&'a str, &'a str)>);

    fn row_of(
        guidance: gertie_core::ToolGuidance,
        memory_writes: bool,
        trial: bool,
        results: Vec<Scripted<'_>>,
    ) -> Row {
        Row {
            endpoint: "m".into(),
            model: "m".into(),
            memory_writes,
            tool_guidance: guidance,
            trial,
            results: results
                .into_iter()
                .map(|(name, passed, calls)| {
                    (
                        name,
                        passed,
                        Outcome {
                            root: PathBuf::new(),
                            calls: calls
                                .into_iter()
                                .map(|(tool, arguments)| {
                                    (tool.into(), arguments.into(), String::new())
                                })
                                .collect(),
                            text: String::new(),
                            entries: Vec::new(),
                            tokens: (0, 0),
                            lint: 0,
                            error: None,
                            _fixture: None,
                        },
                    )
                })
                .collect(),
        }
    }

    /// #42: each result shape gets the setting to try, with the command
    /// that confirms it.
    #[test]
    fn suggestions_follow_the_results() {
        use gertie_core::ToolGuidance::{Standard, Terse};
        let all_pass = || {
            vec![
                ("heed", true, vec![]),
                ("remember", true, vec![("remember", "{}")]),
                ("supersede", true, vec![("remember", "{}")]),
            ]
        };
        let said = |row: Row| suggestions(&[row], &[]).join("\n");

        let standard = said(row_of(Standard, false, false, all_pass()));
        assert!(standard.contains("--set tool_guidance=terse"), "{standard}");

        // A terse trial is judged on mean cost against passing standard
        // runs of the same endpoint, model and memory setting.
        let unjudged = said(row_of(Terse, false, true, all_pass()));
        assert!(unjudged.contains("no passing standard run"), "{unjudged}");
        let run = |guidance: &str, tokens_in: u64, tokens_out: u64| {
            serde_json::json!({
                "endpoint": "m", "model": "m", "tool_guidance": guidance, "memory_writes": false,
                "scenarios": {"heed": {"passed": true, "tokens_in": tokens_in,
                                       "tokens_out": tokens_out, "tools": []}}
            })
            .to_string()
        };
        let trial = || vec![row_of(Terse, false, true, all_pass())];
        // One standard run: not enough for a verdict.
        let thin = suggestions(&trial(), &[run("standard", 40_000, 1_000)]).join("\n");
        assert!(thin.contains("again before deciding"), "{thin}");
        // The trial row costs 0; a recorded terse run at 44k makes terse
        // mean 22k against standard 44k: 50% cheaper, keep it.
        let pays = suggestions(
            &trial(),
            &[
                run("standard", 40_000, 1_000),
                run("standard", 40_000, 1_000),
                run("terse", 40_000, 1_000),
            ],
        )
        .join("\n");
        assert!(
            pays.contains("pays") && pays.contains("tool_guidance = \"terse\""),
            "{pays}"
        );
        // Terse at the same mean cost (88k and the trial 0): no pay.
        let flat = suggestions(
            &trial(),
            &[
                run("standard", 40_000, 1_000),
                run("standard", 40_000, 1_000),
                run("terse", 88_000, 0),
            ],
        )
        .join("\n");
        assert!(
            flat.contains("does not pay") && flat.contains("stay on standard"),
            "{flat}"
        );
        // A failing run, or one with the other memory setting, is no
        // baseline.
        let other_writes = serde_json::json!({
            "endpoint": "m", "model": "m", "tool_guidance": "standard", "memory_writes": true,
            "scenarios": {"heed": {"passed": true, "tokens_in": 1, "tokens_out": 1, "tools": []}}
        })
        .to_string();
        let no_base = suggestions(&trial(), &[other_writes]).join("\n");
        assert!(no_base.contains("no passing standard run"), "{no_base}");

        let settled = said(row_of(Terse, false, false, all_pass()));
        assert!(
            settled.is_empty(),
            "terse from config and passing: nothing to say: {settled}"
        );

        let mut failing = all_pass();
        failing[0].1 = false;
        let back = said(row_of(Terse, false, false, failing));
        assert!(
            back.contains("failed heed") && back.contains("\"standard\""),
            "{back}"
        );

        let mut doubled = all_pass();
        doubled[1].2 = vec![
            ("memory", r#"{"command": "insert", "path": "project/c.md"}"#),
            ("remember", "{}"),
        ];
        let one_path = said(row_of(Standard, true, false, doubled));
        assert!(one_path.contains("--set memory_writes=false"), "{one_path}");

        let mut viewed = all_pass();
        viewed[1].2 = vec![("memory", r#"{"path": "project"}"#), ("remember", "{}")];
        let views = said(row_of(Standard, true, false, viewed));
        assert!(
            !views.contains("memory_writes"),
            "a view is not a write: {views}"
        );
    }

    /// Terse guidance has to be worth having: the real tool set's
    /// definitions under terse are less than half of standard.
    #[test]
    fn terse_definitions_are_under_half_of_standard() {
        let tmp = tempfile::tempdir().unwrap();
        let boot = crate::bootstrap::Bootstrap::load_at(
            None,
            true,
            tmp.path().to_path_buf(),
            Some(tmp.path().join("global")),
        );
        let size = |guidance| {
            gertie_core::agent_loop::tool_specs(&boot.tools, guidance)
                .iter()
                .map(|spec| {
                    spec.name.len() + spec.description.len() + spec.parameters.to_string().len()
                })
                .sum::<usize>()
        };
        let standard = size(gertie_core::ToolGuidance::Standard);
        let terse = size(gertie_core::ToolGuidance::Terse);
        assert!(terse * 2 < standard, "terse {terse} vs standard {standard}");
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
        let rows = evaluate(&config, &[], &Overrides::default()).expect("the eval runs");
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
        let table = render(&rows, &[]);
        assert!(
            table.contains("scripted") && table.contains("supersede"),
            "{table}"
        );
        let line: serde_json::Value =
            serde_json::from_str(&record(&rows, "2026-11-26")[0]).unwrap();
        assert_eq!(line["model"], "scripted");
        assert_eq!(
            line["memory_writes"], false,
            "not a Claude model: remember only"
        );
        assert_eq!(line["scenarios"]["supersede"]["passed"], true);
        assert_eq!(line["scenarios"]["supersede"]["tools"][0], "remember");
    }
}
