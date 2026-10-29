//! The replay provider: scripted turns through the real machinery.
//!
//! "Do not spend a paid model to see a turn render" (learned from
//! t3code's dev tooling — see research/t3code-ideas.md §9): a replay
//! endpoint's model names a *scenario*, a TOML file of assistant steps
//! (prose, tool calls), and the provider streams the next step with
//! simulated pacing through the same [`Event`] contract as the real
//! adapters. The loop, permission broker, session store, and UI all run
//! unmodified; only the model is a script.
//!
//! Scenario resolution by model name: `showcase` and `approval` ship
//! embedded; any other name loads `<base_url>/<model>.toml`, where
//! base_url is the endpoint's configured scenario directory (`~` is
//! expanded in `EndpointConfig::resolve`).

use crate::Endpoint;
use futures_core::stream::BoxStream;
use gertie_core::Event;
use gertie_core::request::Request;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio_stream::wrappers::UnboundedReceiverStream;

/// The dev scenario: prose, a real read, and a markdown answer.
const SHOWCASE: &str = include_str!("replay/showcase.toml");
/// The approval scenario: a scripted write, so the permission card and
/// the deny path get exercised without a paid model anywhere.
const APPROVAL: &str = include_str!("replay/approval.toml");

/// One assistant response: the text streamed as Token deltas, the tool
/// calls emitted whole after it — the shape the normalized contract
/// demands of every adapter.
#[derive(Debug, Deserialize)]
struct Step {
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool_calls: Vec<ScriptedCall>,
}

#[derive(Debug, Deserialize)]
struct ScriptedCall {
    name: String,
    /// A literal JSON arguments string — single-quoted TOML keeps it
    /// painless (`arguments = '{"path": "x"}'`). What the wire carries
    /// is what you write.
    #[serde(default)]
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    #[serde(default)]
    steps: Vec<Step>,
}

pub struct ReplayProvider {
    endpoints: Vec<Endpoint>,
    /// Pause between simulated text deltas, so the UI sees streaming
    /// rather than one dump. Zero in tests.
    delta_delay: Duration,
    /// Own runtime: spawned stream tasks must progress without any
    /// caller entering the runtime (same pattern as OpenAICompatible).
    runtime: tokio::runtime::Runtime,
    /// Which scripted step answers the next request. One provider
    /// instance drives one conversation; the counter only advances
    /// after a step resolves, so a bad scenario name is free to retry.
    cursor: Arc<AtomicUsize>,
}

