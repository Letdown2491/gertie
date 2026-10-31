# gertie: memory

gertie remembers across sessions so you stop re-explaining your project every time
you open a new chat.

## What you'll see

- A **`memory N`** word in the footer when gertie holds facts about this project. Click
  it (or run "memory" from the palette) to see everything it remembers and vote on each
  entry: ▲ when a memory helped, ▼ when it misled. Your votes decide what gets kept.
- A **proposal card** when you settle a session (✓): the same approve/deny card tool
  calls get, listing the facts worth keeping from that conversation. Approve to keep
  them, deny to skip them; either way it never asks about that session again. Short
  sessions and hello-world chats don't produce cards.
- A **weekly cleanup**: the first launch of each week merges duplicate memories and
  archives ones you voted down. It writes a changelog you can read, and since memory
  is plain markdown tracked by git, a bad cleanup is one `git checkout` away from
  undone.

That's the whole interaction. Nothing enters memory without your approval, and turning
it off is one switch in the settings panel.

## The two knobs

The settings panel has a MEMORY section: a switch for memory itself and a switch for
the weekly cleanup. Two things stay in `config.toml` on purpose: `auto_apply`
(remembers without asking, and changes what you should expect) and `extract_endpoint`
(which model makes the proposals). Both are documented in `config.example.toml`.

## If something's remembered wrong

Open the memory panel, vote it down, done; entries the human keeps voting harmful get
archived automatically. Or edit the files directly, they're plain markdown at
`.gertie/memory/` (facts about this repo, committable) and
`~/.local/share/gertie/memory/` (facts about you, all projects). Every entry traces
back to the session that produced it, so a wrong memory shows its receipt.

## Rules are not memory

Things that must always apply belong in `AGENTS.md` files (`~/.config/gertie/` for
your rules, the repo root for project rules), not memory. Gertie reads those every
turn; memory is recall it consults when relevant. Keep keys and credentials in config
or env, never in either.

## Under the hood, briefly

Memory is markdown files, one line per fact, each carrying its source session and a
date. A capped index (600 tokens) is shown to the model every request, frozen per
session so prompt caching works; everything else the model fetches on demand with a
search tool. Proposals come from one cheap model call at session end. The cleanup is a
deterministic pass, no model, so it can't invent a merge. Nothing is ever deleted;
superseded entries gain a marker and stay in the file for history.
