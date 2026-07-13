# wiff design spec

> sniff out the wiff in your diff, from the comfort of your terminal

wiff is a terminal-centric diff and code-review utility. It captures a diff,
lets you browse and annotate it with syntax highlighting, and persists the
review as a local session that both a human (via the TUI) and an agent (via
CLI and a skill) can read and write concurrently.

This document is the v0 design reference. It also records the shape of features
deferred past v0 so the v0 architecture leaves room for them.

## Goals

- Keyboard-first review of a unified diff in the terminal.
- Comment on lines, line ranges, whole files, and the review overall.
- Comments attributed to a named author that is either a human or an agent.
- Sessions persist locally and survive restarts; multiple processes can read
  and append concurrently.
- Comments rebase forward as the underlying diff is regenerated.
- An agent can discover the session for a directory, read the review state,
  and add comments, guided by a skill file.

## Non-goals for v0

- Side-by-side diff view (unified only in v0; the view is designed to be
  selectable later).
- Suggested code-change blocks (plain-text comments only in v0).
- Auto-reloading a new diff capture from another process while the TUI is open
  (a new diff version still needs a manual refresh in v0; another actor's
  committed comment edits are picked up on their own).
- Forge integration (git-pkgs/forge), jj source, whitespace-ignoring diffs.

## Terminology

- **Source**: where a diff comes from (git, piped unified diff; jj and forges
  later). Abstracted behind a trait so new sources do not ripple through the
  code.
- **Session**: a persisted review. Owns an ordered list of diff versions and a
  log of annotations. Identified by a ULID, bucketed under a project.
- **Diff version**: one captured snapshot of the source's diff, with enough
  file content to highlight and to rebase comments against.
- **Annotation**: a comment. Anchored to a line range, a whole file, or the
  review overall. Attributed to an author.
- **Project**: the bucket a session belongs to, derived from the repository
  root (see Identity).

## Storage model

Modeled on wallah's session log: an append-only JSON-lines file, lock-free
reads, and an exclusive `flock` held only for the duration of appends.

### Layout

```
$XDG_DATA_HOME/wiff/                 (e.g. ~/.local/share/wiff)
  sessions/
    <project>/
      <ULID>.jsonl                   the append-only record log
      <ULID>.d/                      sideband files for that session
        v0.diff                      raw captured unified diff for version 0
        v1.diff
        ...
```

- One directory per project; enumerating a project's sessions is a directory
  listing. Enumerating all sessions is a listing of every project directory.
- The **active session** for a project is the most recently modified `.jsonl`.
- `WIFF_DATA_DIR` overrides the base directory (for tests and for an agent's
  discovery); the core also exposes `*_in(base, ...)` seams for tests.
- The captured diff text for each version lives in the sideband `.d/` directory
  (`vN.diff`), not inline in the JSONL, so the log stays small and cheap to
  scan. The `DiffVersion` record holds parsed metadata and references the
  sideband file, which is content-addressed (blake3) for integrity.

### Concurrency

- Reads go by path and are lock-free, so a reader always sees a consistent
  prefix of whole lines even while a writer holds the lock.
- Appends require an exclusive, non-blocking `flock`. A writer that finds the
  file advanced past its in-memory position reports divergence rather than
  duplicating a `seq`.
- Each record's `seq` is its 0-based line position and its stable id within the
  session. External references are `(session-ulid, seq)`.
- A header record is written first, under the lock, at creation.
- While the review sits idle the TUI periodically stat-checks the session file (a
  cheap size and mtime check, no inotify) and, when another actor has appended,
  folds the new committed comments in without disturbing the reviewer's cursor,
  folds, or pending drafts, noting in the status line what was added, updated, or
  removed. The reload waits while a comment edit, search prompt, or exit dialog
  is open and resumes when the reviewer returns to the plain view. A capture of a
  new diff version is left for a refresh the reviewer triggers by hand; on
  reopening a session whose source has moved on since the last capture, the TUI
  offers to refresh from the launch prompt (see Version comparison and refresh).

