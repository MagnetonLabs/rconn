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
#
# Database backup: put Litestream settings in /opt/rconn/litestream.env (mode 600; see
# deploy/docker/litestream.env.example) before or after installing; when that file exists
# the Litestream sidecar starts too (`docker compose --profile backup up -d`).
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
# Server master key (seals org CA keys in the database): outside the data volume and its
# backups. 10001 is the image's rconn user.
install -d -o 10001 -g 10001 -m 700 "$DIR/keys"
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
      - $DIR/keys:/keys
    command: ["serve"]
  # Continuous backup of rconn.db; runs when $DIR/litestream.env exists (profile backup).
  litestream:
    image: litestream/litestream:0.5.18
    profiles: [backup]
    restart: unless-stopped
    user: "10001:10001"
    depends_on: [signaling]
    command: ["replicate"]
    env_file:
      - path: $DIR/litestream.env
    volumes:
      - rconn-data:/data
      - $DIR/litestream.yml:/etc/litestream.yml:ro
volumes:
  rconn-data:
YAML
# Same as deploy/docker/litestream.yml (keep them in step).
cat > "$DIR/litestream.yml" <<'YAML'
dbs:
  - path: /data/rconn.db
    replica:
      type: s3
      bucket: ${LITESTREAM_BUCKET}
      path: ${LITESTREAM_PATH}
      endpoint: ${LITESTREAM_ENDPOINT}
      region: ${LITESTREAM_REGION}
      sync-interval: 1s
snapshot:
  interval: 24h
  retention: 720h
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
PROFILES=()
if [ -f "$DIR/litestream.env" ]; then
  chmod 600 "$DIR/litestream.env"
  PROFILES=(--profile backup)
  BACKUP="Litestream -> bucket $(sed -n 's/^LITESTREAM_BUCKET=//p' "$DIR/litestream.env")"
fi
docker compose -f "$DIR/compose.yaml" "${PROFILES[@]}" up -d

cat <<DONE

rconn server is running.

  Server URL        $URL
  Server pin        $FP
  Open these ports  TCP 443, UDP 3478, UDP 49160-49260
  Database backup   ${BACKUP:-off (add $DIR/litestream.env, then: docker compose -f $DIR/compose.yaml --profile backup up -d)}
  Master key        $DIR/keys/master.key  (copy it somewhere safe, apart from the backups)
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
