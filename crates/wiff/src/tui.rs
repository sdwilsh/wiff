//! Opening a session in the review TUI.
//!
//! This is the bridge from the persisted session to the interactive review: it
//! loads the latest captured diff, renders it, and runs the terminal loop, then
//! resolves how the reviewer chose to leave against the configured `on_exit`
//! default, keeping or removing the session accordingly.

use std::io::{IsTerminal, Write};
use std::path::Path;

use anyhow::Context;
use wiff_config::{Config, OnExit};
use wiff_core::session::remove_session;
use wiff_core::{ReviewState, SessionLog};
use wiff_tui::{App, DiffView, Exit, Theme, run};

/// Open `session_path` in the review TUI, then keep or remove the session per
/// the reviewer's choice and the configured `on_exit` default.
pub fn open(session_path: &Path, config: &Config) -> anyhow::Result<()> {
    let log = SessionLog::open(session_path)?;
    let state = ReviewState::load(session_path)?;
    let version = state
        .latest_version()
        .context("this session has no captured diff to review")?;
    let text = log.read_diff(version.number)?;
    let diff = wiff_diff::parse(&text)?;

    let theme = Theme::dark();
    let sections = wiff_diff::SectionMatchers::new(&config.section)
        .context("a configured section pattern is not a valid regex")?;
    // Withdrawn comments are tombstones in the folded state; the review view
    // shows only the live ones.
    let comments: Vec<_> = state
        .comments
        .iter()
        .filter(|comment| !comment.deleted)
        .cloned()
        .collect();
    let document = DiffView::new(theme.clone())?
        .with_display_context(config.display_context)
        .with_section_matchers(sections)
        .render_review(&diff, &comments, &[]);
    let app = App::new(document, 0, &theme);
    let keymap = config.keymap()?;

    let exit = run(app, keymap)?;
    resolve_exit(exit, config, session_path)
}

/// Keep or remove the session for `exit`, resolving the configured default and
/// prompting when neither the reviewer nor the config settled the choice.
fn resolve_exit(exit: Exit, config: &Config, session_path: &Path) -> anyhow::Result<()> {
    let keep = match keep_decision(exit, config.on_exit) {
        Some(keep) => keep,
        None => prompt_keep()?,
    };
    if keep {
        println!("kept session at {}", session_path.display());
    } else {
        remove_session(session_path)?;
        println!("removed session");
    }
    Ok(())
}

/// Whether to keep the session for `exit`, or `None` when the choice is left to
/// the reviewer: an explicit quit-keep or quit-remove decides itself, and a
/// plain quit follows `on_exit` unless it is `Prompt`.
fn keep_decision(exit: Exit, on_exit: OnExit) -> Option<bool> {
    match exit {
        Exit::Keep => Some(true),
        Exit::Remove => Some(false),
        Exit::Default => match on_exit {
            OnExit::Keep => Some(true),
            OnExit::Remove => Some(false),
            OnExit::Prompt => None,
        },
    }
}

/// Ask on the terminal whether to keep the session, defaulting to keeping it
/// when there is no terminal to ask or the answer is empty, so a session is
/// never dropped without an explicit no.
fn prompt_keep() -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        return Ok(true);
    }
    loop {
        print!("keep this session? [Y/n] ");
        std::io::stdout()
            .flush()
            .context("could not write the exit prompt")?;
        let mut answer = String::new();
        let read = std::io::stdin()
            .read_line(&mut answer)
            .context("could not read the exit answer")?;
        // A closed stdin (no bytes read) keeps the session rather than looping
        // forever on end-of-input.
        if read == 0 {
            return Ok(true);
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("please answer yes or no"),
        }
    }
}

#[cfg(test)]
mod tests {
    use wiff_config::OnExit;
    use wiff_tui::Exit;

    use super::keep_decision;

    #[test]
    fn an_explicit_choice_decides_regardless_of_config() {
        // quit-keep and quit-remove settle themselves whatever on_exit says.
        k9::assert_equal!(keep_decision(Exit::Keep, OnExit::Remove), Some(true));
        k9::assert_equal!(keep_decision(Exit::Remove, OnExit::Keep), Some(false));
    }

    #[test]
    fn a_plain_quit_follows_the_configured_default_or_asks() {
        k9::assert_equal!(keep_decision(Exit::Default, OnExit::Keep), Some(true));
        k9::assert_equal!(keep_decision(Exit::Default, OnExit::Remove), Some(false));
        k9::assert_equal!(keep_decision(Exit::Default, OnExit::Prompt), None);
    }
}