impl ReplayProvider {
    pub fn with_endpoints(endpoints: Vec<Endpoint>) -> Result<Self, reqwest::Error> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime");
        Ok(Self {
            endpoints,
            delta_delay: Duration::from_millis(12),
            runtime,
            cursor: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Tests: no pacing between deltas.
    #[cfg(test)]
    fn without_delay(mut self) -> Self {
        self.delta_delay = Duration::ZERO;
        self
    }

    /// The scenario file for a non-embedded model name: base_url is the
    /// directory. None when the endpoint names no directory — the error
    /// at load time explains the two ways out (embedded names, or a
    /// base_url).
    fn scenario_path(&self, model: &str) -> Option<String> {
        let dir = self.endpoints.first()?.base_url.trim().to_string();
        (!dir.is_empty()).then(|| format!("{}/{model}.toml", dir.trim_end_matches('/')))
    }

    pub fn context_window(&self) -> Option<u64> {
        self.endpoints
            .first()
            .and_then(|endpoint| endpoint.context_window)
    }
}

fn load_scenario(model: &str, path: Option<&str>) -> Result<Scenario, String> {
    let embedded = match model {
        "showcase" => Some(SHOWCASE),
        "approval" => Some(APPROVAL),
        _ => None,
    };
    let raw = match embedded {
        Some(text) => text.to_string(),
        None => {
            let Some(path) = path else {
                return Err(format!(
                    "replay scenario {model:?} not found — set base_url to a scenario \
                     directory, or use the embedded \"showcase\" / \"approval\" scenarios"
                ));
            };
            std::fs::read_to_string(path).map_err(|error| {
                format!("replay scenario {model:?}: could not read {path}: {error}")
            })?
        }
    };
    toml::from_str(&raw).map_err(|error| format!("replay scenario {model:?}: {error}"))
}

/// Char-boundary-safe fixed-size chunks, so simulated deltas can't tear
/// a multi-byte character the way a byte slice would.
fn text_chunks(text: &str, size: usize) -> impl Iterator<Item = &str> {
    text.char_indices().step_by(size).map(move |(index, _)| {
        text[index..]
            .char_indices()
            .nth(size)
            .map_or(&text[index..], |(end, _)| &text[index..index + end])
    })
}

impl gertie_core::Provider for ReplayProvider {
    fn stream(&self, request: Request) -> BoxStream<'static, Event> {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let model = self
            .endpoints
            .first()
            .map(|endpoint| endpoint.model.clone())
            .unwrap_or_default();
        let path = self.scenario_path(&model);
        let cursor = Arc::clone(&self.cursor);
        let delay = self.delta_delay;
        let input_tokens: u64 = (request
            .messages
            .iter()
            .map(|message| message.content.as_deref().unwrap_or("").len())
            .sum::<usize>()
            / 4) as u64;
        self.runtime.spawn(async move {
            let _ = sender.send(Event::Started);
            let scenario = match load_scenario(&model, path.as_deref()) {
                Ok(scenario) => scenario,
                Err(message) => {
                    let _ = sender.send(Event::Error { message });
                    return;
                }
            };
            let step_index = cursor.fetch_add(1, Ordering::Relaxed);
            let Some(step) = scenario.steps.get(step_index) else {
                let _ = sender.send(Event::Error {
                    message: format!(
                        "replay scenario {model:?} is exhausted ({} steps)",
                        scenario.steps.len()
                    ),
                });
                return;
            };

            for chunk in text_chunks(&step.text, 16) {
                let _ = sender.send(Event::Token {
                    delta: chunk.to_string(),
                });
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
            }
            let mut output_bytes = step.text.len();
            for (index, call) in step.tool_calls.iter().enumerate() {
                output_bytes += call.arguments.len();
                let _ = sender.send(Event::ToolCall {
                    id: format!("replay_{step_index}_{index}"),
                    name: call.name.clone(),
                    arguments: if call.arguments.trim().is_empty() {
                        "{}".to_string()
                    } else {
                        call.arguments.clone()
                    },
                });
            }
            let _ = sender.send(Event::Usage {
                input_tokens,
                output_tokens: (output_bytes / 4) as u64,
            });
            let _ = sender.send(Event::Done);
        });
        Box::pin(UnboundedReceiverStream::new(receiver))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Wire;
    use crate::test_support::{drain, user_request};
    use gertie_core::Provider;
    use gertie_core::WireMessage;

    fn replay_endpoint(model: &str, base_url: &str) -> Endpoint {
        Endpoint {
            name: "replay".into(),
            base_url: base_url.into(),
            model: model.into(),
            api_key: None,
            context_window: None,
            wire: Wire::Replay,
        }
    }

    fn request_with(history: &[&str]) -> Request {
        Request {
            messages: history
                .iter()
                .map(|&text| WireMessage::user(text))
                .collect(),
            tools: Vec::new(),
            max_output_tokens: None,
        }
    }

    #[test]
    fn showcase_streams_deltas_a_real_tool_call_and_usage() {
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint("showcase", "")])
            .unwrap()
            .without_delay();
        let events = drain(provider.stream(user_request()));

        assert_eq!(events.first(), Some(&Event::Started));
        // The prose arrives in more than one delta — that's the
        // "simulated streaming" part.
        assert!(
            events
                .iter()
                .filter(|e| matches!(e, Event::Token { .. }))
                .count()
                > 1,
            "text streams, not one dump"
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCall { name, .. }
                if name.as_str() == "read_file")),
            "the scripted read_file call arrives whole"
        );
        assert!(events.iter().any(|e| matches!(e, Event::Usage { .. })));
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn the_next_request_gets_the_next_step() {
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint("showcase", "")])
            .unwrap()
            .without_delay();
        let _ = drain(provider.stream(user_request()));
        let events = drain(provider.stream(user_request()));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Token { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("What I found"), "step two answered: {text}");
    }

    #[test]
    fn an_exhausted_script_is_an_error_not_a_hang() {
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint("approval", "")])
            .unwrap()
            .without_delay();
        let request = request_with(&["go"]);
        // The cursor is per-provider, not per-history: two streams burn
        // both approval steps, the third finds nothing left.
        let _ = drain(provider.stream(request.clone()));
        let _ = drain(provider.stream(request.clone()));
        let events = drain(provider.stream(request));
        assert!(matches!(events.last(), Some(Event::Error { message })
                if message.contains("exhausted")));
    }

    #[test]
    fn unknown_names_need_a_scenario_directory() {
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint("nope", "")])
            .unwrap()
            .without_delay();
        let events = drain(provider.stream(user_request()));
        assert!(matches!(events.last(), Some(Event::Error { message })
                if message.contains("scenario directory")));
    }

    #[test]
    fn file_scenarios_load_from_base_url() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mine.toml"),
            "[[steps]]\ntext = \"hello from a file\"\n",
        )
        .unwrap();
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint(
            "mine",
            dir.path().to_str().unwrap(),
        )])
        .unwrap()
        .without_delay();
        let events = drain(provider.stream(user_request()));

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                Event::Token { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello from a file");
        assert_eq!(events.last(), Some(&Event::Done));
    }

    #[test]
    fn the_approval_scenario_prompts_a_write() {
        let provider = ReplayProvider::with_endpoints(vec![replay_endpoint("approval", "")])
            .unwrap()
            .without_delay();
        let events = drain(provider.stream(user_request()));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCall { name, .. }
                if name.as_str() == "write_file")),
            "the approval scenario's point is a mutating call"
        );
    }

    #[test]
    fn chunks_never_tear_a_multibyte_character() {
        let text = "héllo wörld — ünïcode ✓";
        let joined: String = text_chunks(text, 4).collect();
        assert_eq!(joined, text);
    }
}
