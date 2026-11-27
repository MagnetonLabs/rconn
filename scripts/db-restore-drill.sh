#!/usr/bin/env bash
# Restore drill for the server database: pull the latest Litestream replica into a temp dir
# and prove it is usable. Read-only towards the bucket and the running server.
#
#   scripts/db-restore-drill.sh --env-file /opt/rconn/litestream.env [--master-key /opt/rconn/keys/master.key]
#
# Steps (exit 1 on the first failure):
#   1. litestream restore (latest, or --timestamp) -> <tmp>/rconn.db
#   2. PRAGMA integrity_check == ok
#   3. migrations: every applied migration matches crates/rconn-signaling/migrations
#      (version + sha384), when that directory is at hand
#   4. the server opens it (its own migration run) on a scratch copy:
#      with --master-key, `enroll-host` (also proves the key unseals the org CA);
#      without, `init` (refuses with "already holds org" after opening and migrating)
#   5. row counts per table (--expect FILE compares them, --counts-out FILE saves them)
#
# Options:
#   --env-file FILE     Litestream env file (default /opt/rconn/litestream.env)
#   --config FILE       Litestream config (default deploy/docker/litestream.yml, else /opt/rconn/litestream.yml)
#   --timestamp TS      restore the state at TS (RFC 3339) instead of the latest
#   --out FILE          keep the restored database here (refuses to overwrite)
#   --master-key FILE   the server master key, to check that it unseals the restored CA keys
#   --expect FILE       expected counts ("table count" lines); any difference fails
#   --counts-out FILE   write the counts ("table count" lines)
#   --migrations DIR    migrations to compare with (default: this checkout's)
#   --server-bin PATH   a local rconn-signaling for step 4 (else the image)
#   --image IMAGE       server image for step 4 (default $RCONN_IMAGE or ghcr.io/rconn/signaling:latest)
#   --network NAME      docker network for the Litestream container (e.g. a local MinIO's)
#   --skip-server       skip step 4
# Litestream runs from PATH when installed, else from $LITESTREAM_IMAGE (litestream/litestream:0.5.18).
# Needs python3 (integrity check, counts, migration check).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE=/opt/rconn/litestream.env
CONFIG=""
TIMESTAMP=""
OUT=""
MASTER_KEY=""
EXPECT=""
COUNTS_OUT=""
MIGRATIONS="$ROOT/crates/rconn-signaling/migrations"
SERVER_BIN=""
IMAGE="${RCONN_IMAGE:-ghcr.io/rconn/signaling:latest}"
NETWORK=""
SKIP_SERVER=""
LITESTREAM_IMAGE="${LITESTREAM_IMAGE:-litestream/litestream:0.5.18}"
DB_PATH=/data/rconn.db # the database's path as the config names it

die() { echo "drill: FAIL: $*" >&2; exit 1; }
step() { echo "== $*"; }
need_arg() { [ $# -ge 2 ] || die "$1 needs a value"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --env-file) need_arg "$@"; ENV_FILE="$2"; shift 2 ;;
    --config) need_arg "$@"; CONFIG="$2"; shift 2 ;;
    --timestamp) need_arg "$@"; TIMESTAMP="$2"; shift 2 ;;
    --out) need_arg "$@"; OUT="$2"; shift 2 ;;
    --master-key) need_arg "$@"; MASTER_KEY="$2"; shift 2 ;;
    --expect) need_arg "$@"; EXPECT="$2"; shift 2 ;;
    --counts-out) need_arg "$@"; COUNTS_OUT="$2"; shift 2 ;;
    --migrations) need_arg "$@"; MIGRATIONS="$2"; shift 2 ;;
    --server-bin) need_arg "$@"; SERVER_BIN="$2"; shift 2 ;;
    --image) need_arg "$@"; IMAGE="$2"; shift 2 ;;
    --network) need_arg "$@"; NETWORK="$2"; shift 2 ;;
    --skip-server) SKIP_SERVER=1; shift ;;
    -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

