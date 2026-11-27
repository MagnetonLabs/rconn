#!/usr/bin/env bash
# Move a running file-based rconn server (0.1.x: accounts.json, operators.json, revoked.json)
# onto the database release, with a full backup and a tested way back. DRY RUN unless --apply:
# it checks what it can without changing anything and prints every command it would run.
#
#   scripts/deploy-db.sh --image ghcr.io/rconn/signaling:0.2.0 [--owner-email you@example.com]
#                        [--owner-account admin] [--with-backup] [--dir /opt/rconn] [--apply]
#   scripts/deploy-db.sh rollback --backup /opt/rconn/backups/pre-db-<ts>.tgz [--apply]
#
# migrate (default):
#   0. preflight: compose file, signaling service, a host directory mounted at /keys (the
#      master key must survive container rebuilds and stay out of the data volume), the new
#      image has the database (`org` command); with --with-backup: the litestream service and
#      a mode-600 litestream.env
#   1. stop the server; tar the whole data volume to <dir>/backups/pre-db-<ts>.tgz (mode 600)
#      and copy the compose file next to it
#   2. switch the signaling image, start it: on start, import.rs moves the JSON files into
#      rconn.db and renames them *.imported
#   3. verify against the backup: the same usernames, operator token digests, revoked hosts and
#      people; the CA certificate and its sealed key; the legacy files renamed
#   4. --owner-email: `org set-owner` makes that person the legacy org's owner
#   5. --with-backup: start the Litestream sidecar and wait for its first snapshot
# rollback: stop, put the old compose file back, empty the volume, untar the backup, start.
#   Anything written to the database since the migration is lost.
#
# Other options: --compose FILE (default <dir>/compose.yaml), --service NAME (default signaling).
set -euo pipefail

DIR=/opt/rconn
COMPOSE=""
SERVICE=signaling
IMAGE=""
OWNER_EMAIL=""
OWNER_ACCOUNT=""
WITH_BACKUP=""
APPLY=""
MODE=migrate
BACKUP=""

die() { echo "deploy-db: FAIL: $*" >&2; exit 1; }
step() { echo; echo "== $*"; }
note() { echo "   $*"; }
need_arg() { [ $# -ge 2 ] || die "$1 needs a value"; }

if [ "${1:-}" = rollback ]; then MODE=rollback; shift; fi
while [ $# -gt 0 ]; do
  case "$1" in
    --apply) APPLY=1; shift ;;
    --dir) need_arg "$@"; DIR="$2"; shift 2 ;;
    --compose) need_arg "$@"; COMPOSE="$2"; shift 2 ;;
    --service) need_arg "$@"; SERVICE="$2"; shift 2 ;;
    --image) need_arg "$@"; IMAGE="$2"; shift 2 ;;
    --owner-email) need_arg "$@"; OWNER_EMAIL="$2"; shift 2 ;;
    --owner-account) need_arg "$@"; OWNER_ACCOUNT="$2"; shift 2 ;;
    --with-backup) WITH_BACKUP=1; shift ;;
    --backup) need_arg "$@"; BACKUP="$2"; shift 2 ;;
    -h|--help) sed -n '2,26p' "$0"; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done
COMPOSE="${COMPOSE:-$DIR/compose.yaml}"
[ -f "$COMPOSE" ] || die "no compose file $COMPOSE (--dir or --compose)"
command -v docker >/dev/null || die "docker is needed"
command -v python3 >/dev/null || die "python3 is needed"
dc() { docker compose -f "$COMPOSE" "$@"; }
# The backup profile only resolves once its env file exists.
PROFILE=()
[ ! -f "$DIR/litestream.env" ] || PROFILE=(--profile backup)
# Run, or (dry run) print, a command. Redirections go through `sh -c` so they print too.
x() {
  if [ -n "$APPLY" ]; then "$@"; else printf '   +'; printf ' %q' "$@"; echo; fi
}
[ -n "$APPLY" ] && echo "deploy-db: APPLY ($MODE, $COMPOSE)" || echo "deploy-db: dry run ($MODE, $COMPOSE); add --apply to do it"

