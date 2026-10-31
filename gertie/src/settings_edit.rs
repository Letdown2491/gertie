//! The settings panel as its own view: a summary of the live config
//! plus an editor over the five sections it owns, endpoints (with
//! keys), the default endpoint, the bash sandbox, MCP servers, and
//! agent types.
//!
//! The file stays the single source of truth: Save runs the edits
//! through `toml_edit`, which preserves the user's comments and any
//! sections the panel doesn't own. Endpoint changes apply live (the
//! workspace reloads the bootstrap and fans the new provider out to
//! every open chat); MCP, sandbox, and agents are load-time setup and
//! say "restart to apply" instead of pretending.

use crate::config::{
    AgentConfig, Config, ConfigEdits, EndpointConfig, Invocation, McpServerConfig,
};
use crate::settings::SettingsSummary;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    App, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Window, div, px,
};

/// Emitted after a successful save, the workspace reloads the
/// bootstrap and pushes the new provider into every open tab.
pub enum SettingsEvent {
    Saved,
}

/// One endpoint as the editor holds it, a plain-data copy of
/// `EndpointConfig` so the form never touches parse types.
#[derive(Clone)]
struct EndpointDraft {
    name: String,
    base_url: String,
    model: String,
    wire: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    context_window: Option<u64>,
}

impl EndpointDraft {
    fn from_config(endpoint: &EndpointConfig) -> Self {
        Self {
            name: endpoint.name.clone(),
            base_url: endpoint.base_url.clone(),
            model: endpoint.model.clone(),
            wire: endpoint.wire.clone(),
            api_key: endpoint.api_key.clone(),
            api_key_env: endpoint.api_key_env.clone(),
            context_window: endpoint.context_window,
        }
    }

    fn to_config(&self) -> EndpointConfig {
        EndpointConfig {
            name: self.name.trim().to_string(),
            base_url: self.base_url.trim().to_string(),
            model: self.model.trim().to_string(),
            wire: self.wire.clone(),
            api_key: self.api_key.clone(),
            api_key_env: self.api_key_env.clone(),
            context_window: self.context_window,
        }
    }
}

/// One MCP server as the editor holds it.
#[derive(Clone)]
struct McpDraft {
    name: String,
    url: Option<String>,
    command: Option<String>,
}

impl McpDraft {
    fn from_config(server: &McpServerConfig) -> Self {
        Self {
            name: server.name.clone(),
            url: server.url.clone(),
            command: server.command.clone(),
        }
    }
}

/// One agent type as the editor holds it. `invocation` rides along
/// unedited, the form can't create `user` types (UI dispatch doesn't
/// exist yet), but a hand-written one must survive a save.
#[derive(Clone)]
struct AgentDraft {
    name: String,
    description: String,
    invocation: Invocation,
    endpoint: Option<String>,
    model: Option<String>,
    prompt: Option<String>,
    tools: Option<Vec<String>>,
}

impl AgentDraft {
    fn from_config(agent: &AgentConfig) -> Self {
        Self {
            name: agent.name.clone(),
            description: agent.description.clone(),
            invocation: agent.invocation.clone(),
            endpoint: agent.endpoint.clone(),
            model: agent.model.clone(),
            prompt: agent.prompt.clone(),
            tools: agent.tools.clone(),
        }
    }

    fn to_config(&self) -> AgentConfig {
        AgentConfig {
            name: self.name.trim().to_string(),
            description: self.description.trim().to_string(),
            invocation: self.invocation.clone(),
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            prompt: self.prompt.clone(),
            tools: self.tools.clone(),
        }
    }
}

/// The endpoint form's text fields, created when the form opens.
struct EndpointForm {
    /// Index into `endpoints`; None = a new endpoint.
    editing: Option<usize>,
    name: Entity<InputState>,
    base_url: Entity<InputState>,
    model: Entity<InputState>,
    wire: Entity<InputState>,
    api_key_env: Entity<InputState>,
    api_key: Entity<InputState>,
    context_window: Entity<InputState>,
}

/// The agent form's text fields, created when the form opens.
struct AgentForm {
    /// Index into `agents`; None = a new agent.
    editing: Option<usize>,
    name: Entity<InputState>,
    description: Entity<InputState>,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    tools: Entity<InputState>,
    prompt: Entity<InputState>,
}

