//! Parses a `.scenario` file into a recordable [`Scenario`].
//!
//! A scenario file is two sections. `%prep` is a bash script that builds the
//! throwaway repository the recording runs against; it is handed to a real shell
//! with a deterministic environment already set, so it reads like the commands a
//! person would type. `%record` is the script the recorder replays: shell
//! commands to run and keys to press. Keeping a screencast as a data file means
//! a new one is a new file, not new Rust.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use termwiz::input::{KeyCode, KeyCodeEncodeModes, KeyboardEncoding, Modifiers};
use wiff_tui::{Key, KeyPress};

use crate::scenario::{Scenario, Step};

/// The instant deterministic ids and time anchor at when a file names no `%now`.
const DEFAULT_EPOCH: &str = "2025-06-01T12:00:00Z";

/// The default dwell after a command finishes typing, before its enter is sent,
/// when no `%pause-next-enter` precedes it.
const DEFAULT_READ_LINE: f64 = 0.8;

/// How deep `%prep` includes may nest before we assume a cycle.
const MAX_INCLUDE_DEPTH: usize = 8;

/// Parse the `.scenario` file at `path` into a [`Scenario`], resolving `%prep`
/// includes against the file's own directory.
pub fn parse(path: &Path) -> Result<Scenario> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("scenario file has no name")?
        .to_string();
    let dir = path.parent().unwrap_or(Path::new("."));
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading scenario {}", path.display()))?;

    let mut size: Option<(u16, u16)> = None;
    let mut now = DEFAULT_EPOCH.to_string();
    let mut prep = String::new();
    let mut record = Vec::new();

    let mut section = Section::Header;
    let mut heredoc: Option<Heredoc> = None;
    for raw in text.lines() {
        let trimmed = raw.trim();
        match section {
            Section::Header => {
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if trimmed == "%prep" {
                    section = Section::Prep;
                } else if let Some(rest) = trimmed.strip_prefix("%size") {
                    size = Some(parse_size(rest)?);
                } else if let Some(rest) = trimmed.strip_prefix("%now") {
                    now = rest.trim().to_string();
                } else {
                    bail!("unexpected header line: {raw}");
                }
            }
            Section::Prep => {
                if heredoc.is_none() {
                    if trimmed == "%record" {
                        section = Section::Record;
                        continue;
                    }
                    if let Some(file) = include_target(trimmed) {
                        expand_include(dir, file, &mut prep, 0)?;
                        continue;
                    }
                }
                track_heredoc(&mut heredoc, raw);
                prep.push_str(raw);
                prep.push('\n');
            }
            Section::Record => record.push(raw.to_string()),
        }
    }

    if section == Section::Header {
        bail!("scenario has no %prep section");
    }
    let (width, height) = size.context("scenario is missing a %size directive")?;
    let steps = parse_record(&record)?;
    Ok(Scenario {
        name,
        width,
        height,
        now,
        prep,
        steps,
    })
}

/// Which part of the file the parser is reading.
#[derive(PartialEq)]
enum Section {
    Header,
    Prep,
    Record,
}

/// An open heredoc: the delimiter that closes it, and whether leading tabs are
/// stripped (a `<<-` heredoc), so an indented closing delimiter still matches.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
}

/// Parse a `%size` argument, given as `WIDTH HEIGHT` or `WIDTHxHEIGHT`.
fn parse_size(rest: &str) -> Result<(u16, u16)> {
    let parts: Vec<&str> = rest
        .split(|c: char| c.is_whitespace() || c == 'x' || c == 'X')
        .filter(|p| !p.is_empty())
        .collect();
    match parts.as_slice() {
        [w, h] => Ok((
            w.parse().with_context(|| format!("invalid width '{w}'"))?,
            h.parse().with_context(|| format!("invalid height '{h}'"))?,
        )),
        _ => bail!("expected `%size WIDTH HEIGHT`, got '{}'", rest.trim()),
    }
}

