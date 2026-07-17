//! A compact, number-first listing of a review's comments for `wiff comment
//! list`.
//!
//! Unlike the markdown render, which is grouped prose for a human or an agent
//! prompt, this leads each live comment with its review-scoped number on its own
//! line, for a terse pass without bodies or snippets.

use wiff_core::record::{CommentTarget, Confidence};
use wiff_core::review::{CommentState, ReviewState};

use super::visible_threads;

/// Render `state`'s comments as a compact, number-first list. A thread renders
/// as its root followed by its replies, each reply indented a level. A withdrawn
/// reply drops out; a withdrawn root shows as a tombstone while it still has a
/// live reply, and disappears once its whole thread is withdrawn.
pub(super) fn render(state: &ReviewState) -> String {
    let threads = visible_threads(state);
    if threads.is_empty() {
        return "No comments.\n".to_string();
    }
    let mut out = String::new();
    for thread in threads {
        entry(&mut out, thread.root, 0);
        for reply in thread.replies {
            entry(&mut out, reply, 1);
        }
    }
    out
}

/// Append one comment as a number-first entry indented `depth` levels, its body
/// beneath it.
fn entry(out: &mut String, comment: &CommentState, depth: usize) {
    let indent = "  ".repeat(depth);
    out.push_str(&format!(
        "{indent}{}  {}  {}  {} ({})",
        comment.handle(),
        status(comment),
        location(&comment.target),
        comment.author.name,
        comment.author.kind.as_str(),
    ));
    if let Some(author) = comment.last_changed_by() {
        out.push_str(&format!(
            "  changed by {} ({})",
            author.name,
            author.kind.as_str()
        ));
    }
    out.push('\n');
    for line in comment.body.trim_end().lines() {
        out.push_str(&format!("{indent}  {line}\n"));
    }
}

/// The comment's status: withdrawn, or resolved or open plus its re-anchor
/// confidence when it is not exact.
fn status(comment: &CommentState) -> String {
    if comment.deleted {
        return "withdrawn".to_string();
    }
    let mut parts = vec![if comment.resolved { "resolved" } else { "open" }];
    match comment.confidence {
        Some(Confidence::Approximate) => parts.push("shifted"),
        Some(Confidence::Outdated) => parts.push("outdated"),
        Some(Confidence::Exact) | None => {}
    }
    if let Some(disposition) = comment.disposition {
        parts.push(disposition.as_str());
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
        CommentTarget::Comment { .. } => "reply".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::render::fixture::state;

    #[test]
    fn lists_comments_number_first_threading_replies_and_tombstoning_withdrawn_roots() {
        #[rustfmt::skip]
        wince::snapshot_str!(
            render(&state()),
            "#1  open  main.rs line 2 (after)  wez (human)\n",
            "  why 3?\n",
            "  #6  open  reply  opus (agent)\n",
            "    3 is the loop bound\n",
            "#2  resolved  main.rs (whole file)  assistant (agent)\n",
            "  needs tests\n",
            "#3  open,approve  review  wez (human)\n",
            "  overall solid\n",
            "#4  open,shifted,request_changes  other.rs lines 5-6 (after)  dev (human)  changed by opus (agent)\n",
            "  moved code\n",
            "#5  withdrawn  main.rs line 9 (after)  wez (human)\n",
            "  never mind\n",
            "  #7  open  reply  dev (human)\n",
            "    still relevant though\n",
        );
    }
}
