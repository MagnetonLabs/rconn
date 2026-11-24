#!/usr/bin/env bash
# rconn self-hosted server installer for a fresh Linux VM (Ubuntu/Debian/Amazon Linux/RHEL).
#
#   curl -fsSL <url>/install.sh | sudo RCONN_HOSTNAME=rconn.example.com bash
#
# Installs Docker if needed, writes /opt/rconn/compose.yaml, initialises the fleet CA and
# server TLS identity, creates an admin account (password printed once), starts the server
# with the built-in TURN relay, and prints what to paste into hosts and viewers.
#
# Env:
#   RCONN_HOSTNAME   public DNS name or IP clients use (default: this VM's public IP)
#   RCONN_PUBLIC_IP  public IP for TURN (default: auto-detected)
#   RCONN_IMAGE      server image (default: ghcr.io/rconn/signaling:latest)
#   RCONN_ADMIN      admin account name (default: admin)
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
IMAGE="${RCONN_IMAGE:-ghcr.io/rconn/signaling:latest}"
DIR=/opt/rconn
ADMIN="${RCONN_ADMIN:-admin}"

detect_ip() {
  # Cloud metadata first (works without outbound internet), then a public echo service.
  curl -fs -m 2 -H "Metadata-Flavor: Google" \
    http://169.254.169.254/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip 2>/dev/null && return
  TOKEN=$(curl -fs -m 2 -X PUT -H "X-aws-ec2-metadata-token-ttl-seconds: 60" http://169.254.169.254/latest/api/token 2>/dev/null || true)
  [ -n "$TOKEN" ] && curl -fs -m 2 -H "X-aws-ec2-metadata-token: $TOKEN" http://169.254.169.254/latest/meta-data/public-ipv4 2>/dev/null && return
  curl -fs -m 2 -H "Metadata: true" \
    "http://169.254.169.254/metadata/instance/network/interface/0/ipv4/ipAddress/0/publicIpAddress?api-version=2021-02-01&format=text" 2>/dev/null && return
  curl -fs -m 5 https://api.ipify.org
}

PUBLIC_IP="${RCONN_PUBLIC_IP:-$(detect_ip)}"
HOST="${RCONN_HOSTNAME:-$PUBLIC_IP}"
[ -n "$PUBLIC_IP" ] || { echo "could not detect the public IP; set RCONN_PUBLIC_IP" >&2; exit 1; }
URL="wss://$HOST:443/v1/ws"

if ! command -v docker >/dev/null; then
  echo "== installing Docker"
  curl -fsSL https://get.docker.com | sh
fi
systemctl enable --now docker >/dev/null 2>&1 || true

mkdir -p "$DIR"
cat > "$DIR/compose.yaml" <<YAML
services:
  signaling:
    image: $IMAGE
    network_mode: host
    restart: unless-stopped
    environment:
      RCONN_BIND: 0.0.0.0:443
      RCONN_SERVER_ID: $URL
      RCONN_TURN_PUBLIC_IP: $PUBLIC_IP
      RUST_LOG: info
    volumes:
      - rconn-data:/data
    command: ["serve"]
volumes:
  rconn-data:
YAML

run() { docker compose -f "$DIR/compose.yaml" run --rm -T signaling "$@"; }

if ! docker compose -f "$DIR/compose.yaml" run --rm -T --entrypoint test signaling -f /data/ca.pem 2>/dev/null; then
  echo "== initialising fleet CA and server identity"
  INIT=$(run init --hostname "$HOST")
  echo "$INIT" > "$DIR/init.txt"
  PASSWORD=$(head -c 18 /dev/urandom | base64 | tr -d '/+=' | head -c 20)
  echo "$PASSWORD" | run add-account --username "$ADMIN" >/dev/null
  NEW_ADMIN=1
fi
FP=$(grep -o 'sha256:[0-9a-f]\{64\}' "$DIR/init.txt")

echo "== starting"
docker compose -f "$DIR/compose.yaml" up -d

cat <<DONE

rconn server is running.

  Server URL        $URL
  Server pin        $FP
  Open these ports  TCP 443, UDP 3478, UDP 49160-49260
DONE
if [ -n "${NEW_ADMIN:-}" ]; then
  echo "  Admin login       $ADMIN / $PASSWORD   (shown once; change it)"
fi
cat <<DONE

Viewer:  rconn-viewer --server $URL --fingerprint $FP login --username $ADMIN
Host:    docker compose -f $DIR/compose.yaml run --rm -v \$PWD/host1:/out signaling enroll-host --out /out
         then on the host (admin):  rconn-host-service configure --server $URL --fingerprint $FP --identity-from host1
                                    rconn-host-service install
DONE
