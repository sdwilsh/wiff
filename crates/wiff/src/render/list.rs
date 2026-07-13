//! A compact, id-first listing of a review's comments for `wiff comment list`.
//!
//! Unlike the markdown render, which is grouped prose for a human or an agent
//! prompt, this shows each live comment's id on its own line so it can be fed
//! straight back to `wiff comment resolve` or `wiff comment rm`.

use wiff_core::record::{CommentTarget, Confidence};
use wiff_core::review::{CommentState, ReviewState};

use super::live_comments;

/// Render `state`'s live comments as a compact, id-first list.
pub(super) fn render(state: &ReviewState) -> String {
    let comments = live_comments(state);
    if comments.is_empty() {
        return "No comments.\n".to_string();
    }
    let mut out = String::new();
    for comment in comments {
        out.push_str(&format!(
            "{}  {}  {}  {} ({})\n",
            comment.id,
            status(comment),
            location(&comment.target),
            comment.author.name,
            comment.author.kind.as_str(),
        ));
        for line in comment.body.trim_end().lines() {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out
}

/// The comment's status: resolved or open, plus its re-anchor confidence when
/// it is not exact.
fn status(comment: &CommentState) -> String {
    let mut parts = vec![if comment.resolved { "resolved" } else { "open" }];
    match comment.confidence {
        Some(Confidence::Approximate) => parts.push("shifted"),
        Some(Confidence::Outdated) => parts.push("outdated"),
        Some(Confidence::Exact) | None => {}
    }
    parts.join(",")
}

/// The file-qualified location of a comment's target.
fn location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Lines {
            file,
            side,
            start_line,
            end_line,
        } => {
            let side = side.as_str();
            if start_line == end_line {
                format!("{file} line {start_line} ({side})")
            } else {
                format!("{file} lines {start_line}-{end_line} ({side})")
            }
        }
        CommentTarget::File { file } => format!("{file} (whole file)"),
        CommentTarget::Review => "review".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::render::fixture::state;

    #[test]
    fn lists_live_comments_id_first_and_hides_deleted() {
        let out = render(&state());
        let expected = "\
00000000000000000000000001  open  main.rs line 2 (after)  wez (human)
  why 3?
00000000000000000000000002  resolved  main.rs (whole file)  assistant (agent)
  needs tests
00000000000000000000000003  open  review  wez (human)
  overall solid
00000000000000000000000004  open,shifted  other.rs lines 5-6 (after)  dev (human)
  moved code
";
        wince::assert_eq!(out, expected.to_string());
    }
}
