#!/usr/bin/env bash
# Run a build command in one of two machine-wide build slots, so parallel agents don't
# oversubscribe the laptop (one workspace build already uses all 18 cores).
#
#   scripts/cargo-slot.sh <command...>           wait for one of two slots, 9 jobs
#   HEAVY=1 scripts/cargo-slot.sh <command...>   wait for both slots, run alone, all cores
#
# The slot is held until the command and its children exit, so don't start a long-running
# program (a server) through it: build through the queue, then run target/debug/<name>.
dir="${SLOT_DIR:-$HOME/Library/Caches/rconn-build-slots}"; mkdir -p "$dir"
if [ -n "${HEAVY:-}" ]; then
  exec 8>"$dir/slot1" 9>"$dir/slot2"
  lockf -s 8 && lockf -s 9 && exec "$@"
  exit 1
fi
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-9}"
while :; do
  for i in 1 2; do
    exec 9>"$dir/slot$i"
    if lockf -s -t 0 9; then exec "$@"; fi
  done
  sleep 0.2
done
