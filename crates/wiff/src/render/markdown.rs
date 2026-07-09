//! The markdown rendering of a folded review, for a human or an agent prompt.

use wiff_core::record::{Anchor, CommentTarget, Confidence};
use wiff_core::review::{CommentState, ReviewState};

use super::live_comments;

/// Render `state` as markdown.
pub(super) fn render(state: &ReviewState) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Review {}\n\n", state.session.ulid));
    out.push_str(&format!("- project: {}\n", state.session.project));
    out.push_str(&format!("- source: {}\n", state.session.source.as_str()));
    match state.latest_version() {
        Some(version) => out.push_str(&format!(
            "- version: v{} ({} file{})\n",
            version.number,
            version.files.len(),
            plural(version.files.len()),
        )),
        None => out.push_str("- version: none\n"),
    }

    let comments = live_comments(state);
    out.push_str("\n## Comments\n");
    if comments.is_empty() {
        out.push_str("\nNo comments.\n");
        return out;
    }
    for (heading, group) in group_by_file(&comments) {
        out.push_str(&format!("\n### {heading}\n\n"));
        for comment in group {
            out.push_str(&comment_block(comment));
        }
    }
    out
}

/// One comment rendered as a bullet with its location, attribution, state
/// flags, body, and, for an anchored line range, a fenced context block.
fn comment_block(comment: &CommentState) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "- {} by {} ({}){}\n",
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
    const LANG_BY_EXT: &[(&str, &str)] = &[
        (".rs", "rust"),
        (".toml", "toml"),
        (".md", "markdown"),
        (".py", "python"),
        (".js", "javascript"),
        (".ts", "typescript"),
        (".sh", "bash"),
        (".json", "json"),
        (".yaml", "yaml"),
        (".yml", "yaml"),
    ];
    LANG_BY_EXT
        .iter()
        .find(|(ext, _)| path.ends_with(ext))
        .map(|(_, lang)| *lang)
        .unwrap_or("")
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
    }
}

/// The trailing state flags for a comment: resolved, and re-anchor confidence
/// when it is not exact.
fn flags(comment: &CommentState) -> String {
    let mut flags = Vec::new();
    if comment.resolved {
        flags.push("resolved");
    }
    match comment.confidence {
        Some(Confidence::Approximate) => flags.push("shifted"),
        Some(Confidence::Outdated) => flags.push("outdated"),
        Some(Confidence::Exact) | None => {}
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
    fn markdown_groups_comments_and_hides_deleted() {
        let out = render(&state());
        let expected = "\
# Review 00000000000000000000000000

- project: demo
- source: git_worktree
- version: v0 (1 file)

## Comments

### Review

- review by wez (human)
  overall solid

### main.rs

- whole file by assistant (agent)
  needs tests
- line 2 (after) by wez (human)
  why 3?

  ```rust
       1 | let a = 1;
  >    2 | let b = 3;
       3 | let c = 4;
  ```

### other.rs

- lines 5-6 (after) by dev (human) [shifted]
  moved code
";
        k9::assert_equal!(out, expected.to_string());
    }
}
