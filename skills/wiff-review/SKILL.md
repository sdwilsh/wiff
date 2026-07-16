---
name: wiff-review
description: Read and annotate a wiff code-review session from the command line. Use when asked to review a diff and leave comments in wiff, or to read and address the comments already left in a wiff review.
---

# Wiff Review

wiff is a terminal-first code-review tool. A review lives in a local session that a
human browses in the wiff TUI while an agent reads and writes the same session
through the `wiff` command line. The TUI belongs to the human: never launch or
drive it. Do all of your work through the `wiff` subcommands below.

If there is no session to act on, ask the user to start one with wiff first.

## Selecting the session

Every command acts on the active session for the current project by default, so
run wiff from inside the checkout being reviewed and you can usually omit any
session flag. When the active session is not the one you want, or the project
cannot be derived from the working directory, select it explicitly:

```bash
wiff session list
wiff render --session 01J8ZC0FEXAMPLEULID26
wiff comment add --agent --session 01J8ZC0FEXAMPLEULID26 --review --body "..."
```

- `wiff session list` prints the sessions for this project and their ULIDs.
- `--session <ULID>` targets a specific session instead of the active one.
- `--project <name>` forces the project when the working directory cannot name
  it on its own.

## Reading a review

Start here when asked to read a review or to address the comments it holds.

```bash
wiff render
wiff render --format json
wiff comment list
```

- `wiff render` prints the review as markdown: comments grouped by file, each
  led by its id and showing its author and kind, target location, resolved or
  outdated state, body, and a fenced snippet of the surrounding code. This is
  the one command you need to read the review and to pick up the ids you act on
  below.
- `wiff render --format json` prints the same folded state as JSON for
  programmatic use. Each comment reports `updated_seq` and `updated_at`. To order
  changes or find the most recent one, use `updated_seq`, which always advances;
  `updated_at` is a display timestamp and, for a comment imported from a forge,
  can predate an earlier change.
- `wiff comment list` is an optional compact form: one comment per line, id
  first, with its status and location, when you want a terse pass without the
  bodies and snippets.

When you finish addressing a comment, resolve it so the human sees it is done.
Pass `--agent` here too, so the resolution is attributed to you rather than the
human:

```bash
wiff comment resolve --agent 01J8ZC0FEXAMPLECOMMENT7
```

## Leaving review comments

Use these when asked to review a change and record your findings. Always pass
`--agent` on every command that writes to the review, so your comments,
resolutions, and withdrawals are attributed to you rather than the human.

```bash
wiff comment add --agent --file src/lib.rs --line 42 --body "This can overflow."
wiff comment add --agent --file src/lib.rs --line 10-14 --body "Extract this loop."
wiff comment add --agent --file src/lib.rs --line 42 --side before --body "..."
wiff comment add --agent --file src/lib.rs --body "This module needs tests."
wiff comment add --agent --review --body "Overall the change reads well."
```

- `--file F --line N` comments on a single line; `--line N-M` on an inclusive
  range. Line numbers are 1-based.
- `--side after` (the default) refers to the post-change content; `--side
  before` refers to the pre-change content.
- `--file F` with no `--line` comments on the whole file; `--review` comments on
  the change overall.
- Provide the body with `--body`, or pipe it on stdin for anything long or
  multi-line:

```bash
printf '%s\n' 'First point.' 'Second point.' | wiff comment add --agent --file src/lib.rs --line 42
```

To revise your own comments:

```bash
wiff comment list
wiff comment resolve --agent 01J8ZC0FEXAMPLECOMMENT7
wiff comment resolve --agent --reopen 01J8ZC0FEXAMPLECOMMENT7
wiff comment rm --agent 01J8ZC0FEXAMPLECOMMENT7
```

- `wiff comment resolve <id>` marks a comment resolved; `--reopen` undoes that.
- `wiff comment rm <id>` withdraws a comment.

## Guidelines

- Read the change before commenting. `wiff render` gives you each comment with
  its surrounding code; read the diff itself from the checkout as needed.
- Comment where it matters: intent, correctness, risks, and follow-ups. Do not
  leave a note on every hunk; highlight what the human would not spot alone.
- Anchor each comment on the most specific target you can, a line or a range,
  and fall back to a whole-file or review comment only for points that have no
  single home.
- Quote comment bodies in the shell so punctuation is not mangled, or pipe them
  on stdin.
