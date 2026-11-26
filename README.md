# Rconn

Fast, open-source remote desktop. Install it, your computer gets an ID like `482 915 306`,
and anyone you give that ID and your access password to can see and control it. No account.

- **Peer to peer.** Video, sound, mouse and keyboard go directly between the two devices,
  encrypted end to end (WebRTC, DTLS-SRTP). The server only introduces them, and relays
  encrypted traffic when no direct path exists.
- **The password never leaves your device.** The computer checks it itself with SPAKE2;
  the server stores no passwords and can't test guesses offline.
- **Fast.** Hardware H.264 where available (Media Foundation on Windows, VideoToolbox on
  macOS), only changed areas re-encoded, and the remote resolution follows your window.
- **Self-hostable.** One Docker container runs the introduction server and relay.

Website and downloads: **[rconn.app](https://rconn.app)**

## Platforms

| | Be controlled (host) | Control others (viewer) |
|---|---|---|
| Windows 11 (x64, ARM64); Windows 10 untested | ✅ including the login screen and UAC prompts | ✅ |
| macOS 13+ (Apple silicon, Intel) | ✅ | ✅ |
| Linux (Ubuntu 24.04+, `.deb`) | ✅ X11, and Wayland via the desktop portal (tested on GNOME) | ✅ |
| iPhone / iPad | — | 🚧 in development (`ios/`) |

On Wayland, the desktop asks once for permission to share the screen and allow remote
control, and GNOME doesn't start sharing while the screen is locked. For unattended
Linux machines, log in with an X11 session ("Ubuntu on Xorg") or turn off automatic
screen lock.

## How it works

```
viewer ──(1) "connect to 482 915 306"──▶ server ◀── host (keeps a connection open)
   │                                                   │
   └──────(2) direct encrypted WebRTC session──────────┘
          (relayed through the server's TURN relay only if no direct path exists)
```

1. **Introduction.** Both sides talk to the server over TLS. It forwards the connection
   offer and network candidates. This takes about a second.
2. **Session.** The devices connect directly. Before anything is shown or accepted, the
   viewer and the computer prove to each other that they know the access password
   (SPAKE2 over the session's encrypted data channel). Wrong guesses are rate-limited
   on the computer and on the server.

See [rconn.app/security](https://rconn.app/security) for the full trust model, including
what a malicious server could and couldn't do.

## Repository layout

| Path | What |
|---|---|
| `crates/rconn-core` | Protocol types, settings, password exchange (SPAKE2), IDs |
| `crates/rconn-signaling` | The server: introductions, enrollment, built-in TURN relay, audit log |
| `crates/rconn-peer` | Client side of a session: signaling client, WebRTC, video receive |
| `crates/rconn-media` | Screen capture (DXGI, ScreenCaptureKit, X11, PipeWire), H.264, Opus |
| `crates/rconn-host-agent` | The host: capture, encode, input injection, access control |
| `crates/rconn-host-service` | Windows service that runs the agent on the right desktop |
| `crates/rconn-host-settings` | Host settings model and admin window |
| `crates/rconn-viewer` | Session window (egui + wgpu) and headless test client |
| `crates/rconn-app` | The desktop app: this computer's ID, connect box, settings, tray |
| `crates/rconn-mobile`, `ios/` | iOS viewer: Rust core (uniffi) and SwiftUI app |
| `deploy/` | Docker image, installers (Windows NSIS, macOS DMG, Linux .deb), Terraform |
| `website/` | rconn.app (Astro, static) |

## Building

Rust (stable, see `rust-toolchain.toml`). Everything builds from a Mac:

```sh
cargo build --release -p rconn-app -p rconn-viewer -p rconn-host-agent   # this OS

# Windows (cross-compiled): cargo install cargo-xwin
cargo xwin build --release --target x86_64-pc-windows-msvc -p rconn-app -p rconn-viewer \
  -p rconn-host-agent -p rconn-host-service -p rconn-host-settings

# Linux (in Docker)
docker build -f deploy/linux/Dockerfile.build -t rconn-linux-build deploy/linux
docker run --rm -v "$PWD":/src rconn-linux-build cargo build --release -p rconn-app

# All installers into dist/ (macOS DMG, Windows setup .exe, Linux .deb)
scripts/package.sh

# iOS
ios/build-rust.sh && (cd ios && xcodegen) && open ios/Rconn.xcodeproj
```

H.264 software encoding and decoding use Cisco's prebuilt OpenH264, downloaded at build
or first run (`scripts/fetch-openh264.sh`) so Cisco's patent license applies. Rconn never
ships an OpenH264 it compiled itself.

Before sending changes, run the same checks as CI:

```sh
scripts/check.sh   # fmt, clippy (macOS, Windows, Linux), cargo-deny, tests
```

## Self-hosting the server

```sh
docker build -f deploy/docker/Dockerfile -t rconn-signaling .
docker run --rm -v rconn-data:/data rconn-signaling init --hostname rconn.example.com
docker run -d --network host -v rconn-data:/data \
  -e RCONN_SERVER_ID=wss://rconn.example.com/v1/ws -e RCONN_TURN_PUBLIC_IP=<public ip> \
  rconn-signaling serve
```

Put a TLS proxy such as Caddy in front (with `RCONN_PLAIN=true` and `RCONN_BIND=127.0.0.1:8080`),
or let the server use its own certificate and pin its fingerprint in the apps. Open TCP 443,
UDP 3478 and UDP 49160–49260. `deploy/install.sh` does all of this on a fresh Linux VM.
Then set your server's address in each app under Settings → Server.

Back up the server's data volume: it holds the certificate authorities that computer IDs
are tied to.

## License

[GNU Affero General Public License v3.0](LICENSE). If you run a modified Rconn server or
app for other people, you must offer them your changes under the same license.
