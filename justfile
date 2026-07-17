# awm task runner. `just` is optional — see Makefile for a no-just fallback.

# Build the whole workspace.
build:
    cargo build --workspace

# Run all tests.
test:
    cargo test --workspace

# Format + lint gate (requires rustfmt & clippy components; CI has them).
lint:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

# Record a real stream-json session into fixtures/ for schema ground-truth.
# Usage: just record-fixture my-scenario "your prompt here"
record-fixture name prompt:
    claude -p --output-format stream-json --verbose "{{prompt}}" \
        > fixtures/{{name}}.jsonl
    @echo "recorded fixtures/{{name}}.jsonl — remember to author its .expected.json"

# Placeholder until the runtime lands (Phase 3).
demo:
    cargo run -p awm