### Record schema

Every line is a `Record { seq, at, body }` where `body` is a tagged enum. v0
bodies:

- `Session` (header): format version, project canonical name and aliases, the
  originating cwd, and the source descriptor (how to regenerate: e.g. `git
  diff`, `git diff --cached`, or `stdin` which is not regenerable).
- `DiffVersion`: a monotonically numbered snapshot. Stores the parsed diff: the
  file set and, per file, its status and hunk structure (each hunk's `@@`
  ranges and its tagged lines: context, added, removed). The raw diff text is
  written to the sideband `vN.diff` and referenced by content hash. The diff
  itself is the source of truth (not separately captured file blobs), so any
  source that can produce a unified diff integrates uniformly; full before/after
  blobs are not assumed to be available. Numbered `v0`, `v1`, ... matching the
  sideband file.
- `Comment`: a new annotation. Fields: an annotation ULID (stable identity
  across edits), author, target (see Anchoring), the diff version it was
  authored against, the anchored snippet and its surrounding context lines
  (for rebasing), and the body text.
- `CommentEdit`: revises a prior comment's body, referencing its annotation
  ULID.
- `CommentResolve` / `CommentDelete`: marks a comment resolved or withdrawn,
  attributed to the author who made the change so a reviewer can see who
  resolved or removed it. Deletes are tombstones, not physical removal
  (append-only).

Comment mutations are append-only events keyed by annotation ULID; the current
state of a comment is the fold of its event chain, mirroring how wallah folds
config records.

Unknown/reserved record types are skipped on read so the format can grow.

## Diff sources

A `DiffSource` trait produces a unified diff (with as much context as the source
can give). v0 implementations:

- **git**: runs `git diff`, `git diff --cached`, or the diff a named revision
  introduces (like `git show REF`), requesting expanded context (a large `-U`)
  so hunks carry as much of each file as possible for better highlighting and
  more reliable rebasing, while the diff stays the single artifact. Regenerable,
  so `wiff refresh` can capture a new `DiffVersion`.
- **stdin unified diff**: parses a unified diff piped in, with whatever context
  it happens to carry. Not regenerable; such a session is a one-shot snapshot.

The diff parsing and hunk model are owned by wiff, not delegated to git
plumbing, so all sources are handled uniformly.

### Content reconstitution

Several features need the content of one side of a file within a diff version:
rebasing diffs old against new content, highlighting, and opening a file in an
editor. wiff reconstructs a `(file, side)` side's line content by walking that
file's hunks and emitting context and same-side lines in order. The diff is the
source of truth, so a reconstruction is only as complete as the captured
context: with git's expanded context it is typically the whole file; with a
sparse piped diff there are gaps between hunks.

Gaps are handled two ways. When the source can supply the real current file (git
working tree for the after side), wiff prefers that live content, so the
reconstruction is complete and faithful. Otherwise gaps are marked with a
visible placeholder line noting the omitted region, so a partial reconstruction
never masquerades as the full file. This single reconstruction path backs
rebasing, highlighting, and editor materialization.

## Comment anchoring and rebasing

A session accretes diff versions over time. Comments are **not** duplicated per
version: each comment moves forward onto the latest version, or becomes
outdated.

### Anchoring

A comment target is one of:

- **Line range**: `{ file, side (before|after), start_line, end_line }`.
- **File**: a whole-file comment on a named file.
- **Review**: a comment on the review overall, anchored to no particular file.

When a line-range comment is created, wiff captures the exact text of the
anchored lines plus a window of surrounding context lines (a fixed few lines
each side; the enclosing hunk bounds it), and records which diff version and
which side (before/after) it was authored against.

### Rebasing engine

wiff owns the rebaser; it never assumes git and never shells out to `patch`.
It depends on the `similar` crate for line diffing (the `mpatch` crate solves
an adjacent problem and is a useful reference, but is a file-writing patch
applicator and is not a dependency). On `wiff refresh` (and later on
`--watch`), for each comment anchored to an older version, the primary strategy
is offset mapping and the fallback is a patch-style fuzzy search:

