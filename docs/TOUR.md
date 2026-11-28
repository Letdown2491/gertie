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
them), and the arrow sends. What you've typed but not sent stays with its chat: switch
conversations or start a new one and the draft waits where you left it. Attached
images are kept with the message, so a reopened chat still shows the model what you
sent. Enter while a turn runs queues a follow-up for after it
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
  failure goes back to the agent. The check is the folder's own: switch the chat to
  another project, or open a worktree chat, and that folder's check runs.
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

The rail down the window's left edge holds one circle per project; click one for its
chats (title, state, age), or open the full sidebar from the rail's icon. There is no
tab bar. A running turn keeps going when you open another conversation or start a new
one: its project's circle breathes, a vermilion dot means a chat waits on you, and in
the sidebar a green dot marks a running row, a blue dot a turn that finished while you
were elsewhere. **New worktree chat** (the branch icon on the rail, or under New chat) starts
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

Workers see what the project knows: they search memory, and memory about a file they
touch arrives with the read, as it does for the main agent. They don't write memory
themselves; anything worth keeping goes in their report, and the main agent decides.
A worker that edits code runs the project's check before it reports back. Workers
talk only to the main agent, which passes along what one learned to the next.

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

## Two panes and Gertie's face

The chat is two panes. **Words** on the left, on paper: your messages, quiet; Gertie's
reasoning in a serif; numbered marks (① ②) where a paragraph caused work; memory as
seal notes. A note says which entry the work relied on and, when a file it cites has
changed, asks **still true** or **revise**. What the settle proposes to remember comes
as a "new" seal with **keep**, **edit** or **let it go**. **Work** on the right, in ink:
a one-sentence summary ("2 files changed, the check passed.") over the files touched,
then one card per numbered item. An edit card shows the file, the size of the change,
where in the file it sits, and the diff on demand; a check card shows its command, its
last lines and how many tests passed. Commit and Review the whole diff sit under them,
and the checkpoint timeline at the foot returns the workspace to before any question.
A turn that only talked folds the work side to a strip; under 1100px the work side
opens over the words instead of beside them. A card waiting on your OK always shows.

Between the panes sits Gertie's face, from the app icon, inside a thin ring that fills
as the turn goes from understanding to changing to verifying. The face says the rest:
curious while reading, focused while editing, waiting on you (the ring's head pulses),
smiling when settled, sad when a check failed. Under it, the numbered marks hang on a
thread. The header over the words says what this is and, in a sentence, what is
happening now and what comes next; the `status` tool is how the model states its goal
and next step, everything else is read from the record, never asked for. The small
motions (the pulse, the breathing rail ring, the blinking live mark) can be turned off
under Chat settings, Appearance.

## Settings

Settings opens as a modal over the whole window, from the gear on the rail, `/settings`
in the composer, or the palette (`ctrl-k`, `>settings`): four sections in a left nav
(Chat, Endpoints, Memory, Tools & agents), and every open lands on Chat. Every change
saves the moment you make it, into your `config.toml` with its comments kept; MCP servers'
`args` and `env` pass through untouched. Endpoint changes apply right away, the rest says
when it needs a restart. Escape closes it, so does the ✕ in its corner.

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
