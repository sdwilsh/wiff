//! Drives a program inside a pseudo-terminal and records what it paints as an
//! asciicast v2 stream.
//!
//! A recording is a script of inputs interleaved with points where the harness
//! waits for the program to settle and captures whatever it painted. The
//! captured bytes are the terminal's own output, escape sequences and all, which
//! is what an asciinema player replays. Timestamps come from a logical clock
//! that advances a fixed step per capture rather than from the wall clock, so a
//! rerun over the same inputs and the same (deterministic) program produces a
//! byte-identical cast that can be committed and diffed.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// Seconds the logical clock advances per captured step, pacing the gap between
/// one frame and the next.
const STEP: f64 = 0.6;

/// Seconds the logical clock advances per typed character, giving keystrokes a
/// natural typewriter cadence rather than a whole line appearing in one step.
const TYPE_STEP: f64 = 0.09;

/// How many recent key presses the legend row shows. Enough to convey the
/// sequence of an interaction while staying on one line at the recorded widths.
const LEGEND_KEYS: usize = 6;

/// The per-word character budget for typed input. A word up to this long types
/// at the full [`TYPE_STEP`] cadence; a longer one, such as a pasted id,
/// compresses proportionally so it takes about as long as a budget-length word,
/// the way a person pastes a long string rather than keying it letter by letter.
const TYPE_WORD_CHARS: f64 = 9.0;

/// One recorded terminal write: the logical time it plays at and the bytes the
/// program painted.
struct Event {
    time: f64,
    data: String,
}

/// What the reserved bottom row shows after a captured frame.
enum Legend {
    /// Nothing has been drawn there yet; the row stays as the terminal left it.
    Empty,
    /// The row is painted blank, erasing a previous sequence while no keys are
    /// active, which the shell needs since it never repaints the reserved row
    /// itself.
    Cleared,
    /// A text note -- an opening scene label or an author-written status line --
    /// shown until the first key press replaces it with the key-press sequence.
    Message(String),
    /// The recent key presses, oldest first, shown as a bar of names.
    Keys(Vec<String>),
}

/// A pseudo-terminal running the program under recording, its output collected
/// on a reader thread into `buffer` until the script captures it.
pub struct Recorder {
    writer: Box<dyn Write + Send>,
    buffer: Arc<Mutex<Vec<u8>>>,
    /// A trailing byte sequence held back from a capture that ended part way
    /// through a multi-byte character, prepended to the next capture so every
    /// recorded event decodes as valid UTF-8.
    holdover: Vec<u8>,
    events: Vec<Event>,
    clock: f64,
    width: u16,
    /// The recorded terminal height, one row taller than the child's pty: the
    /// extra bottom row is reserved for the key legend the child never paints.
    height: u16,
    /// The key-press legend drawn into the reserved bottom row, redrawn after
    /// every captured frame since the child repaints over it.
    overlay: Legend,
    /// The running child, kept so teardown can wait for it to exit.
    child: Box<dyn Child + Send + Sync>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The pty master, held open for the recording's duration.
    _master: Box<dyn MasterPty + Send>,
}

impl Drop for Recorder {
    /// Stop the child and wait for it to exit before returning, so a lingering
    /// shell cannot go on touching the stage a later scenario is about to clear.
    fn drop(&mut self) {
        let _ = self.killer.kill();
        let _ = self.child.wait();
    }
}