1. Reconstruct, per `(file, side)`, the line content captured in the old and
   new versions. A comment on an added line rebases against after-content; a
   comment on a removed or context line rebases against before-content.
2. Diff old vs new content with `similar`, yielding an old->new line mapping.
3. Classify the comment's anchor by where its lines fall:
   - within an **equal** run -> move forward to the mapped lines. Tier: exact.
   - within a **changed** run -> the reviewed code itself changed. Tier:
     outdated (this is the desired signal, not a failure). Pinned to a
     best-guess location.
4. Fallback when step 3 is inconclusive (e.g. a rename, or the anchor is not in
   an equal run but its snippet appears elsewhere): search the new content for
   the captured snippet plus context, progressively trimming outer context
   lines in the spirit of `patch` fuzz, accepting the best match at or above a
   `diff_ratio` similarity threshold. Tier: approximate (flagged as shifted).
5. No acceptable match -> outdated, retained (never silently dropped), pinned
   to its last known location.

Confidence tiers are **exact**, **approximate**, and **outdated**. The TUI and
renderers show approximate and outdated comments distinctly. Re-anchoring is
recorded as an append-only event so history is preserved.

## Identity and discovery

- A session's **project** is derived from the repository root containing the
  cwd. This keys the bucket and lets an agent find the right session for a
  directory in one step. The repo root is stored in the header.
- For piped input, wiff still tries to derive the repo root from the cwd; if
  none is found it bails, unless the user forces a project name with a CLI flag
  (`--project <name>`).
- Discovery for the cwd: resolve the project, list its sessions, pick the
  active (most-recently-modified) one unless a specific session is named.

## Author identity

- Author is `{ name, kind: human | agent }`.
- Default name is `$USER` with kind `human`.
- Agents set their identity explicitly via CLI flags (e.g. `--author <name>
  --author-kind agent`), with a suggested default name of `assistant`.
- The kind lets the TUI and renderers distinguish human and agent annotations.

## CLI surface

The binary is `wiff`. There is no bare-invocation review mode: every action is
an explicit subcommand, so behavior is unambiguous. Subcommands (v0):

- `wiff new`: create a session from a source and launch the TUI. Source flags
  select the diff: `--cached` for the staged index, `--rev <REF>` for the
  changes a revision introduces, `--head` as sugar for `--rev HEAD`, or a
  unified diff read from stdin when piped. `--no-tui` creates the session
  without launching the TUI. `--project <name>` forces the project bucket when
  it cannot be derived from the cwd.
- `wiff resume`: resume a session (the active one by default, or `--session`)
  and launch the TUI.
- `wiff session list`: list sessions (optionally across all projects).
- `wiff session rm`: remove a session (deletes its `.jsonl` and `.d/`).
- `wiff refresh`: capture a new diff version into a session and rebase comments.
- `wiff comment add`: append a comment. Flags for target (`--file`, `--line`,
  line range, whole-file, review-level), `--body` (or stdin), author flags, and
  an optional `--session` that defaults to the active session.
- `wiff comment list` / `wiff comment resolve` / `wiff comment rm`. Resolve and
  rm take the same author flags as `add`, so who resolved or withdrew a comment
  is recorded alongside the change.
- `wiff render`: emit the review state for consumption. `--format markdown`
  (default) or `--format json`. The format argument is designed to admit more
  formats later, so it is a value-taking option rather than a boolean flag.

`--session` defaults to the active session throughout. All mutating commands go
through the append + lock path.

### Session lifecycle

- `wiff new` always creates a fresh session; `wiff resume` never creates one.
  This keeps the two intents separate and unambiguous.
- Exiting the TUI resolves to keep or remove the session. The default comes
  from config (`on_exit = "prompt" | "keep" | "remove"`, default `prompt`) and
  can be overridden per run by CLI flags (`--keep` / `--remove`).
