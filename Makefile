check:
	cargo check --all-features

build:
	cargo build --all-features

lint:
	cargo clippy --all-features --all-targets -- -D warnings

fmt:
	cargo +nightly fmt

test:
	cargo nextest run --all-features