pub struct SettingsPanelView {
    summary: SettingsSummary,
    /// Pending edits, the desired next state of the config.
    endpoints: Vec<EndpointDraft>,
    default_endpoint: Option<String>,
    sandbox: bool,
    memory_enabled: bool,
    memory_dream_days: u64,
    /// The round-budget drafts: raw text, empty = the panel doesn't
    /// own the key (the file's value and the default read through).
    max_tool_rounds_draft: String,
    grace_rounds_draft: String,
    mcp_servers: Vec<McpDraft>,
    agents: Vec<AgentDraft>,
    form: Option<EndpointForm>,
    agent_form: Option<AgentForm>,
    /// The turn budget fields' inputs, created once.
    max_tool_rounds_input: Option<Entity<InputState>>,
    grace_rounds_input: Option<Entity<InputState>>,
    /// The MCP add form's fields, when open.
    mcp_name: Option<Entity<InputState>>,
    mcp_url: Option<Entity<InputState>>,
    mcp_command: Option<Entity<InputState>>,
    /// Last save result: (message, is_error).
    status: Option<(String, bool)>,
}

impl SettingsPanelView {
    pub fn new(config: &Config, summary: SettingsSummary) -> Self {
        let mut panel = Self {
            summary,
            endpoints: Vec::new(),
            default_endpoint: None,
            sandbox: false,
            memory_enabled: true,
            memory_dream_days: 7,
            max_tool_rounds_draft: String::new(),
            grace_rounds_draft: String::new(),
            mcp_servers: Vec::new(),
            agents: Vec::new(),
            form: None,
            agent_form: None,
            max_tool_rounds_input: None,
            grace_rounds_input: None,
            mcp_name: None,
            mcp_url: None,
            mcp_command: None,
            status: None,
        };
        panel.reset_pending(config);
        panel
    }

