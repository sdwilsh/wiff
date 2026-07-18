# wiff

> sniff out the wiff in your diff, from the comfort of your terminal

wiff is a terminal-first diff and code-review tool. It captures a diff, lets you
browse and annotate it with syntax highlighting, and stores the review as a
local session that both a human (in the TUI) and an agent (via the CLI and a
skill) can read and write at the same time.

## Install

Build from source with a recent Rust toolchain:

```bash
cargo install --path crates/wiff
```

This puts the `wiff` binary on your PATH.

## Reviewing with an agent

wiff ships an agent skill that teaches your agent to read and annotate a review
through the `wiff` CLI. The TUI stays yours; the agent works only through the
command line on the same session.

Install the skill and get its path with:

```bash
wiff skill-path
```

This expands the bundled skill into your data directory (rewriting each `wiff`
call to the absolute path of your binary) and prints the path to its `SKILL.md`.
Point your agent at that file, or copy the `skills/wiff-review/` directory into
wherever your agent looks for skills. Once installed, ask your agent to review
the diff or address the existing comments; it discovers the active session for
the checkout on its own.

## Usage

Start a review from a diff source and open the TUI:

```bash
wiff new                 # the working tree
wiff new --cached        # the staged index
wiff new --rev HEAD      # the changes a revision introduces
git diff | wiff new      # a unified diff piped in
```

### Navigation

Bindings follow `less`, with review actions layered on top:

- `j` / `k` or arrows to scroll a line, `space` / `b` to page.
- `g` / `G` to jump to the top (the review summary) or bottom.
- `,` / `.` previous/next file, `[` / `]` previous/next hunk, `{` / `}`
  previous/next comment.
- `t` file picker, `C` comment picker, `T` theme picker.
- `/` and `?` search forward/backward, `n` / `N` to repeat.
- `enter` toggles a fold, `tab` toggles a comment, `H` hides all comments,
  `w` toggles line wrapping.

### Views

Switch the diff layout live (or set `diff_mode` in config):

- `1` unified: one column with added and removed lines interleaved.
- `2` side-by-side: before on the left, after on the right.
- `3` after-side only: the resulting file in a single column.
- `4` rendered: the after side through a type-specific renderer (markdown
  today), falling back to the after-side source when there is no renderer.
- `0` auto: side-by-side when the terminal is wide enough, unified otherwise.

### Comments

- `v` starts a linewise selection; extend it with the navigation keys.
- `c` adds a comment on the selection, or on the cursor line with no selection.
- On a comment, `r` replies, `e` edits, `x` resolves, `d` withdraws, `a` sets a
  verdict.
- In the editor, `ctrl-d` moves the body into your drafts and `esc` cancels.

Edits are buffered as drafts and marked as such. `ctrl-s` saves them to the
session; you are also offered to save on exit.

## Session lifetime

A session outlives a single sitting so you and your agent can iterate on it.

- **Create** a review to begin: `wiff new`.
- **Iterate** until done: reopen with `wiff resume`, capture new changes into
  the session with `wiff refresh` (or `ctrl-r` in the TUI), which rebases your
  comments forward onto the new diff.
- **Remove** it when finished: `wiff session rm`, or choose Remove session on
  exit.

List sessions any time with `wiff session list`.