- The action model has explicit `quit_keep` and `quit_remove` actions in
  addition to a `quit` that honors the configured default (prompting when set
  to prompt). Removing deletes the `.jsonl` and `.d/`.

## Agent integration

- `wiff render --format {markdown,json}` produces the review state for an
  agent prompt (markdown) or programmatic use (json). Markdown groups comments
  by file, leading each comment with its id (so it can be resolved or withdrawn
  straight from the render) and showing author and kind, the target location,
  resolved/outdated state (naming who resolved it), the body, and a fenced code
  block of the surrounding context. JSON is
  the folded current state (not the raw event log) under a versioned schema
  (`{ schema_version, session, files, comments }`); the raw log
  remains available by reading the JSONL directly.
- `wiff comment add` lets an agent contribute comments, setting its author name
  and `--author-kind agent`.
- `wiff skill-path` writes the bundled agent skill into the data directory and
  prints the path to its `SKILL.md`. The skill documents, for an agent: how to
  select the session for the cwd, how to read a review with `wiff render`, and
  how to leave and revise comments with `wiff comment add` and its siblings. It
  is checked into the repository at `skills/wiff-review/` as a directory that
  already works as a skill for anyone with `wiff` on their PATH; installation
  rewrites each `wiff` invocation to the running binary's absolute path.

## TUI

Built on ratatui. Priorities: readable and maintainable, minimal boilerplate,
no thousand-line nested match driving the UI.

### Action model

Input is decoded into an **action** (an enum of intents: page up/down, next/prev
file, next/prev hunk, next/prev comment, toggle fold, toggle comment, hide
comments, toggle
wrap, pick file, pick comment, pick theme, add comment, edit comment, resolve
comment, delete comment, submit comment, cancel
comment, save, search forward/backward, search next/prev, refresh,
`open_in_editor`, `quit`, `quit_keep`, `quit_remove`, etc.). Nothing in the UI
logic branches on raw keys; it branches on actions. This keeps bindings
reassignable and keeps the update logic small.

### Keymap

- Config maps **action -> one or more key chords** (multiple bindings per action
  allowed).
- A chord is a sequence of key presses; v0 needs single keys and ctrl/alt/shift
  modified keys, but the schema is a sequence so leader-key and multi-key chords
  can be added without a format change.
- Defaults resemble `less` for navigation (space, b, g, G, q, ...) plus
  review actions layered on top. Sequence navigation follows a paired scheme:
  `,`/`.` prev/next file, `[`/`]` prev/next hunk, `{`/`}` prev/next comment.
  Search follows `less`: `/` and `?` open the prompt forward and backward, and
  `n`/`N` repeat in the same or the opposite direction. The review summary sits
  at the top of the document, so the existing top jump (`g`, and `<` as a
  `less`-style alias) reaches it; there is no separate jump-to-review action.
  `t` opens the file picker, `C` the comment picker, and `T` the theme picker
  (see below). `V` hides and shows all comments. `v` opens the compare-versions
  picker and `R` refreshes (see Version comparison and refresh).

### Modal pickers

A reusable modal list floats centered over the review for choosing one item from
many. It shows the items one per line with one highlighted, moved with the same
navigation bindings as the main view, and enter activates the highlight while
escape cancels. A list taller than the space it is given scrolls to keep the
highlight in view. The widget is generic over what a row does when chosen, so
the same modal serves several pickers. `pick_file` (`t`) lists the diff's files
and jumps the cursor to the chosen file's header; `pick_comment` (`C`) lists the
diff's comments grouped by status -- draft, then open, resolved, and withdrawn,
each group kept in document order -- with a status marker, its location, its
author, and the start of its body, and jumps to the chosen comment; `pick_theme`
(`T`) lists the built-in color themes,
opening on the one in effect, and recolors the whole view to the chosen theme;
`compare_versions` (`v`) lists the captured versions to view the change against
(see Version comparison and refresh).

### Version comparison and refresh

The review normally shows the latest captured diff. The compare-versions picker
(`v`) views the change since an earlier captured version instead: the right side
stays the latest, the left is the chosen version, so the reviewer sees only what
moved since then. The list opens on the version in effect, and choosing the
latest returns to the full diff.

