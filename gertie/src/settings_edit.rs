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
use gpui_kit::TestSupportExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::ThemeMode;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::Icon;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, IntoElement, Render, SharedString, Window, div,
    px,
};

/// Emitted after a successful save, the workspace reloads the
/// bootstrap and pushes the new provider into every open tab.
pub enum SettingsEvent {
    Saved,
    /// The page's own close affordance (the ✕ by the nav's caption);
    /// the shell closes the page.
    Dismissed,
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
    /// Not on the form: carried through so an edit keeps them.
    memory_writes: Option<bool>,
    tool_guidance: Option<String>,
    vision: Option<bool>,
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
            memory_writes: endpoint.memory_writes,
            tool_guidance: endpoint.tool_guidance.clone(),
            vision: endpoint.vision,
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
            memory_writes: self.memory_writes,
            tool_guidance: self.tool_guidance.clone(),
            vision: self.vision,
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
    /// Memory's frozen-index budget draft: empty = the panel doesn't
    /// own the key.
    memory_index_budget_draft: String,
    memory_index_budget_input: Option<Entity<InputState>>,
    /// New sessions' starting permission mode.
    default_mode: gertie_core::PermissionMode,
    /// Auto-settle's idle threshold: None = off.
    auto_settle_days: Option<u64>,
    /// The workspace check: the on/off switch and the command draft
    /// (empty = no override, the workspace's inference reads through).
    verify_enabled: bool,
    verify_command_draft: String,
    verify_command_input: Option<Entity<InputState>>,
    /// The repo map: on/off and its framing budget draft.
    repo_map_enabled: bool,
    repo_map_budget_draft: String,
    repo_map_budget_input: Option<Entity<InputState>>,
    /// The search backend: "brave", "searxng", or "none" (off), plus
    /// its two config fields.
    search_backend: String,
    search_base_url_draft: String,
    search_base_url_input: Option<Entity<InputState>>,
    search_key_env_draft: String,
    search_key_env_input: Option<Entity<InputState>>,
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
    /// The section the two-pane page shows; the nav on the page's
    /// left switches it.
    section: SettingsSection,
}

/// The settings page's sections, in nav order. One shows at a time:
/// the first pass split the config file's real groups across eight
/// near-empty tabs; four sections with substance each is the shape.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SettingsSection {
    Chat,
    Endpoints,
    Memory,
    Tools,
}

