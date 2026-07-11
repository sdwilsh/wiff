//! Background syntax highlighting for a diff under review.
//!
//! Parsing a large diff with syntect is slow, and we want the application to
//! load and be usable immediately, so this module offloads that parse to worker
//! threads and lets the UI thread retrieve the results asynchronously without
//! blocking the reviewer.
//!
//! Coloring a parsed file is cheap and runs on the main thread against the
//! current theme, so this module does not deal with that concern; keeping it
//! off the workers is also why a theme change can never race one.

use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use wiff_diff::{Diff, Parser, Side};

use crate::render::ParsedFile;

/// A monotonic counter representing an epoch. The intent is that you advance
/// the epoch each time a new batch of work is commenced, then tag each async
/// request in that batch with the resulting `Generation` value. When the work
/// items complete, compare the `Generation` in each item with the now-current
/// `Generation` to determine whether it is stale.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
struct Generation(u64);

impl Generation {
    /// Move on to the next epoch, making every result from before it stale.
    fn advance(&mut self) {
        self.0 += 1;
    }

    /// Whether `self` belongs to an earlier epoch than `current`.
    fn is_stale(self, current: Generation) -> bool {
        self != current
    }
}

/// A file's finished parse on its way back from a worker to the pool.
struct ParseMessage {
    generation: Generation,
    /// The file's index within the diff.
    index: usize,
    parsed: ParsedFile,
}

/// A parse result ready to fold into the highlight cache.
pub struct Parsed {
    /// The file's index within the diff.
    pub index: usize,
    /// The file's parsed content, ready to color with the current theme.
    pub parsed: ParsedFile,
}

/// A pool that parses the files of a diff in the background and hands back each
/// result as it becomes ready.
pub struct BackgroundHighlighter {
    parser: Parser,
    /// Results from the workers of the current generation. Replaced by a fresh
    /// channel on each [`start`](Self::start): dropping the previous receiver
    /// makes any workers still running for an earlier diff fail their next send
    /// and stop.
    results: Receiver<ParseMessage>,
    /// The generation the outstanding jobs belong to.
    generation: Generation,
    /// Files in the current generation still awaiting a result.
    pending: usize,
    /// A result taken from the channel by [`wait`](Self::wait) but not yet
    /// returned by [`drain`](Self::drain).
    buffered: VecDeque<ParseMessage>,
}

impl BackgroundHighlighter {
    /// Build a highlighter that parses with `parser`, with no work started yet.
    pub fn new(parser: Parser) -> Self {
        // A disconnected channel until the first start, so draining before any
        // work simply yields nothing.
        let (_sender, results) = channel();
        Self {
            parser,
            results,
            generation: Generation::default(),
            pending: 0,
            buffered: VecDeque::new(),
        }
    }

    /// Begin parsing every file of `diff` in the background, stopping any jobs
    /// still running for an earlier diff. Non-blocking, returning immediately.
    pub fn start(&mut self, diff: Arc<Diff>) {
        self.generation.advance();
        self.pending = diff.files.len();
        self.buffered.clear();
        // A fresh channel per start: replacing the receiver drops the previous
        // one, so workers left over from an earlier diff fail their next send
        // and stop rather than draining a stale queue.
        let (sender, results) = channel();
        self.results = results;
        if diff.files.is_empty() {
            return;
        }
        let count = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(diff.files.len());
        let queue: Arc<Mutex<VecDeque<usize>>> =
            Arc::new(Mutex::new((0..diff.files.len()).collect()));
        let generation = self.generation;
        for n in 0..count {
            let parser = self.parser.clone();
            let sender = sender.clone();
            let queue = Arc::clone(&queue);
            let diff = Arc::clone(&diff);
            let worker = thread::Builder::new().name(format!("wiff-parse-{n}"));
            worker
                .spawn(move || {
                    loop {
                        let index = {
                            let mut queue = queue.lock().expect("the parse queue is not poisoned");
                            queue.pop_front()
                        };
                        let Some(index) = index else { break };
                        let file = &diff.files[index];
                        let before = parser.parse_side(file, Side::Before);
                        let after = parser.parse_side(file, Side::After);
                        let parsed = ParsedFile::from_sides(before, after);
                        // A closed channel means the review moved on and no
                        // longer wants these results, so the worker stops.
                        if sender
                            .send(ParseMessage {
                                generation,
                                index,
                                parsed,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .expect("spawn a background parse worker");
        }
    }

    /// Whether files of the current diff are still being parsed.
    pub fn in_progress(&self) -> bool {
        self.pending > 0
    }

    /// Block until another result arrives or `timeout` elapses, holding it for
    /// the next [`drain`](Self::drain). Returns true if a result arrived, false
    /// on timeout or when nothing is outstanding.
    pub fn wait(&mut self, timeout: Duration) -> bool {
        if self.pending == 0 {
            return false;
        }
        match self.results.recv_timeout(timeout) {
            Ok(message) => {
                self.buffered.push_back(message);
                true
            }
            Err(_) => false,
        }
    }

    /// Take the parse results that have arrived since the last drain, dropping
    /// any left over from a diff that has since been replaced.
    pub fn drain(&mut self) -> Vec<Parsed> {
        let mut ready = Vec::new();
        let buffered = std::mem::take(&mut self.buffered);
        for message in buffered.into_iter().chain(self.results.try_iter()) {
            if message.generation.is_stale(self.generation) {
                continue;
            }
            self.pending = self.pending.saturating_sub(1);
            ready.push(Parsed {
                index: message.index,
                parsed: message.parsed,
            });
        }
        ready
    }
}
