//! `gertie eval nav` (#41): does the repo map make this model find code
//! faster? Each task runs twice in a fresh copy of one fixture repo,
//! once with the map (framing and tool) and once without, and the row
//! compares them: correct edits, steps to the first correct edit, and
//! tokens in.
//!
//! The fixture is a small Python service with decoys: every task names
//! a behavior, never a file, and the value to change also appears in
//! files that must stay untouched. Grep for the number finds all of
//! them; the right file is the one whose names fit the behavior. A
//! task passes when the target holds the new value and no other file
//! changed.

use crate::eval::{Outcome, Overrides, config_for, publish, run_fixture, targets};
use std::path::{Path, PathBuf};

/// One navigation task.
struct Task {
    name: &'static str,
    prompt: &'static str,
    /// The file the fix belongs in, and the text it must then contain.
    target: &'static str,
    fixed: &'static str,
}

const TASKS: [Task; 3] = [
    Task {
        name: "backoff",
        prompt: "When the connection drops, the client waits far too long between \
                 reconnection attempts: the longest wait should be 30 seconds, not 300. Fix it.",
        target: "svc/transport/policy.py",
        fixed: "CEILING_SECONDS = 30\n",
    },
    Task {
        name: "sessions",
        prompt: "Users are signed out after 5 minutes. A signed-in session should last 60 \
                 minutes. Fix it.",
        target: "svc/auth/tokens.py",
        fixed: "LIFETIME_MINUTES = 60\n",
    },
    Task {
        name: "money",
        prompt: "Invoice totals are shown rounded to whole units. Show them with two \
                 decimal places.",
        target: "svc/billing/render.py",
        fixed: "{amount:.2f}",
    },
];

