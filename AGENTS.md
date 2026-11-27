# Notes for coding agents

## Disk: Rust build output is 10-30 GB per checkout

Every git worktree has its own `target/`. Fourteen leftover worktrees once held 174 GB.

1. **Remove a worktree as soon as its branch is merged.** The branch stays in git.
   ```sh
   git worktree remove <path>   # refuses if it has uncommitted changes
   git worktree prune
   ```
   Before removing, check that it is merged: `git merge-base --is-ancestor <branch> main`.
   For cherry-picked work, `git cherry main <branch>` lists every commit as `-`.
2. **In a worktree, build only what you changed.** Use `cargo test -p <crate>`, not the whole
   workspace. Leave cross-target builds (Windows, Linux, iOS) and `scripts/check.sh` /
   `scripts/package.sh` to the main checkout once the branch is merged.
3. **Don't share one `target/` between worktrees** (`CARGO_TARGET_DIR`). Cargo locks it,
   so parallel builds wait on each other.
4. **sccache doesn't help across worktrees.** Its cache key includes the absolute target
   path: a second worktree got 25 hits and 997 misses.
5. **Find what is big:**
   `du -hd 1 . .claude/worktrees target | sort -hr | head`.

## Never

- Never put anything about AI assistants or sessions in commits, branches, tags, PRs or releases.
