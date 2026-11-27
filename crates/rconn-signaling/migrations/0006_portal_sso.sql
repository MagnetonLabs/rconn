-- Company single sign-on (OIDC) and passkeys.

-- A company's identity provider: we are a confidential OIDC client of it.
CREATE TABLE org_sso (
    org_id               TEXT PRIMARY KEY REFERENCES orgs (id) ON DELETE CASCADE,
    enabled              INTEGER NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    issuer               TEXT NOT NULL DEFAULT '',
    client_id            TEXT NOT NULL DEFAULT '',
    -- Sealed with the server master key, purpose `sso-client-secret` (see secrets.rs).
    client_secret_sealed BLOB,
    -- Role of people who join through SSO. Never owner.
    auto_join_role       TEXT NOT NULL DEFAULT 'viewer'
                         CHECK (auto_join_role IN ('admin', 'technician', 'viewer')),
    updated_at           INTEGER NOT NULL
);

-- Email domains a company claims. Proven by a DNS TXT record `rconn-verify=<token>` at the
-- domain apex. Several companies may claim a domain; only one can have it verified.
CREATE TABLE org_domains (
    org_id      TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    domain      TEXT NOT NULL COLLATE NOCASE,
    token       TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    verified_at INTEGER,
    PRIMARY KEY (org_id, domain)
);
CREATE UNIQUE INDEX org_domains_verified ON org_domains (domain) WHERE verified_at IS NOT NULL;

-- Who an IdP account is here: (company, issuer, subject) -> person.
CREATE TABLE sso_identities (
    org_id     TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    issuer     TEXT NOT NULL,
    subject    TEXT NOT NULL,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    last_used  INTEGER NOT NULL,
    PRIMARY KEY (org_id, issuer, subject)
);
CREATE INDEX sso_identities_user ON sso_identities (user_id);

-- SSO sign-ins in progress (single use, ten minutes). The PKCE verifier isn't stored: it
-- is derived from the browser's `__Host-rconn_sso` cookie, which only the browser holds.
CREATE TABLE sso_logins (
    state_sha256   TEXT PRIMARY KEY,
    org_id         TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    browser_sha256 TEXT NOT NULL,
    nonce          TEXT NOT NULL,
    -- The IdP the flow started with; a changed configuration ends the flow.
    issuer         TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    expires_at     INTEGER NOT NULL
);
CREATE INDEX sso_logins_expires ON sso_logins (expires_at);

-- Passkeys (WebAuthn credentials). `passkey` is webauthn-rs's Passkey as JSON: the public
-- key and signature counter, nothing secret.
CREATE TABLE passkeys (
    id         TEXT PRIMARY KEY,
    user_id    TEXT NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- base64url credential id.
    cred_id    TEXT NOT NULL UNIQUE,
    name       TEXT NOT NULL,
    passkey    TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used  INTEGER
);
CREATE INDEX passkeys_user ON passkeys (user_id);

-- WebAuthn ceremonies in progress (single use, five minutes). Registration: keyed by the
-- sign-in that started it. Sign-in: keyed by the browser's `__Host-rconn_passkey` cookie;
-- user_id NULL when the email matched nobody with a passkey (the ceremony can't succeed).
CREATE TABLE webauthn_challenges (
    id_sha256  TEXT PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('register', 'login')),
    user_id    TEXT REFERENCES users (id) ON DELETE CASCADE,
    email      TEXT,
    -- webauthn-rs ceremony state as JSON.
    state      TEXT NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX webauthn_challenges_expires ON webauthn_challenges (expires_at);