impl SettingsSection {
    /// The element id / test handle suffix.
    fn name(self) -> &'static str {
        match self {
            SettingsSection::Chat => "chat",
            SettingsSection::Endpoints => "endpoints",
            SettingsSection::Memory => "memory",
            SettingsSection::Tools => "tools",
        }
    }

    /// The nav's label.
    fn label(self) -> &'static str {
        match self {
            SettingsSection::Chat => "Chat",
            SettingsSection::Endpoints => "Endpoints",
            SettingsSection::Memory => "Memory & search",
            SettingsSection::Tools => "Tools & agents",
        }
    }
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
            memory_index_budget_draft: String::new(),
            memory_index_budget_input: None,
            default_mode: gertie_core::PermissionMode::Supervised,
            auto_settle_days: None,
            verify_enabled: true,
            verify_command_draft: String::new(),
            verify_command_input: None,
            repo_map_enabled: true,
            repo_map_budget_draft: String::new(),
            repo_map_budget_input: None,
            search_backend: "none".into(),
            search_base_url_draft: String::new(),
            search_base_url_input: None,
            search_key_env_draft: String::new(),
            search_key_env_input: None,
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
            section: SettingsSection::Endpoints,
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
    /// A budget saves when its edit is done: Enter, or leaving the
    /// field.
    fn budget_event(
        &mut self,
        _input: Entity<InputState>,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
            self.autosave(cx);
        }
    }

    /// One labelled text field backed by a lazily-created input that
    /// seeds from its draft. The slot and draft resolve by id, so a
    /// call site borrows self once. The draft follows the input;
    /// Save reads the drafts (and the budget inputs directly).
    fn text_field(
        &mut self,
        id: &'static str,
        placeholder: &str,
        label: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (slot, draft): (&mut Option<Entity<InputState>>, &String) = match id {
            "verify-command" => (&mut self.verify_command_input, &self.verify_command_draft),
            "memory-index-budget" => (
                &mut self.memory_index_budget_input,
                &self.memory_index_budget_draft,
            ),
            "repo-map-budget" => (&mut self.repo_map_budget_input, &self.repo_map_budget_draft),
            "search-base-url" => (&mut self.search_base_url_input, &self.search_base_url_draft),
            "search-key-env" => (&mut self.search_key_env_input, &self.search_key_env_draft),
            "turn-max-rounds" => (&mut self.max_tool_rounds_input, &self.max_tool_rounds_draft),
            "turn-grace-rounds" => (&mut self.grace_rounds_input, &self.grace_rounds_draft),
            _ => unreachable!("unregistered text field {id}"),
        };
        let input = slot
            .get_or_insert_with(|| {
                let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
                cx.subscribe(&state, Self::budget_event).detach();
                if !draft.is_empty() {
                    state.update(cx, |state, cx| state.set_value(draft, window, cx));
                }
                state
            })
            .clone();
        // The draft follows the input, one direction only.
        let current = input.read(cx).value().to_string();
        match id {
            "verify-command" => self.verify_command_draft = current,
            "memory-index-budget" => self.memory_index_budget_draft = current,
            "repo-map-budget" => self.repo_map_budget_draft = current,
            "search-base-url" => self.search_base_url_draft = current,
            "search-key-env" => self.search_key_env_draft = current,
            _ => {}
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
            .child(Input::new(&input).small().w_full())
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
        self.default_mode = self.summary.default_mode;
        self.auto_settle_days = self.summary.auto_settle_days;
        self.verify_enabled = self
            .summary
            .verify
            .as_ref()
            .map(|v| v.is_some())
            .unwrap_or(true);
        self.verify_command_draft = self.summary.verify.clone().flatten().unwrap_or_default();
        self.verify_command_input = None;
        self.repo_map_enabled = self.summary.repo_map_enabled;
        self.repo_map_budget_draft =
            if self.summary.repo_map_budget == crate::config::DEFAULT_REPO_MAP_TOKENS {
                String::new()
            } else {
                self.summary.repo_map_budget.to_string()
            };
        self.repo_map_budget_input = None;
        self.memory_index_budget_draft = self
            .summary
            .memory_index_budget
            .map(|budget| budget.to_string())
            .unwrap_or_default();
        self.memory_index_budget_input = None;
        self.search_backend = self.summary.search.clone();
        self.search_base_url_draft = config
            .search
            .as_ref()
            .and_then(|search| search.base_url.clone())
            .unwrap_or_default();
        self.search_base_url_input = None;
        self.search_key_env_draft = config
            .search
            .as_ref()
            .and_then(|search| search.api_key_env.clone())
            .unwrap_or_default();
        self.search_key_env_input = None;
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
                memory_writes: None,
                tool_guidance: None,
                vision: None,
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
            memory_writes: form
                .editing
                .and_then(|index| self.endpoints.get(index))
                .and_then(|endpoint| endpoint.memory_writes),
            tool_guidance: form
                .editing
                .and_then(|index| self.endpoints.get(index))
                .and_then(|endpoint| endpoint.tool_guidance.clone()),
            vision: form
                .editing
                .and_then(|index| self.endpoints.get(index))
                .and_then(|endpoint| endpoint.vision),
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
        self.autosave(cx);
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
        self.autosave(cx);
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
        self.autosave(cx);
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
        self.autosave(cx);
    }

    /// Theme switch: applies immediately and persists, UI state, kept
    /// out of the user's config.toml.
    fn set_theme(&mut self, mode: ThemeMode, cx: &mut Context<Self>) {
        gpui_kit::component::Theme::change(mode, None, cx);
        crate::settings::store_theme(mode);
        cx.notify();
    }

    /// A committed change: write it now and repaint. Settings save as
    /// they are made (no Save button): switches on click, endpoint,
    /// server and agent forms on their own Save or Add, removals at
    /// once, budgets on Enter or leaving the field.
    fn autosave(&mut self, cx: &mut Context<Self>) {
        self.save(cx);
        cx.notify();
    }

    /// Writes the panel's state through toml_edit and emits Saved so
    /// the workspace reloads the bootstrap live.
    fn save(&mut self, cx: &mut Context<Self>) {
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
            // The chat section's choices: always owned by the panel, so
            // they write through every save (the mode as Some(choice),
            // settle as Some(days-or-off), check and map per their
            // switches). Unparseable budget fields above already
            // aborted the save.
            default_mode: Some(Some(self.default_mode)),
            auto_settle_days: Some(self.auto_settle_days),
            verify_enabled: Some(self.verify_enabled),
            verify_command: Some(if self.verify_command_draft.trim().is_empty() {
                None
            } else {
                Some(self.verify_command_draft.trim().to_string())
            }),
            repo_map_enabled: Some(self.repo_map_enabled),
            repo_map_budget: Some(self.repo_map_budget_draft.trim().parse::<usize>().ok()),
            memory_index_budget: Some(self.memory_index_budget_draft.trim().parse::<usize>().ok()),
        };
        match edits.save(&path) {
            Ok(()) => {
                self.status = Some((
                    "Saved. Endpoints apply now; sandbox, memory, MCP servers and agents on restart."
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
            div()
                .id(SharedString::from(format!("endpoint-{index}")))
                .flex_1()
                .min_w_0()
                .px_2()
                .py_1()
                .rounded(px(6.))
                .cursor_pointer()
                .hover(|row| row.bg(cx.theme().foreground.opacity(0.06)))
                .when(is_default, |row| row.bg(cx.theme().accent.opacity(0.14)))
                .tooltip({
                    let tip = SharedString::from(format!("{key_note}, click to edit"));
                    move |window, cx| {
                        gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx)
                    }
                })
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_form(Some(index), window, cx);
                }))
                // Name over model, left-aligned like every other list.
                .child(
                    div()
                        .truncate()
                        .text_size(px(12.5))
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .child(draft.name.clone()),
                )
                .child(
                    div()
                        .truncate()
                        .text_size(px(11.))
                        .text_color(cx.theme().muted_foreground)
                        .child(format!("{} · {key_note}", draft.model)),
                ),
        )
        .child(
            Button::new(SharedString::from(format!("default-{index}")))
                .icon(if is_default {
                    IconName::Star
                } else {
                    IconName::StarOff
                })
                .xsmall()
                .ghost()
                .tooltip(if is_default {
                    "Default endpoint"
                } else {
                    "Make this the default endpoint"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.default_endpoint = Some(this.endpoints[index].name.clone());
                    this.autosave(cx);
                })),
        )
        .child(
            Button::new(SharedString::from(format!("remove-endpoint-{index}")))
                .icon(IconName::Trash)
                .xsmall()
                .ghost()
                .tooltip("Remove")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_endpoint(index, cx);
                })),
        )
}