# The service's resolved compose config, as JSON (image, volumes).
svc_json() {
  dc "${PROFILE[@]}" config --format json 2>/dev/null \
    | python3 -I -c 'import json,sys; c=json.load(sys.stdin); s=c["services"].get(sys.argv[1]); print(json.dumps(s) if s else "")' "$1"
}
compose_image() { svc_json "$SERVICE" | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("image",""))'; }

# ---------------------------------------------------------------------------------------
if [ "$MODE" = rollback ]; then
  [ -n "$BACKUP" ] || die "rollback needs --backup <dir>/backups/pre-db-<ts>.tgz"
  [ -f "$BACKUP" ] || die "no backup $BACKUP"
  OLD_COMPOSE="${BACKUP%.tgz}.compose.yaml"
  [ -f "$OLD_COMPOSE" ] || die "no compose backup $OLD_COMPOSE"
  tar -tzf "$BACKUP" | grep -qx './ca.pem' || die "$BACKUP holds no ca.pem; is it a data-volume backup?"
  step "rollback to $(basename "$BACKUP")"
  note "the database and everything written since the migration are discarded"
  x dc "${PROFILE[@]}" stop
  x cp "$OLD_COMPOSE" "$COMPOSE"
  # The old image has sh, find and tar; it runs as the volume's owner (10001).
  x sh -c 'docker compose -f "$1" run --rm --no-deps -T --entrypoint sh "$2" -c "find /data -mindepth 1 -delete && tar -C /data -xzf -" < "$3"' \
    _ "$COMPOSE" "$SERVICE" "$BACKUP"
  x dc up -d "$SERVICE"
  step "after a rollback"
  note "the Litestream replica now describes the discarded database: before migrating again,"
  note "empty the bucket prefix (LITESTREAM_PATH) or set a new LITESTREAM_PATH."
  exit 0
fi

# ---------------------------------------------------------------------------------------
[ -n "$IMAGE" ] || die "--image <the database release> is needed"
step "0. preflight"
svc="$(svc_json "$SERVICE")"
[ -n "$svc" ] || die "$COMPOSE has no service $SERVICE"
CUR_IMAGE="$(compose_image)"
note "current image: $CUR_IMAGE"
note "new image:     $IMAGE"
[ "$CUR_IMAGE" != "$IMAGE" ] || die "the compose file already runs $IMAGE"
grep -q "image: *$CUR_IMAGE *\$" "$COMPOSE" \
  || die "can't find the line 'image: $CUR_IMAGE' in $COMPOSE (an \${VAR}?); set it by hand to $IMAGE and rerun"
KEYS_DIR="$(printf '%s' "$svc" | python3 -I -c '
import json, sys
for v in json.load(sys.stdin).get("volumes", []):
    if v.get("target") == "/keys" and v.get("type") == "bind":
        print(v["source"])')"
if [ -z "$KEYS_DIR" ]; then
  die "$SERVICE mounts no host directory at /keys. Add under its volumes:
      - $DIR/keys:/keys
    and create it: install -d -o 10001 -g 10001 -m 700 $DIR/keys
    (the master key that seals the org CA keys lives there, outside the data volume and its backups)"
fi
[ -d "$KEYS_DIR" ] || die "$KEYS_DIR (mounted at /keys) does not exist: install -d -o 10001 -g 10001 -m 700 $KEYS_DIR"
note "master key:    $KEYS_DIR/master.key ($([ -e "$KEYS_DIR/master.key" ] && echo present || echo "created on first start"))"
if docker image inspect "$IMAGE" >/dev/null 2>&1; then :; else x docker pull "$IMAGE"; fi
if [ -n "$APPLY" ] || docker image inspect "$IMAGE" >/dev/null 2>&1; then
  docker run --rm "$IMAGE" org --help >/dev/null 2>&1 || die "$IMAGE has no database (no \`org\` command)"
  note "new image has the database: yes"