    /// One budget field. The input is the source of truth from the
    /// moment it exists: created once (seeded from the config draft,
    /// placeholder naming the built-in default), every render copies
    /// its value into the draft, one direction only. The old
    /// reverse-sync forced the input back to the draft each frame,
    /// which wiped every keystroke before Save could see it.
    fn budget_field(
        &mut self,
        id: &'static str,
        placeholder: String,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let seed = match id {
            "turn-max-rounds" => self.max_tool_rounds_draft.clone(),
            _ => self.grace_rounds_draft.clone(),
        };
        let input = match id {
            "turn-max-rounds" => self.max_tool_rounds_input.get_or_insert_with(|| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                if !seed.is_empty() {
                    state.update(cx, |state, cx| state.set_value(&seed, window, cx));
                }
                state
            }),
            _ => self.grace_rounds_input.get_or_insert_with(|| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                if !seed.is_empty() {
                    state.update(cx, |state, cx| state.set_value(&seed, window, cx));
                }
                state
            }),
        };
        // The draft follows the input, one direction only: Save reads
        // the drafts, and a click can land before this frame's redraw,
        // so save_budgets also reads the inputs directly.
        let current = input.read(cx).value().to_string();
        match id {
            "turn-max-rounds" => self.max_tool_rounds_draft = current,
            _ => self.grace_rounds_draft = current,
        }
        v_flex()
            .id(id)
            .gap_0p5()
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(Input::new(input).small().w_full())
    }

    /// Pending state := the current config. Called on construction and
    /// after a save's live-apply round-trip.
    pub fn set_config(
        &mut self,
        config: &Config,
        summary: SettingsSummary,
        cx: &mut Context<Self>,
    ) {
        self.summary = summary;
        self.reset_pending(config);
        self.form = None;
        self.agent_form = None;
        self.mcp_name = None;
        self.mcp_url = None;
        self.mcp_command = None;
        cx.notify();
    }

    fn reset_pending(&mut self, config: &Config) {
        self.endpoints = config
            .endpoints
            .iter()
            .map(EndpointDraft::from_config)
            .collect();
        self.default_endpoint = config.default_endpoint.clone();
        // The sandbox draft starts from what's in force (the summary
        // is resolved against the repo's overlay), flipping the
        // switch then saves your own choice into your config, which
        // outranks the overlay from then on.
        self.sandbox = self.summary.sandbox;
        self.memory_enabled = self.summary.memory_enabled;
        self.memory_dream_days = self.summary.memory_dream_days;
        // The budget drafts pre-fill from the file's keys (absent =
        // empty, the panel doesn't own them). The placeholder shows
        // the built-in default, never a written value: writing the
        // default into every config makes it look chosen. The input
        // entities reset too, so the next render reseeds from the
        // new drafts rather than keeping a stale save's text.
        self.max_tool_rounds_input = None;
        self.grace_rounds_input = None;
        self.max_tool_rounds_draft = config
            .turn
            .as_ref()
            .and_then(|t| t.max_tool_rounds)
            .map(|rounds| rounds.to_string())
            .unwrap_or_default();
        self.grace_rounds_draft = config
            .turn
            .as_ref()
            .and_then(|t| t.grace_rounds)
            .map(|rounds| rounds.to_string())
            .unwrap_or_default();
        self.mcp_servers = config
            .mcp_servers
            .iter()
            .map(McpDraft::from_config)
            .collect();
        self.agents = config.agents.iter().map(AgentDraft::from_config).collect();
    }

    /// Opens the endpoint form for `index` (None = add), pre-filled
    /// from the draft.
    fn open_form(&mut self, index: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let draft = index
            .map(|i| self.endpoints[i].clone())
            .unwrap_or(EndpointDraft {
                name: String::new(),
                base_url: "https://".into(),
                model: String::new(),
                wire: None,
                api_key: None,
                api_key_env: None,
                context_window: None,
            });
        let mut field = |value: String, placeholder: &'static str, masked: bool| {
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .masked(masked)
            });
            if !value.is_empty() {
                state.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
            }
            state
        };
        let wire = draft.wire.clone().unwrap_or_default();
        let api_key_env = draft.api_key_env.clone().unwrap_or_default();
        let api_key = draft.api_key.clone().unwrap_or_default();
        self.form = Some(EndpointForm {
            editing: index,
            name: field(draft.name.clone(), "name", false),
            base_url: field(draft.base_url.clone(), "https://…/v1", false),
            model: field(draft.model.clone(), "model", false),
            wire: field(wire, "openai | anthropic | replay", false),
            api_key_env: field(api_key_env, "env var, e.g. PPQ_API_KEY", false),
            // A key on screen is a key over the user's shoulder: the
            // input masks, like a password field.
            api_key: field(api_key, "literal key, prefer the env var", true),
            context_window: field(
                draft
                    .context_window
                    .map(|window| window.to_string())
                    .unwrap_or_default(),
                "context window, tokens",
                false,
            ),
        });
        cx.notify();
    }

    /// Reads the form fields back into the pending list.
    fn commit_form(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.form else { return };
        let read = |entity: &Entity<InputState>| entity.read(cx).value().trim().to_string();
        let name = read(&form.name);
        let base_url = read(&form.base_url);
        let model = read(&form.model);
        if name.is_empty() || base_url.is_empty() || model.is_empty() {
            self.status = Some((
                "endpoint needs a name, a base URL, and a model".into(),
                true,
            ));
            cx.notify();
            return;
        }
        let context_window_field = read(&form.context_window);
        let context_window = if context_window_field.is_empty() {
            None
        } else {
            match context_window_field.parse::<u64>() {
                Ok(window) => Some(window),
                Err(_) => {
                    self.status = Some(("context window must be a number of tokens".into(), true));
                    cx.notify();
                    return;
                }
            }
        };
        let option_field = |raw: String| (!raw.is_empty()).then_some(raw);
        let draft = EndpointDraft {
            name,
            base_url,
            model,
            wire: option_field(read(&form.wire)),
            api_key: option_field(read(&form.api_key)),
            api_key_env: option_field(read(&form.api_key_env)),
            context_window,
        };
        match form.editing {
            Some(index) => self.endpoints[index] = draft,
            None => {
                if self.default_endpoint.is_none() {
                    self.default_endpoint = Some(draft.name.clone());
                }
                self.endpoints.push(draft);
            }
        }
        self.form = None;
        cx.notify();
    }

    fn remove_endpoint(&mut self, index: usize, cx: &mut Context<Self>) {
        let removed = self.endpoints.remove(index);
        if self.default_endpoint.as_deref() == Some(removed.name.as_str()) {
            self.default_endpoint = self.endpoints.first().map(|e| e.name.clone());
        }
        // The form edits by index; keep it pointing at the same draft.
        if let Some(form) = &mut self.form
            && let Some(editing) = form.editing
        {
            match editing.checked_sub(1) {
                Some(_) if editing > index => form.editing = Some(editing - 1),
                _ if editing == index => self.form = None,
                _ => {}
            }
        }
        cx.notify();
    }

    /// Opens the agent form for `index` (None = add), pre-filled from
    /// the draft.
    fn open_agent_form(
        &mut self,
        index: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = index.map(|i| self.agents[i].clone()).unwrap_or(AgentDraft {
            name: String::new(),
            description: String::new(),
            invocation: Invocation::Model,
            endpoint: None,
            model: None,
            prompt: None,
            tools: None,
        });
        let mut field = |value: Option<String>, placeholder: &'static str| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            if let Some(value) = value
                && !value.is_empty()
            {
                state.update(cx, |state, cx| state.set_value(value, window, cx));
            }
            state
        };
        let joined = |tools: Option<Vec<String>>| tools.map(|tools| tools.join(", "));
        self.agent_form = Some(AgentForm {
            editing: index,
            name: field(Some(draft.name.clone()), "name, e.g. council-anthropic"),
            description: field(
                Some(draft.description.clone()),
                "when the leader should use it",
            ),
            endpoint: field(
                draft.endpoint.clone(),
                "endpoint name, blank = the leader's",
            ),
            model: field(draft.model.clone(), "model override, blank = endpoint's"),
            tools: field(
                joined(draft.tools),
                "allowlist, comma-separated, blank = all",
            ),
            prompt: field(draft.prompt.clone(), "system prompt, blank = default"),
        });
        cx.notify();
    }

    /// Reads the agent form back into the pending list.
    fn commit_agent_form(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(form) = &self.agent_form else { return };
        let read = |entity: &Entity<InputState>| entity.read(cx).value().trim().to_string();
        let name = read(&form.name);
        let description = read(&form.description);
        if name.is_empty() || description.is_empty() {
            self.status = Some(("an agent needs a name and a description".into(), true));
            cx.notify();
            return;
        }
        if name.contains(char::is_whitespace) {
            self.status = Some((
                "agent name must be one word, letters, digits, dashes".into(),
                true,
            ));
            cx.notify();
            return;
        }
        let option_field = |raw: String| (!raw.is_empty()).then_some(raw);
        let tools_field = |raw: String| {
            let tools: Vec<String> = raw
                .split(',')
                .map(|tool| tool.trim().to_string())
                .filter(|tool| !tool.is_empty())
                .collect();
            (!tools.is_empty()).then_some(tools)
        };
        let draft = AgentDraft {
            name,
            description,
            invocation: form
                .editing
                .map(|index| self.agents[index].invocation.clone())
                .unwrap_or(Invocation::Model),
            endpoint: option_field(read(&form.endpoint)),
            model: option_field(read(&form.model)),
            prompt: option_field(read(&form.prompt)),
            tools: tools_field(read(&form.tools)),
        };
        match form.editing {
            Some(index) => self.agents[index] = draft,
            None => self.agents.push(draft),
        }
        self.agent_form = None;
        cx.notify();
    }

    fn remove_agent(&mut self, index: usize, cx: &mut Context<Self>) {
        self.agents.remove(index);
        // The form edits by index; keep it pointing at the same draft.
        if let Some(form) = &mut self.agent_form
            && let Some(editing) = form.editing
        {
            match editing.checked_sub(1) {
                Some(_) if editing > index => form.editing = Some(editing - 1),
                _ if editing == index => self.agent_form = None,
                _ => {}
            }
        }
        cx.notify();
    }

    /// Theme switch: applies immediately and persists, UI state, kept
    /// out of the user's config.toml.
    fn set_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        gpui_kit::component::Theme::change(mode, None, cx);
        crate::settings::store_theme(mode);
        cx.notify();
    }

    /// The Save button: writes the pending edits through toml_edit and
    /// emits Saved so the workspace reloads the bootstrap live.
    fn save_all(
        &mut self,
        _event: &gpui_kit::ClickEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The budgets: parsed from the drafts, an empty or unparseable
        // field means "the panel doesn't own this key" (None = the
        // file's value and the built-in default read through).
        let parse_rounds = |text: &str| -> Result<Option<usize>, String> {
            let text = text.trim();
            if text.is_empty() {
                return Ok(None);
            }
            text.parse::<usize>()
                .map(Some)
                .map_err(|_| format!("{text:?} is not a round count"))
        };
        // A bad number shows the error and saves nothing (the parse
        // runs again below for the value itself; both sides see the
        // same drafts).
        // The inputs are the source of truth while they exist: a
        // click can land before the redraw that syncs the drafts.
        let max_text = self
            .max_tool_rounds_input
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .unwrap_or_else(|| self.max_tool_rounds_draft.clone());
        let grace_text = self
            .grace_rounds_input
            .as_ref()
            .map(|entity| entity.read(cx).value().trim().to_string())
            .unwrap_or_else(|| self.grace_rounds_draft.clone());
        let max_parsed = parse_rounds(&max_text);
        let grace_parsed = parse_rounds(&grace_text);
        if let Err(error) = max_parsed.as_ref().or(grace_parsed.as_ref()) {
            self.status = Some((error.clone(), true));
            cx.notify();
            return;
        }
        let Some(path) = self.summary.config_path.clone() else {
            self.status = Some((
                "no config file location, create ~/.config/gertie/config.toml".into(),
                true,
            ));
            cx.notify();
            return;
        };
        let edits = ConfigEdits {
            endpoints: self.endpoints.iter().map(|d| d.to_config()).collect(),
            default_endpoint: self.default_endpoint.clone(),
            sandbox_bash: self.sandbox,
            memory_enabled: self.memory_enabled,
            memory_dream_days: self.memory_dream_days,
            mcp_servers: self
                .mcp_servers
                .iter()
                .map(|draft| McpServerConfig {
                    name: draft.name.clone(),
                    url: draft.url.clone(),
                    command: draft.command.clone(),
                    args: Vec::new(),
                    env: Default::default(),
                })
                .collect(),
            agents: self.agents.iter().map(|d| d.to_config()).collect(),
            max_tool_rounds: max_parsed.unwrap_or(None),
            grace_rounds: grace_parsed.unwrap_or(None),
        };
        match edits.save(&path) {
            Ok(()) => {
                self.status = Some((
                    "saved, endpoints apply to open chats; sandbox, MCP, and agents on restart"
                        .into(),
                    false,
                ));
                cx.emit(SettingsEvent::Saved);
            }
            Err(error) => self.status = Some((error, true)),
        }
        cx.notify();
    }
}

