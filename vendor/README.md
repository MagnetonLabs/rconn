# Vendored crates

## webrtc 0.21.1 (patched)

Upstream `webrtc` 0.21.1 with `../patches/webrtc-0.21.1-relay-driver.patch` applied: the
peer-connection driver drained TURN relay output before core writes enqueued new relayed
packets, and pending relay output had no wakeup of its own, so relayed packets waited for
an unrelated event or timer.

Measured (Mac ↔ Windows ARM64 VM over Tailscale, 50-ping median):

| Path | Upstream | Patched |
|---|---|---|
| Relay-only RTT | ~150 ms | 1.4 ms |
| Relay-only connect | 4.4 s | 2.2 s |
| Direct RTT | 2.8 ms | 1.2 ms |

Remove this vendored copy (and the `[patch.crates-io]` entry in the workspace
`Cargo.toml`) once the fix is released upstream. Examples, tests and benches were
dropped from the copy to keep it small.
