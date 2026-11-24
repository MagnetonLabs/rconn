rconn — install guide (unsigned preview build)

1. SERVER (once, on a Linux VM with a public IP)
     curl -fsSL <your copy of deploy/install.sh> | sudo RCONN_HOSTNAME=<dns-or-ip> bash
   It prints: the server URL, the server pin (sha256:...), and an admin password.
   Enroll each host on the server:
     sudo docker compose -f /opt/rconn/compose.yaml run --rm -v $PWD/host1:/out signaling enroll-host --out /out
   Copy the host1 folder to the machine you want to control.

2. HOST (each Windows machine to control; PowerShell as Administrator, in this folder)
     .\install-host.ps1 -Server <server URL> -Fingerprint <server pin> -IdentityDir <path to host1>
   Remove later with .\uninstall-host.ps1

3. VIEWER (your computer)
     Windows: viewer\rconn-viewer.exe      macOS: rconn-viewer (from rconn-macos.zip)
     rconn-viewer --server <server URL> --fingerprint <server pin> login --username admin
     rconn-viewer --server <server URL> --fingerprint <server pin> connect <host id>
   The host id is in host1\host.id. Ctrl+Alt+Shift+M shows the toolbar.

Unsigned build: Windows SmartScreen and macOS Gatekeeper will warn on first run
(macOS: right-click > Open, or `xattr -d com.apple.quarantine rconn-viewer`).