Refresh (`R`) recaptures the source into a new version and rebases comments and
drafts forward (see Comments & anchoring). It keeps the reviewer on the version
they were viewing and then opens the compare-versions picker, opening on where
they were and marking where they last committed comments, so they choose how to
see what changed. Cancelling keeps their prior perspective against the fresh
capture, which stays valid since the comparison's right side is always the
latest.

On reopening a session whose source has moved on since the last capture, the TUI
opens over the existing review and asks whether to refresh, rather than silently
showing a stale diff. This launch prompt is skipped for diffs piped on stdin,
which cannot be recaptured once the TUI owns the terminal.

### Rendering

- Unified diff with syntect syntax highlighting of the file content, over as
  much context as the diff carries (expanded for git, so highlighting has
  plenty to work with).
- The whole palette is derived from the chosen syntect theme, so a theme colors
  both the file content and the interface around it. The background, text, and
  selection are taken from the theme; the gutter and other structural text are
  dimmed out of the text color and lifted to a legible contrast against the
  background; the added and removed tints blend a fixed green and red into the
  background; and the accents (added green, removed red, review gold, warning
  orange) keep a fixed hue with only their lightness adapted, so their meaning
  stays constant across themes. The view is painted with the theme background so
  a light or dark theme reads coherently over whatever background the terminal
  itself uses.
- A syntect foreground is chosen to read on the theme background, so wherever a
  glyph is painted over a different background -- an added or removed tint, or
  the cursor wash -- its foreground is re-checked and lifted back to the
  contrast it had on the plain background when the new background would dim it.
  A glyph the theme keeps dim on purpose, such as a comment, stays dim: the
  target never exceeds the contrast it had on the plain background.
- The view type (unified now; side-by-side later) and options like
  ignore-whitespace are designed to be user-selectable, though only unified
  ships in v0.
- Diff content lines soft-wrap to the viewport width by default. The
  `toggle_wrap` action, and the `wrap_lines` config default it starts from,
  switch to clipping them at the edge instead. Wrapping reuses the same
  column-wrapper that wraps comment bodies: a wrapped line keeps its gutter on
  the first row and indents each continuation under the code, reflowing when the
  terminal is resized.

### Search

Search is incremental and modeled on `less`. `/` and `?` open a prompt in the
status line and scan forward or backward; the cursor jumps to the first match as
the pattern is typed, and the pattern is a regular expression, matched with smart
case (case-insensitive unless it contains an uppercase letter). A pattern that is
not yet valid regex syntax, as a half-typed one often is, matches nothing and is
flagged as a bad pattern in the tally until it is well formed. Accepting the
prompt keeps the cursor on the match and arms `n`/`N` to repeat in the same or
opposite direction; abandoning it returns the cursor to where the search opened.
While a search is live the status line shows the term, an `X/Y matches` tally
naming which match the cursor is on out of the total, and, once accepted, the
`n`/`N` keys that step through the matches. The progress percent stays
right-aligned in the status line throughout, so the reviewer keeps that bearing
while searching. When a repeat wraps past an end of the document it adds a wrap
note next to the tally rather than replacing the position, so the wrap is
reported without wiping the term, tally, or percent. Every occurrence of the
term in view is washed in a match color, not just the one the cursor is on, so
the reviewer can see at a glance where else it appears on screen.

Matching runs over the semantic text of each row, not the rendered line, so the
gutter line numbers and change markers never produce spurious hits. File paths,
content lines, and comment authors and bodies are searchable; hunk headers and
box edges are not. A line hidden inside a collapsed fold is skipped, matching the
rule that folded context is out of view. A collapsed comment is still searched,
and a match inside one expands the comment so the matched line becomes visible.

### Comments

Comments render inline as a block immediately above the line they anchor (the
GitHub / `hunk` convention), so a reviewer reads each comment next to the code it
is about. Unlike the `wiff render` markdown output, the TUI does not echo the
anchored snippet: the real code sits directly below.

