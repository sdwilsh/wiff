//! Choosing what happens to the session and its unsaved drafts on the way out.
//!
//! Leaving a review resolves to one of three outcomes: commit the buffered
//! drafts and keep the session, keep it but throw the drafts away, or remove it
//! entirely. When there are pending drafts the reviewer is always asked, since
//! leaving would otherwise silently lose work; with nothing buffered the choice
//! is only keep-or-remove and the host's configured default settles it without a
//! prompt unless the default is to prompt. The dialog that does the asking lives
//! here as [`ExitDialog`], along with the pure [`plan_exit`] that decides
//! between resolving at once and putting the question to the reviewer.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use wiff_diff::Rgb;

use crate::action::Action;
use crate::render::color;

/// The hint shown along the bottom of the exit dialog.
const HINT: &str = "up/down move   enter select   esc cancel";

/// What leaving the review does to the session and its buffered drafts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// Keep the session and flush the pending drafts to the log.
    Commit,
    /// Keep the session but discard the pending drafts.
    Discard,
    /// Remove the session, discarding the drafts along with it.
    Remove,
}

/// The default an exit resolves to, set by the host's `on_exit` policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDefault {
    /// Keep the session, preselecting the commit choice when asking.
    Keep,
    /// Remove the session, preselecting the remove choice when asking.
    Remove,
    /// No default; always ask how to leave.
    Prompt,
}

/// The colors the exit dialog paints with, taken from the theme by the host.
#[derive(Debug, Clone, Copy)]
pub struct ExitColors {
    /// The dialog's border and title color.
    pub border: Rgb,
    /// The background washed over the highlighted choice.
    pub selected_bg: Rgb,
    /// The color of the choice text.
    pub text: Rgb,
    /// The color of the key hint along the bottom.
    pub hint: Rgb,
}

/// Whether leaving resolves at once or the reviewer is asked how to leave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitPlan {
    /// Leave immediately with this outcome.
    Now(Exit),
    /// Ask the reviewer, offering `choices` with `selected` highlighted.
    Ask {
        /// The heading naming the question.
        title: &'static str,
        /// The offered choices, each a label and the outcome it produces.
        choices: Vec<(&'static str, Exit)>,
        /// The index of the choice highlighted when the dialog opens.
        selected: usize,
    },
}

/// Decide how to leave for `action` given the configured `default` and whether
/// drafts are pending. An explicit keep or remove settles itself. A plain quit
/// with pending drafts always asks, since leaving would otherwise lose them,
/// preselecting the default choice; with nothing buffered it follows the
/// default and only asks when that default is to prompt.
pub fn plan_exit(action: Action, default: ExitDefault, has_drafts: bool) -> ExitPlan {
    match action {
        Action::QuitKeep => ExitPlan::Now(Exit::Commit),
        Action::QuitRemove => ExitPlan::Now(Exit::Remove),
        _ if has_drafts => ExitPlan::Ask {
            title: "You have uncommitted comments",
            choices: vec![
                ("Commit review", Exit::Commit),
                ("Quit without saving", Exit::Discard),
                ("Remove session", Exit::Remove),
            ],
            selected: match default {
                ExitDefault::Keep | ExitDefault::Prompt => 0,
                ExitDefault::Remove => 2,
            },
        },
        _ => match default {
            ExitDefault::Keep => ExitPlan::Now(Exit::Commit),
            ExitDefault::Remove => ExitPlan::Now(Exit::Remove),
            ExitDefault::Prompt => ExitPlan::Ask {
                title: "Keep this session?",
                choices: vec![
                    ("Keep session", Exit::Commit),
                    ("Remove session", Exit::Remove),
                ],
                selected: 0,
            },
        },
    }
}

/// The modal asking how to leave the review: a list of outcomes with one
/// highlighted, moved through with the arrows and confirmed or cancelled.
pub struct ExitDialog {
    title: String,
    choices: Vec<(String, Exit)>,
    selected: usize,
    colors: ExitColors,
}

impl ExitDialog {
    /// A dialog titled `title` offering `choices`, opening on `selected`, drawn
    /// with `colors`.
    pub fn new(
        title: &str,
        choices: Vec<(&str, Exit)>,
        selected: usize,
        colors: ExitColors,
    ) -> Self {
        let choices = choices
            .into_iter()
            .map(|(label, exit)| (label.to_string(), exit))
            .collect::<Vec<_>>();
        let selected = selected.min(choices.len().saturating_sub(1));
        Self {
            title: title.to_string(),
            choices,
            selected,
            colors,
        }
    }