/// One endpoint row: edit on click, star to set default, ✕ to remove.
fn endpoint_row(
    index: usize,
    draft: &EndpointDraft,
    key_resolved: Option<bool>,
    is_default: bool,
    cx: &Context<SettingsPanelView>,
) -> impl IntoElement {
    let key_note = match key_resolved {
        Some(true) => "key ✓",
        Some(false) => "no key",
        None => "unsaved",
    };
    h_flex()
        .w_full()
        .gap_0p5()
        .child(
            Button::new(SharedString::from(format!("endpoint-{index}")))
                .label(format!("{} · {}", draft.name, draft.model))
                .small()
                .flex_1()
                .min_w_0()
                .when(is_default, |button| {
                    // The default endpoint gets the same quiet accent
                    // tint as the active session row.
                    button
                        .custom(ButtonCustomVariant::new(cx).color(cx.theme().accent.opacity(0.14)))
                })
                .tooltip(format!("{key_note}, click to edit"))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_form(Some(index), window, cx);
                })),
        )
        .child(
            Button::new(SharedString::from(format!("default-{index}")))
                .label(if is_default { "★" } else { "☆" })
                .xsmall()
                .ghost()
                .tooltip(if is_default {
                    "Default endpoint"
                } else {
                    "Make this the default endpoint"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.default_endpoint = Some(this.endpoints[index].name.clone());
                    cx.notify();
                })),
        )
        .child(
            Button::new(SharedString::from(format!("remove-endpoint-{index}")))
                .label("✕")
                .xsmall()
                .ghost()
                .tooltip("Remove (pending until Save)")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_endpoint(index, cx);
                })),
        )
}

