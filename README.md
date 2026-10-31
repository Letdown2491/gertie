# gertie

A fast, native coding-agent harness. GPU-rendered UI, a headless core that is a plain
library, and everything local: sessions in SQLite, zero telemetry, no accounts.

```
gertie-core       headless agent core: loop, sessions, permissions, checkpoints
gertie-providers  provider adapters (OpenAI-compatible + native Anthropic wire)
gertie-tools      built-in tools, sandbox, skills, MCP client
gertie            the desktop app (gpui) + `gertie run` headless shell
```

## Why

Most agent harnesses are either Electron apps or CLI wrappers glued to a cloud. gertie is
a native Linux desktop app with the same engine available headlessly: one core, two
fronts, nothing shared over the network.

- **Local-first**: sessions persist in SQLite with full-text search over every message
  ever recorded. Every turn is checkpointed (git-style snapshots) before file-mutating
  tools run.
- **Permission broker**: reads are auto-approved; writes and shell commands prompt, with
  the diff shown before you decide. Opt-in [Landlock](https://landlock.io) sandboxing
  isolates bash children (system read-only, writes limited to the workspace and /tmp).
- **Subagents, not a monolith**: eight built-in agent types (explore, plan, implement,
  test, debug, review, research, librarian) dispatched by the leader; define your own in
  TOML, pinned to any configured endpoint: a council of three models can review one diff
  (the bundled `council` skill runs the loop). Workers never spawn workers.
- **Tabs and worktree tabs**: parallel chats in one window. The ⎇ button gives a tab its
  own `git worktree`, so two agents can edit the same repo without colliding.
- **Any endpoint**: speaks the OpenAI-compatible wire (ppq.ai, OpenRouter, Ollama,
  vLLM, Gemini's compat endpoint) plus a native Anthropic adapter. Fallback chains: when
  an endpoint dies before producing content, the next one takes over invisibly.
- **Editable settings**: endpoints, API keys, the sandbox, and MCP servers are
  configurable in the panel; saves go through `toml_edit`, so your config comments and
  hand-written keys survive. Endpoint switches apply to open chats immediately.
- **MCP**: external tools over stdio *or* Streamable HTTP. Server tool descriptions are
  treated as untrusted input (sanitized, capped); every MCP call prompts.
- **Token ledger**: per-session and all-time token totals, persisted locally. No
  prices-per-model by design; they rot, tokens don't.

## Install

From source (Rust 1.97+, Linux on Wayland or X11 with GPU rendering):

```sh
git clone https://github.com/Letdown2491/gertie.git
cd gertie
cargo install --locked --path gertie   # installs the `gertie` binary
```

(The maintainers build and test inside a pinned container via `./scripts/build.sh`; a
plain `cargo build` works too: `gertie` needs the usual Wayland/X11 client libraries.)

## Quick start

```sh
gertie doctor          # checks config, endpoint, and key resolution; exit 0 = ready
gertie                 # the desktop app
gertie run "fix the flaky test in ci and explain why"   # one headless turn
```

First launch writes a starter `~/.config/gertie/config.toml` with comments and pointers
only: no endpoint is built in, so none is invented for you. Add yours in the app
(`/settings` in the composer) or by hand:

```toml
[[endpoints]]
name = "mine"
base_url = "https://your-provider.example/v1"   # any OpenAI-compatible server
model = "the-model-id"                          # the id your endpoint serves
api_key_env = "MY_API_KEY"
```

Keys resolve from the environment first (`MY_API_KEY` above); a local server needs
no key at all.

`config.example.toml` documents everything else: wire selection (`openai` |
`anthropic`), fallback endpoints, MCP servers, custom agents, search backends,
sandboxing. `gertie run` prompts for approval of mutating tool calls; pipe-safe mode
denies them unless `--yes` is given.

## Security posture

- Zero telemetry. The only network traffic is to the endpoints you configure.
- API keys resolve from the environment (`api_key_env`), never required in the config
  file; a literal key is allowed but flagged in the status line.
- Tool calls: reads auto-approved; writes and bash prompt with a diff; bash can be
  Landlock-sandboxed at the kernel level (opt-in).
- MCP tools are third-party code: descriptions are sanitized untrusted input and every
  call prompts: the user is the capability system.

## Status

Feature-complete against [DESIGN.md](DESIGN.md) (see "The original 'Deferred' row" for
every decision and its rationale). [SESSION-REPORT.md](SESSION-REPORT.md) is the honest
engineering log: what shipped, what bit, and why.

## License

MIT. See [LICENSE](LICENSE).
