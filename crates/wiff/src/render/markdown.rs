//! The markdown rendering of a folded review, for a human or an agent prompt.

use std::collections::HashMap;

use ulid::Ulid;
use wiff_core::record::{Anchor, CommentTarget};
use wiff_core::review::{CommentState, ReviewState};

use super::visible_threads;

/// Render `state` as markdown. Comments group by file, and within each group a
/// thread renders as its root followed by its replies. A withdrawn reply drops
/// out; a withdrawn root shows as a tombstone while it still has a live reply,
/// and disappears once its whole thread is withdrawn.
pub(super) fn render(state: &ReviewState) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Review {}\n\n", state.session.ulid));
    out.push_str(&format!("- project: {}\n", state.session.project));
    out.push_str(&format!("- source: {}\n", state.session.source.describe()));
    match state.latest_version() {
        Some(version) => out.push_str(&format!(
            "- version: v{} ({} file{})\n",
            version.number,
            version.files.len(),
            plural(version.files.len()),
        )),
        None => out.push_str("- version: none\n"),
    }

    if let Some(description) = &state.description {
        // The `## Description` heading is wiff's own; the title renders as its
        // bold lead beneath it. The body is emitted as the markdown it is, the
        // same as a comment body.
        out.push_str("\n## Description\n\n");
        let title = description.content.title.trim();
        if !title.is_empty() {
            out.push_str(&format!("**{title}**\n"));
        }
        let body = description.content.body.trim_end();
        if !body.trim().is_empty() {
            if !title.is_empty() {
                out.push('\n');
            }
            for line in body.lines() {
                out.push_str(&format!("{line}\n"));
            }
        }
    }

    if !state.verdicts.is_empty() {
        out.push_str("\n## Verdicts\n\n");
        for verdict in &state.verdicts {
            out.push_str(&format!(
                "- {} ({}): {}\n",
                verdict.author.name,
                verdict.author.kind.as_str(),
                verdict.disposition.as_str(),
            ));
        }
    }

    let mut roots: Vec<&CommentState> = Vec::new();
    let mut replies: HashMap<Ulid, Vec<&CommentState>> = HashMap::new();
    for thread in visible_threads(state) {
        roots.push(thread.root);
        replies.insert(thread.root.id, thread.replies);
    }

    out.push_str("\n## Comments\n");
    if roots.is_empty() {
        out.push_str("\nNo comments.\n");
        return out;
    }
    for (heading, group) in group_by_file(&roots) {
        out.push_str(&format!("\n### {heading}\n\n"));
        for comment in group {
            out.push_str(&comment_block(comment));
            for reply in replies.get(&comment.id).into_iter().flatten() {
                out.push_str(&reply_block(reply));
            }
        }
    }
    out
}

/// Renders one comment as a markdown bullet: its handle, metadata, body, and
/// code context. The handle leads the bullet so a reader can act on the comment
/// (resolve or withdraw it) straight from this rendering.
fn comment_block(comment: &CommentState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "- {} {} by {} ({}){}\n",
        comment.handle(),
        location(&comment.target),
        comment.author.name,
        comment.author.kind.as_str(),
        flags(comment),
    ));
    for line in comment.body.trim_end().lines() {
        out.push_str(&format!("  {line}\n"));
    }
    if let Some(anchor) = &comment.anchor {
        out.push_str(&anchor_block(anchor, &comment.target));
    }
    out
}

/// Renders one reply as an indented sub-bullet under its root. A reply has no
/// location or anchor of its own; it belongs to the thread it is nested under.
fn reply_block(reply: &CommentState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "  - {} reply by {} ({}){}\n",
        reply.handle(),
        reply.author.name,
        reply.author.kind.as_str(),
        flags(reply),
    ));
    for line in reply.body.trim_end().lines() {
        out.push_str(&format!("    {line}\n"));
    }
    out
}

/// The anchored context as a fenced, language-tagged block with a line-number
/// gutter, a chevron in the margin marking the lines the comment is about.
fn anchor_block(anchor: &Anchor, target: &CommentTarget) -> String {
    let (file, start_line) = match target {
        CommentTarget::Lines {
            file, start_line, ..
        } => (file.as_str(), start_line.get()),
        _ => ("", 1),
    };
    let mut out = format!("\n  ```{}\n", language_for(file));
    let before = anchor.context_before.len() as u32;
    let snippet = anchor.context_before.len()..anchor.context_before.len() + anchor.snippet.len();
    let mut number = start_line.saturating_sub(before).max(1);
    for (index, line) in anchor
        .context_before
        .iter()
        .chain(&anchor.snippet)
        .chain(&anchor.context_after)
        .enumerate()
    {
        let marker = if snippet.contains(&index) { '>' } else { ' ' };
        out.push_str(&format!("  {marker} {number:>4} | {line}\n"));
        number += 1;
    }
    out.push_str("  ```\n");
    out
}

/// The markdown fence language for a path's extension, or empty when unknown.
fn language_for(path: &str) -> &'static str {
    wiff_diff::fence_language(path).unwrap_or("")
}