/// The fixture: targets, decoys holding the same values, and filler so
/// a directory listing does not give the answer away.
const FILES: &[(&str, &str)] = &[
    (
        "svc/transport/policy.py",
        "\"\"\"How the client paces reconnects after a dropped link.\"\"\"\n\nCEILING_SECONDS = 300\nFLOOR_SECONDS = 1\n\n\
         class ReconnectPolicy:\n    def __init__(self, factor=2):\n        self.factor = factor\n\n    \
         def next_wait(self, attempt):\n        return min(FLOOR_SECONDS * self.factor ** attempt, CEILING_SECONDS)\n",
    ),
    (
        "svc/transport/link.py",
        "class Link:\n    def __init__(self, host, port):\n        self.host = host\n        self.port = port\n\n    \
         def open(self):\n        return True\n\n    def close(self):\n        return None\n",
    ),
    (
        "svc/transport/frames.py",
        "HEADER_BYTES = 8\n\ndef encode(payload):\n    return bytes(HEADER_BYTES) + payload\n\n\
         def decode(frame):\n    return frame[HEADER_BYTES:]\n",
    ),
    (
        "svc/jobs/scheduler.py",
        "TIMEOUT_SECONDS = 300\nPOLL_MINUTES = 5\n\nclass Scheduler:\n    def __init__(self):\n        self.jobs = []\n\n    \
         def add(self, job):\n        self.jobs.append(job)\n\n    def due(self, now):\n        return [job for job in self.jobs if job.at <= now]\n",
    ),
    (
        "svc/jobs/worker.py",
        "def run(job):\n    return job()\n\ndef drain(queue):\n    while queue:\n        run(queue.pop())\n",
    ),
    (
        "svc/cache/ttl.py",
        "DEFAULT_TTL_SECONDS = 300\nEXPIRY_MINUTES = 5\n\nclass TtlCache:\n    def __init__(self):\n        self.items = {}\n\n    \
         def get(self, key):\n        return self.items.get(key)\n\n    def put(self, key, value):\n        self.items[key] = value\n",
    ),
    (
        "svc/cache/keys.py",
        "def key_for(user, resource):\n    return f\"{user}:{resource}\"\n",
    ),
    (
        "svc/auth/tokens.py",
        "\"\"\"Bearer tokens for signed-in users.\"\"\"\n\nLIFETIME_MINUTES = 5\nREFRESH_WINDOW_MINUTES = 1\n\n\
         def issue(user_id, now):\n    return {\"user\": user_id, \"expires\": now + LIFETIME_MINUTES * 60}\n\n\
         def refresh(token, now):\n    return issue(token[\"user\"], now)\n",
    ),
    (
        "svc/auth/passwords.py",
        "MIN_LENGTH = 12\n\ndef strong_enough(password):\n    return len(password) >= MIN_LENGTH\n",
    ),
    (
        "svc/auth/roles.py",
        "ADMIN = \"admin\"\nMEMBER = \"member\"\n\ndef can_edit(role):\n    return role == ADMIN\n",
    ),
    (
        "svc/billing/render.py",
        "def money_text(amount, currency=\"EUR\"):\n    return f\"{amount:.0f} {currency}\"\n\n\
         def line_text(item):\n    return f\"{item.name}: {money_text(item.amount)}\"\n",
    ),
    (
        "svc/billing/ledger.py",
        "class Ledger:\n    def __init__(self):\n        self.entries = []\n\n    def post(self, amount):\n        self.entries.append(amount)\n\n    \
         def balance(self):\n        return sum(self.entries)\n",
    ),
    (
        "svc/reports/summary.py",
        "def percent_text(share):\n    return f\"{share * 100:.0f}%\"\n\ndef headline(report):\n    return f\"{report.title}: {percent_text(report.share)}\"\n",
    ),
    (
        "svc/reports/export.py",
        "import csv\n\ndef to_csv(rows, path):\n    with open(path, \"w\") as handle:\n        csv.writer(handle).writerows(rows)\n",
    ),
    (
        "svc/api/routes.py",
        "ROUTES = {}\n\ndef route(path):\n    def register(handler):\n        ROUTES[path] = handler\n        return handler\n    return register\n",
    ),
    (
        "svc/api/handlers.py",
        "from svc.api.routes import route\n\n@route(\"/health\")\ndef health():\n    return \"ok\"\n\n@route(\"/me\")\ndef me(user):\n    return user\n",
    ),
    (
        "svc/config/settings.py",
        "DEBUG = False\nPORT = 8080\nWORKERS = 4\n\ndef load(environ):\n    return {\"port\": int(environ.get(\"PORT\", PORT))}\n",
    ),
    (
        "svc/util/clock.py",
        "import time\n\ndef now():\n    return time.time()\n\ndef minutes(n):\n    return n * 60\n",
    ),
    (
        "svc/util/text.py",
        "def slug(text):\n    return \"-\".join(text.lower().split())\n",
    ),
    ("svc/__init__.py", ""),
    (
        "README.md",
        "# svc\n\nA small service: transport, jobs, cache, auth, billing, reports, api.\n",
    ),
];

/// One task's two runs.
pub struct Pair {
    pub task: &'static str,
    pub with_map: Run,
    pub without: Run,
}

/// One run's score.
pub struct Run {
    pub passed: bool,
    /// Tool calls up to and including the first edit of the target
    /// file, None when the target was never edited.
    pub steps: Option<usize>,
    pub calls: usize,
    pub tokens: (u64, u64),
    pub error: Option<String>,
}

/// One endpoint's comparison.
pub struct Row {
    pub endpoint: String,
    pub model: String,
    pub pairs: Vec<Pair>,
}

const EDITS: [&str; 2] = ["edit_file", "write_file"];

fn lay(root: &Path) -> std::io::Result<()> {
    for (path, text) in FILES {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("a file has a parent"))?;
        std::fs::write(path, text)?;
    }
    Ok(())
}

