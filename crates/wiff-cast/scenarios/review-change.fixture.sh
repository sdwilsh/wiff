#!/usr/bin/env bash
# A committed hello-world with an uncommitted edit that names the value before
# printing it: the change a review opens on.
git init -q -b main repo
cd repo
cat > main.rs <<'EOT'
fn main() {
    println!("hello");
}
EOT
git add .
git commit -qm "Initial commit"
cat > main.rs <<'EOT'
fn main() {
    let name = "world";
    println!("hello, {name}!");
}
EOT
