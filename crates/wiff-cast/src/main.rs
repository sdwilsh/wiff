//! Records scripted screencasts of wiff for the documentation site.
//!
//! Each `.scenario` file under `scenarios/` builds a throwaway git repository
//! and wiff home, then drives the real `wiff` binary inside a pseudo-terminal
//! through a fixed sequence of shell commands and keystrokes, capturing the
//! result as an asciicast. Deterministic ids and time (see
//! `wiff_core::determinism`) plus a logical recording clock make the output
//! reproducible, so a regenerated cast differs only when the recorded behaviour
//! genuinely changed.

mod combine;
mod recorder;
mod scenario;
mod script;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Where a recording stages its throwaway repo and wiff home. Fixed and absolute
/// so the paths wiff prints, and the project bucket hashed from the repo's path,
/// are the same on every machine and a regenerated cast stays byte-identical.
/// Overridable through `WIFF_CAST_STAGE` for a machine where `/tmp` is unusable.
const DEFAULT_STAGE: &str = "/tmp/wiff-cast";

fn main() -> Result<()> {
    let wiff = sibling_wiff()?;
    if !wiff.exists() {
        bail!(
            "{} not found; build it in the same profile first (e.g. `cargo build -p wiff`, \
             or `--release` alongside a release recorder)",
            wiff.display()
        );
    }
    let out_dir = workspace_root()?.join("docs/assets/casts");
    std::fs::create_dir_all(&out_dir).ok();

    let stage = std::env::var_os("WIFF_CAST_STAGE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STAGE));

    let mut casts = Vec::new();
    for file in scenario_files()? {
        let scenario = script::parse(&file)?;
        let cast = scenario.record(&wiff, &stage, &out_dir)?;
        println!("wrote {}", cast.display());
        casts.push(cast);
    }

    let combined = out_dir.join("wiff-demo.cast");
    combine::write_combined(&casts, &combined)?;
    println!("wrote {}", combined.display());
    Ok(())
}

/// The `wiff` binary built in the same profile as this recorder, taken from our
/// own executable's directory. Following the recorder's own location tracks a
/// `--release` build, whose syntax highlighting is fast enough to keep the
/// capture settle deadline comfortable, and any `CARGO_TARGET_DIR` override,
/// rather than assuming a debug build under the workspace root.
fn sibling_wiff() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating the recorder executable")?;
    let dir = exe
        .parent()
        .context("recorder executable has no parent directory")?;
    Ok(dir.join("wiff"))
}

/// The `.scenario` files under this crate's `scenarios/` directory, sorted by
/// name for a stable recording order.
fn scenario_files() -> Result<Vec<PathBuf>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scenarios");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("scenario") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// The workspace root, derived from this crate's manifest directory.
fn workspace_root() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .map(Path::to_path_buf)
        .context("locating the workspace root")
}
