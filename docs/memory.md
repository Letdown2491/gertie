# Memory

Gertie remembers across sessions so you stop re-explaining your project every time
you open a new chat.

## What you'll see

- A **`memory N`** word in the footer when Gertie holds facts about this project. Click
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

The settings panel has a Memory card: a switch for memory itself and a switch for
the weekly cleanup. Like every setting there, a switch saves the moment you flip it.
One thing stays in `config.toml` on purpose: `extract_endpoint` (which model makes
the proposals). Proposals only ever apply through the review inbox: nothing is
remembered without asking. Documented in `config.example.toml`.

## If something's remembered wrong

Open the memory panel, vote it down, done; entries the human keeps voting harmful get
archived automatically. Or edit the files directly, they're plain markdown at
`.gertie/memory/` (facts about this repo, committable) and
`~/.local/share/gertie/memory/` (facts about you, all projects). Every entry traces
back to the session that produced it, so a wrong memory shows its receipt.

## Rules are not memory

Things that must always apply belong in `AGENTS.md` files (`~/.config/gertie/` for
your rules, the repo root for project rules), not memory. Gertie reads those every
turn. An `AGENTS.md` in a subdirectory (`docs/AGENTS.md`, `migrations/AGENTS.md`)
holds rules for that part of the tree: it is loaded the first time a tool reads or
edits a file under it, and shows up in that tool's result in the transcript. Each file
is capped at 2048 characters. Memory is recall it consults when relevant. Keep keys
and credentials in config or env, never in either.

## Under the hood, briefly

Memory is markdown files, one line per fact, each carrying its source session and a
date. A capped index (600 tokens) is shown to the model every request, frozen per
session so prompt caching works; everything else the model fetches on demand with a
search tool. Proposals come from one cheap model call at session end. The cleanup is a
deterministic pass, no model, so it can't invent a merge. Nothing is ever deleted;
superseded entries gain a marker and stay in the file for history.

An entry is one line, with its markers in brackets at the end:

```
- [p12] helpful=2 harmful=0 the pool is sized for postgres [links: builds_on p7] [evidence: src/pool.rs@3f2a1c9e0b12] [source: 388; added: 2026-07-11] [superseded by: p31; valid until: 2026-11-20]
```

- `[links: ...]` connects an entry to others: `relates_to`, `solves`, `causes`,
  `builds_on` or `replaces`, then the other entry's id.
- `[evidence: ...]` lists the files that supported the entry when it was written,
  each with its content hash (git's blob id, shortened). Only files the session
  actually read or wrote can be cited.
- `[superseded by: p31; valid until: ...]` says what replaced an entry and when. An
  entry retired with nothing to replace it says only `[valid until: ...]`.

Because these live in the file, they travel with the repo and survive a rebuilt
database. The memory panel shows each superseded entry's chain ("replaced by p31 →
p40") and each entry's links. Edit them by hand like anything else in the file.

## When the code moves out from under a memory

If a file an entry cites has changed since the entry was written, or is gone, the
entry is marked **evidence changed**: in the memory panel, in the index the model
sees, and in its search results, where the model is told to re-check before relying
on it. Nothing else happens on its own. The entry keeps its place and its votes; you
decide whether it still holds (vote, edit, or supersede it). Put the file back and
the mark goes away.

## When a memory is corrected

Superseding an entry also flags the entries that rested on it: ones linked to it with
`builds_on` or `solves`, and ones from the same session. They gain `[recheck: p12]`
and show "p12 was corrected" in the memory panel, with two buttons: **keep** (it still
holds; the flag goes) and **archive** (it doesn't). For anything else, edit the line.
Nothing changes on its own, and the model sees the flag too. The walk goes one step
back only: correcting a flagged entry flags its own dependents, not the whole chain.