impl Recorder {
    /// Spawn `command` in a pty of `width` by `height` and begin collecting its
    /// output. The command's environment and working directory are taken as
    /// already configured on the builder. The recorded cast is one row taller
    /// than the child's pty; that extra bottom row holds the key legend, out of
    /// reach of a child that believes the terminal is `height` rows.
    pub fn spawn(command: CommandBuilder, width: u16, height: u16) -> Result<Self> {
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: height,
                cols: width,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pty")?;
        let child = pair.slave.spawn_command(command).context("spawning")?;
        let killer = child.clone_killer();
        // The child owns the slave; dropping our handle lets its exit close the
        // reader cleanly once the last write drains.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().context("cloning reader")?;
        let writer = pair.master.take_writer().context("taking writer")?;
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buffer);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = reader.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock()
                    .expect("recorder buffer")
                    .extend_from_slice(&chunk[..n]);
            }
        });
        Ok(Self {
            writer,
            buffer,
            holdover: Vec::new(),
            events: Vec::new(),
            clock: 0.0,
            width,
            height: height + 1,
            overlay: Legend::Empty,
            child,
            killer,
            _master: pair.master,
        })
    }

    /// Show `title` in the reserved bottom row as the scenario's opening label,
    /// held across captured frames until the first key press replaces it with
    /// the key-press sequence.
    pub fn set_now_playing(&mut self, title: impl Into<String>) {
        self.overlay = Legend::Message(format!("now playing: {}", title.into()));
    }

    /// Show `text` in the reserved bottom row as an author-written status note,
    /// painting it as its own frame so a following pause holds it on screen. The
    /// next key press replaces it with the key-press sequence.
    pub fn set_status(&mut self, text: impl Into<String>) {
        self.overlay = Legend::Message(text.into());
        self.paint_overlay();
    }

    /// Append `label` to the key-press legend painted into the reserved bottom
    /// row after each subsequent captured frame, keeping the last
    /// [`LEGEND_KEYS`] presses.
    pub fn push_key(&mut self, label: impl Into<String>) {
        let mut keys = match std::mem::replace(&mut self.overlay, Legend::Empty) {
            Legend::Keys(keys) => keys,
            _ => Vec::new(),
        };
        keys.push(label.into());
        let overflow = keys.len().saturating_sub(LEGEND_KEYS);
        keys.drain(..overflow);
        self.overlay = Legend::Keys(keys);
    }

    /// Reset an active key-press sequence, blanking the reserved row so the next
    /// interaction starts a fresh legend. A text note, or an already-blank row,
    /// is left in place so it survives until the first key is actually pressed.
    pub fn clear_overlay(&mut self) {
        if matches!(self.overlay, Legend::Keys(_)) {
            self.overlay = Legend::Cleared;
        }
    }

    /// Send `input` to the program as typed bytes.
    pub fn send(&mut self, input: &str) -> Result<()> {
        self.writer
            .write_all(input.as_bytes())
            .context("writing to the pty")?;
        self.writer.flush().context("flushing the pty")
    }

    /// Type `text` one character at a time, recording each character's echo as
    /// its own short step so it plays back with a typewriter cadence. Does not
    /// send a trailing newline; follow with [`send`](Self::send) and
    /// [`capture`](Self::capture) to run the line and record its output.
    pub fn type_text(&mut self, text: &str) -> Result<()> {
        let idle = Duration::from_millis(40);
        let deadline = Duration::from_millis(500);
        for (ch, step) in char_steps(text) {
            self.send(&ch.to_string())?;
            self.settle(idle, deadline);
            self.emit(step);
        }
        Ok(())
    }

    /// Advance the playback clock by `seconds` without recording anything,
    /// holding the last frame on screen. Used to dwell on a finished command
    /// line before it runs and to let an abrupt transition, such as the TUI
    /// taking the screen, settle.
    pub fn pause(&mut self, seconds: f64) {
        self.clock += seconds;
    }

    /// Wait for the program to stop painting -- no new output for `idle`, or
    /// `deadline` elapsed -- then record everything it painted as one step.
    pub fn capture(&mut self, idle: Duration, deadline: Duration) {
        self.settle(idle, deadline);
        self.emit(STEP);
    }

    /// Wait for the program to settle, then discard what it painted without
    /// recording it and reset the clock. Used to swallow prologue output such as
    /// prompt setup before the recording proper begins.
    pub fn discard(&mut self, idle: Duration, deadline: Duration) {
        self.settle(idle, deadline);
        self.buffer.lock().expect("recorder buffer").clear();
        self.holdover.clear();
        self.clock = 0.0;
    }

    /// Block until the output buffer goes `idle` without growth, or `deadline`
    /// passes since the call.
    fn settle(&self, idle: Duration, deadline: Duration) {
        let start = Instant::now();
        let mut last_len = self.buffer.lock().expect("recorder buffer").len();
        let mut last_change = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let len = self.buffer.lock().expect("recorder buffer").len();
            if len != last_len {
                last_len = len;
                last_change = Instant::now();
            } else if last_change.elapsed() >= idle {
                return;
            }
            if start.elapsed() >= deadline {
                return;
            }
        }
    }

    /// Drain the buffer, emitting its bytes as an event at the current logical
    /// time and advancing the clock by `advance` seconds. Nothing to emit leaves
    /// the clock untouched. A trailing partial character is held back for the
    /// next capture so the event this emits decodes as valid UTF-8.
    fn emit(&mut self, advance: f64) {
        let captured = std::mem::take(&mut *self.buffer.lock().expect("recorder buffer"));
        let mut bytes = std::mem::take(&mut self.holdover);
        bytes.extend_from_slice(&captured);
        let split = complete_utf8_len(&bytes);
        self.holdover = bytes.split_off(split);
        if bytes.is_empty() {
            return;
        }
        if let Some(overlay) = self.overlay_sequence() {
            bytes.extend_from_slice(overlay.as_bytes());
        }
        self.events.push(Event {
            time: self.clock,
            data: String::from_utf8_lossy(&bytes).into_owned(),
        });
        self.clock += advance;
    }

    /// Emit the current overlay as a standalone frame, for a status note that
    /// must appear on its own with no program output in the same frame.
    fn paint_overlay(&mut self) {
        if let Some(overlay) = self.overlay_sequence() {
            self.events.push(Event {
                time: self.clock,
                data: overlay,
            });
            self.clock += STEP;
        }
    }

    /// The escape sequence that paints the current bottom-row overlay -- a text
    /// note or the key-press legend -- or `None` before anything has been drawn
    /// there. A cursor
    /// save and restore brackets the paint so the child's own cursor is left
    /// where it stood, and the row background is filled with the erase-to-line so
    /// the legend reads as a bar the whole width across.
    fn overlay_sequence(&self) -> Option<String> {
        let row = self.height;
        match &self.overlay {
            Legend::Empty => None,
            Legend::Cleared => Some(format!("\x1b7\x1b[{row};1H\x1b[m\x1b[2K\x1b8")),
            Legend::Message(text) => Some(format!(
                "\x1b7\x1b[{row};1H\x1b[48;5;238m\x1b[38;5;252m\x1b[K  {text}\x1b[0m\x1b8"
            )),
            Legend::Keys(keys) => Some(format!(
                "\x1b7\x1b[{row};1H\x1b[48;5;238m\x1b[38;5;252m\x1b[K  key presses: {}\x1b[0m\x1b8",
                keys.join(" ")
            )),
        }
    }

    /// Write the collected events to `path` as an asciicast v2 file.
    pub fn write_cast(&self, path: &Path) -> Result<()> {
        let header = serde_json::json!({
            "version": 2,
            "width": self.width,
            "height": self.height,
            "timestamp": 0,
            "env": { "TERM": "xterm-256color", "SHELL": "/bin/bash" },
        });
        let mut out = String::new();
        out.push_str(&serde_json::to_string(&header)?);
        out.push('\n');
        for event in &self.events {
            let line = serde_json::json!([event.time, "o", event.data]);
            out.push_str(&serde_json::to_string(&line)?);
            out.push('\n');
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))
    }
}

