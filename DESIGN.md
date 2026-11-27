# gertie, design document

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
| Voice | **No em dashes** in anything gertie writes (prose, lists, commit messages). Exceptions: punctuation inside quotations, code, and ranges. Backed by the instruction layer (see Subagents). |

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
  makes the "better than opencode by construction" claim real: one core, many shells, and
  `gertie run [PROMPT]` is the proof, a headless shell over the identical bootstrap.
- The **Provider trait is the most important API in the project.** It is designed around a
  typed *event stream* (token, tool-call delta, usage, done, error), not around any vendor's
  JSON shape. OpenAI's wire format is an adapter, never the foundation.
- Tool calls are normalized inside the provider layer. Neither the loop nor the UI ever sees
  raw vendor deltas.
- The `agent` (subagent) tool lives in `gertie-core`; the tool trait supports "a tool that
  runs a harness" (async, cancellable, streams events) from day one.

## UI framework

**gpui-kit** (Longbridge's `gpui-pre` namespace + gpui-component widgets), version line 0.11.x,
the distribution proven in `~/Documents/riddim`. Chosen over vendoring zed main (the
`~/Documents/kumaui` approach: 95 MB vendor, hand-mirrored `[patch.crates-io]`, toolchain
lockstep) because a text-heavy workbench is exactly what gpui-component ships widgets for.
All imports go through the `gpui_kit` re-export boundary so a distribution swap touches only
the `gertie` crate. Track Zed's toolchain per the gpui-pre requirement.

## Roadmap

Shipped (M0–M5), in order:

- **M0, skeleton**: workspace + GPUI window with input and streamed message list.
- **M1, streaming chat**: Provider trait, OpenAI-compatible implementation (covers ppq.ai,
  OpenRouter, Groq, Together, Ollama, LM Studio, vLLM…), TOML config with named endpoints,
  SQLite session sidebar, resumable sessions.
- **M2, agent loop**: tool calling over the event stream, core tools + approve/deny with
  inline diffs, workspace checkpoints.
- **M3, harness depth**: permission broker, token accounting in the status line, compaction
  (80% trigger, in-memory summary view), MCP client (stdio, tools capability).
- **M4, work, not chrome**: system-prompt framing, runtime skills (SKILL.md + `load_skill`),
  the permission broker (leader *and* subagent asks through one queue), TOML agent types
  (`[[agents]]` → `agent_{name}` tools, depth-1 structural, brief-file dispatch).
- **M5, reach**: `web_fetch` / `web_search` (graceful disable) / `search_history` (FTS5),
  opt-in Landlock sandbox for bash children, UX/security/performance hardening passes.
- **Post-M5, the deferred row**: built-in agent registry (8 types, config replaces by name),
  `gertie run` headless mode (piped-safe approvals, `--config`/`--yes`/`--help`), session
  rename + delete in the sidebar, and subagent runs persisted as child sessions (`parent_id` +
  `kind` columns, brief + report as rows).
- **M6, memory + instructions** (shipped 2026-10-31; `docs/memory.md` is the user guide):
  markdown memory tiers (project + global) with a derived SQLite/FTS5 sidecar, the `memory`
  tool (`memory_20250818` surface, both wires), frozen-index injection after the static system
  prompt, `search_memory`, compaction/settle-hooked extraction as review-inbox delta ops, and
  the thin dreaming pass on the startup sweep (no-model, changelog-per-dream). Ledger gains
  cached-token columns; the metric is cached-input share. Defaults amended in flight:
  extraction and dreaming ship **on** (7-day cadence, empty-store cost is zero, discovery
  beats caution); proposals only apply through the review inbox, which is what makes
  default-on safe.
- **M7, the UI pass** (shipped 2026-10-31): posture-ring safety (de-escalation-only chip, the
  palette + confirm card as full access's only path), the memory surface (footer word,
  panel, ▲/▼ voting as the counters' only writer), the first-run state, composer
  @-mentions, attention marks (● ⏸ ✓ on tabs then, on sidebar rows now), Ctrl+F in-transcript find + copy on assistant
  turns, the footer's compaction line, the `[turn]` budget dial (defaults 60 + 10,
  panel-editable), and the polish items (popup arrow-key navigation, vote feedback in the
  panel rows, find's row highlight). Full record in the Deferred row below.


The original "Deferred" row, every item shipped or decided:

- Worktree tabs shipped (now a worktree chat, the "New worktree chat" sidebar row;
  the tab bar is gone): a chat rooted at its own git worktree
  (`<repo>/.gertie/worktrees/tab-N`, branched from HEAD, excluded via the local exclude,
  never the user's .gitignore). Built-ins and agent tools re-root per tab; web/history/MCP/
  skills carry over by name instead of duplicating connections. Worktrees persist across
  runs (agent work is never deleted behind your back); reopen reuses by name.
  The tab's branch is named from the session's first message, the temporary
  `gertie/tab-N` renamed to the message's slug (`gertie/fix-the-flaky-widget`) under the
  prefix the repo's overlay sets (`[worktree] branch_prefix`, default `gertie/`; anything
  git-hostile falls back). Only gertie's own temporary branches are renamed: a checked-out
  branch is the user's, the main checkout is never touched, and any failure (a taken slug,
  a message with nothing sluggable, a git refusal) leaves the temporary name standing,
  the naming is a courtesy, never a gate. Cleanup is policy plus report:
  `[worktree] cleanup_idle_days` in the *user's* config (the repo never sets deletion
  policy) lets `gertie doctor --clean-worktrees` remove idle gertie-made worktrees,
  and only clean, fully-merged ones; plain `gertie doctor` prints the same ledger
  report-only, every entry removed or kept and why.