/// Scores one run: the target holds the fix, nothing else changed.
fn score(task: &Task, outcome: &Outcome) -> Run {
    let target_ok = std::fs::read_to_string(outcome.root.join(task.target))
        .is_ok_and(|text| text.contains(task.fixed));
    let collateral = FILES.iter().any(|(path, original)| {
        *path != task.target
            && std::fs::read_to_string(outcome.root.join(path))
                .ok()
                .as_deref()
                != Some(original)
    });
    let steps = outcome
        .calls
        .iter()
        .position(|(tool, arguments, _)| {
            EDITS.contains(&tool.as_str())
                && serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()
                    .and_then(|value| value["path"].as_str().map(str::to_string))
                    .is_some_and(|path| path.trim_start_matches("./") == task.target)
        })
        .map(|index| index + 1);
    Run {
        passed: outcome.error.is_none() && target_ok && !collateral,
        steps,
        calls: outcome.calls.len(),
        tokens: outcome.tokens,
        error: outcome.error.clone(),
    }
}

/// Every task, with and without the map, against every named endpoint.
pub fn evaluate(config_path: &Path, endpoints: &[String]) -> Result<Vec<Row>, String> {
    let (raw, targets) = targets(config_path, endpoints)?;
    let mut rows = Vec::new();
    for endpoint in targets {
        let name = endpoint.name;
        let model = endpoint.model;
        let dir = tempfile::tempdir().map_err(|error| error.to_string())?;
        let on_dir = dir.path().join("on");
        let off_dir = dir.path().join("off");
        std::fs::create_dir_all(&on_dir).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&off_dir).map_err(|error| error.to_string())?;
        let with = |enabled: bool, at: &Path| {
            config_for(
                &raw,
                &name,
                &Overrides {
                    repo_map: Some(enabled),
                    ..Overrides::default()
                },
                at,
            )
        };
        let on = with(true, &on_dir)?;
        let off = with(false, &off_dir)?;
        let mut pairs = Vec::new();
        for task in &TASKS {
            eprintln!("· {name}: {} with the map", task.name);
            let with_map = score(task, &run_fixture(task.name, lay, task.prompt, &on));
            eprintln!("· {name}: {} without", task.name);
            let without = score(task, &run_fixture(task.name, lay, task.prompt, &off));
            pairs.push(Pair {
                task: task.name,
                with_map,
                without,
            });
        }
        rows.push(Row {
            endpoint: name,
            model,
            pairs,
        });
    }
    Ok(rows)
}

fn mark(run: &Run) -> String {
    let steps = run
        .steps
        .map_or_else(|| "-".to_string(), |steps| steps.to_string());
    format!("{} {steps:>2}", if run.passed { "✓" } else { "✗" })
}

/// Totals for one side of a row: passes, steps to the first correct
/// edit summed over passing tasks, tokens in.
fn totals(row: &Row, map: bool) -> (usize, usize, u64) {
    row.pairs
        .iter()
        .fold((0, 0, 0), |(passed, steps, tokens), pair| {
            let run = if map { &pair.with_map } else { &pair.without };
            (
                passed + usize::from(run.passed),
                steps
                    + if run.passed {
                        run.steps.unwrap_or(0)
                    } else {
                        0
                    },
                tokens + run.tokens.0,
            )
        })
}

/// The mean tokens in per run, with and without the map, over every
/// recorded run of this endpoint and model (this one included). One
/// run varies as much as the effect; the record settles it.
fn recorded_means(history: &[String], row: &Row) -> Option<(f64, f64, usize)> {
    let mut on: Vec<f64> = Vec::new();
    let mut off: Vec<f64> = Vec::new();
    for line in history {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["endpoint"] != row.endpoint.as_str() || value["model"] != row.model.as_str() {
            continue;
        }
        if let (Some(with), Some(without)) = (
            value["tokens_in_with_map"].as_f64(),
            value["tokens_in_without"].as_f64(),
        ) {
            on.push(with);
            off.push(without);
        }
    }
    on.push(totals(row, true).2 as f64);
    off.push(totals(row, false).2 as f64);
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    Some((mean(&on), mean(&off), on.len()))
}

