#!/usr/bin/env bash
# Pre-commit gate: format, clippy (host + Windows), licenses, tests. Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="$(brew --prefix llvm 2>/dev/null)/bin:$PATH"
cargo fmt --all --check
cargo clippy -q --workspace --all-targets -- -D warnings
if command -v cargo-xwin >/dev/null; then
  cargo xwin clippy -q --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
fi
cargo deny --log-level error check
cargo nextest run --workspace --status-level fail
echo "check: all passed"
