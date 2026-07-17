# No-`just` fallback. fmt/clippy targets require the rustfmt & clippy components
# (present in CI; absent on the source-tarball dev box).
.PHONY: build test lint demo

build:
	cargo build --workspace

test:
	cargo test --workspace

lint:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings

demo:
	cargo run -p awm
