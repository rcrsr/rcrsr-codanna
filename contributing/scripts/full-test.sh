#!/bin/bash
# Local mirror of .github/workflows/full-test.yml
# Run this before pushing to catch ALL GitHub Actions failures
# NOTE: Keep this in sync with full-test.yml - if you update one, update the other!

set -e  # Exit on first error

# Set environment variables like GitHub Actions
export CARGO_TERM_COLOR=always
export RUST_BACKTRACE=1

echo "Running Codanna CI locally (mirrors full-test.yml)"
echo "==================================================="

# Ensure we're using the latest stable Rust (matches GitHub Actions)
echo ""
echo "Ensuring Rust toolchain is up-to-date..."
rustup update stable --no-self-update > /dev/null 2>&1 || true
current_version=$(rustc --version)
echo "   Using: $current_version"

# Job: Test Suite
echo ""
echo "Job: Test Suite"
echo "==============="

# Fast checks first
echo ""
echo "[1/6] Check formatting"
cargo fmt --check
echo "PASS: formatting"

echo ""
echo "[2/6] Clippy with project rules"
cargo clippy --all-targets --all-features -- -D warnings
echo "PASS: clippy"

# Verify no-default-features compiles (check only, no linking)
echo ""
echo "[3/6] Check no-default-features"
cargo check --no-default-features
echo "PASS: no-default-features"

# Run tests (implicitly builds debug binary and all test targets)
echo ""
echo "[4/6] Run tests"
cargo test --verbose
echo "PASS: tests"

# CLI smoke tests using the debug binary built by cargo test
echo ""
echo "[5/6] Verify CLI commands"
./target/debug/codanna --help > /dev/null
echo "  main help: ok"
./target/debug/codanna index --help > /dev/null
echo "  index help: ok"
./target/debug/codanna retrieve --help > /dev/null
echo "  retrieve help: ok"
echo "PASS: CLI commands"

# Documentation
echo ""
echo "[6/6] Check docs build"
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
echo "PASS: docs"

# Local-only: MCP server test (not in GitHub Actions).
# Runs in a scratch workspace it seeds itself: this repo's .codanna/
# is watched by a live MCP server on dev machines (serve.lock), and
# tests never target a live index.
echo ""
echo "Local-only: MCP server test (scratch workspace)"
codanna_bin="$(pwd -P)/target/debug/codanna"
mcp_scratch=$(mktemp -d)
trap 'rm -rf "$mcp_scratch"' EXIT
# Canonical path: a symlinked workspace root breaks module-identity
# strip-base comparison on macOS (/tmp -> /private/tmp).
mcp_scratch=$(cd "$mcp_scratch" && pwd -P)
mkdir -p "$mcp_scratch/src" "$mcp_scratch/.codanna"
cat > "$mcp_scratch/src/probe.rs" <<'FIXTURE'
pub fn mcp_probe_target() -> i32 {
    1
}
FIXTURE
cat > "$mcp_scratch/.codanna/settings.toml" <<SETTINGS
index_path = ".codanna/index"

[indexing]
indexed_paths = ["$mcp_scratch/src"]

[semantic_search]
enabled = false
SETTINGS
mcp_log="$mcp_scratch/mcp-test.log"
# Fail on a non-zero exit OR a tracing WARN/ERROR line in the output, so a
# smoke check that logs a failure and still exits 0 cannot pass.
# tracing colours the level with ANSI escapes even when stderr is a file, so
# they are stripped before matching.
# Allowlist (each entry needs a comment saying why it is benign): see the
# `grep -v` stage in mcp_bad_lines below.
mcp_ok=1
(cd "$mcp_scratch" \
    && "$codanna_bin" index src --no-progress > /dev/null \
    && "$codanna_bin" mcp-test) > "$mcp_log" 2>&1 || mcp_ok=0
# Allowlisted: "current pointer is missing or torn" is logged by the first
# `index` run on a fresh scratch workspace that has no generation pointer yet;
# it is expected and benign there.
mcp_bad_lines=$(sed 's/\x1b\[[0-9;]*m//g' "$mcp_log" \
    | grep -E '(^|[[:space:]])(WARN|ERROR)([[:space:]]|:)' \
    | grep -v 'current pointer is missing or torn' || true)
if [ -n "$mcp_bad_lines" ]; then
    mcp_ok=0
fi
if [ "$mcp_ok" -eq 1 ]; then
    echo "PASS: MCP server"
else
    echo "FAIL: MCP server test (non-zero exit or WARN/ERROR in log); log follows"
    cat "$mcp_log"
    exit 1
fi

echo ""
echo "==================================================="
echo "All checks passed. Safe to push."
echo "==================================================="
