---
description: Write a commit message from the working tree's real changes, in this repo's own voice.
---
Write a commit message for the current changes, then show me the exact command to make the commit.

Read the repo's conventions first: run `git log --format=%s -n 20` and imitate the subjects' language, tense, and shape, a repo's history is its style guide. When the history disagrees with itself, follow the majority and match the newest entries.

Look at what actually changed: `git status` first, then `git diff` for unstaged work and `git diff --cached` for staged. The message says what the change does and why it's right, no narration of our conversation, no "I".

Then show me the message and the exact `git commit` command. Don't run the commit; the commit is mine to make.

{{args}}