/// One agent row: edit on click, ✕ to remove. The pin is the point,
/// a seat without its endpoint visible is just a name.
fn agent_row(
    index: usize,
    draft: &AgentDraft,
    cx: &Context<SettingsPanelView>,
) -> impl IntoElement {
    let mut label = draft.name.clone();
    if let Some(endpoint) = &draft.endpoint {
        label.push_str(&format!("@{endpoint}"));
    }
    if let Some(model) = &draft.model {
        label.push_str(&format!(" · {model}"));
    }
    h_flex()
        .w_full()
        .gap_0p5()
        .child(
            Button::new(SharedString::from(format!("agent-{index}")))
                .label(label)
                .small()
                .flex_1()
                .min_w_0()
                .tooltip(format!("{}, click to edit", draft.description))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_agent_form(Some(index), window, cx);
                })),
        )
        .child(
            Button::new(SharedString::from(format!("remove-agent-{index}")))
                .label("✕")
                .xsmall()
                .ghost()
                .tooltip("Remove (pending until Save)")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_agent(index, cx);
                })),
        )
}

/// The section-header styling for this panel.
fn section_header(title: &'static str, cx: &App) -> impl IntoElement {
    div()
        .mt_3()
        .mb_0p5()
        .text_size(px(11.))
        .text_color(cx.theme().muted_foreground)
        .child(title)
}

