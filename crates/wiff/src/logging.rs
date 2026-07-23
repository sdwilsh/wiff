//! Terminal-aware routing for the process-wide log subscriber.
//!
//! A command's command-line work (capturing a diff, fetching a pull request)
//! benefits from log output on stderr, and a slow or failing fetch is what a
//! reader reaches for RUST_LOG to trace. The review TUI, though, owns the
//! terminal, and a log line written over its alternate screen corrupts it. So a
//! single subscriber writes to stderr while a command runs on the command line
//! and drops its output once the TUI takes the terminal, rather than installing
//! no subscriber at all for any command that eventually opens the TUI.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use tracing_subscriber::EnvFilter;

/// Whether the review TUI has taken the terminal. Set once, as the TUI opens,
/// and read for every log event to choose stderr or a sink.
static TUI_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Install the process-wide log subscriber. The default filter reports warnings
/// and above; RUST_LOG overrides it.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(gated_writer)
        .init();
}

/// Route later log output to a sink instead of the terminal, called as the
/// review TUI takes the terminal over. Events are still filtered and formatted,
/// but nothing reaches the alternate screen.
pub fn silence_for_tui() {
    TUI_ACTIVE.store(true, Ordering::Relaxed);
}

/// The destination for a log event: stderr while the command line owns the
/// terminal, a sink once the TUI does.
fn gated_writer() -> Box<dyn Write> {
    if TUI_ACTIVE.load(Ordering::Relaxed) {
        Box::new(io::sink())
    } else {
        Box::new(io::stderr())
    }
}
