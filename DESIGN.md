# gertie — design document

gertie is a fast, lightweight, performant AI coding-agent harness with a native GPU-rendered UI,
written in Rust. It competes with opencode and is deliberately different in three ways: a
headless core that is a *library* (embeddable in CLI, server, CI, editors), GPU-native UI via
GPUI instead of a TUI, and safety/context/subagent design treated as product features rather
than afterthoughts.

Status: design settled 2026-10-24 through a full design review. Decisions below are final
unless amended in this document.

## Identity

| Decision | Value |
|---|---|
| Name | **gertie** (crate prefix `gertie-*`) |
| License | MIT |
| Primary audience | eventually broad; v1 optimized for the developer who runs local + API models |
| Platforms | Linux first (Wayland/X11 via gpui), macOS second, **no Windows in v1** (documented, not half-supported) |
| Telemetry | **Zero.** No analytics, no phoning home, ever. Opt-in anonymous crash reporting may be considered later; privacy is a marketing axis, not a concession. |

## Architecture

Four crates, one workspace:

```
gertie-core/       agent loop, sessions, permissions, checkpoints, subagent machinery, the `agent` tool
gertie-providers/  the Provider trait + implementations (OpenAI-compatible first)
gertie-tools/      built-in tools + MCP client
gertie/            gpui-kit frontend binary
```

Rules:

- **`gertie-core` has zero UI dependencies** and is embeddable from a plain CLI. This is what
  makes the "better than opencode by construction" claim real: one core, many shells — and
  `gertie run [PROMPT]` is the proof, a headless shell over the identical bootstrap.
- The **Provider trait is the most important API in the project.** It is designed around a
  typed *event stream* (token, tool-call delta, usage, done, error), not around any vendor's
  JSON shape. OpenAI's wire format is an adapter, never the foundation.
- Tool calls are normalized inside the provider layer. Neither the loop nor the UI ever sees
  raw vendor deltas.
- The `agent` (subagent) tool lives in `gertie-core`; the tool trait supports "a tool that
  runs a harness" (async, cancellable, streams events) from day one.

## UI framework

