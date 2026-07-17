//! The action vocabulary.
//!
//! Input events are decoded into an [`Action`] before the UI reacts, so the UI
//! logic branches on intent rather than on raw keys. This keeps key bindings
//! reassignable and the update logic small. Action names double as their config
//! identifiers via [`Action::name`].
//!
//! The whole vocabulary is declared through [`declare_actions!`], which is the
//! single source of truth for the variant and the label the help overlay shows
//! for it. A variant's config identifier is its own name in snake_case, derived
//! at compile time rather than spelled out. Because the macro also records which
//! group each action belongs to, the help overlay reads its layout straight from
//! the declaration rather than a second table that could drift. The doc comment
//! on each variant is the help label the reviewer reads; developer notes about
//! an action's behavior belong in ordinary `//` comments, which the macro does
//! not capture.

use serde::{Deserialize, Serialize};

/// The number of bytes `pascal` occupies once rewritten in snake_case: its own
/// length plus one underscore before each uppercase letter after the first.
const fn snake_len(pascal: &[u8]) -> usize {
    let mut len = 0;
    let mut i = 0;
    while i < pascal.len() {
        if i > 0 && pascal[i].is_ascii_uppercase() {
            len += 1;
        }
        len += 1;
        i += 1;
    }
    len
}

/// Rewrite the PascalCase identifier `pascal` in snake_case, lowercasing each
/// letter and inserting an underscore before each uppercase letter after the
/// first. `N` must be [`snake_len`] of `pascal`.
const fn snake_bytes<const N: usize>(pascal: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    let mut i = 0;
    let mut j = 0;
    while i < pascal.len() {
        let c = pascal[i];
        if c.is_ascii_uppercase() {
            if i > 0 {
                out[j] = b'_';
                j += 1;
            }
            out[j] = c.to_ascii_lowercase();
        } else {
            out[j] = c;
        }
        j += 1;
        i += 1;
    }
    out
}

/// Declare the [`Action`] enum together with each variant's help label and
/// group. The label is the variant's own doc comment; its config identifier is
/// the variant name in snake_case. Emits the enum, [`Action::name`],
/// [`Action::description`], and the [`ACTION_GROUPS`] table the help overlay
/// reads.
macro_rules! declare_actions {
    (
        $(
            $group:literal => {
                $(
                    $(#[doc = $doc:literal])+
                    $variant:ident
                ),+ $(,)?
            }
        ),+ $(,)?
    ) => {
        /// A single reviewer intent the UI can act on.
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(rename_all = "snake_case")]
        pub enum Action {
            $($(
                $(#[doc = $doc])+
                $variant,
            )+)+
        }

        impl Action {
            /// The action's stable snake_case identifier, used as its key in config.
            pub fn name(self) -> &'static str {
                match self {
                    $($( Action::$variant => {
                        const N: usize = snake_len(stringify!($variant).as_bytes());
                        const BYTES: [u8; N] = snake_bytes::<N>(stringify!($variant).as_bytes());
                        match std::str::from_utf8(&BYTES) {
                            Ok(name) => name,
                            Err(_) => unreachable!(),
                        }
                    } )+)+
                }
            }

            /// Returns the label the help overlay shows for this action, taken
            /// from the action's own documentation.
            pub fn description(self) -> &'static str {
                match self {
                    $($( Action::$variant => concat!($($doc),+).trim(), )+)+
                }
            }
        }

        /// The actions grouped by purpose, in the order the help overlay lists
        /// them. Every action belongs to one group, so a new binding shows in
        /// the overlay without a second table to update.
        pub const ACTION_GROUPS: &[ActionGroup] = &[
            $(
                ActionGroup {
                    name: $group,
                    actions: &[ $( Action::$variant ),+ ],
                },
            )+
        ];
    };
}

/// A named group of related actions, for presenting the bindings by purpose.
pub struct ActionGroup {
    /// The heading shown above the group.
    pub name: &'static str,
    /// The group's actions, in display order.
    pub actions: &'static [Action],
}

