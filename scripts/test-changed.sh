#!/usr/bin/env bash
# Run only the tests a change can affect: the tests of every crate with a changed file and
# of every crate that depends on one (dev-dependencies included). Use it while working; run
# `HEAVY=1 scripts/cargo-slot.sh scripts/check.sh` (all tests, all platforms) before merging.
#
#   scripts/test-changed.sh [nextest args...]       changes against main
#   BASE=HEAD~1 scripts/test-changed.sh             changes against another commit
#   scripts/test-changed.sh --dry-run               print the selection, run nothing
#
# "Changed" = committed since the branch left BASE, plus uncommitted and untracked files.
# It builds the whole workspace as usual (rule 4 in AGENTS.md), so nothing recompiles
# because of the narrower selection; only fewer tests run.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

dry=""
if [ "${1:-}" = "--dry-run" ]; then dry=1; shift; fi
base="$(git merge-base HEAD "${BASE:-main}")"

changed="$( { git diff --name-only "$base"; git ls-files --others --exclude-standard; } | sort -u)"
if [ -z "$changed" ]; then echo "test-changed: nothing changed since ${BASE:-main}"; exit 0; fi

# Files that change how every crate builds: run everything.
everything=""
while IFS= read -r f; do
  case "$f" in
    Cargo.toml | Cargo.lock | rust-toolchain.toml | rust-toolchain | .cargo/* | deny.toml | \
      clippy.toml | .clippy.toml | build.rs | scripts/check.sh | scripts/fetch-openh264.sh)
      everything="$f"; break ;;
  esac
done <<<"$changed"

if [ -n "$everything" ]; then
  echo "test-changed: $everything changed, so every test runs"
  filter="all()"
else
  # Each file belongs to the workspace member whose folder holds it (longest match wins).
  crates="$(cargo metadata --no-deps --format-version 1 --offline 2>/dev/null |
    jq -r '.workspace_root as $r | .packages[]
           | "\(.name)\t\(.manifest_path | ltrimstr($r + "/") | rtrimstr("Cargo.toml"))"' |
    FILES="$changed" awk -F'\t' '
      { name[NR] = $1; dir[NR] = $2 }
      END {
        n = split(ENVIRON["FILES"], f, "\n")
        for (i = 1; i <= n; i++) {
          best = ""; len = 0
          for (j in dir) if (index(f[i], dir[j]) == 1 && length(dir[j]) > len) {
            best = name[j]; len = length(dir[j])
          }
          if (best != "") hit[best] = 1
        }
        for (c in hit) print c
      }' | sort)"
  if [ -z "$crates" ]; then
    echo "test-changed: no Rust crate changed (website, docs, deploy...), no Rust tests to run"
    exit 0
  fi
  filter="rdeps(${crates//$'\n'/) | rdeps(})"
  echo "test-changed: crates changed: $(paste -sd ' ' - <<<"$crates")"
fi
echo "test-changed: filter: $filter"
[ -z "$dry" ] || exit 0
exec "$(dirname "$0")/cargo-slot.sh" cargo nextest run --workspace -E "$filter" "$@"
