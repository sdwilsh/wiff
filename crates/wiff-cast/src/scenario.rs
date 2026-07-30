//! Drives a scenario: builds its repository by running the `%prep` bash in a
//! deterministic environment, then replays the recorded steps against the real
//! `wiff` binary inside a pseudo-terminal, writing the result as an asciicast.
//!
//! The environment is prepared so a plain `git commit` or `wiff new` in prep is
//! already reproducible: an isolated home, a fixed author and fixed git dates,
//! and the deterministic-id anchor. Prep that wants a session to look older than
//! the recording's now runs one command through the `asof` helper on `PATH`.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

use crate::recorder::Recorder;

/// The value `$USER` takes, which a human comment is attributed to, pinned so
/// the author shown in a review is the same on every machine.
const AUTHOR: &str = "ada";

/// The committed author identity, shown in reviews and in commit metadata.
const AUTHOR_NAME: &str = "Ada Lovelace";
const AUTHOR_EMAIL: &str = "ada@example.com";

/// Seconds held after a command's output settles, so an abrupt transition such
/// as the TUI taking the screen has room to be followed.
const SETTLE_VIEW: f64 = 1.2;

/// How long a capture waits with no new output before deciding the program is
/// done painting.
const IDLE: Duration = Duration::from_millis(250);

/// The cap on how long a single capture waits for output to settle. Generous
/// enough for the TUI's first paint, which includes syntax highlighting; a quick
/// command returns as soon as it goes idle, well under this.
const CAPTURE: Duration = Duration::from_secs(8);

/// The name, under `stage`, of the file a prep script may write `name=value`
/// lines to. The runner reads it back after prep and substitutes `{{name}}`
/// placeholders in the recorded commands, letting a scenario show a value that
/// prep computed, such as a minted session id, without transcribing it by hand.
const RECORD_VARS_FILE: &str = "record-vars";

/// The environment variable that names the [`RECORD_VARS_FILE`] path for prep.
const RECORD_VARS_VAR: &str = "WIFF_CAST_VARS";

/// A helper installed on the prep `PATH` that runs one command as of a chosen
/// instant, anchoring its deterministic ids and git dates there. It lets prep
/// mint a session that looks older than the recording's now without naming any
/// environment variable.
const ASOF: &str = "\
#!/bin/sh
instant=$1
shift
export WIFF_DETERMINISTIC=$instant
export GIT_AUTHOR_DATE=$instant
export GIT_COMMITTER_DATE=$instant
exec \"$@\"
";

/// One action in a scenario's recorded script.
pub enum Step {
    /// Type a shell command with a typewriter cadence, dwell `read_line` seconds
    /// so the viewer can read it, run it, and capture what it paints.
    Command { line: String, read_line: f64 },
    /// Send a key to the running program and capture the repaint it causes,
    /// showing `label` in the key legend.
    Press { bytes: String, label: String },
    /// Type `text` a character at a time without running anything, to fill an
    /// open editor field.
    Type(String),
    /// Show `text` as a status note in the reserved bottom row, painting it as a
    /// frame that a following [`Pause`](Step::Pause) holds until the next key
    /// press replaces it.
    Status(String),
    /// Hold the last frame on screen for `seconds` so the viewer can read it.
    Pause(f64),
}

