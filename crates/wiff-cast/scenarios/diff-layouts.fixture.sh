#!/usr/bin/env bash
# A committed markdown guide with an uncommitted revision, so the review opens on
# a prose diff that the rendered layout can display as formatted markdown.
git init -q -b main repo
cd repo
cat > GUIDE.md <<'EOT'
# Deploy guide

Ship a release in two steps.

## Steps

1. Tag the commit.
2. Push the tag.

The pipeline builds and publishes from the tag.

```rust
fn tag_name(version: &str) -> String {
    format!("v{version}")
}
```
EOT
git add .
git commit -qm "Initial commit"
cat > GUIDE.md <<'EOT'
# Deploy guide

Ship a release in three steps.

## Steps

1. Tag the commit with `git tag`.
2. Push the tag.
3. Watch the pipeline finish.

The pipeline builds and publishes from the tag. Roll back by deleting the
release and re-tagging an earlier commit.

```rust
fn tag_name(version: &str, channel: &str) -> String {
    format!("v{version}-{channel}")
}
```
EOT
