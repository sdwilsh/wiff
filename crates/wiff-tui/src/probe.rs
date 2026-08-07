//! Reads the terminal background color at startup via OSC 11, returning an
//! [`Rgb`] or `None` when the terminal does not report it.
//!
//! A terminal answers an OSC 11 background query on the same input stream as
//! keystrokes, and many terminals answer nothing at all. To avoid waiting on a
//! reply that never comes, the query is followed by a Primary Device Attributes
//! (DA1) query, which every real terminal answers. A terminal replies to the
//! two queries in the order it received them, so the OSC 11 answer precedes the
//! DA1 that follows it: a DA1 reply with no background ahead of it means the
//! terminal does not support the query rather than that its reply is still on
//! its way. Decoding is separated from the tty read loop, letting unit tests
//! drive it with synthetic input.
//!
//! The read consumes and discards anything else on the tty during this brief
//! window, so keys typed ahead of the first draw (during a slow session load,
//! say) are lost. The reply arrives within milliseconds on a real terminal, so
//! the window is small, and type-ahead into a review that has not yet drawn is
//! rare.

use std::io::{IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
use termwiz::escape::csi::{CsiParam, Device};
use termwiz::escape::osc::{ColorOrQuery, DynamicColorNumber, OperatingSystemCommand};
use termwiz::escape::parser::Parser;
use termwiz::escape::{Action, CSI};
use wiff_diff::Rgb;

/// How long to wait for the terminal's replies before giving up. A real
/// terminal answers within milliseconds even over SSH, and the read stops the
/// moment the reply arrives, so this bounds only the stall on a terminal that
/// answers nothing. It is kept short because that stall is a blank screen before
/// the review draws, and the cost of giving up early is merely keeping the dark
/// theme, which the reviewer can change with the theme picker.
const REPLY_DEADLINE: Duration = Duration::from_millis(250);

/// Query the terminal for its background color, returning `None` when it does
/// not answer: stdout is not a terminal, `TERM` is unset or `dumb`, or no reply
/// arrives before the deadline.
///
/// Blocking: writes the query and waits (up to [`REPLY_DEADLINE`]) for the
/// reply. The caller must not read the tty for the duration, or the reply is
/// consumed as input.
pub(crate) fn terminal_background() -> Option<Rgb> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    if std::env::var_os("TERM").is_none_or(|term| term == *"dumb") {
        return None;
    }
    query_terminal().ok().and_then(|reply| reply.background)
}

/// Open the tty, write the OSC 11 query and the DA1 sentinel, and decode the
/// replies. Separated from [`terminal_background`] so the fallible tty setup
/// funnels through one `?` chain.
fn query_terminal() -> std::io::Result<TerminalReply> {
    // Open the tty blocking for the write, so a momentarily full output buffer
    // parks the write rather than failing it: a non-blocking write treats a full
    // buffer as WouldBlock, which would abandon the probe and keep the dark
    // theme on what may be a light terminal.
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")?;
    // OSC 11 asks for the background; the DA1 that follows is the sentinel every
    // terminal answers, so a silent query is a definite "unsupported" once DA1
    // replies rather than a reply still on its way.
    tty.write_all(b"\x1b]11;?\x07\x1b[c")?;
    tty.flush()?;
    // Switch to non-blocking for the read loop, which is driven by poll against
    // the deadline rather than parking in read() when the terminal stays silent.
    fcntl(tty.as_raw_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;
    Ok(TerminalReply::read(&mut tty))
}

/// What the terminal reported in reply to the probe queries, together with the
/// sentinel that marks the end of its replies.
#[derive(Default)]
struct TerminalReply {
    /// The terminal background from OSC 11, or `None` when the terminal did not
    /// answer the query.
    background: Option<Rgb>,
    /// Whether the Device Attributes reply, the sentinel marking the end of the
    /// terminal's replies, arrived.
    sentinel_seen: bool,
}

impl TerminalReply {
    /// Read the tty until the DA1 sentinel arrives or the deadline elapses,
    /// decoding each chunk as it arrives. A single parser is fed the chunks
    /// incrementally, so a reply split across reads is still decoded and the
    /// growing buffer is never re-parsed.
    fn read(tty: &mut std::fs::File) -> Self {
        let deadline = Instant::now() + REPLY_DEADLINE;
        let mut parser = Parser::new();
        let mut reply = Self::default();
        let mut chunk = [0u8; 256];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let timeout = u16::try_from(remaining.as_millis()).unwrap_or(u16::MAX);
            let mut fds = [PollFd::new(tty.as_fd(), PollFlags::POLLIN)];
            match poll(&mut fds, PollTimeout::from(timeout)) {
                Ok(n) if n > 0 => {}
                // The deadline elapsed with no reply, or a signal interrupted
                // the wait; on a timeout stop, on EINTR wait out the rest.
                Ok(_) => break,
                Err(Errno::EINTR) => continue,
                Err(_) => break,
            }
            match tty.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => reply.absorb(&mut parser, &chunk[..n]),
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => break,
            }
            // Stop once the background is known, or once the sentinel reports
            // the terminal has answered all it will, rather than waiting out the
            // deadline. Breaking on a known background also covers a minimal
            // terminal that answers OSC 11 but never DA1.
            if reply.background.is_some() || reply.sentinel_seen {
                break;
            }
        }
        reply
    }

    /// Feed one chunk of reply bytes through `parser`, folding the background and
    /// the DA1 sentinel out of the escape sequences it yields. termwiz's parser
    /// covers the range of standard and non-standard replies terminals send and
    /// retains state between calls, so a sequence spanning two chunks is decoded
    /// once it completes.
    fn absorb(&mut self, parser: &mut Parser, bytes: &[u8]) {
        parser.parse(bytes, |action| match action {
            Action::OperatingSystemCommand(osc) => {
                if let OperatingSystemCommand::ChangeDynamicColors(
                    DynamicColorNumber::TextBackgroundColor,
                    colors,
                ) = *osc
                    && let Some(ColorOrQuery::Color(srgba)) = colors.into_iter().next()
                {
                    let (r, g, b, _) = srgba.to_srgb_u8();
                    self.background = Some(Rgb { r, g, b });
                }
            }
            Action::CSI(csi) if is_device_attributes_reply(&csi) => {
                self.sentinel_seen = true;
            }
            _ => {}
        });
    }
}

