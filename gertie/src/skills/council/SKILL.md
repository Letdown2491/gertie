---
name: council
description: Trigger this when the user says "council" or asks for a panel of independent reviews.
---

# Council

Before summoning: address outstanding review comments and known issues, and
run the relevant checks. Council reviews the updated state of a change,
never a known-broken one. If the user wants to discard the implementation
and explore new ideas, skip fixes to code being replaced.

## Seats

The seats are configured in `[[agents]]` in config.toml: custom agent types
named `council-*`, each pinned to its own `endpoint` (its own provider or
wire). They appear in the tool list as `agent_council_*`. Write one brief
file per member and dispatch every seat in the same turn — they run one
after another; that is fine. Give each seat the same brief.

If fewer than three `agent_council_*` tools exist, or a seat failed to
load (a startup warning said its endpoint is not configured), say so and
stop. Never substitute another model or your own judgment for a missing
seat — a council of substitutes is just you, three times.

## The brief

Review only, read-only. One brief per round, self-contained:

```
Review only. Do not edit, commit, push, or comment on PRs.
Repo: <abs path>  Base: <sha>
Intent: <one sentence>  Out of scope: <paths>
See: git diff <base> [-- <paths>]
Read the surrounding code. Find bugs, broken callers, and mismatch with
intent. Skip style unless it hides a bug.
Severity: blocker = the intended case is wrong; should = a real bug
callers will hit; nit = the rest.
Reply:
SCORE: n/10
VERDICT: APPROVE or CHANGES
<id> [blocker|should|nit] <file>:<line> <claim>. Fix: <suggestion>
```

## Rounds

Count each full pass as one round; a fresh review after fixes starts a new
round. Default budget: 3 rounds. Extend to 5 only for security, auth, data
loss, or concurrency issues with evidence, or when the user asks. Later
rounds check only changed code and open findings; stop early if a round
returns only nits.

Check each finding yourself before acting:

- Agree: make the smallest fix — prefer deleting or narrowing over new
  locks, caches, helpers, or options. One focused test that fails without
  the fix, if the intent needs it.
- Disagree: say why, with evidence (file:line, test, output).
- Nit or out of scope: decline in one line; list as follow-up.

Do not agree just to end the loop. Do not let repeated rounds become scope
creep — it is your job to keep scope limited. Budget end is not approval.

## Reporting

Summarize agreements, disagreements, and recommended next steps, carrying
each seat's scores forward:

| Seat | Round 1 | Round 2 |
| --- | --- | --- |
| council-anthropic | 7/10 | 9/10 |

If a seat gave no score, write `no score` — do not invent one. When the
council is satisfied, make the changes and report: rounds, findings fixed
and declined with reasons, and what changed.