impl EventEmitter<SettingsEvent> for SettingsPanelView {}

impl Render for SettingsPanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        let form = self.form.as_ref().map(|form| {
            let labeled = |label: &'static str, field: &Entity<InputState>| {
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(Input::new(field).small().w_full())
            };
            v_flex()
                .id("endpoint-form")
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .p_2()
                .child(labeled("name", &form.name))
                .child(labeled("base url", &form.base_url))
                .child(labeled("model", &form.model))
                .child(labeled("wire", &form.wire))
                .child(labeled("key env var", &form.api_key_env))
                .child(labeled("literal key (written to file)", &form.api_key))
                .child(labeled("context window", &form.context_window))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("commit-endpoint")
                                .label("Save endpoint")
                                .small()
                                .primary()
                                .on_click(cx.listener(Self::commit_form)),
                        )
                        .child(
                            Button::new("cancel-endpoint")
                                .label("Cancel")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.form = None;
                                    cx.notify();
                                })),
                        ),
                )
        });

        let mcp_form = self
            .mcp_name
            .clone()
            .zip(self.mcp_url.clone())
            .zip(self.mcp_command.clone())
            .map(|((name, url), command)| {
                v_flex()
                    .gap_1()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .p_2()
                    .child(Input::new(&name).small().w_full())
                    .child(Input::new(&url).small().w_full())
                    .child(Input::new(&command).small().w_full())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("commit-mcp")
                                    .label("Add")
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let value =
                                            |entity: &Entity<InputState>, cx: &Context<Self>| {
                                                entity.read(cx).value().trim().to_string()
                                            };
                                        let name = value(&name, cx);
                                        let url = value(&url, cx);
                                        let command = value(&command, cx);
                                        if name.is_empty() || (url.is_empty() && command.is_empty())
                                        {
                                            this.status = Some((
                                                "mcp server needs a name and a url or command"
                                                    .into(),
                                                true,
                                            ));
                                            cx.notify();
                                            return;
                                        }
                                        this.mcp_servers.push(McpDraft {
                                            name,
                                            url: (!url.is_empty()).then_some(url),
                                            command: (!command.is_empty()).then_some(command),
                                        });
                                        this.mcp_name = None;
                                        this.mcp_url = None;
                                        this.mcp_command = None;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("cancel-mcp")
                                    .label("Cancel")
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.mcp_name = None;
                                        this.mcp_url = None;
                                        this.mcp_command = None;
                                        cx.notify();
                                    })),
                            ),
                    )
            });

        let agent_form = self.agent_form.as_ref().map(|form| {
            let labeled = |label: &'static str, field: &Entity<InputState>| {
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(label),
                    )
                    .child(Input::new(field).small().w_full())
            };
            v_flex()
                .id("agent-form")
                .gap_1p5()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .p_2()
                .child(labeled("name", &form.name))
                .child(labeled("description", &form.description))
                .child(labeled("endpoint", &form.endpoint))
                .child(labeled("model override", &form.model))
                .child(labeled("tool allowlist", &form.tools))
                .child(labeled("system prompt", &form.prompt))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new("commit-agent")
                                .label("Save agent")
                                .small()
                                .primary()
                                .on_click(cx.listener(Self::commit_agent_form)),
                        )
                        .child(
                            Button::new("cancel-agent")
                                .label("Cancel")
                                .small()
                                .ghost()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.agent_form = None;
                                    cx.notify();
                                })),
                        ),
                )
        });

        let mcp_form_open = mcp_form.is_some();
        v_flex()
            .id("settings-panel")
            .w(px(300.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .pt_3()
            .px_3()
            .pb_3()
            .gap_1()
            .overflow_y_scroll()
            .child(section_header("APPEARANCE", cx))
            .child({
                let current = gpui_kit::component::Theme::global(cx).mode;
                h_flex()
                    .gap_1()
                    .children([ThemeMode::Dark, ThemeMode::Light].map(|mode| {
                        let button =
                            Button::new(SharedString::from(format!("theme-{}", mode.name())))
                                .label(mode.name())
                                .small();
                        let button = if current == mode {
                            button.primary()
                        } else {
                            button.ghost()
                        };
                        button.on_click(cx.listener(move |this, _, _, cx| this.set_theme(mode, cx)))
                    }))
            })
            .child(section_header("ENDPOINTS", cx))
            .when(self.endpoints.is_empty(), |col| {
                col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("none configured, add one below"),
                )
            })
            .children(self.endpoints.iter().enumerate().map(|(index, draft)| {
                let key_resolved = self
                    .summary
                    .endpoints
                    .iter()
                    .find(|info| info.name == draft.name)
                    .map(|info| info.key_resolved);
                let is_default = self.default_endpoint.as_deref() == Some(draft.name.as_str());
                endpoint_row(index, draft, key_resolved, is_default, cx).into_any_element()
            }))
            .child(
                Button::new("add-endpoint")
                    .label("Add endpoint")
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_form(None, window, cx);
                    })),
            )
            .children(form)
            .child(section_header("SANDBOX", cx))
            .child(
                Switch::new("sandbox-switch")
                    .label(format!(
                        "Landlock-isolate bash children, from {}",
                        self.summary.sandbox_origin.as_str()
                    ))
                    .small()
                    .checked(self.sandbox)
                    .tooltip(concat!(
                        "Restart to apply. Your config sets this; while it",
                        " doesn't, the repo's .gertie/config.toml wins, and",
                        " that file may also write \"inherit\" to stand down."
                    ))
                    .on_click({
                        let panel = cx.entity().downgrade();
                        move |checked, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.sandbox = *checked;
                                    cx.notify();
                                });
                            }
                        }
                    }),
            )
            .child(section_header("MEMORY", cx))
            .child(
                Switch::new("memory-switch")
                    .label(format!(
                        "Remember across sessions, from {}",
                        self.summary.memory_origin.as_str()
                    ))
                    .small()
                    .checked(self.memory_enabled)
                    .tooltip(concat!(
                        "Session-end extraction proposes memory entries",
                        " as review cards on settle; dreaming consolidates",
                        " the store on startup. Applies on restart; the",
                        " files live in .gertie/memory/ and the data dir."
                    ))
                    .on_click({
                        let panel = cx.entity().downgrade();
                        move |checked, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.memory_enabled = *checked;
                                    cx.notify();
                                });
                            }
                        }
                    }),
            )
            .child(
                Switch::new("dream-switch")
                    .label(format!(
                        "Consolidate memory every {} days",
                        self.memory_dream_days
                    ))
                    .small()
                    .checked(self.memory_enabled && self.memory_dream_days > 0)
                    .tooltip(concat!(
                        "Startup pass that merges duplicate entries and",
                        " archives counter-dead ones. A changelog lands in",
                        " .gertie/memory/dreams/. Applies on restart."
                    ))
                    .on_click({
                        let panel = cx.entity().downgrade();
                        move |checked, _, cx| {
                            if let Some(panel) = panel.upgrade() {
                                panel.update(cx, |panel, cx| {
                                    panel.memory_dream_days = if *checked { 7 } else { 0 };
                                    cx.notify();
                                });
                            }
                        }
                    }),
            )
            .child(section_header("TURN BUDGETS", cx))
            .child(
                v_flex()
                    .gap_1p5()
                    .px_1()
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme.muted_foreground)
                            .child(format!(
                                "Tool rounds before a wrap-up nudge, then grace rounds before \
                                 the turn pauses (resumable). Defaults {} + {}; empty = the \
                                 default. Long work (a wide sweep, a migration) wants more.",
                                gertie_core::DEFAULT_MAX_TOOL_ROUNDS,
                                gertie_core::DEFAULT_GRACE_ROUNDS
                            )),
                    )
                    .child(self.budget_field(
                        "turn-max-rounds",
                        format!("{} (default)", gertie_core::DEFAULT_MAX_TOOL_ROUNDS),
                        "max tool rounds",
                        _window,
                        cx,
                    ))
                    .child(self.budget_field(
                        "turn-grace-rounds",
                        format!("{} (default)", gertie_core::DEFAULT_GRACE_ROUNDS),
                        "grace rounds",
                        _window,
                        cx,
                    )),
            )
            .child(section_header("MCP SERVERS", cx))
            .when(self.mcp_servers.is_empty(), |col| {
                col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("none configured"),
                )
            })
            .children(self.mcp_servers.iter().enumerate().map(|(index, server)| {
                let target = server
                    .url
                    .clone()
                    .or_else(|| server.command.clone())
                    .unwrap_or_default();
                h_flex()
                    .w_full()
                    .gap_0p5()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .child(format!("{} · {}", server.name, target)),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-mcp-{index}")))
                            .label("✕")
                            .xsmall()
                            .ghost()
                            .tooltip("Remove (pending until Save)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.mcp_servers.remove(index);
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }))
            .children(mcp_form)
            .when(!mcp_form_open, |col| {
                col.child(
                    Button::new("add-mcp")
                        .label("Add server")
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.mcp_name =
                                Some(cx.new(|cx| InputState::new(window, cx).placeholder("name")));
                            this.mcp_url = Some(cx.new(|cx| {
                                InputState::new(window, cx).placeholder("url (https://…/mcp)")
                            }));
                            this.mcp_command = Some(cx.new(|cx| {
                                InputState::new(window, cx).placeholder("or command (stdio)")
                            }));
                            cx.notify();
                        })),
                )
            })
            .child(section_header("AGENTS", cx))
            .when(self.agents.is_empty(), |col| {
                col.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme.muted_foreground)
                        .child("none configured, the eight built-ins need none"),
                )
            })
            .children(
                self.agents
                    .iter()
                    .enumerate()
                    .map(|(index, draft)| agent_row(index, draft, cx).into_any_element()),
            )
            .children(agent_form)
            .when(self.agent_form.is_none(), |col| {
                col.child(
                    Button::new("add-agent")
                        .label("Add agent")
                        .small()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_agent_form(None, window, cx);
                        })),
                )
            })
            .child(section_header("CONFIG FILE", cx))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(match &self.summary.config_path {
                        Some(path) => format!(
                            "written on save: {}, comments and unknown keys are kept",
                            path.display()
                        ),
                        None => "no config file location".into(),
                    }),
            )
            .child(
                Button::new("save-config")
                    .label("Save to config.toml")
                    .small()
                    .primary()
                    .on_click(cx.listener(Self::save_all)),
            )
            .children(self.status.iter().map(|(message, is_error)| {
                div()
                    .text_size(px(11.5))
                    .text_color(if *is_error {
                        theme.danger
                    } else {
                        theme.success
                    })
                    .child(message.clone())
            }))
    }
}
