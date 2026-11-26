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

The composer is one box: the paperclip attaches images, PDFs, and text files (or paste
them), and the arrow sends. Enter while a turn runs steers it; Ctrl+Enter queues a
follow-up for after. With the composer empty, the arrow becomes Stop.

Each message has small icon actions: on your messages, fork (a new conversation from
that point) and rewind; on the agent's, copy and quote (the reply lands in the
composer as a quote, so you can answer one part of it). The agent's final reply in a
turn shows them faintly; earlier messages in the turn show them on hover.

The header above the conversation names it (click the title to rename) and the folder
its tools run in (click to move the agent to another folder).

The status line above the composer names the current action in plain words: the tool
being run, the file being read, the question being asked. The footer counts what the
work costs (tokens in/out, the cache share, the live context gauge) and, while a run
is live, how long it has taken.

## Checkpoints and rewind

Before any file-mutating tool runs, the workspace is snapshotted. Open **turn diff**
on any finished tool card to see exactly what that turn changed, and **rewind** to
restore the workspace to any earlier turn: keep your files, or restore theirs too.
Every rewind is itself recorded; nothing is ever silently undone.

## Parallel chats and worktree chats

The sidebar is the only list of conversations; there is no tab bar. A running turn
keeps going when you open another conversation or start a new one, and its sidebar row
shows a green dot. A blue dot means a turn finished while you were elsewhere; a pause
icon means the run is paused waiting on you. **New worktree chat**, under New chat, starts
a chat in its own git worktree: a real checkout in `<repo>/.gertie/worktrees/`,
branched from HEAD, so two agents can edit the same repo at once without colliding.
Each chat keeps its own memory tier and permission mode.

While a turn runs, the send button becomes Stop when the composer is empty. Stop ends
the turn at once: running commands are killed and open tool cards close as stopped.

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

## Settings

Settings opens from the foot of the sidebar (or `/settings`): endpoints and keys, the
sandbox, memory, turn budgets, MCP servers, and agents, one card each. Every change
saves the moment you make it, into your `config.toml` with its comments kept.
Endpoint changes apply right away; the rest says when it needs a restart.

## MCP: your tools, its hands

Gertie speaks MCP over stdio and Streamable HTTP, so file-indexers, issue trackers,
database explorers, anything that speaks the protocol becomes callable. Server tool
descriptions are treated as untrusted input, and every MCP call prompts: the user is
the capability system. Configure in `config.toml` (`[[mcp_servers]]`) or the panel.

## Headless: the same engine, no window

`gertie run "one-turn task"` runs the identical loop with stdout for the answer and
pipe-safe approvals (mutating calls deny themselves unless you pass `--yes`, and
`--yes` refuses a git workspace with uncommitted changes unless you also pass
`--allow-dirty`: auto-approval over work you can't roll back is opt-in). The
library behind it (`gertie-core`) is plain Rust: embed the agent in CI, an editor, a
server. Same sessions, same memory, same permission broker.

## Doctor

`gertie doctor` checks the setup end to end: config resolution, endpoint and key,
MCP servers, agent pins, the session store, and whether a newer release exists.
Exit 0 means ready; every line says what it checked and what it found.