- *Settings UI*: shipped as a settings panel, the live config visible in-app (endpoints,
  sandbox, search, MCP, skills, agents, config path), with the theme as the one writable
  setting (persisted outside config.toml). The config file remains the single source of
  truth; in-place TOML editing stays out on purpose (comment/unknown-key round-trip risk
  for zero capability the file doesn't already provide better).
  *Superseded:* TOML editing is in, the panel edits endpoints (with keys), the default
  endpoint, sandbox, MCP servers, and agent types, written through `toml_edit` so comments and
  unknown keys survive the round trip (`ConfigEdits`). Endpoint changes apply live (the
  bootstrap's live state reloads and every tab re-reads it); MCP, sandbox, and
  already-built worker agents are load-time setup and honestly say "restart to apply".
  Changes save as they are made (no Save button): switches on click, the endpoint,
  server, and agent forms on their own Save or Add, removals at once, the turn budgets
  on Enter or leaving the field. Where a value comes from (your config or the project
  overlay) is a note under its switch, not part of the label.
- *Token ledger*: sessions persist input/output token totals (one UPDATE per Usage
  event). A resumed chat's header shows its whole spend, not just since reopen; the
  settings panel shows the all-sessions total (chats + worker runs). Prices-per-model
  stay out on purpose, they rot; tokens don't.
- *Native Anthropic wire*: shipped, `wire = "anthropic"` per endpoint selects the
  Messages API adapter (the Provider trait's second wire): system text top-level,
  `max_tokens` required, tool results as user-message blocks, `x-api-key` headers,
  content-block SSE normalized to whole tool calls.
- *Plugin surface*: declared, not built, gertie's extension points are MCP servers
  (external tools, stdio transport), TOML `[[agents]]` (custom subagents with pinned
  models *or endpoints*), and SKILL.md skills (runtime behavior). A second plugin mechanism would
  duplicate MCP for no capability MCP doesn't already provide.
- *Google/Gemini*: no native adapter, Gemini exposes an OpenAI-compatible endpoint
  (`generativelanguage.googleapis.com/v1beta/openai/`); gertie speaks it today with
  `wire = "openai"`. Anthropic's native wire shipped because its protocol genuinely
  differs; Google's compat layer makes an adapter dead weight.
- *The UI pass (M7, shipped 2026-10-31)*: seven PRs amending earlier decisions on purpose. The posture ring is
  de-escalation-only: clicking the chip can never reach full access (auto wraps home),
  the palette command plus a confirm card is the only path, and the tooltip names each
  mode's meaning. Rule: escalation costs a keystroke of intent, de-escalation is
  frictionless. Memory has a surface: the footer's `memory N` word opens a panel with
  ▲/▼ voting, the helpful/harmful counters' only writer and the input to dreaming's
  retirement. First-run: the empty transcript names the blocking fact (no endpoint
  offers "Add an endpoint") or gives the one-line tour, never a wizard. The composer
  gains @-mentions (workspace file picker; arrows walk it, Enter takes the walked row,
  the walk resets only when the query changes). Tabs carry attention marks (● running,
  ⏸ limited, ✓ done-unseen, cleared on activation); with the tab bar gone they are
  sidebar dots (green running, blue done-unseen, the pause icon for limited). Ctrl+F finds in the transcript with
  a live count and wrapping walk; assistant turns carry copy beside quote. The footer
  names the coming compaction's tail (imported from `KEEP_TAIL`, so footer and loop
  cannot disagree). Turn budgets are config (`[turn] max_tool_rounds` default 60,
  `grace_rounds` 10, shown and editable in the panel, empty field = the default) after
  long sessions kept tripping the old 25-round stop; the pause valve stays. Polish:
  popup arrow-key navigation shared by slash and mentions, votes speak through the
  panel rows instead of notices, the find bar marks the current match's row.

## Providers

- v1 standard: any **OpenAI-compatible** endpoint (daily driver: glm-5.3-flash via ppq.ai).
- One `Provider` trait; streaming everywhere; cancellation mid-stream.
- **Retry policy**: auto-retry with exponential backoff + jitter on 429/5xx/connection errors
  (default 3 attempts, configurable). **Never** auto-retry 401/403, surface immediately.
  Mid-stream failure: resume if the provider supports it, else retry the request and show the
  user exactly what was lost. Every retry visible in the UI, never silent.
- Per-request model routing and cost/token accounting per session are first-class.

## Configuration & secrets

- **TOML**, layered: built-in defaults → `~/.config/gertie/config.toml` → the project
  overlay → CLI flags. A directory containing `gertie.toml` is a recognized workspace
  (and the project-trust marker).
- **The project overlay** (`<repo>/.gertie/config.toml`) carries a deliberate subset,
  starting permission mode, bash sandbox, auto-settle days, the
  worktree branch prefix, and nothing that runs on your machine: no endpoints, keys,
  MCP servers, or agent types, and no settle command either: a shell command is
  process, not posture, and stays the user's. A cloned repo never configures what
  executes locally.
  The chain is user config → project overlay → built-in default, and the overlay
  is bounded at the accept_edits ceiling: a repo can start its sessions at
  accept_edits (checkpointed edits run unasked and reversible; bash and MCP still
  prompt) and turn the bash sandbox on, but auto, full_access, standing the sandbox
  down, and the private-fetch gate are the user's alone, so a checkout's own
  `config.toml` can never reach the shell or the network by itself. Every
  resolved value says which tier decided: the settings panel
  and `gertie doctor` print the origin. Inheritance is an explicit value, a field may
  hold `"inherit"` to stand down to the next tier, a state the UI can show, not an
  absence to infer. Overlay problems (unreadable, unparseable) surface in the status
  label and the doctor, never fatal, never silent. **Clones bring a memory directory,
  not instructions**: entries under `.gertie/memory/` are recall, provenance and
  counters, advisory by definition, never policy, and dream/extraction ops targeting
  project memory render for review like any other proposal.
- **Keys, hybrid**: env vars win (`GERTIE_<PROVIDER>_API_KEY`, falling back to common
  `<PROVIDER>_API_KEY` conventions); plaintext keys allowed in config with a warning and
  enforced 0600 file mode; `gertie keys` command manages a 0600 keys file. No system keyring in v1.
  The writer's side holds too: config.toml is created 0600 (a umask can only strip bits,
  never add them) and a loose pre-existing file is tightened on save, the doctor's
  mode warning must never be one gertie caused. An `Endpoint`'s `Debug` shows a key's
  presence, never its content, and the settings editor masks the key field.

## Data layout (XDG)

```
~/.config/gertie/     config.toml, keys
~/.local/share/gertie/  SQLite (sessions, messages, snapshots index), content-addressed snapshot store,
                       global memory/ (cross-project memory tier)
~/.cache/gertie/      scratch
```

Per-workspace state keyed by canonicalized project path hash. No dotfile spray in project
directories beyond the optional `gertie.toml`.

## Safety model

- **Middle ground by default**: reads and searches auto-approved; **writes and bash always
  prompt**, with the diff shown. Both stricter and looser are config. (Persistent allowlists
  and per-workspace trust are deferred.)
- **Permission modes live on the turn, not the broker** (shipped, `PermissionMode`): the
  composer's selector picks a session's mode, *supervised* (everything asks), *accept-edits*
  (exactly what the checkpoint hook snapshots runs unasked, reversible by construction;
  bash still asks), *auto* (edits as accept-edits, plus bash that runs inside the Landlock
  sandbox, with the sandbox off it asks like accept-edits, never pretending a kernel that
  isn't enforcing is enforcing), *full_access* (nothing asks). The mode rides the `TurnSpec`,
  fixed for the turn's life, a flip mid-turn applies next turn, and workers inherit it at
  dispatch. Pre-approval converges on the same checkpoint-and-run path a human's approval
  takes: the ledger is not optional. The tools' policy vet stays below every mode; persisted
  per session (`sessions.mode`), default from `[permissions] default_mode`.
- Subagents **inherit** the leader's broker and can never escalate it: worker asks surface in
  the same approval queue, tagged as worker cards. Depth-1 is structural, workers have no
  agent tools at all.
- **Checkpoints without git**: own content-addressed snapshot store (copy-on-write file hashes,
  SQLite-indexed) taken before each approved edit. Git integration is an *enhancement* when the
  workspace is a git repo, never a requirement. Snapshots double as fork points.
- **Landlock sandbox, opt-in** (`[security] sandbox_bash = true`, default off): bash children
  get read-only system paths, write only the workspace and /tmp. Fail-closed, a kernel that
  can't enforce refuses the spawn rather than downgrading silently. The prompt gate + hard
  deny list remain the base layer.
- **Invisible characters never reach a prompt**: bidi controls, zero-width spacing and
  joiners, and Unicode tag letters are stripped from user text at the system's entrance,
  composer send (queued, steered, and template-expanded text included) and `gertie run`
  alike, so the database, the transcript, and the prompt hold the same words. The same
  strip guards the other entrances: tool output is stripped where it joins the loop (one
  seam feeds both the prompt and the transcript), MCP and skill descriptions pass through
  the sanitizer, and the transcript renders stored text and model-chosen arguments
  stripped too. Emoji ZWJ sequences, soft hyphens, and variation selectors survive; a
  transcript notice says when cleaning happened.

## Tools

Thirteen built-ins, each done well; MCP fills the rest:

`read_file` · `write_file` · `edit_file` (search/replace with occurrence validation) · `bash` ·
`glob` · `grep` · `web_fetch` (reqwest + readability extraction) · `web_search` (pluggable
backend: Brave/Exa/SearXNG; **gracefully disabled with a clear message when no backend is
configured**) · `search_history` (native, FTS5 over our own session DB) · `memory` (the
`memory_20250818` command surface over both memory tiers, mutation audited to the index's
own trail) · `search_memory` (FTS5 over the derived sidecar, ranked pointers) · `remember`
(the structured write path: add, supersede, update, archive, link as a JSON schema, applied
through the same delta ops extraction uses, so any model writes valid entries without
learning the grammar) · `repo_map` (the workspace's code structure from tree-sitter: the
directories, a directory's files and their top-level symbols, or a file's signatures with
line numbers; Rust, Python, Go, JavaScript, TypeScript).

Beside them, two surfaces the UI answers rather than the world: `ask_user` (a question
with clickable options, answered through the permission broker) and `status` (the model's
stated goal and next step, shown in the status column; read-only, no card to approve, set
when the plan changes rather than every turn).

Explicitly out of v1: LSP, notebooks, native web browsing beyond fetch/search.

**web_fetch dials the open web only**: a URL is judged as an address, not a string,
literal private IPs (RFC 1918, loopback, the 169.254 metadata range, CGNAT), localhost-ish
names, and public names that resolve into private space are all refused, and every dial,
first fetch and every redirect hop, resolves through that same filter, so DNS rebinding
has no gap to live in. A localhost dev server is a legitimate target for a coding agent,
so `[security] allow_private_fetch = true` opens the gate (user config only: the
overlay carries no such key; never a model ask).

**MCP**: stdio **and Streamable HTTP** transports, **tools capability only**, config-defined
servers (`[[mcp_servers]]` with `command` or `url`). Resource/prompt capabilities come later.
Server tool descriptions are **untrusted input** (prompt-injection surface): sanitized,
length-capped, prefixed `[mcp]` in the registry; server tools register under
`mcp_{server}_{tool}`: a server's `bash` can never shadow gertie's own, and every MCP
call goes through the permission prompt (every mode short of full access: third-party code
is pre-approved by nothing), the user is the capability system.

## Context management

Boring and correct, no RAG in v1: token-budget-aware, triggered at ~80% of the model window,
capped by `[turn] compact_at_tokens` (default 160k, what a 200k window gets anyway). A 1M
window without the cap let a session reach 405k tokens with no compaction, and a minute to
first token on every question; the selection is sized against half the capped window, so it
lands well under the trigger;
compaction is a **budgeted selection, not a summary**, the original request verbatim, then
verbatim 240-char prefixes of the omitted turns (newest-first within a budget that reserves
max(16k, window/4) tokens for the work to come), plus the kept tail, all inside one system
message. A `read_session` tool gives the model full-text access to everything the selection
left out, the SQLite wire record is never rewritten. No summarizer model, no invented
state, nothing that can fail on the provider. Hard-truncate oversized tool outputs with a
"truncated" marker and re-read hint. Measure before building anything fancier.

Two cheaper steps run long before compaction (#39). **At entry**, bash output is shaped:
passing tests collapse to a count, a build diagnostic keeps its whole block (the `-->`
location and the source excerpt, not just the `error:` line), wrapped commands
(`cd x && cargo test`, `./scripts/build.sh test`) classify by their last known segment, and
long output of any kind keeps its head, the failure lines of the middle, and its tail,
where the head-only cap used to cut the verdict. **In history**, a `read_file` result that
a later read of the same lines fully covers collapses to a one-line pointer (attached
AGENTS.md and memory blocks stay). Providers cache the request prefix and rewriting an old
message re-bills everything after it, so collapsing is batched: nothing changes until about
6k tokens are recoverable, then everything superseded goes at once. The decision is a pure
function of the history, so a resumed session prunes to the same bytes.

**The project check (#40).** "Done" from a model is a claim; the harness checks it. When the
model ends a turn that edited files, gertie appends a `bash` call for the project's check to
the model's final message and dispatches it through the ordinary path, so the permission
mode, the sandbox, output shaping and the transcript card all apply unchanged. A pass ends
the turn with no extra model round; a denial ends it too (the human's call); a failure goes
back as "the turn is not done: fix it or say why it is expected", up to three runs a turn,
the last asking for a report instead. The command comes from the user's `[verify]` config
(global, or per workspace path) or is inferred (`cargo check --message-format=short`, `go
build ./...`) only when the program is on PATH. The repo's overlay may not name it: the
settle_command rule, a cloned repo does not get to choose a command gertie runs.

**The repo map (#41).** Models spend rounds finding code: list a directory, read the wrong
file, grep, read again; weak models wander longest. On a session's first turn the harness
renders a map of the workspace's source files with their top-level symbols (tree-sitter,
not regexes: types, functions, impls and their methods, module constants), ranks it for the
session (a path or file name the user wrote, files already touched, files live memory
cites, file names the user's words contain, entry points, then shallow before deep) and
cuts it to `[repo_map] budget_tokens` (default 500), with one summary line for the rest.
It rides after the memory index as framing, frozen for the session like the index, so the
cached prefix holds; the `repo_map` tool answers anything newer or deeper. Outlines cache
per file under the git blob hash (memory evidence's hash), so a changed file re-parses and
an unchanged one never does; the walk skips build output and dependencies, stops at 20k
files, never parses a file over 512 KB, and the framing parses at most 400 files, so a
monorepo degrades to its summary line. The first build of gertie's own map takes about
65 ms, a cached one about 2. The render runs on its own thread, off the executor the loop
shares with the UI. `gertie eval nav` measures it per model: three find-and-fix tasks in a
fixture whose values to change also sit in decoy files, each run with the map and without.
On glm-5.3-flash over two runs: 3/3 both ways, steps to the first correct edit 11 and 8
with the map against 23 and 33 without, tokens in 38% lower. `[repo_map] enabled = false`
turns off the framing and the tool together.

## Projects

A project is a name, not a folder: chats belong to a project only when
you put them there, and the sidebar groups by that name. Deliberately
**pure grouping**: projects carry no memory tier, no instruction files,
and no per-project configuration. Memory keys on workspace scope, and
instructions on AGENTS.md; if a project-scoped context need shows up in
real use, it is a design decision to make then, not a default to grow
into by accident.

Membership is as easy to leave as to join: a filed chat's row carries a one-click
remove (#31), next to delete, which sends it back to Other and changes nothing else about
it; the notice names the project, so a slip is one move away from undone. The move list
says "→ Out of <project>" for the same thing.

## Memory

**Entry states (the validity lattice)**: an entry is `current` (no
valid_until), `superseded` (a named successor exists in superseded_by),
or `archived` (retired by the dream pass or the correction walk, reason
in the line). There is no other state and no time-based retirement:
entries leave `current` only by an explicit act (supersede, archive),
never by age.

**The tie-break**: when two entries state the same fact, the newest
wins: new information beats old by definition, and a genuine conflict
is resolved by superseding the loser (which records the replacement),
not by leaving both current. When retrieval surfaces both, the result
labels the relationship rather than hiding it.


Long-term memory is plain markdown plus a derived SQLite index. The field's convergent
answer is files, Letta's filesystem agent beat specialized memory tools on LoCoMo, every
first-party agent (Claude Code, Devin, Codex, Gemini CLI, Cline) landed on markdown, and
none uses embedding search, and the hard negative results are the budget caps below.
Two tiers, both human-editable in any editor:

- **Project tier**: `<repo>/.gertie/memory/`, versionable, travels with clones.
- **Global tier**: `~/.local/share/gertie/memory/`, per-user, cross-project (preferences,
  machine conventions).

Each tier holds `MEMORY.md` (an index, capped) and topic files of one-line bullet entries:
`- [id] helpful=2 harmful=0 cite the flake before blaming the test [source: 388; added: 2026-07-11]`.
Provenance rides every entry, it is the memory-poisoning mitigation and the audit surface.

A `memories` table + FTS5 index (the `messages_fts` pattern) mirrors the files: ids, counters,
dates, validity, typed links, source session. Markdown is the record; the sidecar is derived
and rebuilt on hash mismatch. Superseding an entry sets `valid_until`, history is never
deleted, so "what did we believe when we wrote this code" is a query. A supersede that knows
its replacement writes it on the line (`[superseded by: p31; valid until: DATE]`), and typed
links live on the `from` entry's line (`[links: builds_on p7]`): the structure travels with
the files (a clone, a wiped sidecar), and `memory_links` is rebuilt from them on every sync.
Links that once lived only in the sidecar moved onto their lines once, on the first sync
after upgrade.

**Evidence that knows it is stale.** An entry may cite the files that supported it,
`[evidence: src/pool.rs@3f2a1c9e0b12]`: workspace-relative path and git blob id (computed
in-process, so non-git workspaces work too, and `git log --find-object` finds the state).
Extraction sees the list of files the session's tool calls touched and may cite only
those; anything else is dropped before the card. Whether a cited file moved is computed on
read (a hash per cited file, no stored state), and a moved or missing file flags the entry
in the panel, the frozen index and `search_memory`. Flag only: ranking is untouched,
because a changed file does not make a belief wrong, it makes it unverified, and the
human decides. Age-based decay was rejected for the same reason (no schedule has survived
validation); this marks *which* beliefs the code moved under, not how old they are.

**Retroactive correction.** A supersede walks one hop back: live entries that `builds_on`
or `solves` the corrected one, and live entries sharing a source session, gain
`[recheck: p12]` on their line (the replacement never does). The flag is a file marker,
not a transcript card, so it survives restarts and travels with the repo; the panel
answers it with keep (clear the flag) or archive, and editing stays the files' job. The
index and `search_memory` carry it, so the model re-checks too. One hop, never
transitive: a correction of a flagged entry starts its own walk. A recall log (which
sessions used an entry, so a correction could name past work) is deferred.

**Read path: frozen index, fetched body.** Cache prefixes build tools→system→messages, and any
byte change busts everything after the change, a mutable index inside the system prompt is a
cache bomb (the pi-context experiment watched a memory layer cost 2.34× tokens this way). So
the index is snapshotted at session start, frozen for the session (mutations land next
session), injected *after* the static system prompt, and capped (`[memory] index_budget`,
default 600 tokens). Everything else is on demand: `search_memory` (FTS5 over the sidecar,
returning ranked id/date/title pointers the model reads the file for), ordinary file reads,
validity-filtered queries. Typed links (relates_to, solves, causes, builds_on, replaces) ride
`search_memory` hits by id and show in the panel, beside a superseded entry's replacement
chain; PPR reranking over them waits until the linked graph is large enough to need them, graph structure helps multi-hop
queries, and coding queries are mostly single-hop identifier lookups FTS5 already wins.

**Three write paths, all delta-only** (the one hard law in the ablation literature: Dynamic
Cheatsheet's memory rewrote itself from 18,282 tokens to 122 in a single monolithic pass and
dropped below its no-adaptation baseline, no consolidation may ever regenerate the store):

1. **In-session**, a `memory` tool implementing Anthropic's `memory_20250818` command surface
   (view/create/str_replace/insert/delete/rename) over the two memory roots, path-traversal
   guarded. Every wire gets gertie's definition of those commands (one handler; no wire maps it
   to a native tool type). Which endpoints see the write commands is a per-model profile
   setting (#42): `memory_writes` on the endpoint, defaulting on for Claude models (trained on
   that command set) and off for the rest, where the tool is view only and `remember` is the
   one write path. Models given two write paths wrote the same fact twice, and the write
   commands are most of the tool's ~440 tokens per request. Edits apply immediately (the user
   is watching them happen); the frozen index picks them up next session.
   The second profile setting is `tool_guidance`: `standard`, or `terse` (each tool's short
   description, its first sentence unless the tool writes one, and schemas without field
   descriptions; about 60% fewer tool-definition tokens). `gertie eval memory --set` tries a
   profile without touching the config, and the eval's suggestions judge a terse trial on cost,
   not score: the mean of every passing run per setting in the results file (same endpoint,
   model and memory setting), output tokens weighted x4, two runs per side before a verdict,
   kept only at 10% cheaper or better. Less coaching can buy cheaper requests and spend them on
   more steps: on glm-5.3-flash terse passed 3/3 but cost within 5% of standard.
   Every file write passes `MemoryStore::normalize_edit`, so a model's headings, bullets,
   rewritten counters or invented hashes never reach the store as malformed entries. The
   `remember` tool is the preferred write path: a schema instead of a grammar.
   Two checks keep it honest across models: `gertie doctor` lints both tiers (lines that do
   not parse, prose, double-wrapped entries, duplicate ids, links to nothing, cited files that
   are gone), and `gertie eval memory` runs live scenarios per configured endpoint in
   throwaway repos (heed a recalled constraint, save one valid fact, supersede a changed one)
   and prints one row per model with its tokens.
2. **Session end**, extraction rides the two reliable boundaries: compaction (the digest is
   already built) and settle (the ✓ is the human's "done"; process exit is not reliable).
   Eligibility first: skip short and still-active sessions; flag sessions that ingested
   untrusted web/repo content. One call to the extraction model (default the daily driver;
   `extract_endpoint` pins another) proposes delta ops, add/update/supersede/archive/link,
   which merge deterministically in Rust under a token budget with dedup. The default gate is
   a review inbox: proposed ops render as a diff the user approves; auto-apply-with-changelog
   is the opt-in.
   **Corrections (#44)** are the most valuable thing a session holds and used to be invisible:
   a denial with the user's reason lives in a tool row, and the digest skipped tool rows. The
   digest now carries each one in place (`user (rejected bash \`rm -rf target\`): use cargo
   clean`); the prompt asks for corrections first, worded as the rule, `source: correction`,
   citing the file involved (so recall on touch brings it back when the work returns there).
   A session with a correction is eligible however short it is, the card labels the add as a
   correction, and the entry starts one helpful vote up. A bare "no" is not a correction: it
   says what, not why. Undone edits carry no reason and are not saved, so they are not a
   source yet.
3. **Dreaming**, a thin consolidation pass riding the startup sweep (idle-gated, like
   auto-settle): merge duplicates keeping both provenance links, archive counter-dead entries
   (helpful/harmful votes and budget pressure, retirement is never time-decay; no decay
   schedule has survived validation anywhere), rebuild the index, print a changelog. Delta ops
   only; the store is git-trackable files, so a bad dream is reversible.

**Procedural memory promotes into skills.** When a task's success is confirmed (tests pass),
session-end extraction may draft an abstract, placeholder-generalized workflow, or a
SKILL.md candidate, into the review inbox. The skills runtime is the destination; memory
only nominates.

**Defaults** (amended 2026-10-31): extraction on, dreaming on a 7-day cadence, the review
inbox the only gate. The original default-off stance borrowed anti-evidence from
unbounded-injection architectures gertie did not build; an empty store renders zero index
tokens, and the discovery cost of an invisible feature outweighed the caution. The review
inbox is what makes default-on safe: proposals apply only when approved (the never-wired
`auto_apply` knob is gone; a documented switch that did nothing was two surprises
waiting to happen).

**Posture**: memory is advisory. Instruction files and AGENTS.md stay the source of record
for rules that must always apply; memory is recall, and Codex's own docs draw the same line.
A nested AGENTS.md (below the root) is not framing: it rides the result of the first tool
call that touches a path under its directory, so the cached prefix never changes, and
"already loaded" is a scan of the history for its marker, so a compaction that drops the
block lets the next touch load it again (`gertie-core/src/directory_instructions.rs`).
Every instruction file, global, root or nested, is sanitized and capped at 2048
characters on its own.
Wrong entries are corrected by the human in an editor, because the store is files. The
memory surface (M7): a footer word and a right-column panel make the store visible and give
the counters their writer, because a shipped feature with no surface is invisible.

**Not built, on the evidence**: embeddings as the primary index (BM25 is mechanically stronger
on identifier-dense text; sqlite-vec is the later add-on if paraphrase recall demonstrably
suffers), LLM-extracted keyword graphs and community summaries (GraphRAG sits in maintenance
mode; summary noise hurts factual recall), Graphiti-style LLM ingestion (four to six
structured-output calls per episode, breaks on cheap models), and any external service or
resident worker (Mem0, Zep, claude-mem all want a daemon; gertie wants a directory).

**Measurement**: the ledger gains cached-token columns. The success metric is cached-input
share and repeated-explanation tokens, never raw totals, raw deltas are cache artifacts.
Independent 2026 experiments found heavy memory layers costing two to three times the tokens
with success flat; the caps and the frozen index are those findings turned into defaults, and
the ledger is how gertie checks them against itself.

## Subagents

- Mechanism: **a regular tool**, `agent(type, prompt)`, running a full child loop with its
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
, inspectable and forkable.
- **Permission asks bubble to the parent transcript.** A worker's loop runs inside a tool nobody
  drains, so its approval requests register on the shared broker (`PromptSource::Worker`) and fan
  out to every subscribed surface; the streaming parent renders them as pending cards in its own
  transcript, labeled `worker`, same approve/deny/reply path as its own asks, and the human's
  answer resolves the parked worker loop. A dropped
  responder (the UI went away) resolves as deny: doing nothing is the safe move. Stopping the
  turn drops the tool future, which drops the worker, cancellation by construction.
  (Pinned end to end: `a_worker_ask_bubbles_into_the_transcript_and_the_answer_reaches_the_worker`.)
- **A brief can't forge its envelope.** The worker's prompt embeds the brief between bare
  `---` lines, with gertie's own trailer after the closing one. Brief and guidance are
  model-authored, so every whole-line run of three or more hyphens in them is
  backslash-escaped at embed time, the only bare `---` lines in the prompt are the
  envelope's own two, and the persisted run record quotes the same escaped text the
  prompt showed.

### Agent types

Types are **data, not code**: a type = `system_prompt`, `tools` allow/deny, `model` override,
an `endpoint` pin (the worker talks to that `[[endpoints]]` entry, its wire included, which
is the whole mechanism behind the bundled `council` skill: three seats, three providers, one
diff), `description` (what the leader sees in the `agent` tool schema), optional `report_format`.
The leader discovers types from the tool schema; description quality is type quality.
Custom types are five-line TOML entries from day one.

Built-in registry at launch (shipped, `builtin_agent_types()`; a `[[agents]]` entry with a
matching name replaces the built-in, and may pin its own `model` or `endpoint`):

| Type | Toolset | Posture |
|---|---|---|
| `explore` | read-only + web_fetch | read-only research workhorse (fetches ask); cheap model |
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
  never a flat list, forking, "retry from here," and subagent-transcripts-as-branches are
  trivial later and migrations impossible to avoid if we start flat.
- Forks point at the matching checkpoint snapshot.

## UI scope

- **v1**: transcript (rendered markdown, inline diffs, tool-call cards with approve/deny) +
  session sidebar. Startup < 100 ms; nothing ever blocks the UI on the agent.
- **M4**: cost/token dashboard, settings UI. (The agents panel once on this row shipped
  post-M5 and was later removed: a toggleable column listing the chat's worker runs,
  read-only rows, every state it showed already visible in the transcript's tool cards.)
- **Not an IDE.** No file tree, no terminal pane, Zed exists. gertie is the harness.
- Parallel sessions run against separate worktrees, visible simultaneously (M4).
- **The status column** shipped after M7: the open session's WORKING ON / NEXT UP /
  WAITING ON YOU / REMEMBERED / TOUCHED / DEPENDS ON / CORRECTIONS beside the
  transcript, plus the riding card on the latest turn (see the window-shell note for
  the full account).

(The agents panel shipped post-M5 and was later removed: a read-only column listing the
chat's persisted worker runs with live report status; every state it showed was already
visible as a transcript tool card. Parallel sessions shipped as tabs (now the sidebar alone: see the window shell
note below), including the worktree half: New worktree chat opens a chat rooted at its own git worktree with a re-rooted tool registry, so
two agents can edit the same repo without colliding. The settings panel closed the UI
scope: live config visible in-app, theme persisted. The native Anthropic wire shipped as
the Provider trait's second adapter (`wire = "anthropic"` per endpoint); the token ledger
persists per-session spend. Nothing left open from the original row, see "The original
'Deferred' row" above for what shipped and what was decided against, and why.)

The composer grew the t3code set: a command palette (Ctrl+K, or the sidebar's search
button) that answers with FTS5 hits from the store, every session's memory, ranked, with
confirm jumping to the hit's turn (a cross-session hit switches first; the jump breaks
tail-follow, scrolling back to the bottom re-engages it). Attachments broadened past
images: a paperclip button opens the picker (the paperclip was once inlined as bytes,
when the app shipped only the kit's default icon bundle; the app now embeds the full
Lucide catalog through `gpui_kit::assets::AllAssets`, about 1 MB of binary, so every
`IconName` renders), paste intercepts images and copied files, PDFs
ride the wire as typed document/file blocks, and text files inline into the message, the
extension table refuses (with a reason) what no endpoint can read. The permission mode
lives on the footer line under the composer, a quiet clickable word that cycles the ring,
same click, none of the composer row's width. A paste at or above
32 KiB never becomes message text: it files into the workspace (`.gertie/pastes/`, kept
out of git by the local exclude) and the message cites the path for the agent's
`read_file`, the context carries the pointer, not the document. An assistant turn worth
commenting on carries a **quote** action: the response lands in the composer as a
blockquote (bounded, enough to pin the referent, never the whole answer back into
the prompt) and the comment types under it; sending shows the model the exact words.
Steer and queue are verbs with a visible stack: while a turn runs, Enter queues a
follow-up (the safe default, it doesn't redirect work in flight) and the secondary
Enter steers (the model reads the words at the next round boundary). The composer's
ghost text names which action each key fires, swapped in only while a turn runs and
the composer sits empty (a placeholder renders where the user is typing, and vanishes
with anything typed). Queued follow-ups go out together, as one message, when the
turn ends: one answer to all of it, not a turn per afterthought. A message carrying
attachments always queues, since a steer is words only. A steer shows as "steering…"
until the loop confirms it joined; one the turn ends without reading leads the next
message instead of vanishing. A stop holds the queue, and says so, until the human
sends it, clears it, or sends a new message (which carries it, queue first).
Launch resume is **workspace-scoped**: the session store is one global memory across
every folder, but the newest unsettled session resumes only when its recorded root
(the `workspace_root` set at first send) matches the folder the app opened in,
otherwise a new folder's first tab starts blank, because one conversation's file
paths must not open in another folder's window. Opening a conversation rooted
elsewhere (sidebar click) says so in the transcript instead of letting the paths
surprise anyone. A footer workspace switch re-roots the session at its next send,
the conversation follows the tab, so resume and the settle hook land where it now
runs, not where it was born.
Queued messages are **rows in the session store**, rendered as dashed drafts that can
be sent now, pulled back into the composer, or dropped; they survive stops, crashes,
and restarts because they were never UI state, and the drain reads the rows, not the
shadow. The footer's identity
line leads with the working folder, and the folder word is the workspace switch:
clicking it opens a folder picker and the chat re-roots mid-session (tool registry,
skills, templates, and the overlay's answers rebuilt for the new root; MCP connections
and web backends carry over, they never captured the root). Mode-chip doctrine
applies: a running turn keeps the root it started with, and the swap lands between
turns (the transcript notes the move; the durable record is the ledger's, below). A
worktree tab's word stays plain text, its root is the checkout it was opened for.
Slash builtins cover the panel actions (/compact,
/export, /new, /settings, /agents) beside file templates. Two templates ship,
write-if-absent like the skills (a user edit is theirs, a project file shadows by
name): `/explain`, and `/commit`, read the repo's recent subjects for its
conventions, read the working diff, hand back a message plus the exact command;
the commit itself stays the human's.

Recall and edit-from-here close the loop on one's own words: ArrowUp in an empty
composer walks back through the session's sent prompts (ArrowDown returns; typing
ends the walk, the walked text becomes a normal draft the moment it's edited), and
any sent user turn carries **Edit from here**: the transcript rewinds to that message,
the composer carries the text as a draft, and the confirm row asks which flavor of
rewind, **Revert and keep changes** (the conversation forgets; the files keep what
the turns did) or **Revert files too** (the checkpoints restore each path to its
pre-turn content). Rewind is fail-closed: a restore is refused while another tab's
session is live in the same checkout (the workspace's guard), a restore target that
resolves outside the workspace (symlinks included) is refused outright, and history truncation
and file restore commit as one step, never files-without-history. The rewound work
is not destroyed either way: the store is append-only, the cut rows remain
recoverable by a reload.

Turn-scoped diffs came from the source-control chapter: every done tool card carries a
"turn diff" button, and the panel answers with what *the tools did*, one entry per path
the turn's checkpoints recorded, before-object against after-object (or disk when never
touched again), bounded lines, plus "restore turn" to put each path back to its pre-turn
content. Git's own churn (pulls, merges) is absent by construction, bash records no
checkpoints. The ledger learned turn windows for this: the chat mints one per send
(session, user-message id, checkpoint boundary), so attribution survives reloads and
steered messages resolve to the turn they steered. The checkpoint hook also gained the
workspace root, a relative tool argument now snapshots the file the tool will actually
touch, not whatever the process CWD reads, and each checkpoint keeps the workspace its
turn ran in, so after a mid-session switch a restore writes back where the turn wrote
(not where the chat now stands), and the rewind guard watches that recorded checkout.
Checkpoints of deleted sessions are swept once per launch, on their own thread: their
ledger rows go, then every object no remaining row names (objects younger than an hour
are spared, so a turn recording right then never loses one).

Runs of finished tool calls collapse into one expandable summary, under honesty rules
transcribed from t3code's activity-log chapter and binding on the collapse code: the
counts are the cards' own ("Ran 3× bash, 1 failed"), failures stay counted, never
rounded into successes; a call still waiting is not a finished one and never joins the
run; and when a card's details are unavailable the summary says something broader
rather than guessing how many threads were affected. Expanding shows every call in
full; nothing is hidden behind the summary but the repetition.

The performance doctrines trimmed the surface too: the spinners came out (static state
text instead), and the transcript stopped loading whole sessions, it opens on the
newest 400 rows with a "load earlier" row paging the archive in, holding the reader's
place per batch. A deep session costs one page at open, not the whole record.

The sidebar became a lifecycle machine (t3code's inbox chapter): the ✓ on a session row
settles it, the human put the work down, folding it into a collapsed "settled"
section (muted rows, ↩ to restore) while the record keeps everything; opening a
settled session is activity and un-settles it. Settling refuses a session with a turn
in flight. Auto-settle ships opt-in (`auto_settle_days = 3`): the startup sweep lays
chats idle that long down as settled, state only, run before any view loads, so a
live turn can't race it. The settle command (`settle_command`, the user's config
alone, the repo overlay may not set a shell command) is the reclaim hook: a
manual settle runs it in the settling session's recorded workspace (recorded at first
send, so whichever view settles it later, the command lands in the right checkout),
off the UI thread, outcome surfaced in the transcript; the sweep never runs commands,
and a session older than the recording says so instead of guessing a folder. Destructive
list actions keep no dialog: ✕ removes a session from every list at once, sidebar, fold,
palette, and a toast at the sidebar's foot holds the store row for a five-second undo
window (button or Ctrl+Z outside the composer, where it stays text undo); the window
closing purges the row, undo popping the entry beats it.
(The row's glyphs are icons now: rename is a pencil, settle an archive box, restore
an archive-restore box, delete a trash can, shown while the pointer is on the row so
a list of conversations reads as titles.)

The keyboard is a surface, not an afterthought. `keymap.toml` (beside the other
settings) holds rules of `{key, command, when}`, last match wins, the file's order is
the binding order. The file is created with the defaults; on later startups new
defaults are appended, never replacing a rule the user wrote: a default is skipped
when its command already appears in the file, or when a user rule holds its key and
`when`, including a rule written as `command = "none"`, which claims a key and binds
nothing. Invalid rules (unknown command, unparsable key, unknown `when`) are ignored
at runtime and left in the file to fix; an unparseable file falls back to the defaults
in memory without a rewrite, the user's only copy of their edits is never destroyed
by the program that failed to read it. `when` names real key contexts:
`composerFocus`, `editableFocus`, `turnRunning`, `turnDiffFocus`. F6/Shift+F6 cycle
the surface's areas, sidebar → transcript → composer, the settings panel joining while
open, wrapping, restoring each area's last-focused control (stale restores fall back
to the container and the next press self-heals); modals stand the cycle down. The
palette answers sessions (FTS5, the picked query ranked up for next time, the MRU
ledger is its own table), and `>` filters to commands: the same set the slash
commands drive, run from anywhere.

A provider rate limit is a state, not an error. When 429 answers every retry, the turn
ends **Limited**, its own event through the whole stack (`Event::Limited` → `LoopEvent::Limited`,
distinct from `Error` at every seam, so no string-matching), persisted as `limited_at` on
the session. The sidebar row carries a `limited` chip and the transcript a slim bar with
**Resume**: a new turn that continues from history as it stands, no new user message, no
retyped ask, and clears the flag. So does any new send (the human is driving; a fresh
limit re-marks it). Divergence from t3code, deliberate: no resume-after-restart, gertie
has no daemon, so a closed app cannot keep a promise. **Snooze** rides the same bar as a
*deferred click*: "Snooze 30 min" schedules the very same resume, one shot, on an in-app
timer, nothing re-arms itself, so every cycle still starts with a human click and
t3code's three-consecutive-auto-resumes cap has nothing to cap. The reported reset time
(Retry-After and friends) isn't parsed; a fired-but-still-limited turn just ends limited
again for a human decision. The timer lives in the running app like every clock gertie
has: close it and the snooze dies with it, the persisted limited state doesn't. The
round budget borrows this whole shape: at `MAX_TOOL_ROUNDS` (25) the loop nudges the
model in band (`LoopEvent::Nudged`, a history user row demanding wrap-up), gives it
`GRACE_ROUNDS` (5) to land, and only then pauses the turn as `LoopEvent::Budgeted`,
which rides the same Limited state, chip, bar, and Resume as a rate limit (steers at
the boundary still land: drained before the pause, each buying its round). The old
hard error is gone; long work is no longer killed mid-flight, runaway loops still
are. The sidebar carries the working marker: a live turn shows a green dot on its
conversation's row, a finish seen by nobody a blue one, a rate-limit pause the pause
icon.

**Projects** group the sidebar, and only the human files anything into one. A project
is a name in the session store (`projects`), not a folder or a workspace: where a chat
runs stays the chat's own (`workspace_root`, the header's folder switch). Membership is
a column on the session (`project_id`, one project at most), set by the row's move list
or by starting a chat from the project's +, never inferred: two chats in one folder stay
unrelated until filed. The + starts a blank chat in the current folder, filed when its
first message creates it; plain New chat files nothing. Removing a project is list-only
(its chats go back to Other; no session is deleted). Each group folds, and the fold
persists; with no projects the sidebar is the old flat list. A view that cannot take a
project's new chat (a turn running, a worktree tab) asks the shell for a new one. The
palette matches project names above history hits.

The window shell has no tab bar: two lists of the same conversations was one
too many. The shell keeps one chat view per conversation that has something to say (a
running turn, an unseen finish, a pause) and shows the active one; an idle view off
screen is dropped, since its conversation lives in the store and the sidebar, unless
it holds something only it has (a delete inside its undo window, an unsent draft or
attachment, a rename in progress). The shell repaints for an off-screen view only when
what the sidebar shows changes (running, limited, which conversation), not on every
streamed token. A
sidebar click routes: a view that already has the conversation open is shown; an idle
clicked view loads it (adopting its workspace if needed); a clicked view that is
running a turn is never dragged, a new view opens instead. New chat while a turn runs
opens a new view the same way. Stop is the composer's send button while a turn runs
with nothing typed: it drops the turn's future (cancellation by construction), bash
kills its command's whole process group, and every unanswered call gets a "stopped by
the user" result in the record and the model's history, so the next request is
legal. The window holds running work to the same
standard: with a turn live anywhere, the first close is refused with a notice in
each running chat, and a second close inside five seconds is taken at its word,
an accidental quit kills a running turn, so it costs one deliberate repeat.

The same pass reworked the surface around it. The sidebar is a list, not a column of
buttons: the logo and name on top, New chat and New worktree chat (worktree chats
need a label; a bare branch glyph was a riddle), the conversations as plain rows (the open one tinted), and Search and Settings at the
foot. Every transcript row (messages, tool runs, the activity line), the composer,
and the footer under it share one reading width (820 px, centered), past which lines
get too long to follow. Message actions are icons with tooltips: a user
turn's fork and rewind stay visible (hiding rewind behind hover made it
undiscoverable); an assistant's copy and quote show ghosted on the reply that ends a
turn and only on hover for the narration before it, so a long turn is not dotted
with icons. A conversation header names the open conversation (click to rename) and
the folder its tools run in (behind a folder icon, at full text strength so a bare `~`
still reads as a place; click to switch workspace; a worktree chat shows its checkout
with the branch icon, fixed), the orientation the tab bar used to give; the
footer under the composer keeps the ledger and the endpoint. The
composer is one rounded box: attach on the left, the field, and send (or Stop) on the
right; its idle ghost text is "Message Gertie, or type / for commands". List markers
sit by an item's first line, not the middle of a wrapped one. The settings panel is
cards (Appearance, Endpoints, Sandbox, Memory, Turn budgets, MCP servers, Agents,
Config file); endpoint rows read name over model, left-aligned.

The status column is the open session's durable state beside the transcript
(toggleable, collapsible to a rail), fed by folding the session's rows on open and
then live tool events, so nothing is inferred twice and the model is never asked for
what the record already shows; `status` is the only call the model makes, stating
goal and next step when the plan changes. The sections read plain human: WORKING ON
(the model's stated goal, else the prompt it is working from) with the run clock,
NEXT UP (its stated next step), WAITING ON YOU (asks from the other open chats of
this chat's project, placed above the ledgers because it is the one section that
demands action and the only one that hides badly), REMEMBERED (the session's memory
writes, runs of the same verb and file folded to one row counted ×N), TOUCHED (files
read and edited, newest at top, edited's badge winning; build output is dropped,
writes to `target/`, `node_modules` and logs are not file state, but reads stay,
because a crash diagnosis is real work), DEPENDS ON (the memory entries the work
consulted or whose cited files it touched), and CORRECTIONS (the walk behind a
flagged dependency entry). Every ledger is newest-first, shown through a viewport
sized to the space the column actually has (measured per render, not guessed pixels),
the whole list still scrollable beneath the cap. On the latest assistant turn a
riding card carries the same state inline when it has something to say: the goal and
clock, the dependencies, or an ask, attached to the reply the work produced. The
vocabulary was renamed once the labels met a reader: the spec's internal words
(GOAL, GROUNDED IN, FILES) became the ones above, on the doctrine that a label
promises its content.

## Onboarding

First run with the default path writes a starter `config.toml` of comments and pointers,
no endpoint is built in, so none is invented for the user, and says so in the status
line; the settings panel (`/settings`) owns endpoints and keys, and an explicit
`--config` override never gets one written. `gertie doctor` prints the
setup as gertie sees it: config parse state, endpoint + wire, the API key's resolution
source (or the exact `export` to run), MCP servers by transport, the project overlay's
state and the origin tier of what it resolves, sandbox, the default
permission mode's honest reach (what it enforces and what it deliberately leaves
asking, with the sandbox off, auto says it degrades), all-time
token totals, and the worktree ledger, every gertie-made worktree, kept or removed
and why (`--clean-worktrees` lets the removal happen; report-only without it). The
missing-key warning names the variable. Exit 0 from doctor = ready.

## Distribution

GitHub Releases tarball + `cargo install --locked` (cargo-binstall-compatible metadata).
Flatpak/AppImage deferred, Flatpak in particular fights an agent harness's need to spawn
processes in the user's environment. Linux-only packaging until macOS support lands.

## Doctrine

Four lines taken from t3code's AGENTS.md, they pass the
gertie test of restating what we already believe, from the other side of the harness
boundary:

- **Reverse states.** If you added a way in, add the way out and the way to see it. Snooze
  needs unsnooze; expand needs collapse; a one-way door is a bug.
- **Labels promise the action.** A visible label, tooltip, menu wording, or accessible
  name describes the same action on the same object, the permission card shows the diff
  it will run, never a verb alone, and a copy-message button never promises to copy a link.
  This is the standing invariant for every UI change, human- or agent-authored: when a
  control's name and its behavior disagree, the behavior is the bug.
- **Authority comes from configuration and the human at the card**, never from
  conversation: no message, tool result, or skill description can grant a permission.
- **Stored events stay decodable.** Persisted rows load after every upgrade; the reload
  path is contract, not best-effort. `gertie doctor` replays old sessions as the smoke test.
- **Answers are typed, not sentinel strings.** What can be empty, unknown, or errored
  comes back as distinct variants, a turn file's "after" is the next checkpoint's object
  or disk (`AfterState`), a refused file is a refusal with a reason, never `""` meaning
  three things. Record what was *asked* as well as what was *heard*: absence from an
  answered map is "never looked", not "looked and found nothing", conflating them
  reports every cleared entry as changed.

Four more, from t3code's performance doctrines:

- **No continuously repainting animations.** Motion repaints every frame for as long as
  it is visible; a state the words already state gains nothing from a spinner. Static
  text carries state ("running", "thinking…"), animation is for what only motion can
  say. One deliberate carve-out, the **working tick**: while a turn runs, the status
  line steps a 2-fps half-moon spinner and a seconds clock (`Thinking… · 14s`), a
  running tool's card ticks with its own clock (`◐ running · 2m 05s`), and past twenty
  seconds without a loop event the line says `quiet 2m`, but not while an approval
  card waits: that silence is the human's, and the ask's own clock tells it. The
  difference between slow and stopped is information, not decoration, and the tick
  dies with the turn: nothing animates at rest.
- **Turn end ≠ settlement.** The loop yields `Done` and returns, no trailing
  persistence, no summary, nothing after the turn's last event (pinned by test). The
  caller owns any post-turn work, on its own schedule.
- **Bounded loads.** A view page is not the archive: the transcript opens on the newest
  window and pages older rows in on demand. Wire history, export, and fork read
  everything; the render path never does.
- **Raw output streams on an ephemeral channel.** If long-running tool output ever
  streams to the UI, it rides a throttled side channel, never a persisted row per
  delta. (No streaming exists yet; the doctrine pre-empts the tempting table.)

Two reservations, parked until the feature they govern exists:

- **Monitors buy their wakes.** Any future background monitor (a timer that starts a
  turn) carries a self-termination budget, a cap on consecutive wakes that bring no
  news, and deduplicates what it has already reported. A monitor that cannot decide
  to stop is a bug, not a feature.
- **Default bindings never steal focused text input.** If gertie ever embeds a surface
  that owns keystrokes (a terminal pane, a full editor), every default binding that
  could shadow it carries a `!terminalFocus`-style guard condition, and remapping a
  default keeps that guard.

## Working agreements

- Local git from the first commit; remote added when the owner is ready.
- `DESIGN.md` is the source of truth; amendments land here first, code second.
