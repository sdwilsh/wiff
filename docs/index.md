# wiff

wiff is a terminal-centric diff and code-review utility: it captures a diff,
lets you browse and annotate it with syntax highlighting, and persists the
review as a local session that both a human (through the TUI) and an agent
(through the CLI and a skill) can read and write concurrently.

Here is a short tour: opening a change, leaving a comment, switching themes, and
resuming a session by id. See [Screencasts](screencasts.md) for each of these on
its own.

<div class="asciinema-cast" data-cast="assets/casts/wiff-demo.cast" data-autoplay="true" data-loop="true"></div>

## Where to next

- [Install](install.md) wiff, from git or a checkout, and set up the agent skill.
- [Reviewing](usage.md) covers the three kinds of review: a local change, a
  GitHub pull request, and an explore over existing code.
- [The review interface](interface.md) walks through the terminal UI, and
  [Key bindings](keybindings.md) lists every default key.
- [Configuration](configuration.md) documents `config.toml` and every option.

## Local first, forge when you want it

wiff has a rich local code-review model that needs no network access: it
captures a diff from your working tree, index, or history and stores the review
on your own disk, which makes it a good fit for private review of code that
never leaves your machine. On top of that same model, a forge integration
mirrors a GitHub pull request into a local session and publishes your review
back, so you can review a PR from the comfort of the terminal.
