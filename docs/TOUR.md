# The tour: what Gertie does

Everything below ships in the binary; nothing needs a plugin. Ten minutes of reading,
then try each thing as you go.

## The loop

You type, the agent works: it reads files, runs commands, edits code, and shows you
every change. Reads run without asking; every write and every shell command shows up
as a card with the diff (or the command) before it runs. Approve, deny, or reply with
corrections in the card's text field: your reply goes back to the model as the tool's
result.

The agent asks *you* questions too: when a decision is genuinely ambiguous it sends an
`ask_user` card with clickable options (plus a free-text field when it makes sense).
Answer by clicking; the answer rides back into the work.

The status line above the composer names the current action in plain words: the tool
being run, the file being read, the question being asked. The footer counts what the
work costs (tokens in/out, the cache share, the live context gauge) and, while a run
is live, how long it has taken.

## Checkpoints and rewind

Before any file-mutating tool runs, the workspace is snapshotted. Open **turn diff**
on any finished tool card to see exactly what that turn changed, and **rewind** to
restore the workspace to any earlier turn: keep your files, or restore theirs too.
Every rewind is itself recorded; nothing is ever silently undone.

## Tabs and worktree tabs

New-tab opens a parallel chat. The ⎇ button gives a tab its own git worktree: a real
checkout in `<repo>/.gertie/worktrees/`, branched from HEAD, so two agents can edit
the same repo at once without colliding. Each tab keeps its own memory tier and
permission mode. A ✦ in the tab bar means the turn finished while you were elsewhere;
⏸ means it is paused waiting on you.

## Subagents

The agent dispatches specialists instead of doing everything itself: `explore` reads
broadly and reports, `plan` turns fuzzy goals into steps, `implement` edits within a
brief, `test` runs suites and reproduces failures, `debug` diagnoses without patching,
`review` critiques before you merge, `research` reads the web, `librarian` mines past
sessions. Each run is a card in the transcript; briefs are files you can read.

Define your own in `config.toml` (`[[agents]]`), pinned to any endpoint: put a
cheap model on `explore` and a strong one on `review`, or run a council of three
different models over one diff (the bundled `council` skill does exactly this).

## Skills

Skills are markdown files the agent loads when a task matches: bundled ones live in
`gertie/src/skills/`, yours in `~/.config/gertie/skills/` and `.gertie/skills/` per
repo. A skill is a name, a description of when to use it, and instructions. The
`writing` skill shapes kept prose; `council` runs multi-model review. Write your own
for your team's conventions.

## Memory

The agent remembers across sessions, and you control every entry. Facts it proposes
at session end arrive as review cards; approve or deny, and vote ▲/▼ on what it
recalls so the useful stuff survives the weekly cleanup. Rules that must always apply
belong in `AGENTS.md` instead; the `memory N` footer word opens the panel.
[docs/memory.md](memory.md) has the details.

## MCP: your tools, its hands

Gertie speaks MCP over stdio and Streamable HTTP, so file-indexers, issue trackers,
database explorers, anything that speaks the protocol becomes callable. Server tool
descriptions are treated as untrusted input, and every MCP call prompts: the user is
the capability system. Configure in `config.toml` (`[[mcp_servers]]`) or the panel.

## Headless: the same engine, no window

`gertie run "one-turn task"` runs the identical loop with stdout for the answer and
pipe-safe approvals (mutating calls deny themselves unless you pass `--yes`). The
library behind it (`gertie-core`) is plain Rust: embed the agent in CI, an editor, a
server. Same sessions, same memory, same permission broker.

## Doctor

`gertie doctor` checks the setup end to end: config resolution, endpoint and key,
MCP servers, agent pins, the session store, and whether a newer release exists.
Exit 0 means ready; every line says what it checked and what it found.
