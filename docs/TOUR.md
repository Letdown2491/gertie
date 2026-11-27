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
them), and the arrow sends. Enter while a turn runs queues a follow-up for after it
(queued messages go out together, as one message); Ctrl+Enter steers the running turn
instead. A stop holds the queue until you send it, clear it, or send a new message. With
the composer empty, the arrow becomes Stop.

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

## Help for any model

Whichever model you pick, the harness does the parts weaker models skip:

- **A map of the code.** The first turn of a session sends a short, ranked map of the
  repo's source files and what each defines, so the agent opens the right file first.
  The `repo_map` tool lists a directory or a file's signatures when it needs more.
- **Memory on touch.** When the agent reads or edits a file that a memory entry cites,
  the entry arrives with the result.
- **The check before "done".** If a turn edited files, gertie runs the project's check
  (`cargo check`, `go build`, or your `[verify]` command) before the turn can end; a
  failure goes back to the agent.
- **Fewer wasted tokens.** Long command output keeps its head, its failures and its
  tail; a file read twice keeps only the newer copy; and long sessions compact at 160k
  tokens even when the model claims a far bigger window, so replies stay quick.
- **Eyes on the work.** `screenshot` renders a page (your dev server, or an HTML file in
  the project) in headless Chromium, light or dark, viewport-sized or the full page, or
  captures a window or the screen, and the agent looks at it. `read_file` on an image
  shows the picture. A model that can't read images gets `vision = false` on its
  endpoint and a note in place of each picture.

`gertie eval memory` and `gertie eval nav` measure each configured model (memory use,
and how much faster the map finds code) and suggest settings for it.

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

## Projects

Projects organize conversations: a project is a name, not a folder. Make one with the
button beside the list heading, then file a chat under it with the move button on the
chat's row, or start a chat from the project's **+** (it runs in your current folder,
already filed). A chat is in one project at most; ones you haven't filed sit in
**Other**. To take a chat out again, click the remove button on its row (beside
delete): it goes back to Other, kept whole. Each group folds, removing a project only
takes it off the list, and Ctrl+K finds a project by name.

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
recalls so the useful stuff survives the weekly cleanup. Entries cite the files they
came from and are flagged when those files change; correcting an entry flags the ones
that rested on it, for you to keep or archive. Rules that must always apply belong in
`AGENTS.md` instead (one in a subdirectory applies when the agent works under it); the
`memory N` footer word opens the panel.
[docs/memory.md](memory.md) has the details.

## The status column

To the right of the transcript (toggleable, collapsible to a rail) sits what the open
session actually is, folded from its own record and live tool events: WORKING ON with
the run clock, NEXT UP, WAITING ON YOU when another chat's ask needs you, REMEMBERED
(memory it wrote), TOUCHED (the files in play), DEPENDS ON (the memory entries this
work rests on) and CORRECTIONS (what changed under it). Each section is newest-first
and only appears when it has something to say. On the latest reply, a small card
repeats the same state inline when there is something worth carrying. The `status`
tool is how the model states its goal and next step; everything else in the column is
read from the record, never asked for.

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
server. Same sessions, same memory, same permission broker. `gertie eval memory` and
`gertie eval nav` run live scenarios in throwaway repos against each configured model
and keep a record of the results.

## Doctor

`gertie doctor` checks the setup end to end: config resolution, endpoint and key,
MCP servers, agent pins, the session store, and whether a newer release exists.
Exit 0 means ready; every line says what it checked and what it found.
