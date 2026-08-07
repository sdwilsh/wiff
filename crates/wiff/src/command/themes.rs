//! `wiff themes`: print the bundled syntax theme names, one per line

/// Print each bundled theme name on its own line.
pub fn run() -> anyhow::Result<()> {
    for name in wiff_diff::theme_names() {
        println!("{name}");
    }
    Ok(())
}