declare_actions! {
    "Navigation" => {
        /// Move down one line
        LineDown,
        /// Move up one line
        LineUp,
        /// Scroll down one page
        PageDown,
        /// Scroll up one page
        PageUp,
        /// Jump to the top
        Top,
        /// Jump to the bottom
        Bottom,
        /// Next file
        NextFile,
        /// Previous file
        PrevFile,
        /// Next hunk
        NextHunk,
        /// Previous hunk
        PrevHunk,
        /// Next comment
        NextComment,
        /// Previous comment
        PrevComment,
    },
    "View" => {
        /// Expand or collapse the fold
        ToggleFold,
        /// Expand or collapse the comment
        ToggleComment,
        /// Toggle line wrapping
        ToggleWrap,
        // Leaves only the code, so rounds of annotation do not crowd out the diff.
        /// Show or hide comments
        HideComments,
        // Side-by-side once the viewport is wide enough, else unified.
        /// Diff layout: auto
        DiffModeAuto,
        /// Diff layout: unified
        DiffModeUnified,
        /// Diff layout: side by side
        DiffModeSideBySide,
        /// Choose a color theme
        PickTheme,
    },
    "Jump to" => {
        /// Jump to a file
        PickFile,
        /// Jump to a comment
        PickComment,
    },
    "Comments" => {
        // Seeds from a draft comment's range when the cursor is on one.
        /// Select lines for a comment
        SelectLines,
        /// Add a comment
        AddComment,
        /// Reply to the focused comment
        ReplyComment,
        /// Edit the focused comment
        EditComment,
        /// Resolve or unresolve the comment
        ResolveComment,
        /// Delete or restore the comment
        DeleteComment,
        /// Cycle your verdict on the comment
        SetVerdict,
    },
    "Editor" => {
        // The editor's own keys, which act only while it is open.
        /// Submit the comment
        SubmitComment,
        // Confirms first when the body has unsaved changes.
        /// Cancel the comment
        CancelComment,
        // Floats the editor at a screen edge and frees the cursor to roam the
        // diff for something to reference.
        /// Detach the editor to reference the diff
        DetachEditor,
    },
    "Search" => {
        /// Search forward
        SearchForward,
        /// Search backward
        SearchBackward,
        /// Next match
        SearchNext,
        /// Previous match
        SearchPrev,
    },
    "Session" => {
        // Moves the pending drafts into the session log, keeping the review open.
        /// Commit pending comments
        Save,
        // Recaptures the diff and rebases comments onto the new version.
        /// Capture a new diff version
        Refresh,
        /// Compare against an earlier version
        CompareVersions,
        /// Open the file in your editor
        OpenInEditor,
        /// Show this help
        Help,
        // Honors the configured keep-or-remove default.
        /// Quit
        Quit,
        /// Quit and keep the session
        QuitKeep,
        /// Quit and remove the session
        QuitRemove,
    },
}

#[cfg(test)]
mod tests {
    use super::ACTION_GROUPS;

    #[test]
    fn the_derived_name_matches_serde_for_every_action() {
        // `name()` derives the config identifier from the variant by case
        // conversion; serde derives the same identifier for config parsing. A
        // variant whose snake_case is not a simple conversion (an acronym, say)
        // would drift the two apart, so pin them equal for every action.
        let mismatches: Vec<(&str, String)> = ACTION_GROUPS
            .iter()
            .flat_map(|group| group.actions)
            .filter_map(|action| {
                let serde_name = serde_json::to_value(action)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default();
                (action.name() != serde_name).then(|| (action.name(), serde_name))
            })
            .collect();
        wince::assert_eq!(mismatches, Vec::<(&str, String)>::new());
    }

    /// The whole vocabulary as `group` headings over `name = description` lines,
    /// so a test asserts every action's group, config identifier, and help label
    /// together rather than any single one in isolation.
    fn vocabulary() -> String {
        let mut out = String::new();
        for group in ACTION_GROUPS {
            out.push_str(group.name);
            out.push('\n');
            for action in group.actions {
                out.push_str(&format!("  {} = {}\n", action.name(), action.description()));
            }
        }
        out
    }

    #[test]
    fn every_action_declares_its_group_name_and_help_label() {
        // The description of each action is its own doc comment, trimmed.
        #[rustfmt::skip]
        wince::snapshot_str!(
            vocabulary(),
            "Navigation\n",
            "  line_down = Move down one line\n",
            "  line_up = Move up one line\n",
            "  page_down = Scroll down one page\n",
            "  page_up = Scroll up one page\n",
            "  top = Jump to the top\n",
            "  bottom = Jump to the bottom\n",
            "  next_file = Next file\n",
            "  prev_file = Previous file\n",
            "  next_hunk = Next hunk\n",
            "  prev_hunk = Previous hunk\n",
            "  next_comment = Next comment\n",
            "  prev_comment = Previous comment\n",
            "View\n",
            "  toggle_fold = Expand or collapse the fold\n",
            "  toggle_comment = Expand or collapse the comment\n",
            "  toggle_wrap = Toggle line wrapping\n",
            "  hide_comments = Show or hide comments\n",
            "  diff_mode_auto = Diff layout: auto\n",
            "  diff_mode_unified = Diff layout: unified\n",
            "  diff_mode_side_by_side = Diff layout: side by side\n",
            "  pick_theme = Choose a color theme\n",
            "Jump to\n",
            "  pick_file = Jump to a file\n",
            "  pick_comment = Jump to a comment\n",
            "Comments\n",
            "  select_lines = Select lines for a comment\n",
            "  add_comment = Add a comment\n",
            "  reply_comment = Reply to the focused comment\n",
            "  edit_comment = Edit the focused comment\n",
            "  resolve_comment = Resolve or unresolve the comment\n",
            "  delete_comment = Delete or restore the comment\n",
            "  set_verdict = Cycle your verdict on the comment\n",
            "Editor\n",
            "  submit_comment = Submit the comment\n",
            "  cancel_comment = Cancel the comment\n",
            "  detach_editor = Detach the editor to reference the diff\n",
            "Search\n",
            "  search_forward = Search forward\n",
            "  search_backward = Search backward\n",
            "  search_next = Next match\n",
            "  search_prev = Previous match\n",
            "Session\n",
            "  save = Commit pending comments\n",
            "  refresh = Capture a new diff version\n",
            "  compare_versions = Compare against an earlier version\n",
            "  open_in_editor = Open the file in your editor\n",
            "  help = Show this help\n",
            "  quit = Quit\n",
            "  quit_keep = Quit and keep the session\n",
            "  quit_remove = Quit and remove the session\n",
        );
    }
}
