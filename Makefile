.PHONY: build test clippy fmt fmt-check ci

build:
	cargo build --release

test:
	cargo test --workspace

clippy:
	cargo clippy --workspace --all-targets -- -D warnings -D clippy::unwrap_used

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

ci: clippy fmt-check test
