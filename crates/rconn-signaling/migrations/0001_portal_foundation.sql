-- Portal foundation: orgs, people, computers, tokens.
--
-- Conventions:
-- * Ids are UUIDs as lowercase hyphenated TEXT (the same strings the wire protocol uses).
-- * Times are Unix seconds (INTEGER).
-- * Every org-owned table carries org_id. Child rows reference their parent by
--   (org_id, id), so a row can never point at another org's row.

CREATE TABLE orgs (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    -- The org that existed before the portal (imported from the data dir's ca.pem).
    legacy        INTEGER NOT NULL DEFAULT 0 CHECK (legacy IN (0, 1)),
    created_at    INTEGER NOT NULL,
    -- Fleet CA certificate (PEM).
    ca_cert_pem   TEXT NOT NULL,
    -- Fleet CA private key, sealed with the server master key (see secrets.rs);
    -- the associated data is the org id. NULL: this server can't issue for the org.
    ca_key_sealed BLOB
);
-- At most one legacy org.
CREATE UNIQUE INDEX orgs_one_legacy ON orgs (legacy) WHERE legacy = 1;

-- People are global (one person can work for several orgs); id = OperatorId.
CREATE TABLE users (
    id           TEXT PRIMARY KEY,
    email        TEXT COLLATE NOCASE UNIQUE,
    display_name TEXT,
    created_at   INTEGER NOT NULL,
    disabled_at  INTEGER
);

CREATE TABLE memberships (
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    org_id     TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    role       TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'technician', 'viewer')),
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, org_id)
);
CREATE INDEX memberships_org ON memberships (org_id);

-- Username + password logins (argon2id PHC). Global, like users.
CREATE TABLE accounts (
    username      TEXT PRIMARY KEY,
    user_id       TEXT NOT NULL UNIQUE REFERENCES users (id) ON DELETE CASCADE,
    password_hash TEXT NOT NULL,
    created_at    INTEGER NOT NULL
);

-- Groups of people (technician teams).
CREATE TABLE groups (
    id         TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, name),
    UNIQUE (org_id, id)
);

-- Groups of computers; per-group session policy arrives with the policies phase.
CREATE TABLE host_groups (
    id         TEXT PRIMARY KEY,
    org_id     TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (org_id, name),
    UNIQUE (org_id, id)
);

-- Computer inventory of orgs. Computers without an account (the public CA) are never here.
-- The id is unique across all orgs.
CREATE TABLE hosts (
    id           TEXT PRIMARY KEY,
    org_id       TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    -- Name an admin gave it in the portal.
    admin_name   TEXT,
    -- What the host reports (HostHello).
    hostname     TEXT,
    os           TEXT,
    version      TEXT,
    -- JSON array of strings.
    capabilities TEXT NOT NULL DEFAULT '[]',
    enrolled_at  INTEGER,
    -- User id of whoever enrolled it (informational: not a foreign key, since tokens from
    -- an identity provider name operators that have no users row yet).
    enrolled_by  TEXT,
    last_seen    INTEGER,
    group_id     TEXT,
    revoked_at   INTEGER,
    -- A group of the same org only. Empty a group before deleting it.
    FOREIGN KEY (org_id, group_id) REFERENCES host_groups (org_id, id)
);
CREATE INDEX hosts_org ON hosts (org_id);
CREATE INDEX hosts_revoked ON hosts (revoked_at) WHERE revoked_at IS NOT NULL;

-- Revoked computers without an account (they have no inventory row).
CREATE TABLE revoked_public_hosts (
    host_id    TEXT PRIMARY KEY,
    revoked_at INTEGER NOT NULL
);

-- Operator tokens, stored as SHA-256 (hex). kind 'static': from operators.json or
-- `add-operator`, never expires unless expires_at is set.
CREATE TABLE api_tokens (
    id           TEXT PRIMARY KEY,
    org_id       TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    user_id      TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    token_sha256 TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL CHECK (kind IN ('static', 'v2')),
    label        TEXT,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER,
    last_used_at INTEGER,
    revoked_at   INTEGER
);
CREATE INDEX api_tokens_org ON api_tokens (org_id);

-- Admin and security events (DB-backed audit arrives with the audit phase; JSONL stays).
CREATE TABLE audit (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    org_id   TEXT REFERENCES orgs (id) ON DELETE CASCADE,
    ts_ms    INTEGER NOT NULL,
    actor_id TEXT,
    event    TEXT NOT NULL,
    -- JSON object with the event's fields.
    detail   TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX audit_org_ts ON audit (org_id, ts_ms);

-- Bumped by the triggers below whenever something the running server caches changes
-- (CAs, tokens, accounts, memberships, revocations). The server polls it.
CREATE TABLE config_version (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    version INTEGER NOT NULL
);
INSERT INTO config_version (id, version) VALUES (1, 0);

CREATE TRIGGER orgs_ins AFTER INSERT ON orgs
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER orgs_upd AFTER UPDATE ON orgs
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER orgs_del AFTER DELETE ON orgs
BEGIN UPDATE config_version SET version = version + 1; END;

CREATE TRIGGER users_upd AFTER UPDATE OF disabled_at ON users
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER users_del AFTER DELETE ON users
BEGIN UPDATE config_version SET version = version + 1; END;

CREATE TRIGGER memberships_ins AFTER INSERT ON memberships
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER memberships_upd AFTER UPDATE ON memberships
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER memberships_del AFTER DELETE ON memberships
BEGIN UPDATE config_version SET version = version + 1; END;

CREATE TRIGGER accounts_ins AFTER INSERT ON accounts
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER accounts_upd AFTER UPDATE ON accounts
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER accounts_del AFTER DELETE ON accounts
BEGIN UPDATE config_version SET version = version + 1; END;

CREATE TRIGGER api_tokens_ins AFTER INSERT ON api_tokens
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER api_tokens_upd AFTER UPDATE OF revoked_at, expires_at ON api_tokens
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER api_tokens_del AFTER DELETE ON api_tokens
BEGIN UPDATE config_version SET version = version + 1; END;

-- Only revocation matters to the cache; online/offline upserts must not bump the version.
CREATE TRIGGER hosts_ins_revoked AFTER INSERT ON hosts WHEN NEW.revoked_at IS NOT NULL
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER hosts_upd_revoked AFTER UPDATE OF revoked_at ON hosts
WHEN OLD.revoked_at IS NOT NEW.revoked_at
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER hosts_del_revoked AFTER DELETE ON hosts WHEN OLD.revoked_at IS NOT NULL
BEGIN UPDATE config_version SET version = version + 1; END;

CREATE TRIGGER revoked_public_ins AFTER INSERT ON revoked_public_hosts
BEGIN UPDATE config_version SET version = version + 1; END;
CREATE TRIGGER revoked_public_del AFTER DELETE ON revoked_public_hosts
BEGIN UPDATE config_version SET version = version + 1; END;