/// Whether `csi` is a Device Attributes reply, the sentinel that marks the end
/// of the terminal's replies. termwiz classifies the VT220/320/420 forms it
/// knows as [`Device::DeviceAttributes`], but a terminal reporting another model
/// (a VT525 answering `?65;...`, say) parses as an unspecified private CSI
/// ending in `c`. Both are DA replies, and the reported model does not matter to
/// the probe.
fn is_device_attributes_reply(csi: &CSI) -> bool {
    match csi {
        CSI::Device(device) => matches!(**device, Device::DeviceAttributes(_)),
        CSI::Unspecified(unspec) => {
            unspec.control == 'c' && matches!(unspec.params.first(), Some(CsiParam::P(b'?')))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use termwiz::escape::parser::Parser;
    use wiff_diff::Rgb;

    use super::TerminalReply;

    /// A decoded probe reply as `(background, sentinel_seen)` for full-value
    /// assertions.
    type Decoded = (Option<Rgb>, bool);

    /// Decode `bytes` through the same incremental path the read loop uses.
    fn decode(bytes: &[u8]) -> Decoded {
        let mut reply = TerminalReply::default();
        reply.absorb(&mut Parser::new(), bytes);
        (reply.background, reply.sentinel_seen)
    }

    #[test]
    fn a_dark_background_reply_with_the_sentinel_decodes() {
        // A typical dark terminal: 16-bit channels, ST-terminated OSC 11,
        // followed by the DA1 sentinel.
        wince::assert_eq!(
            decode(b"\x1b]11;rgb:0000/2b2b/3636\x1b\\\x1b[?62;c"),
            (
                Some(Rgb {
                    r: 0x00,
                    g: 0x2b,
                    b: 0x36
                }),
                true
            )
        );
    }

    #[test]
    fn a_light_background_reply_decodes() {
        // A light terminal near white, BEL-terminated.
        wince::assert_eq!(
            decode(b"\x1b]11;rgb:fdfd/f6f6/e3e3\x07\x1b[?1;2c"),
            (
                Some(Rgb {
                    r: 0xfd,
                    g: 0xf6,
                    b: 0xe3
                }),
                true
            )
        );
    }

    #[test]
    fn eight_bit_channels_scale_up() {
        // Some terminals answer with 8-bit channels; `ff` scales to the same 255
        // as `ffff`.
        wince::assert_eq!(
            decode(b"\x1b]11;rgb:1e/1e/1e\x07\x1b[?62;c"),
            (
                Some(Rgb {
                    r: 0x1e,
                    g: 0x1e,
                    b: 0x1e
                }),
                true
            )
        );
    }

    #[test]
    fn the_sentinel_without_a_color_reports_unsupported() {
        // A terminal that answers DA1 but no color query: the sentinel arrived,
        // so the missing background is a definite "unsupported", not a slow
        // reply.
        wince::assert_eq!(decode(b"\x1b[?62;c"), (None, true));
    }

    #[test]
    fn a_vt525_model_sentinel_is_recognized() {
        // A terminal reporting the VT525 model answers DA1 with a leading `65`,
        // which termwiz does not classify as a Device Attributes reply. The
        // probe still recognizes the private CSI ending in `c` as its sentinel.
        wince::assert_eq!(
            decode(b"\x1b]11;rgb:0000/0000/0000\x1b\\\x1b[?65;4;6;18;22;52c"),
            (
                Some(Rgb {
                    r: 0x00,
                    g: 0x00,
                    b: 0x00
                }),
                true
            )
        );
    }

    #[test]
    fn no_reply_at_all_decodes_to_nothing() {
        wince::assert_eq!(decode(b""), (None, false));
    }

    #[test]
    fn a_reply_split_across_reads_still_decodes() {
        // The read loop feeds each tty chunk to one persistent parser, so an OSC
        // 11 reply split mid-sequence across two reads decodes once the second
        // chunk completes it.
        let mut reply = TerminalReply::default();
        let mut parser = Parser::new();
        reply.absorb(&mut parser, b"\x1b]11;rgb:0000/2b");
        reply.absorb(&mut parser, b"2b/3636\x1b\\\x1b[?62;c");
        wince::assert_eq!(
            (reply.background, reply.sentinel_seen),
            (
                Some(Rgb {
                    r: 0x00,
                    g: 0x2b,
                    b: 0x36
                }),
                true
            )
        );
    }
}