/// The review-level comments first (they set the tone for the rest), then one
/// group per file ordered by path. Within a file, whole-file comments precede
/// line-range comments; comments otherwise keep their creation order.
fn group_by_file<'a>(comments: &[&'a CommentState]) -> Vec<(String, Vec<&'a CommentState>)> {
    let mut review: Vec<&'a CommentState> = Vec::new();
    let mut groups: Vec<(String, Vec<&'a CommentState>)> = Vec::new();
    for comment in comments {
        let file = match &comment.target {
            CommentTarget::Lines { file, .. } | CommentTarget::File { file } => file.clone(),
            CommentTarget::Review => {
                review.push(comment);
                continue;
            }
            // A reply is placed by its thread, not grouped on its own.
            CommentTarget::Comment { .. } => continue,
        };
        match groups.iter_mut().find(|(heading, _)| *heading == file) {
            Some((_, group)) => group.push(comment),
            None => groups.push((file, vec![comment])),
        }
    }
    groups.sort_by(|(a, _), (b, _)| a.cmp(b));
    for (_, group) in &mut groups {
        group.sort_by_key(|comment| target_rank(&comment.target));
    }

    let mut ordered = Vec::new();
    if !review.is_empty() {
        ordered.push(("Review".to_string(), review));
    }
    ordered.extend(groups);
    ordered
}

/// Within a file group, whole-file comments sort before line-range comments.
fn target_rank(target: &CommentTarget) -> u8 {
    match target {
        CommentTarget::File { .. } => 0,
        CommentTarget::Lines { .. } => 1,
        CommentTarget::Review => 2,
        // group_by_file threads replies through their root, so a reply never
        // reaches this ranking.
        CommentTarget::Comment { .. } => unreachable!("replies are threaded, not grouped"),
    }
}

/// The human-readable location of a comment's target within its file section.
fn location(target: &CommentTarget) -> String {
    match target {
        CommentTarget::Lines {
            side,
            start_line,
            end_line,
            ..
        } => {
            let side = side.as_str();
            if start_line == end_line {
                format!("line {start_line} ({side})")
            } else {
                format!("lines {start_line}-{end_line} ({side})")
            }
        }
        CommentTarget::File { .. } => "whole file".to_string(),
        CommentTarget::Review => "review".to_string(),
        // group_by_file threads replies through their root, so a reply never
        // reaches this location lookup.
        CommentTarget::Comment { .. } => unreachable!("replies are threaded, not grouped"),
    }
}

/// The trailing state flags for a comment: withdrawn with who withdrew it, or
/// resolved with who resolved it, re-anchor confidence when it is not exact, and
/// who last changed it when that was someone other than its author.
fn flags(comment: &CommentState) -> String {
    if comment.deleted {
        return match &comment.deleted_by {
            Some(author) => {
                format!(" [withdrawn by {} ({})]", author.name, author.kind.as_str())
            }
            None => " [withdrawn]".to_string(),
        };
    }
    let mut flags = Vec::new();
    if comment.resolved {
        flags.push(match &comment.resolved_by {
            Some(author) => format!("resolved by {} ({})", author.name, author.kind.as_str()),
            None => "resolved".to_string(),
        });
    }
    if let Some(flag) = comment.confidence.and_then(|c| c.flag()) {
        flags.push(flag.to_string());
    }
    if let Some(disposition) = comment.disposition {
        flags.push(disposition.as_str().to_string());
    }
    if let Some(author) = comment.last_changed_by() {
        flags.push(format!(
            "changed by {} ({})",
            author.name,
            author.kind.as_str()
        ));
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", flags.join(", "))
    }
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

#[cfg(test)]
mod tests {
    use super::render;
    use crate::render::fixture::state;

    #[test]
    fn markdown_groups_comments_threading_replies_and_tombstoning_withdrawn_roots() {
        #[rustfmt::skip]
        wince::snapshot_str!(
            render(&state()),
            "# Review 00000000000000000000000000\n",
            "\n",
            "- project: demo\n",
            "- source: git worktree\n",
            "- version: v0 (1 file)\n",
            "\n",
            "## Description\n",
            "\n",
            "**Tidy the parser**\n",
            "\n",
            "Split the lexer out and cover it with tests.\n",
            "\n",
            "## Verdicts\n",
            "\n",
            "- wez (human): approve\n",
            "- dev (human): request_changes\n",
            "\n",
            "## Comments\n",
            "\n",
            "### Review\n",
            "\n",
            "- #3 review by wez (human) [approve]\n",
            "  overall solid\n",
            "\n",
            "### main.rs\n",
            "\n",
            "- #2 whole file by assistant (agent) [resolved by wez (human)]\n",
            "  needs tests\n",
            "- #1 line 2 (after) by wez (human)\n",
            "  why 3?\n",
            "\n",
            "  ```rust\n",
            "       1 | let a = 1;\n",
            "  >    2 | let b = 3;\n",
            "       3 | let c = 4;\n",
            "  ```\n",
            "  - #6 reply by opus (agent)\n",
            "    3 is the loop bound\n",
            "- #5 line 9 (after) by wez (human) [withdrawn by wez (human)]\n",
            "  never mind\n",
            "  - #7 reply by dev (human)\n",
            "    still relevant though\n",
            "\n",
            "### other.rs\n",
            "\n",
            "- #4 lines 5-6 (after) by dev (human) [shifted, request_changes, changed by opus (agent)]\n",
            "  moved code\n",
        );
    }
}