/// One agent row: edit on click, ✕ to remove. The pin is the point,
/// a seat without its endpoint visible is just a name.
/// One MCP server row: name with its url or command, ✕ to remove.
fn mcp_row(index: usize, draft: &McpDraft, cx: &Context<SettingsPanelView>) -> impl IntoElement {
    let detail = match (&draft.url, &draft.command) {
        (Some(url), _) => url.clone(),
        (None, Some(command)) => command.clone(),
        (None, None) => String::new(),
    };
    h_flex()
        .w_full()
        .gap_0p5()
        .child(
            Button::new(SharedString::from(format!("mcp-{index}")))
                .label(draft.name.clone())
                .small()
                .flex_1()
                .min_w_0()
                .tooltip(format!("{detail}, restart to apply")),
        )
        .child(
            Button::new(SharedString::from(format!("remove-mcp-{index}")))
                .icon(IconName::Trash)
                .xsmall()
                .ghost()
                .tooltip("Remove")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.mcp_servers.remove(index);
                    this.autosave(cx);
                })),
        )
}

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
                .icon(IconName::Trash)
                .xsmall()
                .ghost()
                .tooltip("Remove")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.remove_agent(index, cx);
                })),
        )
}

/// The section-header styling for this panel.
/// One settings group: a titled card, the panel's unit of reading.
/// The settings page's content column: the transcript's reading
/// width, plus a little for forms. Centered, never hugging the nav.
fn settings_card(title: &'static str, cx: &App) -> gpui_kit::Div {
    v_flex()
        .w_full()
        .flex_shrink_0()
        .gap_1p5()
        .p_3()
        .rounded(px(10.))
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().secondary.opacity(0.35))
        .child(
            div()
                .text_size(px(12.))
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .child(title),
        )
}

impl SettingsPanelView {
    /// Jumps the dialog to a section, what the shell does on open so
    /// every visit starts on Chat instead of the last visit's section.
    pub fn show_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }
}

impl EventEmitter<SettingsEvent> for SettingsPanelView {}