Anchoring by target:

- **Line range**: the block sits above `start_line`; the anchored line span is
  marked in the margin across `start..=end`.
- **File**: the block sits under the file header.
- **Review**: a review summary row is always present at the top of the document,
  even with no review comment yet, so it is a stable target for a review-level
  comment and the destination of the top jump. Review comments render there.

Several comments on one line stack in creation order. Each comment is
independently collapsible. A collapsed comment occupies a single line showing a
marker, the author (name and kind), and status badges naming who resolved or
withdrew it; metadata only, no body preview. Expanding adds the body. Resolved comments default to collapsed.
Collapse state is per-process view state keyed by annotation ULID, not persisted
across runs. The `hide_comments` action drops every comment from the view,
leaving only the code, so several rounds of annotation do not crowd out the diff;
it is a single per-process flag independent of the per-comment collapse state.

Confidence is shown distinctly: an approximate re-anchor carries a `shifted`
badge and an outdated one an `outdated` badge. An outdated comment whose anchored
line still exists renders on that line; when the line is gone entirely (rename or
deletion), it floats up to the file header block so it stays near its file.

A comment anchored inside a run that would otherwise fold splits the fold rather
than expanding it: the anchored line is kept like a change, with `display_context`
lines of surrounding context, and the rest of the run stays folded. This keeps a
comment always visible with its code without unfolding a potentially huge region.

Comment bodies are plain wrapped text in v0. A body line wider than the comment
box soft-wraps to fit inside it, so a long line an agent writes on one physical
row spreads across several rows rather than being clipped; the wrap follows the
viewport, reflowing when the terminal is resized. The wrapping is a shared
column-wrapper that breaks at spaces and hard-splits an overlong word while
preserving span styling, and the same wrapper wraps syntax-highlighted diff
lines when `toggle_wrap` is on. The body-to-lines step is isolated so a markdown block
renderer can replace it later; the collapse model (a header line plus body lines)
already accommodates a body that renders as several lines.

### Authoring and drafts

Editing in the TUI is buffered. Adding, editing, resolving, and deleting are held
in memory as **drafts** against the session and are not written until the review
is committed. Drafts render distinctly (a `draft` badge) so pending work is
obvious. Reviews are not heavily concurrent (typically one human and sometimes
one agent), so buffering the whole review and flushing on commit is acceptable;
a crash loses only uncommitted drafts, like an editor's unsaved buffer.

The inline comment editor is a separate input mode: while it is open the app
hands most presses to the text buffer and honors only two actions,
`submit_comment` (default `ctrl-d`) to move the typed body into the drafts and
`cancel_comment` (default `esc`) to abandon it, confirming first when the body
has changed. These are ordinary keymap actions, so they route through config
like the rest. Submitting into the drafts is deliberately a different key and
concept from `save`, which flushes the whole draft buffer to disk: an accidental
repeat of the editor's confirm key cannot cross the draft-to-committed boundary.

Committing flushes the pending drafts as append events. The `save` action does
this while leaving the review open; leaving via the Commit choice does it on the
way out (see Exit behavior). Save is also the natural flush point a future
`--watch` / auto-refresh mode would use.

A draft holds the same anchor data a committed comment does (snippet, surrounding
context, authored-against version and side), so `refresh` rebases drafts forward
onto a new diff version alongside committed comments. A draft never authored
against a persisted version still carries its anchor, so it is not stranded when
the review advances.

### Exit behavior

Quitting with pending drafts opens a dialog with three choices:

- **Commit review**: flush the drafts as append events, keep the session.
- **Quit without saving**: discard the drafts, keep the session.
- **Remove session**: discard the drafts and remove the session.

The `on_exit` config (and any `--keep` / `--remove` override) sets the default
choice: `keep` defaults to Commit, `remove` to Remove, `prompt` leaves no default
and always shows the dialog. With no pending drafts there is nothing to lose, so
the dialog is skipped and `on_exit` (with overrides) decides keep-or-remove
directly. `quit_keep` and `quit_remove` actions still choose explicitly.