fi
if [ -n "$WITH_BACKUP" ]; then
  envf="$DIR/litestream.env"
  [ -f "$envf" ] || die "--with-backup: no $envf (see deploy/docker/litestream.env.example)"
  [ -n "$(svc_json litestream)" ] || die "--with-backup: $COMPOSE has no litestream service (copy it from deploy/install.sh or deploy/docker/compose.yaml)"
  if [ "$(uname)" = Linux ]; then m=$(stat -c %a "$envf"); else m=$(stat -f %Lp "$envf"); fi
  [ "$m" = 600 ] || [ "$m" = 400 ] || die "$envf is mode $m; chmod 600 it"
  note "litestream:    service present, $envf mode $m"
fi

TS="$(date -u +%Y%m%dT%H%M%SZ)"
BK_DIR="$DIR/backups"
BACKUP="$BK_DIR/pre-db-$TS.tgz"
step "1. stop the server and back up the data volume -> $BACKUP"
x install -d -m 700 "$BK_DIR"
x dc stop "$SERVICE"
x sh -c 'umask 077; docker compose -f "$1" run --rm --no-deps -T --entrypoint tar "$2" -C /data -czf - . > "$3"' \
  _ "$COMPOSE" "$SERVICE" "$BACKUP"
x cp "$COMPOSE" "${BACKUP%.tgz}.compose.yaml"
if [ -n "$APPLY" ]; then
  [ -s "$BACKUP" ] || die "backup is empty"
  if tar -tzf "$BACKUP" | grep -qx './rconn.db'; then
    x dc start "$SERVICE"
    die "the data volume already has rconn.db: this server was migrated before (restarted it unchanged)"
  fi
  tar -tzf "$BACKUP" | grep -qx './ca.pem' || { dc start "$SERVICE"; die "no ca.pem in the data volume (restarted it unchanged)"; }
  note "backup: $(wc -c < "$BACKUP" | tr -d ' ') bytes, $(tar -tzf "$BACKUP" | grep -vc '/$') files"
fi

step "2. switch to $IMAGE and start (imports the JSON files)"
x python3 -I -c 'import sys; p,old,new=sys.argv[1:]; s=open(p).read(); s=s.replace("image: "+old, "image: "+new, 1); open(p,"w").write(s)' \
  "$COMPOSE" "$CUR_IMAGE" "$IMAGE"
x dc up -d "$SERVICE"
if [ -n "$APPLY" ]; then
  for _ in $(seq 60); do
    dc logs --no-color "$SERVICE" 2>&1 | grep -q 'imported data-dir files' && break
    dc ps --status running -q "$SERVICE" | grep -q . || { dc logs --tail 30 "$SERVICE"; die "the server stopped; roll back: $0 rollback --backup $BACKUP --apply"; }
    sleep 1
  done
  dc logs --no-color "$SERVICE" 2>&1 | grep 'imported data-dir files' | tail -1 | sed 's/^/   /'
fi

step "3. verify the database against the backup"
if [ -n "$APPLY" ]; then
  V="$(mktemp -d)"; trap 'rm -rf "$V"' EXIT; chmod 700 "$V"
  mkdir "$V/before" "$V/after"
  tar -xzf "$BACKUP" -C "$V/before"
  ok=""
  for _ in 1 2 3; do
    rm -rf "$V/after"; mkdir "$V/after"
    dc exec -T "$SERVICE" tar -C /data -cf - . | tar -x -C "$V/after"
    if python3 -I - "$V/before" "$V/after" <<'PY'
import json, os, sqlite3, sys
before, after = sys.argv[1:3]
def load(name, default):
    p = os.path.join(before, name)
    return json.load(open(p)) if os.path.exists(p) else default
accounts = load("accounts.json", {})
operators = load("operators.json", {})
revoked = set(load("revoked.json", []))
con = sqlite3.connect(os.path.join(after, "rconn.db"))
if [r[0] for r in con.execute("PRAGMA integrity_check")] != ["ok"]:
    sys.exit(2)  # a torn copy of a live database: copy again
