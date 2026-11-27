-- Admin portal sign-in: browser sessions, magic links, invites.
--
-- Secrets (session ids, link tokens, codes, nonces) are stored as SHA-256 hex only.
-- Rate-limit state lives in memory (the server is single-node).

-- What a company pays for; the legacy org is a self-hosted one.
ALTER TABLE orgs ADD COLUMN plan TEXT NOT NULL DEFAULT 'free';
UPDATE orgs SET plan = 'self_hosted' WHERE legacy = 1;

-- Portal sign-ins (cookie `__Host-rconn_session`). Revocable; never shared with /v1/ws.
CREATE TABLE sessions (
    id_sha256   TEXT PRIMARY KEY,
    user_id     TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    ip          TEXT,
    user_agent  TEXT,
    revoked_at  INTEGER
);
CREATE INDEX sessions_user ON sessions (user_id);
CREATE INDEX sessions_expires ON sessions (expires_at);

-- Sign-in links sent by email. Single use, short-lived, bound to the requesting browser
-- by a nonce cookie; from another browser the email's 6-digit code is needed too.
CREATE TABLE magic_links (
    token_sha256  TEXT PRIMARY KEY,
    email         TEXT NOT NULL COLLATE NOCASE,
    -- NULL: nobody has this email yet (hosted sign-up creates the person on use).
    user_id       TEXT REFERENCES users (id) ON DELETE CASCADE,
    -- SHA-256 of "<token>:<code>".
    code_sha256   TEXT NOT NULL,
    nonce_sha256  TEXT NOT NULL,
    code_attempts INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    used_at       INTEGER,
    ip            TEXT
);
CREATE INDEX magic_links_email ON magic_links (email);
CREATE INDEX magic_links_expires ON magic_links (expires_at);

-- Invitations to join an org (accepted with the emailed token).
CREATE TABLE invites (
    id           TEXT PRIMARY KEY,
    org_id       TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    email        TEXT NOT NULL COLLATE NOCASE,
    role         TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'technician', 'viewer')),
    token_sha256 TEXT NOT NULL UNIQUE,
    invited_by   TEXT REFERENCES users (id) ON DELETE SET NULL,
    created_at   INTEGER NOT NULL,
    expires_at   INTEGER NOT NULL,
    accepted_at  INTEGER,
    accepted_by  TEXT REFERENCES users (id) ON DELETE SET NULL,
    revoked_at   INTEGER,
    UNIQUE (org_id, id)
);
CREATE INDEX invites_org ON invites (org_id);
CREATE INDEX invites_email ON invites (email);
