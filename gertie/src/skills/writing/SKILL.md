---
name: writing
description: Draft or edit prose the reader will keep (README and docs pages, ADRs, design notes, commit messages, PR descriptions, emails, posts) so it reads like an expert human wrote it, not a chatbot. Also use when asked to fix text that sounds AI-written. Chat replies are covered by the system prompt's voice rules; this skill is for kept prose.
---

# Writing: prose that reads like a person wrote it

Rewrite or draft text so a careful reader can't point at a line and say "a model wrote this." Keep what the text says. Never make anything up.

## How to work

1. Read the whole text once and mark the tells below, strongest first. Look at shape (paragraphs, lists, closers), not only at sentences: a contrast split across two sentences, three parallel examples, and the same closer after every section are the same tells at a larger scale.
2. Rewrite. Keep every supported claim; cut what only adds weight. You may shorten, merge, split, and reorder. Do not add a fact, name, number, date, quote, or citation that is not in the source or the brief. If a sentence needs a detail you don't have, write a simpler sentence. An opinion is allowed when the voice calls for one; a factual claim is not.
3. Read the draft aloud (mentally is fine). Whatever you stumble on is still wrong; rewrite the paragraph around its point rather than patching the phrase.
4. Return the final text. When editing a file, change prose only: code blocks, commands, paths, inline code, and link targets stay untouched.

## Voice sample

A project may define a voice in `.gertie/voice.md` at the workspace root. When it exists, read it first and match its sentence length, word choice, punctuation, and openness about uncertainty. The sample overrides the rules below wherever they conflict, including the dash rule.

A copy of the user-level sample may also sit beside this file as `voice.md`; load it with `load_skill` (`path: "voice.md"`) when no project sample exists.

## The tells

Strongest first. §1–§4 justify an edit on one sighting; a pattern marked *weak alone* needs company from other tells before you act. The list watches for defaults a careful writer wouldn't make on purpose, not words that are always wrong.

### 1. Not X, but Y

Contrasts that name an objection no one made: "It's not just X, it's Y", "not merely", "X rather than Y", the split form ("This does not mean X. It means Y."), the clipped tail ("…, no guessing"). State the point; keep a contrast only when the negative half corrects a belief the reader actually holds.

> Before: It's not just a theme picker — it's a statement about how your tool respects you.
> After: The theme picker uses the system setting by default.

### 2. One-line closers and dramatic fragments

A one-sentence paragraph that restates the one before it ("That is the real win."), a line that names what an example just showed ("The message was clear:"), rows of fragments ("No aesthetic prior. No nostalgia."), ALL CAPS or every. single. word. Cut a closer that repeats. One short sentence may stand when it carries a new fact.

### 3. Staged run-ups and chatbot residue

"Let's dive in", "Here's what you need to know", "Honestly?", "Great question!", "Certainly!", "I hope this helps", "Let me know if…", "Would you like me to…". Remove the wrapper; keep the content. Nothing that survives should sound like it came with a chat bubble attached.

### 4. Arguing with no one

"This isn't mainly about X", "I'm not saying…", "A tempting approach would be…", "Some might say… but". Defense of an option that appears nowhere else is a leftover from an earlier draft. If the objection holds a real claim, state the claim.

### 5. Forced triads

Ideas arriving in threes to sound complete ("innovation, inspiration, and industry insights"). Each item must add a distinct idea; otherwise merge, or develop the strongest one. Keep three when the meaning needs three.

### 6. Dashes as the universal connector

Em dashes in every paragraph, usually in pairs, letting the writer skip choosing how two clauses relate. At most one per paragraph, never consecutive; prefer a period, comma, colon, or parentheses. *Weak alone*: many good writers use dashes; a text full of them is not a coincidence. Dashes inside code, commands, paths, and ranges are punctuation, not prose; leave them.

### 7. Inflation

Ordinary facts dressed as pivotal: "stands as a testament", "plays a crucial role", "underscores its importance", "evolving landscape", "rich heritage", "nestled", "groundbreaking". Shallow `-ing` riders bolt a significance claim onto a plain fact ("…, highlighting the enduring legacy of…"). Borrowed authority ("experts argue", "profiled in Forbes and Wired", "active social media presence") props up claims the source doesn't carry. Keep the fact, drop the dressing; end on the last concrete thing, not a send-off about the future.

### 8. Costume verbs and stacked qualifiers

"Serves as", "stands as", "boasts", "features", "offers" doing the work of is and has. Qualifiers stacked until every claim apologizes ("could potentially possibly affect"). Use the plain verb; keep a qualifier only when the doubt is real. *Weak alone* each; a page of both is not.

### 9. Formatting by rule

Bold labels on every list item, title case in headings, emoji as bullets, a horizontal rule between every section, a top-level heading repeating the document title. Decoration on every item is the tell; turn labeled lists into prose when the labels carry no information. Keep headings in sentence case and earn any bold.

### 10. Writing for the wrong reader

A reply that rebuilds the background, walks the diagnosis, and buries the decision in the last line. The reader of a reply already has the context. Lead with the decision; keep the one or two facts that would change whether they agree. The full write-up belongs in the doc or ticket that follows.

### 11. Writing about the writing

"The table below compares…", "this section is organized by…", "anything unconfirmed is flagged rather than guessed", "was added to replace the previous approach of…". The text describes itself instead of its subject. Say the thing once, where the reader needs it. *Weak alone.*

## When not to act

Every pattern is a default, and a person can choose it on purpose. Leave phrases alone inside quotations, titles, proper names, and passages that discuss the phrase rather than use it. Salutations and sign-offs on real correspondence predate chatbots. Keep what carries voice: a specific odd detail, mixed feelings, a dated in-joke, a genuine aside. Removing tells is half the job; the result must still sound like a person.

## Credit

Adapted from [blader/humanizer](https://github.com/blader/humanizer) (MIT) and Wikipedia's ["Signs of AI writing"](https://en.wikipedia.org/wiki/Wikipedia:Signs_of_AI_writing) (WikiProject AI Cleanup).
