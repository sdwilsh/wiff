# Key bindings

These are wiff's default bindings. The name in the last column is the key you
use in the `[keymap]` section of the config file to rebind that action; see
[Configuration](configuration.md#keybindings). To start from a clean slate, set
`disable_default_keymap`.

Pressing `h` in the TUI shows your effective bindings, including any you have
rebound.

A binding may be a single key (`j`), a modified key (`ctrl-f`, `shift-tab`), or
a sequence pressed in turn (`g g`).

## Navigation

| Keys | Action | Config name |
| --- | --- | --- |
| `j`, `down` | Move down one line | `line_down` |
| `k`, `up` | Move up one line | `line_up` |
| `space`, `ctrl-f`, `pagedown` | Scroll down one page | `page_down` |
| `b`, `ctrl-b`, `pageup` | Scroll up one page | `page_up` |
| `ctrl-d` | Scroll down half a page | `page_down_half` |
| `ctrl-u` | Scroll up half a page | `page_up_half` |
| `g`, `<`, `home` | Jump to the top | `top` |
| `G`, `>`, `end` | Jump to the bottom | `bottom` |
| `.` | Next file | `next_file` |
| `,` | Previous file | `prev_file` |
| `]` | Next hunk | `next_hunk` |
| `[` | Previous hunk | `prev_hunk` |
| `}` | Next comment | `next_comment` |
| `{` | Previous comment | `prev_comment` |

## View

| Keys | Action | Config name |
| --- | --- | --- |
| `enter` | Expand or collapse the fold | `toggle_fold` |
| `tab` | Expand or collapse the comment | `toggle_comment` |
| `w` | Toggle line wrapping | `toggle_wrap` |
| `L` | Show or hide line numbers | `toggle_line_numbers` |
| `H` | Show or hide comments | `hide_comments` |
| `0` | Diff layout: auto | `diff_mode_auto` |
| `1` | Diff layout: unified | `diff_mode_unified` |
| `2` | Diff layout: side by side | `diff_mode_side_by_side` |
| `3` | Diff layout: after side only | `diff_mode_only_after` |
| `4` | Diff layout: rendered after side | `diff_mode_rendered` |
| `T` | Choose a color theme | `pick_theme` |

## Jump to

| Keys | Action | Config name |
| --- | --- | --- |
| `t` | Jump to a file | `pick_file` |
| `C` | Jump to a comment | `pick_comment` |

## Comments

| Keys | Action | Config name |
| --- | --- | --- |
| `v` | Select lines for a comment | `select_lines` |
| `c` | Add a comment | `add_comment` |
| `r` | Reply to the focused comment | `reply_comment` |
| `e` | Edit the focused comment | `edit_comment` |
| `x` | Resolve or unresolve the comment | `resolve_comment` |
| `d` | Delete or restore the comment | `delete_comment` |
| `a` | Cycle your verdict on the comment | `set_verdict` |

## Editor

These keys act only while the comment editor is open.

| Keys | Action | Config name |
| --- | --- | --- |
| `ctrl-d` | Submit the comment | `submit_comment` |
| `esc` | Cancel the comment | `cancel_comment` |
| `ctrl-o` | Detach the editor to reference the diff | `detach_editor` |

## Search

| Keys | Action | Config name |
| --- | --- | --- |
| `/` | Search forward | `search_forward` |
| `?` | Search backward | `search_backward` |
| `n` | Next match | `search_next` |
| `N` | Previous match | `search_prev` |

## Session

| Keys | Action | Config name |
| --- | --- | --- |
| `ctrl-s` | Commit pending comments | `save` |
| `ctrl-r` | Capture a new diff version | `refresh` |
| `P` | Publish the review to the forge | `publish` |
| `A` | Add a file to the review | `add_file` |
| `V` | Compare against an earlier version | `compare_versions` |
| `ctrl-z` | Suspend to the shell | `suspend` |
| `h` | Show the help overlay | `help` |
| `q` | Quit | `quit` |
| (unbound) | Quit and keep the session | `quit_keep` |
| (unbound) | Quit and remove the session | `quit_remove` |

`q` honours the configured keep-or-remove default; `quit_keep` and `quit_remove`
have no default binding but can be bound to skip the prompt in a chosen
direction.
