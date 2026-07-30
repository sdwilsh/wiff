# Screencasts

These recordings are scripted and regenerated from the `wiff-cast` tool, so they
stay current with the real program. Each plays back as selectable text, not
video.

## Opening a review

Make a change, look at it with `git status`, then open it in wiff with
`wiff new`:

<div class="asciinema-cast" data-cast="../assets/casts/review-basics.cast" data-autoplay="true" data-loop="true"></div>

## Leaving a comment

Move onto the changed line, press `c` to open the editor, write the note, and
submit it with `ctrl-d`:

<div class="asciinema-cast" data-cast="../assets/casts/add-comment.cast" data-autoplay="true" data-loop="true"></div>

## Switching the diff layout

Step a markdown diff through the unified, side-by-side, and after-side layouts,
ending on the rendered view that shows it as formatted text:

<div class="asciinema-cast" data-cast="../assets/casts/diff-layouts.cast" data-autoplay="true" data-loop="true"></div>

## Changing the theme

Press `T` to open the theme picker and select a theme; the whole diff recolors
to match:

<div class="asciinema-cast" data-cast="../assets/casts/theme-picker.cast" data-autoplay="true" data-loop="true"></div>

## Listing and resuming sessions

`wiff session list` shows each session in the project with its source, age, and
comment tally, marking the active one with a `*`. Resume any of them by id with
`wiff resume --session <ID>`, without checking out its branch:

<div class="asciinema-cast" data-cast="../assets/casts/session-list.cast" data-autoplay="true" data-loop="true"></div>
