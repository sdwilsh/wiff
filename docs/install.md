# Install

wiff is written in Rust and builds with a recent stable toolchain. You do not
need to clone the repository to install it.

## Prebuilt binaries

The rolling
[`continuous`](https://github.com/wez/wiff/releases/tag/continuous) release is
rebuilt from `main` on every commit. It attaches a
`wiff-<target>.tar.gz` per platform, with a `.sha256` beside it; each archive
holds the `wiff` binary, `LICENSE`, and `README.md`.

The most convenient way to install is:

```bash
curl -fsSL https://github.com/wez/wiff/releases/download/continuous/install.sh | sh
```

The script picks the archive for your platform, checks it against the published
SHA-256 to catch a corrupted download, and installs to `~/.local/bin` (override
by setting `WIFF_BIN_DIR` in the environment).

| Platform | Target |
| --- | --- |
| Linux, x86_64 | `x86_64-unknown-linux-musl` |
| Linux, arm64 | `aarch64-unknown-linux-musl` |
| macOS, Apple silicon | `aarch64-apple-darwin` |
| macOS, Intel | `x86_64-apple-darwin` |
| Windows | Not currently supported |

## From git

Install straight from the repository:

```bash
cargo install --git https://github.com/wez/wiff.git --locked wiff
```

This builds the `wiff` binary and places it on your PATH (usually
`~/.cargo/bin`).

## From a checkout

If you have cloned the repository, install from the workspace instead:

```bash
cargo install --path crates/wiff --locked
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
