# The review interface

Every review opens in the same terminal UI, whatever its source. It scrolls like
`less`, with review actions layered on top. This page describes moving around,
switching the diff layout, and annotating; for the full list of keys and how to
rebind them, see [Key bindings](keybindings.md). Press `h` while browsing the
review for the in-app help overlay.

## Navigation

- `j` / `k` or the arrow keys scroll a line; `space` / `b` page.
- `g` / `G` jump to the top (the review summary) or the bottom.
- `,` / `.` move to the previous/next file, `[` / `]` to the previous/next hunk,
  and `{` / `}` to the previous/next comment.
- `t` opens the file picker, `C` the comment picker, `T` the theme picker.
- `/` and `?` search forward and backward; `n` / `N` repeat the search.
- `enter` toggles a fold, `tab` toggles a comment, `H` hides all comments, `w`
  toggles line wrapping, and `L` toggles the line-number gutter.

## Diff layouts

Switch the layout live, or set the startup layout with `diff_mode` in the
config:

- `1` unified: one column with added and removed lines interleaved.
- `2` side-by-side: before on the left, after on the right.
- `3` after-side only: the resulting file in a single column.
- `4` rendered: the after side through a type-specific renderer (markdown
  today), falling back to the after-side source when no renderer fits the file.
- `0` auto: side-by-side when the terminal is wide enough, unified otherwise.

This recording steps a markdown diff through the layouts, ending on the rendered
view that shows it as formatted text.

<div class="asciinema-cast" data-cast="../assets/casts/diff-layouts.cast" data-autoplay="true" data-loop="true"></div>

## Comments

- `v` starts a linewise selection; extend it with the navigation keys.
- `c` adds a comment on the selection, or on the cursor line when nothing is
  selected.
- With a comment focused, `r` replies, `e` edits, `x` resolves, and `d`
  withdraws. `a` cycles your verdict on it: first `approve`, then
  `request_changes`, then back to a plain comment with no verdict.
- In the editor, `ctrl-d` submits (moving the body into your drafts) and `esc`
  cancels. `ctrl-o` detaches the editor to float it at a screen edge, freeing
  the cursor to roam the diff for a line to reference.
- You can also detach without reaching for `ctrl-o`: pressing an arrow past the
  top or bottom edge of the editor nudges it loose and floats it the same way,
  and the same arrow moves the cursor on into the diff. Set `nudge_to_detach =
  false` in the config to keep the editor in place and turn this off.

Edits are buffered as drafts and marked as such. `ctrl-s` commits them to the
session; you are also offered to save on exit.

This recording walks the whole loop: comment on the changed line, cycle a
verdict onto it, reply, resolve the thread, and jump between comments with the
`C` picker.

<div class="asciinema-cast" data-cast="../assets/casts/add-comment.cast" data-autoplay="true" data-loop="true"></div>

## Session and forge actions

- `ctrl-r` refreshes: it recaptures the diff and rebases your comments onto the
  new version. A forge review refreshes by fetching its pull request again.
- `P` publishes a forge review, reconciling the pull request's state and then
  sending your review back.
- `A` adds a file to an explore review (a no-op in a change review, whose file
  set is fixed by its diff).
- `V` compares the review against an earlier version.
- `ctrl-z` suspends wiff to the shell.