/// The file a prep line includes, if it is a `. FILE` or `source FILE` line.
fn include_target(line: &str) -> Option<&str> {
    let rest = line
        .strip_prefix(". ")
        .or_else(|| line.strip_prefix("source "))?;
    let file = rest.trim();
    (!file.is_empty()).then_some(file)
}

/// Append the contents of an included fixture to `prep`, expanding any includes
/// it makes in turn. Each file tracks its own heredocs, so an include-like line
/// inside a heredoc body is left alone.
fn expand_include(dir: &Path, file: &str, prep: &mut String, depth: usize) -> Result<()> {
    if depth >= MAX_INCLUDE_DEPTH {
        bail!("prep includes nested too deeply at '{file}'");
    }
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading included fixture {}", path.display()))?;
    let mut heredoc: Option<Heredoc> = None;
    for raw in text.lines() {
        if heredoc.is_none()
            && let Some(nested) = include_target(raw.trim())
        {
            expand_include(dir, nested, prep, depth + 1)?;
            continue;
        }
        track_heredoc(&mut heredoc, raw);
        prep.push_str(raw);
        prep.push('\n');
    }
    Ok(())
}

/// Update the open-heredoc state for `line`: close an open heredoc when the line
/// is its delimiter, or open one when the line starts a `<<` heredoc.
fn track_heredoc(heredoc: &mut Option<Heredoc>, line: &str) {
    if let Some(open) = heredoc {
        let candidate = if open.strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line
        };
        if candidate == open.delimiter {
            *heredoc = None;
        }
        return;
    }
    if let Some(open) = heredoc_start(line) {
        *heredoc = Some(open);
    }
}

/// The heredoc a line opens, if any: the delimiter word directly after the `<<`
/// (or `<<-`) redirection operator, with a surrounding quote dropped.
///
/// This is a deliberately shallow scan, not a shell parse. It treats `<<` as an
/// opener only when it stands as the redirection operator (at the line start or
/// after whitespace) and the delimiter follows immediately, so `<<<` here-string
/// and `<<=` arithmetic are excluded. It does not track quoting, so a `<<EOT`
/// written inside a string literal would still be read as an opener; scenario
/// prep has no reason to do that.
fn heredoc_start(line: &str) -> Option<Heredoc> {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(rel) = line[from..].find("<<") {
        let pos = from + rel;
        from = pos + 2;
        let stands_alone = pos == 0 || bytes[pos - 1].is_ascii_whitespace();
        let after = &line[pos + 2..];
        if !stands_alone || after.starts_with('<') || after.starts_with('=') {
            continue;
        }
        let (strip_tabs, after) = match after.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, after),
        };
        let after = after
            .strip_prefix('\'')
            .or_else(|| after.strip_prefix('"'))
            .unwrap_or(after);
        let delimiter: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !delimiter.is_empty() {
            return Some(Heredoc {
                delimiter,
                strip_tabs,
            });
        }
    }
    None
}

/// Turn the `%record` lines into the steps the recorder replays.
fn parse_record(lines: &[String]) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut next_read_line: Option<f64> = None;
    for raw in lines {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(command) = trimmed.strip_prefix('$') {
            steps.push(Step::Command {
                line: command.trim_start().to_string(),
                read_line: next_read_line.take().unwrap_or(DEFAULT_READ_LINE),
            });
        } else if let Some(rest) = trimmed.strip_prefix("%keys") {
            for token in rest.split_whitespace() {
                let (bytes, label) = key(token)?;
                steps.push(Step::Press { bytes, label });
            }
        } else if let Some(rest) = trimmed.strip_prefix("%pause-next-enter") {
            next_read_line = Some(parse_seconds(rest)?);
        } else if let Some(rest) = trimmed.strip_prefix("%pause") {
            steps.push(Step::Pause(parse_seconds(rest)?));
        } else if let Some(rest) = trimmed.strip_prefix("%status") {
            steps.push(Step::Status(rest.trim().to_string()));
        } else if let Some(rest) = raw.trim_start().strip_prefix("%type") {
            steps.push(Step::Type(
                rest.strip_prefix(' ').unwrap_or(rest).to_string(),
            ));
        } else {
            bail!("unexpected record line: {raw}");
        }
    }
    Ok(steps)
}

