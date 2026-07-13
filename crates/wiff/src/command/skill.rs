//! `wiff skill-path`: expand the bundled agent skill into the data directory
//! and print the path to the expanded `SKILL.md`.
//!
//! The skill prose is checked into the repository at `skills/wiff-review/` as a
//! directory that is already a usable skill for anyone with `wiff` on their
//! PATH. It is baked into the binary here so deployment is a single file copy.
//! Expansion rewrites every `wiff` command invocation to this binary's absolute
//! path, so an agent that can find the skill does not also need `wiff` on its
//! PATH.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use tempfile::NamedTempFile;
use wiff_core::session::data_dir;

/// The bundled skill, checked in as a runnable skill and baked into the binary.
const SKILL_TEMPLATE: &str = include_str!("../../../../skills/wiff-review/SKILL.md");

/// The skill's directory name under the data directory's `skills/` tree, kept in
/// step with its home in the repository.
const SKILL_NAME: &str = "wiff-review";

/// Expand the bundled skill into the data directory and print the path to the
/// written `SKILL.md`.
pub fn run() -> anyhow::Result<()> {
    let exe = std::env::current_exe()
        .context("could not determine the path to the running wiff executable")?
        .canonicalize()
        .context("could not resolve the wiff executable to an absolute path")?;
    let bin = exe
        .to_str()
        .context("the wiff executable path is not valid UTF-8")?;

    let content = expand_binary(SKILL_TEMPLATE, bin);
    let target = data_dir()?.join("skills").join(SKILL_NAME).join("SKILL.md");
    write_if_changed(&target, &content)?;
    println!("{}", target.display());
    Ok(())
}

/// Rewrite each `wiff` command invocation in `template` to `bin`, leaving prose
/// untouched. Only a `wiff` that begins a command inside a ```bash fence is
/// rewritten: one at the start of a line or right after a pipeline or sequence
/// operator. Mentions of wiff in prose, the skill's own name, and paths keep
/// their bare form.
fn expand_binary(template: &str, bin: &str) -> String {
    let mut out = String::with_capacity(template.len() + bin.len());
    let mut in_bash = false;
    for line in template.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            // A fence opening with the `bash` info string starts a runnable
            // block; any other fence (including the matching close) ends one.
            in_bash = !in_bash && trimmed.trim_start_matches('`').trim() == "bash";
            out.push_str(line);
        } else if in_bash {
            out.push_str(&rewrite_command_heads(line, bin));
        } else {
            out.push_str(line);
        }
    }
    out
}

/// Replace every `wiff` that heads a command in `line` with `bin`.
fn rewrite_command_heads(line: &str, bin: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(offset) = rest.find("wiff") {
        let after = &rest[offset + "wiff".len()..];
        if is_word_end(after) && heads_command(&out, &rest[..offset]) {
            out.push_str(&rest[..offset]);
            out.push_str(bin);
        } else {
            out.push_str(&rest[..offset + "wiff".len()]);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Whether the text following a `wiff` match ends the word, so `wiff-review` and
/// `wiffle` are not mistaken for the command.
fn is_word_end(after: &str) -> bool {
    match after.chars().next() {
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_' || c == '-'),
        None => true,
    }
}

/// Whether a `wiff` match heads a command, given everything already emitted for
/// the line (`emitted`) and the run of characters between the last emitted text
/// and the match (`between`). A command head sits at the start of the line or
/// right after a pipeline or sequence operator, allowing intervening spaces. The
/// relevant character is the last non-space one before the match, which is in
/// `between` unless that is only spaces, in which case it is in `emitted`.
fn heads_command(emitted: &str, between: &str) -> bool {
    let before = if between.trim_end().is_empty() {
        emitted.trim_end()
    } else {
        between.trim_end()
    };
    match before.chars().next_back() {
        Some(c) => matches!(c, '|' | '&' | ';' | '('),
        None => true,
    }
}

/// Write `content` to `path`, creating parent directories, but only touch the
/// file when its current bytes differ. The replacement is written to a sibling
/// temporary file and renamed into place so a concurrent reader never sees a
/// partial skill.
fn write_if_changed(path: &Path, content: &str) -> anyhow::Result<()> {
    if let Ok(existing) = std::fs::read_to_string(path)
        && existing == content
    {
        return Ok(());
    }
    let parent = path
        .parent()
        .context("the skill destination has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("could not create {}", parent.display()))?;
    let mut temp = NamedTempFile::new_in(parent)
        .with_context(|| format!("could not create a temporary file in {}", parent.display()))?;
    temp.write_all(content.as_bytes())
        .with_context(|| format!("could not write {}", temp.path().display()))?;
    temp.persist(path)
        .with_context(|| format!("could not place {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{SKILL_TEMPLATE, expand_binary, rewrite_command_heads};

    /// Expanding the bundled skill rewrites every runnable `wiff` invocation to
    /// the given binary and touches nothing else, so a second pass is a no-op.
    #[test]
    fn expanding_the_bundled_skill_is_idempotent_and_rewrites_only_commands() {
        let bin = "/opt/tools/wiff";
        let expanded = expand_binary(SKILL_TEMPLATE, bin);
        // Idempotent: the second pass finds no bare command head to rewrite,
        // which is the invariant that no runnable `wiff` invocation was missed.
        wince::assert_eq!(expand_binary(&expanded, bin), expanded.clone());
        // The invocations became the absolute path; the prose, the skill name,
        // and the example ULIDs kept their bare form.
        wince::assert_eq!(expanded.contains("/opt/tools/wiff render"), true);
        wince::assert_eq!(expanded.contains("| /opt/tools/wiff comment add"), true);
        wince::assert_eq!(expanded.contains("name: wiff-review"), true);
        wince::assert_eq!(expanded.contains("browses in the wiff TUI"), true);
        wince::assert_eq!(expanded.contains("skills/wiff-review/"), false);
    }

    /// A command head is rewritten at the start of a line and right after a
    /// pipeline or sequence operator, while a bare-word or path `wiff` is left
    /// alone.
    #[test]
    fn rewrite_targets_command_heads_only() {
        let bin = "/bin/wiff";
        wince::assert_eq!(
            rewrite_command_heads("wiff render\n", bin),
            "/bin/wiff render\n".to_string()
        );
        wince::assert_eq!(
            rewrite_command_heads("  cat x | wiff comment add\n", bin),
            "  cat x | /bin/wiff comment add\n".to_string()
        );
        wince::assert_eq!(
            rewrite_command_heads("a && wiff render; wiff render\n", bin),
            "a && /bin/wiff render; /bin/wiff render\n".to_string()
        );
        wince::assert_eq!(
            rewrite_command_heads("the wiff-review skill wraps wiff nicely\n", bin),
            "the wiff-review skill wraps wiff nicely\n".to_string()
        );
    }
}