pub fn render(rows: &[Row], history: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<24} {:>10} {:>14} {:>14} {:>12}\n",
        "endpoint", "task", "with map", "without", "tokens in"
    ));
    for row in rows {
        for (index, pair) in row.pairs.iter().enumerate() {
            out.push_str(&format!(
                "{:<24} {:>10} {:>14} {:>14} {:>12}\n",
                if index == 0 {
                    row.endpoint.as_str()
                } else {
                    ""
                },
                pair.task,
                mark(&pair.with_map),
                mark(&pair.without),
                format!("{} / {}", pair.with_map.tokens.0, pair.without.tokens.0),
            ));
            for (side, run) in [("with map", &pair.with_map), ("without", &pair.without)] {
                if let Some(error) = &run.error {
                    out.push_str(&format!("  {} {side}: error: {error}\n", pair.task));
                }
            }
        }
        let (on_passed, on_steps, on_tokens) = totals(row, true);
        let (off_passed, off_steps, off_tokens) = totals(row, false);
        let change = |on: f64, off: f64| {
            if off == 0.0 {
                "n/a".to_string()
            } else {
                format!("{:+.0}%", (on / off - 1.0) * 100.0)
            }
        };
        out.push_str(&format!(
            "  this run: passed {on_passed}/{n} with the map, {off_passed}/{n} without; steps to \
             the first correct edit {on_steps} vs {off_steps}; tokens in {on_tokens} vs \
             {off_tokens} ({})\n",
            change(on_tokens as f64, off_tokens as f64),
            n = row.pairs.len(),
        ));
        if let Some((on, off, runs)) = recorded_means(history, row)
            && runs > 1
        {
            out.push_str(&format!(
                "  over {runs} recorded runs: mean tokens in {on:.0} with the map vs {off:.0} \
                 without ({})\n",
                change(on, off)
            ));
        }
    }
    out.push_str(
        "\neach cell: ✓/✗ (the fix landed in the right file, nothing else changed) and the tool \
         calls up to the first edit of that file\n",
    );
    out
}

pub fn record(rows: &[Row], date: &str) -> Vec<String> {
    rows.iter()
        .map(|row| {
            let tasks: serde_json::Map<String, serde_json::Value> = row
                .pairs
                .iter()
                .map(|pair| {
                    let side = |run: &Run| {
                        serde_json::json!({
                            "passed": run.passed,
                            "steps": run.steps,
                            "calls": run.calls,
                            "tokens_in": run.tokens.0,
                            "tokens_out": run.tokens.1,
                            "error": run.error,
                        })
                    };
                    (
                        pair.task.to_string(),
                        serde_json::json!({
                            "with_map": side(&pair.with_map),
                            "without": side(&pair.without),
                        }),
                    )
                })
                .collect();
            serde_json::json!({
                "suite": "nav",
                "date": date,
                "version": crate::cli::VERSION,
                "endpoint": row.endpoint,
                "model": row.model,
                "tokens_in_with_map": totals(row, true).2,
                "tokens_in_without": totals(row, false).2,
                "tasks": tasks,
            })
            .to_string()
        })
        .collect()
}

