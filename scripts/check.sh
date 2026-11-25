#!/usr/bin/env bash
# Pre-commit gate: format, clippy (host + Windows + Linux), licenses, tests. Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."
LLVM_PREFIX="$(brew --prefix llvm 2>/dev/null || true)"
export PATH="$LLVM_PREFIX/bin:$PATH"
cargo fmt --all --check
cargo clippy -q --workspace --all-targets -- -D warnings
if command -v cargo-xwin >/dev/null; then
  cargo xwin clippy -q --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
fi
if docker image inspect rconn-linux-build >/dev/null 2>&1; then
  # Linux (deploy/linux/Dockerfile.build); target dir and caches live in Docker volumes.
  docker run --rm -v "$PWD":/src -v rconn-cargo:/usr/local/cargo/registry \
    -v rconn-rustup:/usr/local/rustup -v rconn-linux-target:/target rconn-linux-build \
    cargo clippy -q --workspace --all-targets -- -D warnings
fi
cargo deny --log-level error check
if command -v shellcheck >/dev/null; then shellcheck -S warning deploy/install.sh scripts/*.sh; fi
if command -v tofu >/dev/null; then tofu fmt -check -recursive deploy/terraform >/dev/null; fi
cargo nextest run --workspace --status-level fail
echo "check: all passed"
