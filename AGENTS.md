# Notes for coding agents

## Builds and disk: one build uses the whole laptop, and `target/` only grows

Every git worktree has its own `target/`. Fourteen leftover worktrees once held 174 GB.
Editing code does not grow `target/`. New configurations do (a version bump, another `-p`
selection, check vs build vs test), and Cargo never deletes the old ones.

1. **Remove a worktree as soon as its branch is merged.** The branch stays in git.
   ```sh
   git worktree remove <path>   # refuses if it has uncommitted changes
   git worktree prune
   ```
   Before removing, check that it is merged: `git merge-base --is-ancestor <branch> main`.
   For cherry-picked work, `git cherry main <branch>` lists every commit as `-`.
2. **Before the first build in a new worktree, clone the main checkout's build output.**
   It takes 4 s and no extra disk; the first build then compiles 11 crates instead of 598.
   ```sh
   main="$(git worktree list --porcelain | sed -n '1s/^worktree //p')"
   wt="$(git rev-parse --show-toplevel)"
   mkdir -p "$wt/target/build.noindex"
   [ -e "$wt/target/debug" ]    || cp -c -R "$main/target/debug"    "$wt/target/debug"
   [ -e "$wt/target/openh264" ] || cp -c -R "$main/target/openh264" "$wt/target/openh264"
   [ -e "$wt/target/build.noindex/debug" ] ||
     cp -c -R "$main/target/build.noindex/debug" "$wt/target/build.noindex/debug"
   ```
3. **Run every build and test through the queue:** `scripts/cargo-slot.sh cargo <args>`
   (two at a time, 9 jobs each). One build already uses the whole machine: six at once
   took 186 s each and 17 GB of RAM, against 25 s and 4 GB alone. For `scripts/check.sh`,
   `scripts/package.sh`, iOS and Docker builds use `HEAVY=1 scripts/cargo-slot.sh <command>`.
   Don't start a long-running program through it: build through the queue, then run
   `target/debug/<name>` directly.
4. **One build selection per worktree, and keep it.** Default to the whole workspace,
   which reuses the clone from rule 2: `cargo build --workspace`,
   `cargo clippy --workspace --all-targets -- -D warnings`, `cargo nextest run --workspace`.
   Each different `-p <crate>` selection recompiled 41-131 crates and added 0.2-1.4 GiB.
   Leave cross-target builds (Windows, Linux, iOS) and `scripts/check.sh` /
   `scripts/package.sh` to the main checkout once the branch is merged.
   **While working, run only the tests your change can affect:** `scripts/test-changed.sh`
   (the changed crates and every crate that depends on them, same build selection, so
   nothing recompiles). After an `rconn-mobile` edit it ran 17 tests in 2.7 s instead of
   419 in 25 s; an `rconn-core` edit still runs all 419. Before merging, run the full
   `HEAVY=1 scripts/cargo-slot.sh scripts/check.sh`.
5. **Run `cargo clean --workspace` after a merge that brings a version bump, or when
   `target/debug` passes 12 GiB.** It removes this workspace's old output and keeps the
   dependencies (7.8 to 2.4 GiB in a test); the rebuild takes 23 s. A version bump alone
   leaves 4.2 GiB of stale output per checkout, so bump `[workspace.package] version`
   once per release, not per change.
6. **Never share `target/` or `build-dir` between worktrees** (`CARGO_TARGET_DIR`,
   `CARGO_BUILD_BUILD_DIR`). On Rust 1.100 the second checkout silently ran the first
   checkout's binary. Leave `CARGO_TARGET_DIR` unset: the scripts and the codec lookup
   expect `target/` inside the checkout.
7. **Leave sccache off.** Between two checkouts it hit 66 % (422 of 637) and saved 0 s;
   an empty cache cost +41 %. The earlier "25 hits, 997 misses" came from setting
   `CARGO_TARGET_DIR` as an environment variable (sccache 0.17 hashes every `CARGO_*`
   variable), not from the target path.
8. **Build intermediates live in `target/build.noindex`** (Spotlight skips `.noindex`
   folders). This Mac's `~/.cargo/config.toml` sets
   `build-dir = "{workspace-root}/target/build.noindex"`; `~/.local/bin/rust-noindex`
   sets it and moves older build output there. Final binaries stay in `target/debug` and
   `target/release`.
9. **Find what is big:**
   `du -hd 1 . .claude/worktrees target | sort -hr | head`.

## Never

- Never put anything about AI assistants or sessions in commits, branches, tags, PRs or releases.