/// `gertie eval nav`.
pub fn run(config_path: Option<PathBuf>, endpoints: Vec<String>) -> i32 {
    let Some(config_path) = config_path.or_else(crate::config::Config::path) else {
        eprintln!("error: no config file (pass --config)");
        return 1;
    };
    match evaluate(&config_path, &endpoints) {
        Ok(rows) => {
            publish("nav.jsonl", &rows, render, record);
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

    /// Every task's target holds the value to change, and so does a
    /// decoy: the number alone never identifies the file.
    #[test]
    fn every_task_has_a_target_and_a_decoy() {
        let tmp = tempfile::tempdir().unwrap();
        lay(tmp.path()).unwrap();
        for task in &TASKS {
            let target = std::fs::read_to_string(tmp.path().join(task.target)).unwrap();
            assert!(!target.contains(task.fixed), "{}: not fixed yet", task.name);
        }
        let decoys = |needle: &str| {
            FILES
                .iter()
                .filter(|(_, text)| text.contains(needle))
                .count()
        };
        assert!(decoys("300") >= 3, "backoff: 300 in several files");
        assert!(
            decoys("_MINUTES = 5") >= 3,
            "sessions: 5 minutes in several files"
        );
        assert!(decoys(":.0f}") >= 2, "money: whole units in two files");
    }

    /// The map names each target's telling symbols within its budget.
    #[test]
    fn the_map_names_what_each_task_needs() {
        let tmp = tempfile::tempdir().unwrap();
        lay(tmp.path()).unwrap();
        let map = gertie_tools::repo_map::RepoMap::for_root(tmp.path());
        let body = map
            .framing(&gertie_core::repo_map::MapHints::default(), 500)
            .unwrap();
        assert!(body.len() <= 2_000, "{body}");
        for needle in [
            "svc/transport/policy.py: CEILING_SECONDS; FLOOR_SECONDS; class ReconnectPolicy",
            "svc/auth/tokens.py: LIFETIME_MINUTES",
            "svc/billing/render.py: def money_text",
        ] {
            assert!(body.contains(needle), "{needle}\n{body}");
        }
    }

    fn outcome_in(root: &Path, calls: Vec<(String, String, String)>) -> Outcome {
        Outcome::for_test(root, calls, (1_000, 100))
    }

    /// Scoring: the fix in the target passes, a decoy edited fails it,
    /// and steps count the calls up to the target's first edit.
    #[test]
    fn a_run_passes_only_with_the_fix_and_no_collateral() {
        let tmp = tempfile::tempdir().unwrap();
        lay(tmp.path()).unwrap();
        let task = &TASKS[0];
        let path = tmp.path().join(task.target);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("CEILING_SECONDS = 300", "CEILING_SECONDS = 30"),
        )
        .unwrap();
        let calls = vec![
            ("grep".into(), r#"{"pattern": "300"}"#.into(), String::new()),
            (
                "read_file".into(),
                r#"{"path": "svc/transport/policy.py"}"#.into(),
                String::new(),
            ),
            (
                "edit_file".into(),
                r#"{"path": "./svc/transport/policy.py"}"#.into(),
                String::new(),
            ),
        ];
        let run = score(task, &outcome_in(tmp.path(), calls.clone()));
        assert!(run.passed);
        assert_eq!(run.steps, Some(3));
        let decoy = tmp.path().join("svc/jobs/scheduler.py");
        std::fs::write(&decoy, "TIMEOUT_SECONDS = 30\n").unwrap();
        assert!(
            !score(task, &outcome_in(tmp.path(), calls)).passed,
            "a decoy changed"
        );
    }

    #[test]
    fn the_table_compares_both_sides() {
        let run = |passed, steps, tokens| Run {
            passed,
            steps,
            calls: 4,
            tokens: (tokens, 10),
            error: None,
        };
        let rows = vec![Row {
            endpoint: "m".into(),
            model: "m".into(),
            pairs: vec![Pair {
                task: "backoff",
                with_map: run(true, Some(2), 6_000),
                without: run(true, Some(5), 10_000),
            }],
        }];
        let table = render(&rows, &[]);
        assert!(table.contains("✓  2") && table.contains("✓  5"), "{table}");
        assert!(table.contains("tokens in 6000 vs 10000 (-40%)"), "{table}");
        let line: serde_json::Value =
            serde_json::from_str(&record(&rows, "2026-11-26")[0]).unwrap();
        assert_eq!(line["suite"], "nav");
        assert_eq!(line["tasks"]["backoff"]["with_map"]["steps"], 2);
        let again = render(&rows, &record(&rows, "2026-11-26"));
        assert!(again.contains("over 2 recorded runs"), "{again}");
    }
}