/// A single screencast: the terminal size to record at, the bash that builds the
/// repository, and the steps to replay against it.
pub struct Scenario {
    /// The cast's file stem under `docs/assets/casts`.
    pub name: String,
    pub width: u16,
    pub height: u16,
    /// The instant deterministic ids and time anchor at for the recording.
    pub now: String,
    /// The bash script that builds and populates `repo`.
    pub prep: String,
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Build the repository and drive the recording inside a pseudo-terminal
    /// staged under `stage`, writing the cast into `out_dir`. Returns its path.
    pub fn record(&self, wiff: &Path, stage: &Path, out_dir: &Path) -> Result<PathBuf> {
        let home = stage.join("home");
        let repo = stage.join("repo");
        let tmp = stage.join("tmp");
        let bin = stage.join("bin");
        reset_dir(stage)?;
        std::fs::create_dir_all(&home)?;
        std::fs::create_dir_all(&tmp)?;
        install_asof(&bin)?;

        let env = self.base_env(wiff, &home, &tmp, &bin, stage);
        self.run_prep(stage, &env)?;
        let vars = read_record_vars(&stage.join(RECORD_VARS_FILE))?;

        let mut command = CommandBuilder::new("bash");
        command.args(["--noprofile", "--norc", "-i"]);
        command.cwd(&repo);
        for (key, value) in &env {
            command.env(key, value);
        }

        let mut rec = Recorder::spawn(command, self.width, self.height)?;
        // Label the opening frames with the scenario's name until its first key
        // press takes over the reserved row, so a viewer of the amalgamated tour
        // sees which scenario each scene is.
        rec.set_now_playing(format!("{}.scenario", self.name));

        // Drop the shell's startup noise, then send ctrl-l so readline clears
        // the screen and redraws the empty prompt in place, with no command
        // echoed. Recording that as the opening frame means the first typed
        // command appears after a visible prompt.
        rec.discard(IDLE, CAPTURE);
        rec.send("\u{c}")?;
        rec.capture(IDLE, CAPTURE);

        self.replay(&mut rec, &vars)?;

        let cast = out_dir.join(format!("{}.cast", self.name));
        rec.write_cast(&cast)?;
        Ok(cast)
    }

    /// Play the recorded steps against a recorder already at a clean prompt,
    /// substituting `{{name}}` placeholders in typed text with the values `vars`
    /// captured from prep.
    fn replay(&self, rec: &mut Recorder, vars: &HashMap<String, String>) -> Result<()> {
        for step in &self.steps {
            match step {
                Step::Command { line, read_line } => {
                    let line = expand_vars(line, vars)?;
                    rec.clear_overlay();
                    rec.type_text(&line)?;
                    rec.pause(*read_line);
                    rec.push_key("enter");
                    rec.send("\r")?;
                    rec.capture(IDLE, CAPTURE);
                    rec.pause(SETTLE_VIEW);
                }
                Step::Press { bytes, label } => {
                    rec.push_key(label.clone());
                    rec.send(bytes)?;
                    rec.capture(IDLE, CAPTURE);
                }
                Step::Type(text) => rec.type_text(&expand_vars(text, vars)?)?,
                Step::Status(text) => rec.set_status(text.clone()),
                Step::Pause(seconds) => rec.pause(*seconds),
            }
        }
        Ok(())
    }

    /// Run the `%prep` bash in `stage` with the deterministic environment, so it
    /// leaves a populated `repo` for the recording to open. The script runs on a
    /// pty so its `wiff` calls see a terminal on stdin: a seed runs `wiff new`
    /// with source flags like `--base`, which reject a piped, non-terminal
    /// stdin because they treat it as a diff.
    fn run_prep(&self, stage: &Path, env: &[(String, OsString)]) -> Result<()> {
        let script = format!("set -euo pipefail\n{}", self.prep);
        let mut cmd = CommandBuilder::new("bash");
        cmd.arg("-c");
        cmd.arg(&script);
        cmd.cwd(stage);
        for (key, value) in env {
            cmd.env(key, value);
        }
        let (out, ok) = run_on_pty(cmd).context("running prep")?;
        if !ok {
            bail!("prep for {} failed:\n{}", self.name, out.trim());
        }
        Ok(())
    }