/// Pairs each character of `text` with the seconds to advance the clock after
/// typing it. Characters in one word share a per-word time budget: a word up to
/// [`TYPE_WORD_CHARS`] long keeps the full [`TYPE_STEP`] per character, and a
/// longer one scales down in proportion so the whole word takes about the same
/// time. Whitespace between words always keeps the full step.
fn char_steps(text: &str) -> Vec<(char, f64)> {
    let chars: Vec<char> = text.chars().collect();
    let mut steps = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() {
            steps.push((chars[i], TYPE_STEP));
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        let scale = (TYPE_WORD_CHARS / (i - start) as f64).min(1.0);
        for &ch in &chars[start..i] {
            steps.push((ch, TYPE_STEP * scale));
        }
    }
    steps
}

/// Length of the longest prefix of `bytes` that is complete, valid UTF-8. A
/// capture that stops mid-write can split a multi-byte character; only its
/// completed prefix is safe to decode now, with the remainder held back for the
/// next capture. Genuinely invalid bytes, which terminal output should never
/// contain, count as complete and fall to a lossy decode rather than being held
/// back forever waiting for a completion that never comes.
fn complete_utf8_len(bytes: &[u8]) -> usize {
    match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bytes.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_utf8_len_holds_back_only_a_split_character() {
        // A three-byte character split after its lead byte holds the incomplete
        // tail back for the next capture.
        let snowman = "hi\u{2603}".as_bytes();
        assert_eq!(complete_utf8_len(b"hello"), 5);
        assert_eq!(complete_utf8_len(&snowman[..3]), 2);
        assert_eq!(complete_utf8_len(snowman), snowman.len());
        // A genuinely invalid byte counts as complete rather than held forever.
        assert_eq!(complete_utf8_len(&[0xff]), 1);
    }

    #[test]
    fn char_steps_scales_within_a_word_and_keeps_whitespace_whole() {
        assert_eq!(
            char_steps("a b"),
            vec![('a', TYPE_STEP), (' ', TYPE_STEP), ('b', TYPE_STEP)]
        );
        // A word longer than the budget shares one budget across its characters.
        let word = "abcdefghijkl";
        let scale = TYPE_WORD_CHARS / word.chars().count() as f64;
        let expected: Vec<_> = word.chars().map(|ch| (ch, TYPE_STEP * scale)).collect();
        assert_eq!(char_steps(word), expected);
    }
}
