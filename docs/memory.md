# Memory

Gertie remembers across sessions so you stop re-explaining your project every time
you open a new chat.

## What you'll see

- A **memory** button in the composer, with a count of what this chat used and learned.
  Click it for those entries, each with **keep** (it's still true), **edit** and
  **forget**. **See all** at its foot (or "memory" from the palette) opens everything
  Gertie remembers here, grouped by topic: each entry says where it came from and what
  you've marked, and offers **still true**, **wrong**, **edit** and **forget**. Your marks
  decide what gets kept.
- A **seal** in the conversation when you settle a session (✓), listing the facts worth
  keeping from it: **keep** them, **edit** (keeps them and opens the panel to change
  them), or **let it go**. Either way it never asks about that session again. Short
  sessions and hello-world chats don't produce one.
- A **seal note** when the work relied on a memory, naming it; when a file it cites has
  changed, the note asks **still true** or **revise**.
- A **weekly cleanup**: the first launch of each week merges duplicate memories and
  archives ones you marked wrong. It writes a changelog you can read, and since memory
  is plain markdown tracked by git, a bad cleanup is one `git checkout` away from
  undone.

That's the whole interaction. Nothing enters memory without your approval, and turning
it off is one switch in Settings.

## The two knobs

Settings has a Memory section: a switch for memory itself and a switch for
the weekly cleanup. Like every setting there, a switch saves the moment you flip it.
One thing stays in `config.toml` on purpose: `extract_endpoint` (which model makes
the proposals). Proposals only ever apply through your answer on the seal: nothing is
remembered without asking. Documented in `config.example.toml`.

## If something's remembered wrong

Open the memory panel and mark it **wrong**, or **forget** it outright; entries marked
wrong often enough get archived by the weekly cleanup. Or edit the files directly, they're plain markdown at
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
database. The memory panel shows what replaced an entry and what each entry links to, in the
other entries' own words; replaced and forgotten entries fold into a section at its end. Edit them by hand like anything else in the file.

## When the agent touches a file

Memory doesn't wait for the model to look it up. When a tool reads or edits a file
that a memory entry cites, the strongest few entries about that file arrive with the
tool's result, so you see in the transcript what the agent was given and where. If
what it is about to do contradicts one, it names the entry and asks you first. Each
file's entries arrive once per session (again after a compaction drops them).

## Subagents and memory

Workers (`explore`, `implement`, `debug` and the rest) read memory the same way: every
built-in type can `search_memory`, and recall on touch works for the files a worker
reads or edits. Writing stays with the main agent: workers never get `remember` or
the `memory` tool, even when a custom agent's allowlist names them. A worker puts
what it thinks is worth keeping in its report, and the main agent decides whether to
save it, so every write still happens where you can see it.

## When the agent saves a fact

The agent saves facts with a `remember` tool: one fact per call, and a changed fact
is superseded (the old entry retired, a new one replacing it) instead of rewritten.
Gertie writes the entry's line itself, so any model produces valid entries. If you
deny an action and say why ("no, we use tabs here"), that correction becomes a
memory proposal at session end, even in a short session, labeled as a correction on
the seal.

## When the code moves out from under a memory

If a file an entry cites has changed since the entry was written, or is gone, the
entry is marked **evidence changed**: in the memory panel, in the index the model
sees, and in its search results, where the model is told to re-check before relying
on it. Nothing else happens on its own. The entry keeps its place and its marks, listed under **Needs a look** in the panel; you
decide whether it still holds (mark it still true, edit, or forget it). Put the file back and
the mark goes away.

## When a memory is corrected

Superseding an entry also flags the entries that rested on it: ones linked to it with
`builds_on` or `solves`, and ones from the same session. They gain `[recheck: p12]`
and show under **Needs a look** in the memory panel, saying what was corrected, with two
buttons: **still holds** (the flag goes) and **forget it** (it doesn't). For anything else, edit the line.
Nothing changes on its own, and the model sees the flag too. The walk goes one step
back only: correcting a flagged entry flags its own dependents, not the whole chain.
