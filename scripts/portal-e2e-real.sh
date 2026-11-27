#!/usr/bin/env bash
# The admin portal end to end in a real browser against a real rconn-signaling: builds the
# web app and the server, starts it on localhost with a throwaway data dir (mail goes to its
# log, never out), seeds an owner, runs portal/e2e-real, and stops the server.
#
#   scripts/portal-e2e-real.sh            # build, run
#   SKIP_BUILD=1 scripts/portal-e2e-real.sh
#   PORTAL_SHOTS=/some/dir scripts/portal-e2e-real.sh   # screenshots (default portal/test-results/real-shots)
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}"

if [ -z "${SKIP_BUILD:-}" ]; then
  (cd portal && npm ci --no-audit --no-fund && npm run build)
  cargo build -q -p rconn-signaling --features portal-ui
  cargo build -q -p rconn-peer --example portal_host
fi
TARGET=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
BIN="$TARGET/debug/rconn-signaling"
HOST_BIN="$TARGET/debug/examples/portal_host"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/rconn-portal-e2e.XXXXXX")
PORT=${PORT:-$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')}
BASE="http://localhost:$PORT"
LOG="$WORK/server.log"
SERVER_PID=
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
  if [ -z "${KEEP_WORK:-}" ]; then rm -rf "$WORK"; else echo "kept $WORK"; fi
}
trap cleanup EXIT

export RCONN_DATA_DIR="$WORK/data" NO_COLOR=1
# Never mail anyone: without a Resend key the server writes mail to its log.
unset RESEND_API_KEY
OWNER_EMAIL=owner@acme.test
OWNER_PASSWORD="e2e-$(python3 -c 'import secrets; print(secrets.token_urlsafe(18))')"
"$BIN" init --hostname localhost >/dev/null
printf '%s\n' "$OWNER_PASSWORD" | "$BIN" add-account --username owner >/dev/null
"$BIN" org set-owner --email "$OWNER_EMAIL" --account owner >/dev/null

# --plain: localhost http (Chromium keeps Secure __Host- cookies for http://localhost).
"$BIN" serve --plain --bind "127.0.0.1:$PORT" --server-id "ws://localhost:$PORT/v1/ws" \
  --portal-url "$BASE" --allow-signup >"$LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  curl -fsS -o /dev/null "$BASE/api/v1/auth/options" 2>/dev/null && break
  kill -0 "$SERVER_PID" 2>/dev/null || { cat "$LOG"; exit 1; }
  sleep 0.1
done

cd "$ROOT/portal"
export PORTAL_SHOTS="${PORTAL_SHOTS:-$ROOT/portal/test-results/real-shots}"
RCONN_E2E_BASE="$BASE" RCONN_E2E_LOG="$LOG" RCONN_E2E_WORK="$WORK" \
  RCONN_E2E_OWNER_EMAIL="$OWNER_EMAIL" RCONN_E2E_OWNER_PASSWORD="$OWNER_PASSWORD" \
  RCONN_E2E_HOST_BIN="$HOST_BIN" RCONN_E2E_WS="ws://localhost:$PORT/v1/ws" \
  RCONN_E2E_SERVER_ID="ws://localhost:$PORT/v1/ws" \
  npx playwright test -c playwright.real.config.ts "$@" || { status=$?; echo "server log: $LOG"; KEEP_WORK=1; exit $status; }
