//! Amalgamates the per-scenario casts into one continuous recording for the
//! demo gif and the docs player.
//!
//! Each scenario is recorded on its own and starts at time zero, so combining
//! them replays each after the one before, with a short hold between scenarios
//! so a final frame rests on screen before the next clears it and begins.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// Seconds the amalgamated recording holds a scenario's final frame before the
/// next scenario clears the screen and starts.
const SCENE_GAP: f64 = 1.5;

/// Concatenate `casts`, in order, into one asciicast written to `out`, offsetting
/// each scenario's events past the end of the one before with [`SCENE_GAP`]
/// between them. Fails unless every cast was recorded at the same terminal size,
/// since the amalgamation keeps a single header for the whole recording.
pub fn write_combined(casts: &[PathBuf], out: &Path) -> Result<()> {
    let mut header: Option<String> = None;
    let mut size: Option<(u64, u64)> = None;
    let mut events: Vec<String> = Vec::new();
    let mut offset = 0.0;
    for cast in casts {
        let text =
            std::fs::read_to_string(cast).with_context(|| format!("reading {}", cast.display()))?;
        let mut lines = text.lines();
        let head = lines
            .next()
            .with_context(|| format!("{} has no header", cast.display()))?;
        let dims =
            header_size(head).with_context(|| format!("reading the size of {}", cast.display()))?;
        match size {
            None => size = Some(dims),
            Some(first) if first != dims => bail!(
                "{} was recorded at {}x{}, but the recording started at {}x{}",
                cast.display(),
                dims.0,
                dims.1,
                first.0,
                first.1
            ),
            Some(_) => {}
        }
        header.get_or_insert_with(|| head.to_string());

        let mut end = offset;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let mut event: Value = serde_json::from_str(line)
                .with_context(|| format!("parsing an event in {}", cast.display()))?;
            let array = event
                .as_array_mut()
                .with_context(|| format!("event is not an array in {}", cast.display()))?;
            let time = array
                .first()
                .and_then(Value::as_f64)
                .with_context(|| format!("event has no numeric time in {}", cast.display()))?;
            end = offset + time;
            array[0] = Value::from(end);
            events.push(serde_json::to_string(&event)?);
        }
        offset = end + SCENE_GAP;
    }

    let Some(header) = header else {
        bail!("no casts to combine");
    };
    let mut out_text = header;
    out_text.push('\n');
    for event in events {
        out_text.push_str(&event);
        out_text.push('\n');
    }
    std::fs::write(out, &out_text).with_context(|| format!("writing {}", out.display()))
}

/// The (width, height) an asciicast header line declares.
fn header_size(header: &str) -> Result<(u64, u64)> {
    let value: Value = serde_json::from_str(header).context("header is not valid JSON")?;
    let width = value["width"].as_u64().context("header has no width")?;
    let height = value["height"].as_u64().context("header has no height")?;
    Ok((width, height))
}