fails = []
def check(what, want, got):
    ok = want == got
    print(f"   {'ok  ' if ok else 'FAIL'} {what}: files {len(want) if hasattr(want, '__len__') else want}, database {len(got) if hasattr(got, '__len__') else got}")
    if not ok:
        fails.append(what)
q = lambda sql: {r[0] for r in con.execute(sql)}
orgs = con.execute("SELECT ca_cert_pem, ca_key_sealed IS NOT NULL FROM orgs WHERE legacy = 1").fetchall()
check("legacy org", 1, len(orgs))
if orgs:
    check("CA certificate", open(os.path.join(before, "ca.pem")).read().strip(), orgs[0][0].strip())
    if os.path.exists(os.path.join(before, "ca.key.pem")):
        check("CA key sealed", True, bool(orgs[0][1]))
check("accounts (usernames)", set(accounts), q("SELECT username FROM accounts"))
check("operator tokens (sha256)", set(operators), q("SELECT token_sha256 FROM api_tokens WHERE kind = 'static'"))
people = {a["operator_id"] for a in accounts.values()} | set(operators.values())
check("people (operator ids)", people, q("SELECT id FROM users") & people)
check("revoked hosts", revoked, q("SELECT id FROM hosts WHERE revoked_at IS NOT NULL UNION SELECT host_id FROM revoked_public_hosts") & revoked)
for f in ("accounts.json", "operators.json", "revoked.json"):
    if os.path.exists(os.path.join(before, f)):
        check(f"{f} renamed", True, os.path.exists(os.path.join(after, f + ".imported")) and not os.path.exists(os.path.join(after, f)))
for (t,) in con.execute("SELECT name FROM sqlite_master WHERE type = 'table' AND substr(name, 1, 1) != '_' AND name NOT LIKE 'sqlite_%' ORDER BY name"):
    n = con.execute('SELECT count(*) FROM "%s"' % t).fetchone()[0]
    print(f"        {t}: {n}")
sys.exit(1 if fails else 0)
PY
    then ok=1; break
    else rc=$?; [ $rc -eq 2 ] || break; sleep 2; fi
  done
  [ -n "$ok" ] || die "the database doesn't match the files; roll back: $0 rollback --backup $BACKUP --apply"
else
  note "(compares usernames, token digests, people, revoked hosts, CA cert/key and renamed files"
  note " between $BACKUP and a copy of the live data dir)"
fi

if [ -n "$OWNER_EMAIL" ]; then
  step "4. owner of the legacy org: $OWNER_EMAIL"
  own=(rconn-signaling org set-owner --email "$OWNER_EMAIL")
  [ -z "$OWNER_ACCOUNT" ] || own+=(--account "$OWNER_ACCOUNT")
  x dc exec -T "$SERVICE" "${own[@]}"
else
  step "4. owner: skipped (--owner-email); set one before opening sign-ups:"
  note "docker compose -f $COMPOSE exec $SERVICE rconn-signaling org set-owner --email <email> [--account <username>]"
fi

if [ -n "$WITH_BACKUP" ]; then
  step "5. start Litestream"
  x dc --profile backup up -d litestream
  if [ -n "$APPLY" ]; then
    for _ in $(seq 60); do
      dc --profile backup logs --no-color litestream 2>&1 | grep -q 'snapshot complete' && break
      sleep 1
    done
    if dc --profile backup logs --no-color litestream 2>&1 | grep -q 'snapshot complete'; then
      note "first snapshot uploaded"
    else
      dc --profile backup logs --tail 20 litestream; die "Litestream has not uploaded a snapshot after 60 s (the server keeps running)"
    fi
  fi
else
  step "5. Litestream: skipped (--with-backup)"
fi

step "done"
note "check a restore now:  scripts/db-restore-drill.sh --env-file $DIR/litestream.env --master-key $KEYS_DIR/master.key"
note "copy $KEYS_DIR/master.key somewhere safe and apart from the bucket (password manager)"
note "way back:             $0 rollback --backup $BACKUP --apply"