## Opening files in an editor

The `open_in_editor` action hands the focused file to the user's editor so they
can explore it with a familiar interface, then returns to the TUI. It targets
the after side by default (the current state of the code).

The file the editor opens is a reconstruction of the after side (see Content
reconstitution). For a git source with the file present in the working tree this
is effectively that file; for other sources it is rebuilt from the diff, with
placeholders for any gaps. wiff materializes it to a cache directory rather than
editing anything in place, so exploration never disturbs the working tree; edits
made there are not fed back into the review (a later feature could).

Materialization has two scopes, both under a session-scoped cache directory
(`<ULID>.d/cache/vN/after/...`, a regenerable cache kept separate from the
source-of-truth `vN.diff`, cleaned up with the session):

- single file: materialize just the focused file and open it.
- whole tree: materialize the after side of every file in the version into a
  directory tree, then open the editor there, so cross-file navigation and
  search work. This is the richer mode and the reason for a directory rather
  than a lone temp file.

The editor command comes from config, then `$VISUAL`, then `$EDITOR`, then a
sensible default. Because jumping to a line differs per editor, the editor is
configured as a command template with `{file}` and `{line}` placeholders (a
template without `{line}` simply opens the file), so the focused line can be
passed through where the editor supports it.

As of v0 this is designed but not yet wired: the `open_in_editor` action and its
default `o` binding exist, but the materialization and editor launch are not
implemented. The reconstitution path they depend on is shared with rebasing and
highlighting, so the groundwork is present, and the feature is expected to land
after the rest of v0.

## Configuration

- TOML at `$XDG_CONFIG_HOME/wiff/config.toml` (e.g. `~/.config/wiff`).
- Keymap is `action -> [chords]`, action names in snake_case (`page_down`,
  `next_hunk`, `next_comment`, `add_comment`, `save`, `quit_keep`, ...). A chord
  is a space-separated
  sequence of key presses; each press is a key with optional `ctrl-`/`alt-`/
  `shift-` modifier prefixes, lowercased (e.g. `"ctrl-f"`, `"g g"`).
- `on_exit` selects keep/remove/prompt behavior on quit.
- `wrap_lines` (default on) soft-wraps diff content to the viewport width rather
  than clipping it at the edge; the `toggle_wrap` action flips it within a
  session.
- `editor` is a command template (`{file}`, `{line}` placeholders) for
  `open_in_editor`; when unset, `$VISUAL` then `$EDITOR` then a default is used.
- Where user choices are persisted back (e.g. remembered view options), use
  `toml_edit` so the user's file structure and comments are preserved.
- Author defaults may be configured here (overridden by CLI flags).

## Crate layout

A cargo workspace under `crates/`:

- `wiff-core`: session model, records, append/lock, project identity and
  discovery, the diff-source trait, and the rebasing engine (uses `similar`).
  Honors `WIFF_DATA_DIR` and exposes `*_in(base, ...)` test seams.
- `wiff-diff`: unified diff parsing, the hunk model, and syntect highlighting.
- `wiff-tui`: the ratatui UI, the action enum, and the keymap layer.
- `wiff`: the binary: CLI subcommands, `render`, and launching the TUI.

## Deferred (post-v0), designed for

- Generic boxed `DiffSource` so sources are not concrete enum variants
  everywhere.
- jj source; forge integration via git-pkgs/forge (pull, review, push PR
  commentary).
- Suggested code-change blocks in comments.
- Markdown rendering of comment bodies in the TUI (plain wrapped text in v0).
- Side-by-side view; ignore-whitespace and other diff options.
- `--watch` live reload of a new diff capture from another process.
- Opening files in an editor (`open_in_editor`): the action and its default
  binding exist, but materialization and editor launch, single-file then
  whole-tree, and feeding editor changes back into the review are deferred.
- Additional `wiff render` output formats.
- Leader-key and multi-key chords (schema already accommodates them).