    /// The environment shared by the prep shell and the recorded shell: an
    /// isolated home and XDG layout, a private temp directory, a fixed author,
    /// fixed git identity and dates, and the deterministic-id anchor and state.
    /// A prep command overrides the anchor and dates for itself through `asof`.
    fn base_env(
        &self,
        wiff: &Path,
        home: &Path,
        tmp: &Path,
        bin: &Path,
        stage: &Path,
    ) -> Vec<(String, OsString)> {
        let wiff_dir = wiff.parent().unwrap_or(Path::new("."));
        vec![
            ("HOME".into(), home.into()),
            ("XDG_DATA_HOME".into(), home.join("data").into()),
            ("XDG_CONFIG_HOME".into(), home.join("config").into()),
            ("XDG_STATE_HOME".into(), home.join("state").into()),
            ("TMPDIR".into(), tmp.into()),
            ("TERM".into(), "xterm-256color".into()),
            ("PATH".into(), path_with(&[bin, wiff_dir]).into()),
            ("PS1".into(), "$ ".into()),
            ("USER".into(), AUTHOR.into()),
            (
                "WIFF_DETERMINISTIC_STATE".into(),
                stage.join("det-state").into(),
            ),
            ("WIFF_DETERMINISTIC".into(), self.now.clone().into()),
            (RECORD_VARS_VAR.into(), stage.join(RECORD_VARS_FILE).into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("GIT_CONFIG_SYSTEM".into(), "/dev/null".into()),
            ("GIT_AUTHOR_NAME".into(), AUTHOR_NAME.into()),
            ("GIT_AUTHOR_EMAIL".into(), AUTHOR_EMAIL.into()),
            ("GIT_COMMITTER_NAME".into(), AUTHOR_NAME.into()),
            ("GIT_COMMITTER_EMAIL".into(), AUTHOR_EMAIL.into()),
            ("GIT_AUTHOR_DATE".into(), self.now.clone().into()),
            ("GIT_COMMITTER_DATE".into(), self.now.clone().into()),
        ]
    }
}

/// Run `command` to completion on a fresh pty, returning everything it wrote and
/// whether it exited successfully. The pty gives its children a terminal on
/// stdin, and merges their output for the caller to report on failure.
fn run_on_pty(command: CommandBuilder) -> Result<(String, bool)> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 50,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a pty")?;
    let mut child = pair.slave.spawn_command(command).context("spawning bash")?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().context("cloning reader")?;
    let mut out = String::new();
    // Read to the pty's end, which arrives once the child exits and closes the
    // slave; a pty reports that as an I/O error rather than a clean EOF, so the
    // bytes already collected are what we keep.
    let _ = reader.read_to_string(&mut out);
    let status = child.wait().context("waiting for bash")?;
    Ok((out, status.success()))
}

/// Read the `name=value` lines a prep script left at `path` into a map. A prep
/// that captured nothing leaves no file, which reads as no variables.
fn read_record_vars(path: &Path) -> Result<HashMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", path.display()));
        }
    };
    let mut vars = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, value) = line
            .split_once('=')
            .with_context(|| format!("record var line has no '=': {line}"))?;
        vars.insert(name.trim().to_string(), value.trim().to_string());
    }
    Ok(vars)
}

/// Replace every `{{name}}` in `text` with the matching value from `vars`,
/// failing if a placeholder names a variable prep did not capture.
fn expand_vars(text: &str, vars: &HashMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find("}}")
            .with_context(|| format!("unterminated {{{{ placeholder in {text:?}"))?;
        let name = after[..end].trim();
        let value = vars
            .get(name)
            .with_context(|| format!("record command references unknown variable {name:?}"))?;
        out.push_str(value);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Write the `asof` helper into `bin`, making it executable.
fn install_asof(bin: &Path) -> Result<()> {
    std::fs::create_dir_all(bin)?;
    let path = bin.join("asof");
    std::fs::write(&path, ASOF).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("making {} executable", path.display()))?;
    Ok(())
}

/// `PATH` with `dirs` prepended, so the prep and recording shells find our built
/// `wiff` and the `asof` helper ahead of anything else.
fn path_with(dirs: &[&Path]) -> String {
    let mut parts: Vec<String> = dirs.iter().map(|d| d.display().to_string()).collect();
    if let Some(existing) = std::env::var_os("PATH") {
        parts.push(existing.to_string_lossy().into_owned());
    }
    parts.join(":")
}

/// Remove `dir` if present and recreate it empty.
fn reset_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}