/// Parse the seconds argument of a `%pause` or `%pause-next-enter` directive.
fn parse_seconds(rest: &str) -> Result<f64> {
    rest.trim()
        .parse()
        .with_context(|| format!("invalid duration '{}'", rest.trim()))
}

/// The bytes a `%keys` token sends and the label the key legend shows for it.
/// The token is a key press in wiff's own binding syntax (`j`, `ctrl-d`,
/// `enter`): its bytes are the xterm-style sequence a terminal sends for that
/// press, and its label is the press written back in that same syntax.
fn key(token: &str) -> Result<(String, String)> {
    let press: KeyPress = token.parse().map_err(|e: String| anyhow!("{e}"))?;
    Ok((encode(&press)?, press.to_string()))
}

/// Encode a key press as the xterm-style byte sequence a terminal sends for it,
/// letting termwiz serialize the mapped key rather than hand-maintaining a
/// table of control and escape sequences.
fn encode(press: &KeyPress) -> Result<String> {
    let mut mods = Modifiers::NONE;
    mods.set(Modifiers::CTRL, press.ctrl);
    mods.set(Modifiers::ALT, press.alt);
    mods.set(Modifiers::SHIFT, press.shift);
    let modes = KeyCodeEncodeModes {
        encoding: KeyboardEncoding::Xterm,
        application_cursor_keys: false,
        newline_mode: false,
        modify_other_keys: None,
    };
    keycode(press.key)
        .encode(mods, modes, true)
        .map_err(|e| anyhow!("encoding {press}: {e}"))
}

/// Map wiff's key model onto termwiz's, whose `encode` knows the byte sequence
/// each key and modifier combination sends.
fn keycode(key: Key) -> KeyCode {
    match key {
        Key::Char(c) => KeyCode::Char(c),
        Key::Function(n) => KeyCode::Function(n),
        Key::Enter => KeyCode::Enter,
        Key::Escape => KeyCode::Escape,
        Key::Tab => KeyCode::Tab,
        Key::Backspace => KeyCode::Backspace,
        Key::Delete => KeyCode::Delete,
        Key::Insert => KeyCode::Insert,
        Key::Left => KeyCode::LeftArrow,
        Key::Right => KeyCode::RightArrow,
        Key::Up => KeyCode::UpArrow,
        Key::Down => KeyCode::DownArrow,
        Key::Home => KeyCode::Home,
        Key::End => KeyCode::End,
        Key::PageUp => KeyCode::PageUp,
        Key::PageDown => KeyCode::PageDown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delimiter and tab-stripping flag a line's heredoc opener yields.
    fn opener(line: &str) -> Option<(String, bool)> {
        heredoc_start(line).map(|h| (h.delimiter, h.strip_tabs))
    }

    #[test]
    fn heredoc_openers_are_recognized() {
        assert_eq!(opener("cat <<EOF"), Some(("EOF".to_string(), false)));
        assert_eq!(opener("cat <<-EOF"), Some(("EOF".to_string(), true)));
        assert_eq!(opener("cat <<'EOF'"), Some(("EOF".to_string(), false)));
        assert_eq!(
            opener("cat <<\"END\" >out"),
            Some(("END".to_string(), false))
        );
        assert_eq!(
            opener("cat > file <<DATA"),
            Some(("DATA".to_string(), false))
        );
    }

    #[test]
    fn non_openers_are_ignored() {
        assert_eq!(opener("echo no heredoc here"), None);
        assert_eq!(opener("grep <<< word"), None);
        assert_eq!(opener("x=$((1<<2))"), None);
    }
}