    /// Move the highlight to the previous choice, stopping at the first.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the highlight to the next choice, stopping at the last.
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1).min(self.choices.len().saturating_sub(1));
    }

    /// The outcome the highlighted choice produces.
    pub fn selected_exit(&self) -> Exit {
        self.choices[self.selected].1
    }

    /// The dialog's border and title color.
    pub fn border(&self) -> Rgb {
        self.colors.border
    }

    /// The heading for the dialog's border.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The width of the dialog's content, inside its border.
    pub fn width(&self) -> usize {
        // Two columns for the selection marker, then the widest label or hint.
        2 + self.inner_width()
    }

    /// The height of the dialog's content, inside its border: one row per
    /// choice, a blank spacer, and the hint.
    pub fn height(&self) -> usize {
        self.choices.len() + 2
    }

    /// The dialog's content lines: each choice, marked and highlighted when it
    /// is the selection, then a spacer and the key hint.
    pub fn lines(&self) -> Vec<Line<'static>> {
        let inner = self.inner_width();
        let mut lines: Vec<Line<'static>> = self
            .choices
            .iter()
            .enumerate()
            .map(|(index, (label, _))| {
                let selected = index == self.selected;
                let marker = if selected { "> " } else { "  " };
                let text = format!("{marker}{label:<inner$}");
                let mut style = Style::default().fg(color(self.colors.text));
                if selected {
                    style = style.bg(color(self.colors.selected_bg));
                }
                Line::from(Span::styled(text, style))
            })
            .collect();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  {HINT:<inner$}"),
            Style::default().fg(color(self.colors.hint)),
        )));
        lines
    }

    /// The widest content the dialog must fit: its widest label or the hint.
    fn inner_width(&self) -> usize {
        self.choices
            .iter()
            .map(|(label, _)| label.chars().count())
            .max()
            .unwrap_or(0)
            .max(HINT.chars().count())
    }
}

#[cfg(test)]
mod tests {
    use super::{Exit, ExitDefault, ExitPlan, plan_exit};
    use crate::action::Action;

    #[test]
    fn an_explicit_choice_settles_itself_whatever_the_default() {
        // quit-keep commits and keeps; quit-remove removes, regardless of the
        // configured default or whether drafts are pending.
        k9::assert_equal!(
            plan_exit(Action::QuitKeep, ExitDefault::Remove, true),
            ExitPlan::Now(Exit::Commit)
        );
        k9::assert_equal!(
            plan_exit(Action::QuitRemove, ExitDefault::Keep, false),
            ExitPlan::Now(Exit::Remove)
        );
    }

    #[test]
    fn a_plain_quit_with_drafts_always_asks_preselecting_the_default() {
        // Whatever the default, pending drafts mean the reviewer is asked; the
        // default only picks which choice opens highlighted.
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Keep, true),
            ExitPlan::Ask {
                title: "You have uncommitted comments",
                choices: vec![
                    ("Commit review", Exit::Commit),
                    ("Quit without saving", Exit::Discard),
                    ("Remove session", Exit::Remove),
                ],
                selected: 0,
            }
        );
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Remove, true),
            ExitPlan::Ask {
                title: "You have uncommitted comments",
                choices: vec![
                    ("Commit review", Exit::Commit),
                    ("Quit without saving", Exit::Discard),
                    ("Remove session", Exit::Remove),
                ],
                selected: 2,
            }
        );
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Prompt, true),
            ExitPlan::Ask {
                title: "You have uncommitted comments",
                choices: vec![
                    ("Commit review", Exit::Commit),
                    ("Quit without saving", Exit::Discard),
                    ("Remove session", Exit::Remove),
                ],
                selected: 0,
            }
        );
    }

    #[test]
    fn a_plain_quit_without_drafts_follows_the_default_or_asks_keep_remove() {
        // Nothing is buffered, so keep and remove resolve at once and only a
        // prompt default asks, offering just keep or remove.
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Keep, false),
            ExitPlan::Now(Exit::Commit)
        );
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Remove, false),
            ExitPlan::Now(Exit::Remove)
        );
        k9::assert_equal!(
            plan_exit(Action::Quit, ExitDefault::Prompt, false),
            ExitPlan::Ask {
                title: "Keep this session?",
                choices: vec![
                    ("Keep session", Exit::Commit),
                    ("Remove session", Exit::Remove)
                ],
                selected: 0,
            }
        );
    }
}
