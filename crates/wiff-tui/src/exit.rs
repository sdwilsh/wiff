//! Choosing what happens to the session and its unsaved drafts on the way out.
//!
//! Leaving a review resolves to one of three outcomes: commit the buffered
//! drafts and keep the session, keep it but throw the drafts away, or remove it
//! entirely. When there are pending drafts the reviewer is always asked, since
//! leaving would otherwise silently lose work; with nothing buffered the choice
//! is only keep-or-remove and the host's configured default settles it without a
//! prompt unless the default is to prompt. The pure [`plan_exit`] here decides
//! between resolving immediately and putting the question to the reviewer, which
//! the app then asks through its modal picker.

use crate::action::Action;

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

/// Whether leaving resolves immediately or the reviewer is asked how to leave.
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
