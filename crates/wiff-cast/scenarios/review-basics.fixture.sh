#!/usr/bin/env bash
# A committed program with uncommitted edits spread down the file, so the review
# opens on a diff taller than the screen. Browsing it with j and the space page
# key scrolls through the change.
git init -q -b main repo
cd repo
cat > main.rs <<'EOT'
fn main() {
    let name = "world";
    println!("hello, {name}");
    let report = Report::new(name);
    report.emit();
}

struct Report {
    who: String,
    lines: Vec<String>,
}

impl Report {
    fn new(who: &str) -> Self {
        Report {
            who: who.to_string(),
            lines: Vec::new(),
        }
    }

    fn emit(&self) {
        println!("report for {}", self.who);
        for line in &self.lines {
            println!("  {line}");
        }
    }
}

fn farewell() {
    println!("bye");
}
EOT
git add .
git commit -qm "Initial commit"
cat > main.rs <<'EOT'
fn main() {
    let name = "world";
    println!("hello, {name}!");
    let mut report = Report::new(name);
    report.push("first pass complete");
    report.emit();
    farewell();
}

struct Report {
    who: String,
    lines: Vec<String>,
}

impl Report {
    fn new(who: &str) -> Self {
        Report {
            who: who.to_string(),
            lines: Vec::new(),
        }
    }

    fn push(&mut self, line: &str) {
        self.lines.push(line.to_string());
    }

    fn emit(&self) {
        println!("report for {} ({} lines)", self.who, self.lines.len());
        for line in &self.lines {
            println!("  - {line}");
        }
    }
}

fn farewell() {
    println!("goodbye, and thanks for all the fish");
}
EOT
