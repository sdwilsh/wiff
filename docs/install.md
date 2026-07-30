# Install

wiff is written in Rust and builds with a recent stable toolchain. You do not
need to clone the repository to install it.

## From git

Install straight from the repository:

```bash
cargo install --git https://github.com/wez/wiff.git wiff
```

This builds the `wiff` binary and places it on your PATH (usually
`~/.cargo/bin`).

## From a checkout

If you have cloned the repository, install from the workspace instead:

```bash
cargo install --path crates/wiff
```

## Verify

Confirm that the binary is runnable:

```bash
wiff --help
```

## Reviewing with an agent

wiff ships an agent skill that teaches an agent to read and annotate a review
through the `wiff` command line, while the TUI stays yours. Expand the bundled
skill into your data directory and print the path to its `SKILL.md` with:

```bash
wiff skill-path
```

Point your agent at that file, or copy the `skills/wiff-review/` directory into
wherever your agent looks for skills. The expansion rewrites each `wiff` call to
the absolute path of your binary. Once installed, ask your agent to review the
diff or address the existing comments; it discovers the active session for the
checkout on its own.

Some agent harnesses can load the skill on demand, without setting up its path
in advance. If yours does, a prompt like this is enough to bootstrap it:

> Load the wiff skill; run `wiff skill-path` to find it.
