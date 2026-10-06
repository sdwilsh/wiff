//! Reproducible identifiers and wall-clock time for demos and recordings.
//!
//! Session ids, comment ids, and record timestamps are otherwise drawn from a
//! fresh short id or ULID and the system clock, so two runs of the same commands
//! never agree byte for byte. That defeats a screencast harness that wants to
//! regenerate an artifact and commit only genuine changes. When
//! `WIFF_DETERMINISTIC` is set the ids and clock instead follow a fixed sequence
//! anchored at a chosen instant, coordinated across the separate `wiff`
//! processes a recording drives through a small state file. The facility is
//! inert unless that variable is set, so ordinary use keeps real ids and real
//! time.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::OnceLock;

use nix::fcntl::{Flock, FlockArg};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::short_id::ShortId;

/// Enables deterministic ids and time. An RFC3339 instant anchors the sequence
/// at that moment; a plain enable token (`1`, `true`, `yes`, `on`, or empty)
/// anchors at [`DEFAULT_EPOCH`]. Any other value is rejected as a malformed
/// anchor rather than silently shifting every id and timestamp.
const ENABLE_VAR: &str = "WIFF_DETERMINISTIC";

/// Overrides the path of the shared state file that separate processes advance
/// together. Defaults to `wiff-deterministic-state.json` in the temp directory.
const STATE_VAR: &str = "WIFF_DETERMINISTIC_STATE";

/// The instant the sequence starts from when [`ENABLE_VAR`] names no timestamp.
const DEFAULT_EPOCH: &str = "2025-01-01T00:00:00Z";

/// How far each successive timestamp advances past the last.
const CLOCK_STEP: time::Duration = time::Duration::seconds(1);

/// The anchor instant, resolved once from the environment.
fn epoch() -> Option<OffsetDateTime> {
    static EPOCH: OnceLock<Option<OffsetDateTime>> = OnceLock::new();
    *EPOCH.get_or_init(|| {
        let value = std::env::var(ENABLE_VAR).ok()?;
        let rfc3339 = &time::format_description::well_known::Rfc3339;
        if let Ok(anchor) = OffsetDateTime::parse(&value, rfc3339) {
            return Some(anchor);
        }
        match value.as_str() {
            "1" | "true" | "yes" | "on" | "" => {
                Some(OffsetDateTime::parse(DEFAULT_EPOCH, rfc3339).expect("valid default epoch"))
            }
            other => {
                panic!("{ENABLE_VAR}={other:?} is neither an enable token nor an RFC3339 instant")
            }
        }
    })
}

/// The shared state file path.
fn state_path() -> PathBuf {
    std::env::var_os(STATE_VAR)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("wiff-deterministic-state.json"))
}

/// The next sequence values for ids and timestamps, persisted between the
/// processes a recording drives.
#[derive(Default)]
struct Counters {
    id: u64,
    clock: u64,
}

/// Advance the shared counters under an exclusive lock and hand `pick` the value
/// it consumes, returning `pick`'s result.
///
/// Every step of reading, locking, and writing the state file is required: this
/// runs only under the deterministic env we control, and quietly falling back to
/// unsequenced counters would mint colliding ids and frozen timestamps, the one
/// outcome the facility exists to prevent. A failure therefore panics rather
/// than corrupting the recording it is meant to make reproducible.
fn with_counters<T>(pick: impl FnOnce(&mut Counters) -> T) -> T {
    let path = state_path();
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .unwrap_or_else(|e| panic!("opening deterministic state {}: {e}", path.display()));
    let mut locked = Flock::lock(file, FlockArg::LockExclusive)
        .unwrap_or_else(|(_, e)| panic!("locking deterministic state {}: {e}", path.display()));
    let mut text = String::new();
    locked
        .read_to_string(&mut text)
        .unwrap_or_else(|e| panic!("reading deterministic state {}: {e}", path.display()));
    let mut counters = parse_counters(&text).unwrap_or_else(|| {
        panic!(
            "deterministic state {} is corrupt: {text:?}",
            path.display()
        )
    });
    let outcome = pick(&mut counters);
    let encoded = format!("{}\n{}\n", counters.id, counters.clock);
    locked
        .seek(SeekFrom::Start(0))
        .and_then(|_| locked.set_len(0))
        .and_then(|_| locked.write_all(encoded.as_bytes()))
        .and_then(|_| locked.flush())
        .unwrap_or_else(|e| panic!("writing deterministic state {}: {e}", path.display()));
    outcome
}

/// Read the two counter lines the state file holds. An empty file is a fresh
/// start with both counters at zero; a non-empty file that does not parse as two
/// counter lines is corrupt recording state, reported as `None` so the caller
/// fails loudly rather than silently resetting to zero and re-minting ids.
fn parse_counters(text: &str) -> Option<Counters> {
    if text.trim().is_empty() {
        return Some(Counters::default());
    }
    let mut lines = text.lines();
    let id = lines.next()?.trim().parse().ok()?;
    let clock = lines.next()?.trim().parse().ok()?;
    Some(Counters { id, clock })
}

/// Whether deterministic mode is active, i.e. [`ENABLE_VAR`] is set. A recording
/// harness reads this to settle otherwise-asynchronous work, such as background
/// syntax highlighting, synchronously, so each captured frame is reproducible.
pub fn enabled() -> bool {
    epoch().is_some()
}

/// Returns a fresh comment id: a real [`Ulid::generate`] normally, or the
/// next id in the fixed sequence under deterministic mode, where ids sort in
/// creation order.
pub fn new_ulid() -> Ulid {
    let Some(anchor) = epoch() else {
        return Ulid::generate();
    };
    let seq = next_id_seq();
    let millis = (anchor.unix_timestamp_nanos() / 1_000_000).max(0) as u128;
    Ulid::from(millis << 80 | u128::from(seq))
}

/// Returns the next session id in the fixed sequence under deterministic mode,
/// or `None` when the facility is inert and real minting should be used. The
/// ids share the sequence counter [`new_ulid`] draws from, so a recording that
/// interleaves session and comment mints still reproduces byte for byte.
pub(crate) fn deterministic_short_id() -> Option<ShortId> {
    let anchor = epoch()?;
    Some(ShortId::from_instant_and_seq(anchor, next_id_seq()))
}

/// Draw and advance the shared id sequence counter.
fn next_id_seq() -> u64 {
    with_counters(|counters| {
        let seq = counters.id;
        counters.id += 1;
        seq
    })
}

/// Returns the current instant: real wall-clock time normally, or successive
/// instants [`CLOCK_STEP`] apart from the anchor under deterministic mode.
pub fn now() -> OffsetDateTime {
    let Some(anchor) = epoch() else {
        return OffsetDateTime::now_utc();
    };
    let step = with_counters(|counters| {
        let step = counters.clock;
        counters.clock += 1;
        step
    });
    let steps = i32::try_from(step).expect("deterministic clock step fits in i32");
    anchor + CLOCK_STEP * steps
}