impl Render for SettingsPanelView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let section = self.section;

        // The nav: one row per section, the active one filled. The
        // dialog's left pane; the content pane shows one section whole
        // instead of eight cards in a strip.
        let nav = v_flex()
            .id("settings-nav")
            .w(px(150.))
            .h_full()
            .flex_shrink_0()
            .border_r_1()
            .border_color(theme.border)
            .pt_3()
            .px_2()
            .gap_0p5()
            .child(
                div()
                    .px_2()
                    .pb_2()
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child("Settings"),
            )
            .children(
                [
                    SettingsSection::Chat,
                    SettingsSection::Endpoints,
                    SettingsSection::Memory,
                    SettingsSection::Tools,
                ]
                .map(|id| {
                    let active = section == id;
                    div()
                        .id(SharedString::from(format!("nav-{}", id.name())))
                        .test_support()
                        .px_2()
                        .py_1()
                        .rounded(px(6.))
                        .text_size(px(12.5))
                        .cursor_pointer()
                        .text_color(if active {
                            theme.foreground
                        } else {
                            theme.muted_foreground
                        })
                        .when(active, |row| row.bg(theme.foreground.opacity(0.09)))
                        .hover(|row| row.bg(theme.foreground.opacity(0.06)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.section = id;
                            cx.notify();
                        }))
                        .child(id.label())
                }),
            );

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
                                        this.autosave(cx);
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

        // The save-result messages live above the content column, so a
        // save reports no matter which section shows. Inside the
        // column's width, so it reads as the page's status, not a
        // banner over everything.
        // The save-result messages ride the dialog's top: full width
        // of the right pane, so a save reports no matter which section
        // shows. Empty until a save happens, so nothing paints.
        let status_line = v_flex()
            .px_6()
            .pt_3()
            .flex_shrink_0()
            .children(self.status.iter().map(|(message, is_error)| {
                div()
                    .px_1()
                    .pb_1()
                    .text_size(px(11.))
                    .text_color(if *is_error {
                        theme.danger
                    } else {
                        theme.success
                    })
                    .child(message.clone())
            }));

        // One section's cards at a time, in nav order; the nav rows
        // pick which. The content pane holds the active section only.
        let mut content: Vec<AnyElement> = Vec::new();

        let section_note = |text: &str| {
            div()
                .text_size(px(10.5))
                .text_color(theme.muted_foreground)
                .child(text.to_string())
        };

        if self.section == SettingsSection::Chat {
            // Theme: UI state (ui.json), not config.toml.
            let current = gpui_kit::component::Theme::global(cx).mode;
            content.push(
                settings_card("Appearance", cx)
                    .child(
                        h_flex()
                            .gap_1()
                            .children([ThemeMode::Dark, ThemeMode::Light].map(|mode| {
                                let button = Button::new(SharedString::from(format!(
                                    "theme-{mode}",
                                    mode = mode.name()
                                )))
                                .label(mode.name())
                                .small();
                                let button = if current == mode {
                                    button.primary()
                                } else {
                                    button.ghost()
                                };
                                button.on_click(
                                    cx.listener(move |this, _, _, cx| this.set_theme(mode, cx)),
                                )
                            })),
                    )
                    .into_any_element(),
            );
            // New sessions' permission mode: the composer's chip
            // overrides per session; this seeds it.
            let modes = [
                (gertie_core::PermissionMode::Supervised, "Supervised"),
                (gertie_core::PermissionMode::AcceptEdits, "Accept edits"),
                (gertie_core::PermissionMode::Auto, "Auto (sandboxed)"),
                (gertie_core::PermissionMode::FullAccess, "Full access"),
            ];
            content.push(
                settings_card("New sessions start in", cx)
                    .child(section_note(
                        "The composer's mode chip overrides per conversation. Auto needs the sandbox on to run bash unasked; with it off, Auto asks like Accept edits.",
                    ))
                    .child(
                        h_flex().gap_1().flex_wrap().children(modes.map(|(mode, label)| {
                            let button = Button::new(SharedString::from(format!(
                                "mode-{mode}",
                                mode = mode.as_str()
                            )))
                            .label(label)
                            .small();
                            let button = if self.default_mode == mode {
                                button.primary()
                            } else {
                                button.ghost()
                            };
                            button.on_click(cx.listener(move |this, _, _, cx| {
                                this.default_mode = mode;
                                this.autosave(cx);
                            }))
                        })),
                    )
                    .into_any_element(),
            );
            // Auto-settle: the idle threshold, off by default.
            content.push(
                settings_card("Auto-settle", cx)
                    .child(section_note(
                        "Settle chats idle this many days on startup: the transcript folds, the worktree (if any) is reclaimed. Settling is always manual otherwise.",
                    ))
                    .child(
                        h_flex().gap_1().children(
                            [
                                (None, "Off"),
                                (Some(7u64), "7 days"),
                                (Some(14), "14 days"),
                                (Some(30), "30 days"),
                            ]
                            .map(|(days, label)| {
                                let button = Button::new(SharedString::from(format!(
                                    "settle-{label}"
                                )))
                                .label(label)
                                .small();
                                let button = if self.auto_settle_days == days {
                                    button.primary()
                                } else {
                                    button.ghost()
                                };
                                button.on_click(cx.listener(move |this, _, _, cx| {
                                    this.auto_settle_days = days;
                                    this.autosave(cx);
                                }))
                            }),
                        ),
                    )
                    .into_any_element(),
            );
            // The workspace check (#40) with its command override.
            content.push(
                settings_card("Check before a turn ends", cx)
                    .child(section_note(
                        "The check gertie runs before an editing turn ends. Each workspace's inference (a Makefile, scripts/build.sh) wins when there is one; a command here overrides it everywhere.",
                    ))
                    .child({
                        let panel = cx.entity().downgrade();
                        Switch::new("verify-switch")
                            .label("Run the check")
                            .small()
                            .checked(self.verify_enabled)
                            .tooltip("Off = no check, the turn ends when the model wraps up.")
                            .on_click(move |checked, _, cx| {
                                if let Some(panel) = panel.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.verify_enabled = *checked;
                                        panel.autosave(cx);
                                    });
                                }
                            })
                    })
                    .child(self.text_field(
                            "verify-command",
                            "e.g. ./scripts/build.sh check (empty = the workspace's inference)",
                        "command override",
                        _window,
                        cx,
                    ))
                    .into_any_element(),
            );
        }

        if self.section == SettingsSection::Endpoints {
            content.push(
                settings_card("Endpoints", cx)
                    .when(self.endpoints.is_empty(), |col| {
                        col.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child("no endpoints configured"),
                        )
                    })
                    .children(self.endpoints.iter().enumerate().map(|(index, draft)| {
                        let is_default =
                            self.default_endpoint.as_deref() == Some(draft.name.as_str());
                        let key_resolved = self
                            .summary
                            .endpoints
                            .iter()
                            .find(|info| info.name == draft.name)
                            .map(|info| info.key_resolved);
                        endpoint_row(index, draft, key_resolved, is_default, cx).into_any_element()
                    }))
                    .children(form)
                    .when(self.form.is_none(), |col| {
                        col.child(
                            Button::new("add-endpoint")
                                .label("Add endpoint")
                                .small()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_form(None, window, cx);
                                })),
                        )
                    })
                    .into_any_element(),
            );
        }

        if self.section == SettingsSection::Memory {
            content.push(
                settings_card("Memory", cx)
                    .child({
                        let panel = cx.entity().downgrade();
                        Switch::new("memory-switch")
                            .label("Remember across sessions")
                            .small()
                            .checked(self.memory_enabled)
                            .tooltip(concat!(
                                "Restart to apply. Off disables the session-end extraction AND",
                                " dreaming; the memory and search_memory tools stay available",
                                " either way."
                            ))
                            .on_click(move |checked, _, cx| {
                                if let Some(panel) = panel.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.memory_enabled = *checked;
                                        panel.autosave(cx);
                                    });
                                }
                            })
                    })
                    .child(origin_note(self.summary.memory_origin, cx))
                    .child({
                        let panel = cx.entity().downgrade();
                        Switch::new("dream-switch")
                            .label(format!(
                                "Consolidate memory every {} days",
                                self.memory_dream_days
                            ))
                            .small()
                            .checked(self.memory_enabled && self.memory_dream_days > 0)
                            .tooltip(concat!(
                                "Startup pass that merges duplicate entries and archives",
                                " counter-dead ones. A changelog lands in",
                                " .gertie/memory/dreams/. Applies on restart."
                            ))
                            .on_click(move |checked, _, cx| {
                                if let Some(panel) = panel.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.memory_dream_days = if *checked { 7 } else { 0 };
                                        panel.autosave(cx);
                                    });
                                }
                            })
                    })
                    .child(self.text_field(
                        "memory-index-budget",
                        "tokens for the frozen index (empty = the default)",
                        "index budget",
                        _window,
                        cx,
                    ))
                    .into_any_element(),
            );
            // The search backend: config-only until now.
            content.push(
                settings_card("Web search", cx)
                    .child(section_note(
                        "web_search's backend. None leaves the tool explaining how to configure it instead of failing mysteriously. Applies on restart.",
                    ))
                    .child(
                        h_flex().gap_1().children(
                            [
                                ("none", "None"),
                                ("brave", "Brave"),
                                ("searxng", "SearXNG"),
                            ]
                            .map(|(backend, label)| {
                                let button = Button::new(SharedString::from(format!(
                                    "search-{backend}"
                                )))
                                .label(label)
                                .small();
                                let button = if self.search_backend == backend {
                                    button.primary()
                                } else {
                                    button.ghost()
                                };
                                button.on_click(cx.listener(move |this, _, _, cx| {
                                    this.search_backend = backend.to_string();
                                    this.autosave(cx);
                                }))
                            }),
                        ),
                    )
                    .when(
                        self.search_backend == "searxng",
                        |col| {
                            col.child(self.text_field(
                                "search-base-url",
                                "https://searx.example (must allow format=json)",
                                "instance base URL",
                                _window,
                                cx,
                            ))
                        },
                    )
                    .when(
                        self.search_backend == "brave",
                        |col| {
                            col.child(self.text_field(
                                "search-key-env",
                                "env var holding the Brave API key (e.g. BRAVE_API_KEY)",
                                "API key env var",
                                _window,
                                cx,
                            ))
                        },
                    )
                    .into_any_element(),
            );
        }

        if self.section == SettingsSection::Tools {
            // Sandbox, MCP servers, turn budgets, agents: what the
            // model can reach and how much rope it gets.
            content.push(
                settings_card("Sandbox", cx)
                    .child({
                        let panel = cx.entity().downgrade();
                        Switch::new("sandbox-switch")
                            .label("Landlock-isolate bash children")
                            .small()
                            .checked(self.sandbox)
                            .tooltip(concat!(
                                "Restart to apply. Your config sets this; while it doesn't, the",
                                " repo's .gertie/config.toml wins, and that file may also write",
                                " \"inherit\" to stand down."
                            ))
                            .on_click(move |checked, _, cx| {
                                if let Some(panel) = panel.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.sandbox = *checked;
                                        panel.autosave(cx);
                                    });
                                }
                            })
                    })
                    .child(origin_note(self.summary.sandbox_origin, cx))
                    .into_any_element(),
            );
            content.push(
                settings_card("Turn budgets", cx)
                    .child(
                        v_flex()
                            .gap_1p5()
                            .px_1()
                            .child(section_note(&format!(
                                "Tool rounds before a wrap-up nudge, then grace rounds before \
                                 the turn pauses (resumable). Defaults {} + {}; empty = the \
                                 default. Long work (a wide sweep, a migration) wants more.",
                                gertie_core::DEFAULT_MAX_TOOL_ROUNDS,
                                gertie_core::DEFAULT_GRACE_ROUNDS
                            )))
                            .child(self.text_field(
                                "turn-max-rounds",
                                &format!("{} (default)", gertie_core::DEFAULT_MAX_TOOL_ROUNDS),
                                "max tool rounds",
                                _window,
                                cx,
                            ))
                            .child(self.text_field(
                                "turn-grace-rounds",
                                &format!("{} (default)", gertie_core::DEFAULT_GRACE_ROUNDS),
                                "grace rounds",
                                _window,
                                cx,
                            )),
                    )
                    .into_any_element(),
            );
            content.push(
                settings_card("MCP servers", cx)
                    .when(self.mcp_servers.is_empty(), |col| {
                        col.child(
                            div()
                                .text_size(px(12.))
                                .text_color(theme.muted_foreground)
                                .child("none configured"),
                        )
                    })
                    .children(
                        self.mcp_servers
                            .iter()
                            .enumerate()
                            .map(|(index, draft)| mcp_row(index, draft, cx).into_any_element()),
                    )
                    .children(mcp_form)
                    .when(self.mcp_name.is_none(), |col| {
                        col.child(Button::new("add-mcp").label("Add server").small().on_click(
                            cx.listener(|this, _, window, cx| {
                                this.mcp_name = Some(
                                    cx.new(|cx| InputState::new(window, cx).placeholder("name")),
                                );
                                this.mcp_url = Some(cx.new(|cx| {
                                    InputState::new(window, cx).placeholder("url (https://…/mcp)")
                                }));
                                this.mcp_command = Some(cx.new(|cx| {
                                    InputState::new(window, cx).placeholder("or command (stdio)")
                                }));
                                cx.notify();
                            }),
                        ))
                    })
                    .into_any_element(),
            );
            content.push(
                settings_card("Agents", cx)
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
                    .into_any_element(),
            );
            // The repo map: what the model sees of the workspace.
            content.push(
                settings_card("Repo map", cx)
                    .child(section_note(&format!(
                        "The ranked map of the workspace's code, sent at session start, and the repo_map tool. Budget {}/tokens frames it; empty = the default.",
                        self.summary.repo_map_budget
                    )))
                    .child({
                        let panel = cx.entity().downgrade();
                        Switch::new("repo-map-switch")
                            .label("Send the repo map")
                            .small()
                            .checked(self.repo_map_enabled)
                            .tooltip("Off turns both the framing and the tool off.")
                            .on_click(move |checked, _, cx| {
                                if let Some(panel) = panel.upgrade() {
                                    panel.update(cx, |panel, cx| {
                                        panel.repo_map_enabled = *checked;
                                        panel.autosave(cx);
                                    });
                                }
                            })
                    })
                    .child(self.text_field(
                            "repo-map-budget",
                            &format!("{} (default)", self.summary.repo_map_budget),
                        "framing budget",
                        _window,
                        cx,
                    ))
                    .into_any_element(),
            );
        }

        // The dialog's right pane: the save status rides on top, the
        // section's cards scroll under it, centered like the
        // transcript's reading column.
        let content_column = v_flex()
            .id("settings-content")
            .test_support()
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .pt_3()
            .pb_4()
            .gap_3()
            .child(
                // The dialog is the bounded width (760px), so the
                // cards simply fill the pane right of the nav; no
                // centering to engage, no void either side.
                v_flex()
                    .id("settings-column")
                    .test_support()
                    .gap_3()
                    .px_6()
                    .w_full()
                    .children(content),
            )
            .child(
                // The config-file fact, once, at the pane's foot: a
                // path, not a card, and not repeated per section.
                div()
                    .w_full()
                    .px_6()
                    .text_size(px(10.5))
                    .text_color(theme.muted_foreground)
                    .child(match &self.summary.config_path {
                        Some(path) => format!(
                            "Saved to {} as you make them; its comments and unknown keys are kept.",
                            path.display()
                        ),
                        None => "no config file location".into(),
                    }),
            );

        // The dialog body: nav left, the right pane carries the save
        // status over one section's scrolling content. The shell's
        // modal card wraps this, so the body just fills it: a definite
        // height here sizes the card. The ✕ sits top-right on the
        // dialog itself, the modal convention, not tucked in the nav.
        h_flex()
            .id("settings-panel")
            .test_support()
            .w_full()
            .h(px(680.))
            .relative()
            .child(nav)
            // The ✕: a bordered circle top-right, visible at rest (the
            // bare muted glyph hid against the background until hover).
            .child(
                div()
                    .id("settings-close")
                    .test_support()
                    .absolute()
                    .top_2()
                    .right_2()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size(px(24.))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.border)
                    .text_size(px(11.))
                    .text_color(theme.foreground)
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.secondary).text_color(theme.foreground))
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.emit(SettingsEvent::Dismissed);
                    }))
                    .tooltip(|window, cx| Tooltip::new("Close (esc)").build(window, cx))
                    // The real X glyph from the icon catalog: a text
                    // ✕ at 11px renders as a dot.
                    .child(
                        Icon::new(IconName::Close)
                            .with_size(px(14.))
                            .text_color(theme.foreground),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .bg(theme.background)
                    .child(status_line)
                    .child(content_column),
            )
    }
}

/// Where a switch's value comes from, under the switch: the label
/// stays the setting, the provenance reads as a note. Shown only when
/// it is not the built-in default.
fn origin_note(origin: crate::config::Origin, cx: &App) -> impl IntoElement {
    div()
        .pl(px(36.))
        .text_size(px(10.5))
        .text_color(cx.theme().muted_foreground)
        .when(origin != crate::config::Origin::Default, |note| {
            note.child(format!("set by your {}", origin.as_str()))
        })
}