if [ -z "$CONFIG" ]; then
  for c in "$ROOT/deploy/docker/litestream.yml" /opt/rconn/litestream.yml; do
    [ -f "$c" ] && { CONFIG="$c"; break; }
  done
fi
[ -f "$CONFIG" ] || die "no Litestream config (--config)"
[ -f "$ENV_FILE" ] || die "no env file $ENV_FILE (--env-file)"
if [ "$(uname)" = Linux ]; then mode=$(stat -c %a "$ENV_FILE"); else mode=$(stat -f %Lp "$ENV_FILE"); fi
[ "$mode" = 600 ] || [ "$mode" = 400 ] || die "$ENV_FILE is mode $mode; chmod 600 it (it holds credentials)"
[ -z "$OUT" ] || [ ! -e "$OUT" ] || die "$OUT exists; pick a new --out path"
[ -z "$MASTER_KEY" ] || [ -r "$MASTER_KEY" ] || die "can't read the master key $MASTER_KEY"
command -v python3 >/dev/null || die "python3 is needed"
abspath() { python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$1"; }
CONFIG="$(abspath "$CONFIG")"; ENV_FILE="$(abspath "$ENV_FILE")"
[ -z "$MASTER_KEY" ] || MASTER_KEY="$(abspath "$MASTER_KEY")"

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rconn-drill.XXXXXX")"
trap 'rm -rf "$TMP"' EXIT
chmod 700 "$TMP"
RESTORED="$TMP/restore/rconn.db"
mkdir -p "$TMP/restore"

# -- 1. restore -------------------------------------------------------------------------
step "restore ${TIMESTAMP:-latest} from the replica of $DB_PATH"
ts_args=()
[ -z "$TIMESTAMP" ] || ts_args=(-timestamp "$TIMESTAMP")
started=$(date +%s)
if command -v litestream >/dev/null && [ -z "$NETWORK" ]; then
  # A subshell, so the credentials never reach this script's environment or argv.
  # shellcheck source=/dev/null
  ( set -a; . "$ENV_FILE"; set +a
    litestream restore -config "$CONFIG" -o "$RESTORED" "${ts_args[@]}" "$DB_PATH" )
else
  net=()
  [ -z "$NETWORK" ] || net=(--network "$NETWORK")
  docker run --rm "${net[@]}" --user "$(id -u):$(id -g)" --env-file "$ENV_FILE" \
    -v "$CONFIG:/etc/litestream.yml:ro" -v "$TMP/restore:/restore" \
    "$LITESTREAM_IMAGE" restore -o /restore/rconn.db "${ts_args[@]}" "$DB_PATH"
fi
[ -s "$RESTORED" ] || die "restore produced no database"
echo "restored $(wc -c < "$RESTORED" | tr -d ' ') bytes in $(( $(date +%s) - started ))s"

# -- 2 + 3 + 5. integrity, migrations, counts (python3's sqlite3, read-only) -----------
step "integrity check, migrations, row counts"
mig_dir=""
if [ -d "$MIGRATIONS" ]; then mig_dir="$MIGRATIONS"; else echo "(no migrations dir at $MIGRATIONS: migration file check skipped)"; fi
python3 -I - "$RESTORED" "$mig_dir" "$TMP/counts" <<'PY'
import hashlib, os, re, sqlite3, sys
db, mig_dir, counts_path = sys.argv[1:4]
# immutable: a WAL database opened read-only would need to create its -shm file
con = sqlite3.connect(f"file:{db}?mode=ro&immutable=1", uri=True)
res = [r[0] for r in con.execute("PRAGMA integrity_check")]
if res != ["ok"]:
    sys.exit("integrity_check: " + "; ".join(res[:10]))
print("integrity_check: ok")
fk = con.execute("PRAGMA foreign_key_check").fetchall()
if fk:
    sys.exit(f"foreign_key_check: {len(fk)} rows violate foreign keys, first {fk[0]}")
print("foreign_key_check: ok")
applied = {v: (bytes(c).hex(), ok) for v, c, ok in
           con.execute("SELECT version, checksum, success FROM _sqlx_migrations")}
bad = [v for v, (_, ok) in applied.items() if not ok]
if bad:
    sys.exit(f"migrations {bad} are marked failed")
if mig_dir:
    files = {}
    for f in os.listdir(mig_dir):
        m = re.match(r"(\d+)_.*\.sql$", f)
        if m:
            files[int(m.group(1))] = (f, hashlib.sha384(open(os.path.join(mig_dir, f), "rb").read()).hexdigest())
    for v, (f, digest) in sorted(files.items()):
        if v not in applied:
            print(f"migration {f}: not applied yet (the server applies it on start)")
        elif applied[v][0] != digest:
            sys.exit(f"migration {f}: checksum differs from the one applied")
    newer = sorted(set(applied) - set(files))
    if newer:
        sys.exit(f"migrations {newer} are applied but not in {mig_dir}: the database is newer than this checkout")
print(f"migrations applied: {sorted(applied)}")
tables = [r[0] for r in con.execute(
    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")]
with open(counts_path, "w") as out:
    for t in tables:
        n = con.execute(f'SELECT count(*) FROM "{t}"').fetchone()[0]
        out.write(f"{t} {n}\n")
PY
[ $? -eq 0 ] || die "database checks"

# -- 4. the server opens it -------------------------------------------------------------
if [ -z "$SKIP_SERVER" ]; then
  # Scratch tree: data/ (a copy of the restored database), keys/, out/. The server runs as
  # this user, so it can read the key copy.
  mkdir -p "$TMP/server/data" "$TMP/server/keys" "$TMP/server/out"
  cp "$RESTORED" "$TMP/server/data/rconn.db"
  if [ -n "$MASTER_KEY" ]; then
    install -m 600 "$MASTER_KEY" "$TMP/server/keys/master.key"
    step "server: open, migrate, unseal the org CA with the master key (enroll-host on a scratch copy)"
    cmd=(enroll-host --out SRV/out/host)
  else
    step "server: open and migrate a scratch copy (init refuses once it has read the org)"
    cmd=(init)
  fi
  if [ -n "$SERVER_BIN" ]; then
    srv=("$SERVER_BIN"); root="$TMP/server"
  else
    srv=(docker run --rm --user "$(id -u):$(id -g)" -e RUST_LOG=warn -v "$TMP/server:/srv" "$IMAGE")
    root=/srv
  fi
  set +e
  out=$("${srv[@]}" --data-dir "$root/data" --master-key-file "$root/keys/master.key" \
    "${cmd[@]/#SRV/$root}" 2>&1)
  rc=$?
  set -e
  if [ -n "$MASTER_KEY" ]; then
    [ $rc -eq 0 ] || die "server could not use the restored database with that master key: $out"
    echo "server: opened, migrated, unsealed the CA and issued a scratch host cert"
  else
    case "$out" in
      *"already holds org"*) echo "server: opened and migrated (org ${out##*already holds org })" ;;
      *) die "server could not open the restored database (exit $rc): $out" ;;
    esac
  fi
fi

# -- 5. counts --------------------------------------------------------------------------
step "row counts"
column -t < "$TMP/counts" 2>/dev/null || cat "$TMP/counts"
[ -z "$COUNTS_OUT" ] || cp "$TMP/counts" "$COUNTS_OUT"
if [ -n "$EXPECT" ]; then
  if diff <(sort "$EXPECT") <(sort "$TMP/counts") >"$TMP/diff"; then
    echo "counts match $EXPECT"
  else
    cat "$TMP/diff"; die "counts differ from $EXPECT"
  fi
fi
if [ -n "$OUT" ]; then
  mkdir -p "$(dirname "$OUT")"
  cp "$RESTORED" "$OUT" && chmod 600 "$OUT"
  echo "kept the restored database at $OUT"
fi
echo "drill: PASS"