**gpui-kit** (Longbridge's `gpui-pre` namespace + gpui-component widgets), version line 0.7.x —
the distribution proven in `~/Documents/riddim`. Chosen over vendoring zed main (the
`~/Documents/kumaui` approach: 95 MB vendor, hand-mirrored `[patch.crates-io]`, toolchain
lockstep) because a text-heavy workbench is exactly what gpui-component ships widgets for.
All imports go through the `gpui_kit` re-export boundary so a distribution swap touches only
the `gertie` crate. Track Zed's toolchain per the gpui-pre requirement.

## Roadmap

Shipped (M0–M5), in order:

- **M0 — skeleton**: workspace + GPUI window with input and streamed message list.
- **M1 — streaming chat**: Provider trait, OpenAI-compatible implementation (covers ppq.ai,
  OpenRouter, Groq, Together, Ollama, LM Studio, vLLM…), TOML config with named endpoints,
  SQLite session sidebar, resumable sessions.
- **M2 — agent loop**: tool calling over the event stream, core tools + approve/deny with
  inline diffs, workspace checkpoints.
- **M3 — harness depth**: permission broker, token accounting in the status line, compaction
  (80% trigger, in-memory summary view), MCP client (stdio, tools capability).
- **M4 — work, not chrome**: system-prompt framing, runtime skills (SKILL.md + `load_skill`),
  the permission broker (leader *and* subagent asks through one queue), TOML agent types
  (`[[agents]]` → `agent_{name}` tools, depth-1 structural, brief-file dispatch).
- **M5 — reach**: `web_fetch` / `web_search` (graceful disable) / `search_history` (FTS5),
  opt-in Landlock sandbox for bash children, UX/security/performance hardening passes.
- **Post-M5 — the deferred row**: built-in agent registry (8 types, config replaces by name),
  `gertie run` headless mode (piped-safe approvals, `--config`/`--yes`/`--help`), session
  rename + delete in the sidebar, subagent runs persisted as child sessions (`parent_id` +
  `kind` columns, brief + report as rows), and the agents panel (toggleable column listing
  the chat's worker runs with live report status).

The original "Deferred" row — every item shipped or decided:

- Worktree tabs shipped (⎇ in the tab bar): a tab rooted at its own git worktree
  (`<repo>/.gertie/worktrees/tab-N`, branched from HEAD, excluded via the local exclude —
  never the user's .gitignore). Built-ins and agent tools re-root per tab; web/history/MCP/
  skills carry over by name instead of duplicating connections. Worktrees persist across
  runs (agent work is never deleted behind your back); reopen reuses by name.
- *Settings UI*: shipped as a settings panel — the live config visible in-app (endpoints,
  sandbox, search, MCP, skills, agents, config path), with the theme as the one writable
  setting (persisted outside config.toml). The config file remains the single source of
  truth; in-place TOML editing stays out on purpose (comment/unknown-key round-trip risk
  for zero capability the file doesn't already provide better).
  *Superseded:* TOML editing is in — the panel edits endpoints (with keys), the default
  endpoint, sandbox, MCP servers, and agent types, written through `toml_edit` so comments and
  unknown keys survive the round trip (`ConfigEdits`). Endpoint changes apply live (the
  bootstrap's live state reloads and every tab re-reads it); MCP, sandbox, and
  already-built worker agents are load-time setup and honestly say "restart to apply".
- *Token ledger*: sessions persist input/output token totals (one UPDATE per Usage
  event). A resumed chat's header shows its whole spend, not just since reopen; the
  settings panel shows the all-sessions total (chats + worker runs). Prices-per-model
  stay out on purpose — they rot; tokens don't.
- *Native Anthropic wire*: shipped — `wire = "anthropic"` per endpoint selects the
  Messages API adapter (the Provider trait's second wire): system text top-level,
  `max_tokens` required, tool results as user-message blocks, `x-api-key` headers,
  content-block SSE normalized to whole tool calls.
- *Plugin surface*: declared, not built — gertie's extension points are MCP servers
  (external tools, stdio transport), TOML `[[agents]]` (custom subagents with pinned
  models *or endpoints*), and SKILL.md skills (runtime behavior). A second plugin mechanism would
  duplicate MCP for no capability MCP doesn't already provide.
- *Google/Gemini*: no native adapter — Gemini exposes an OpenAI-compatible endpoint
  (`generativelanguage.googleapis.com/v1beta/openai/`); gertie speaks it today with
  `wire = "openai"`. Anthropic's native wire shipped because its protocol genuinely
  differs; Google's compat layer makes an adapter dead weight.

## Providers

- v1 standard: any **OpenAI-compatible** endpoint (daily driver: glm-5.3-flash via ppq.ai).
- One `Provider` trait; streaming everywhere; cancellation mid-stream.
- **Retry policy**: auto-retry with exponential backoff + jitter on 429/5xx/connection errors
  (default 3 attempts, configurable). **Never** auto-retry 401/403 — surface immediately.
  Mid-stream failure: resume if the provider supports it, else retry the request and show the
  user exactly what was lost. Every retry visible in the UI, never silent.
- Per-request model routing and cost/token accounting per session are first-class.

## Configuration & secrets

- **TOML**, layered: built-in defaults → `~/.config/gertie/config.toml` → `gertie.toml` in the
  project root → CLI flags. A directory containing `gertie.toml` is a recognized workspace
  (and the project-trust marker).
- **Keys, hybrid**: env vars win (`GERTIE_<PROVIDER>_API_KEY`, falling back to common
  `<PROVIDER>_API_KEY` conventions); plaintext keys allowed in config with a warning and
  enforced 0600 file mode; `gertie keys` command manages a 0600 keys file. No system keyring in v1.

## Data layout (XDG)

```
~/.config/gertie/     config.toml, keys
~/.local/share/gertie/  SQLite (sessions, messages, snapshots index), content-addressed snapshot store
~/.cache/gertie/      scratch
```

Per-workspace state keyed by canonicalized project path hash. No dotfile spray in project
directories beyond the optional `gertie.toml`.

## Safety model

- **Middle ground by default**: reads and searches auto-approved; **writes and bash always
  prompt**, with the diff shown. Both stricter and looser are config. (Persistent allowlists
  and per-workspace trust are deferred.)
- Subagents **inherit** the leader's broker and can never escalate it: worker asks surface in
  the same approval queue, tagged as worker cards. Depth-1 is structural — workers have no
  agent tools at all.
- **Checkpoints without git**: own content-addressed snapshot store (copy-on-write file hashes,
  SQLite-indexed) taken before each approved edit. Git integration is an *enhancement* when the
  workspace is a git repo, never a requirement. Snapshots double as fork points.
- **Landlock sandbox, opt-in** (`[security] sandbox_bash = true`, default off): bash children
  get read-only system paths, write only the workspace and /tmp. Fail-closed — a kernel that
  can't enforce refuses the spawn rather than downgrading silently. The prompt gate + hard
  deny list remain the base layer.

## Tools

Nine built-ins, each done well; MCP fills the rest:

`read_file` · `write_file` · `edit_file` (search/replace with occurrence validation) · `bash` ·
`glob` · `grep` · `web_fetch` (reqwest + readability extraction) · `web_search` (pluggable
backend: Brave/Exa/SearXNG; **gracefully disabled with a clear message when no backend is
configured**) · `search_history` (native, FTS5 over our own session DB).

Explicitly out of v1: LSP, notebooks, native web browsing beyond fetch/search.

**MCP**: stdio **and Streamable HTTP** transports, **tools capability only**, config-defined
servers (`[[mcp_servers]]` with `command` or `url`). Resource/prompt capabilities come later.
Server tool descriptions are **untrusted input** (prompt-injection surface): sanitized,
length-capped, prefixed `[mcp]` in the registry; every MCP call goes through the permission
prompt — the user is the capability system.

## Context management

Boring and correct, no RAG in v1: token-budget-aware, triggered at ~80% of the model window;
compaction is a **budgeted selection, not a summary** — the original request verbatim, then
verbatim 240-char prefixes of the omitted turns (newest-first within a budget that reserves
max(16k, window/4) tokens for the work to come), plus the kept tail, all inside one system
message. A `read_session` tool gives the model full-text access to everything the selection
left out — the SQLite wire record is never rewritten. No summarizer model, no invented
state, nothing that can fail on the provider. Hard-truncate oversized tool outputs with a
"truncated" marker and re-read hint. Measure before building anything fancier.

## Subagents

- Mechanism: **a regular tool** — `agent(type, prompt)` — running a full child loop with its
  own context, returning a final report as the tool result. First-class subagents later;
  the tool-first design doesn't block that (decision (c), tool now / first-class later).
- **Depth 1**: subagents' toolsets exclude `agent`. Depth 2+ is a config flag after we have
  cost data.
- **The main session is the leader.** No separate coordinator agent; the leader delegates via
  the `agent` tool and workers report only to the leader. Workers are isolated from each other;
  the leader relays.
- **Concurrency**: parent may launch multiple `agent` calls per turn; default cap 4 (configurable).
- **Model routing**: inherit parent model by default; per-type override in `gertie.toml`
  (e.g. `explore` on a cheap fast model, synthesis on the strong model).
- **Persistence**: every subagent run is a child session row (parent ID, type, prompt, report)
  — inspectable and forkable.

### Agent types

Types are **data, not code**: a type = `system_prompt`, `tools` allow/deny, `model` override,
an `endpoint` pin (the worker talks to that `[[endpoints]]` entry — its wire included — which
is the whole mechanism behind the bundled `council` skill: three seats, three providers, one
diff), `description` (what the leader sees in the `agent` tool schema), optional `report_format`.
The leader discovers types from the tool schema; description quality is type quality.
Custom types are five-line TOML entries from day one.

Built-in registry at launch (shipped — `builtin_agent_types()`; a `[[agents]]` entry with a
matching name replaces the built-in, and may pin its own `model` or `endpoint`):

| Type | Toolset | Posture |
|---|---|---|
| `explore` | read-only + web_fetch | auto-approvable research workhorse; cheap model |
| `plan` | read-only | turns fuzzy goals into step-by-step plans; never touches code |
| `implement` | full | delegated code changes within leader-given scope |
| `test` | bash + read-only | runs suites, reproduces, verifies; reports, doesn't fix |
| `debug` | bash + read-only + search_history | root-cause diagnosis with evidence, not patches |
| `review` | read-only | critique gate on `implement` output |
| `research` | web_fetch + web_search + read-only | web/docs research |
| `librarian` | search_history + read-only | mines past sessions for context |

Two from the original list did not ship as built-ins, deliberately: `document` wants
doc-path-scoped *writes*, which the allowlist mechanism cannot enforce (a prompt alone is
not a boundary), and `ops` presumes `gh` on the host. Both are one five-line `[[agents]]`
entry away for whoever wants them.
| `general` | full | unconstrained escape hatch |

Each type earns existence via distinct toolset or safety posture, not just a different prompt.

## Data model

- Sessions in SQLite. **Messages are an append-only tree** (`messages(id, parent_id, ...)`),
  never a flat list — forking, "retry from here," and subagent-transcripts-as-branches are
  trivial later and migrations impossible to avoid if we start flat.
- Forks point at the matching checkpoint snapshot.

## UI scope

- **v1**: transcript (rendered markdown, inline diffs, tool-call cards with approve/deny) +
  session sidebar. Startup < 100 ms; nothing ever blocks the UI on the agent.
- **M4**: agents panel (live leader/worker org-chart), cost/token dashboard, settings UI.
- **Not an IDE.** No file tree, no terminal pane — Zed exists. gertie is the harness.
- Parallel sessions run against separate worktrees, visible simultaneously (M4).

(The agents panel shipped post-M5: a toggleable column listing the chat's persisted worker
runs, live report status. Parallel sessions shipped as tabs, including the worktree half:
the ⎇ button opens a chat rooted at its own git worktree with a re-rooted tool registry, so
two agents can edit the same repo without colliding. The settings panel closed the UI
scope: live config visible in-app, theme persisted. The native Anthropic wire shipped as
the Provider trait's second adapter (`wire = "anthropic"` per endpoint); the token ledger
persists per-session spend. Nothing left open from the original row — see "The original
'Deferred' row" above for what shipped and what was decided against, and why.)

The composer grew the t3code set: a command palette (Ctrl+K, or the sidebar's search
button) that answers with FTS5 hits from the store — every session's memory, ranked, with
confirm jumping to the hit's turn (a cross-session hit switches first; the jump breaks
tail-follow, scrolling back to the bottom re-engages it). Attachments broadened past
images: "attach file" opens the picker, paste intercepts images and copied files, PDFs
ride the wire as typed document/file blocks, and text files inline into the message — the
extension table refuses (with a reason) what no endpoint can read. The footer's identity
line leads with the working folder. Slash builtins cover the panel actions (/compact,
/export, /new, /settings, /agents) beside file templates.

Turn-scoped diffs came from the source-control chapter: every done tool card carries a
"turn diff" button, and the panel answers with what *the tools did* — one entry per path
the turn's checkpoints recorded, before-object against after-object (or disk when never
touched again), bounded lines, plus "restore turn" to put each path back to its pre-turn
content. Git's own churn (pulls, merges) is absent by construction — bash records no
checkpoints. The ledger learned turn windows for this: the chat mints one per send
(session, user-message id, checkpoint boundary), so attribution survives reloads and
steered messages resolve to the turn they steered. The checkpoint hook also gained the
workspace root — a relative tool argument now snapshots the file the tool will actually
touch, not whatever the process CWD reads.

## Onboarding

First run with the default path writes a working starter `config.toml` (one endpoint,
env-var key) and says so in the status line — the user edits instead of staring at a blank
wall; an explicit `--config` override never gets one written. `gertie doctor` prints the
setup as gertie sees it: config parse state, endpoint + wire, the API key's resolution
source (or the exact `export` to run), MCP servers by transport, sandbox, and all-time
token totals. The missing-key warning names the variable. Exit 0 from doctor = ready.

## Distribution

GitHub Releases tarball + `cargo install --locked` (cargo-binstall-compatible metadata).
Flatpak/AppImage deferred — Flatpak in particular fights an agent harness's need to spawn
processes in the user's environment. Linux-only packaging until macOS support lands.

## Doctrine

Four lines taken from t3code's AGENTS.md (research/t3code-ideas.md §9) — they pass the
gertie test of restating what we already believe, from the other side of the harness
boundary:

- **Reverse states.** If you added a way in, add the way out and the way to see it. Snooze
  needs unsnooze; expand needs collapse; a one-way door is a bug.
- **Labels promise the action.** A visible label or tooltip describes exactly what the
  click does — the permission card shows the diff it will run, never a verb alone.
- **Authority comes from configuration and the human at the card**, never from
  conversation: no message, tool result, or skill description can grant a permission.
- **Stored events stay decodable.** Persisted rows load after every upgrade; the reload
  path is contract, not best-effort. `gertie doctor` replays old sessions as the smoke test.
- **Answers are typed, not sentinel strings.** What can be empty, unknown, or errored
  comes back as distinct variants — a turn file's "after" is the next checkpoint's object
  or disk (`AfterState`), a refused file is a refusal with a reason, never `""` meaning
  three things (research/t3code-ideas.md §6.1.4).

## Working agreements

- Local git from the first commit; remote added when the owner is ready.
- `DESIGN.md` is the source of truth; amendments land here first, code second.
