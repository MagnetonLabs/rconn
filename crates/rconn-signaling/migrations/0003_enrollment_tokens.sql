-- Enrollment tokens: a computer that presents one joins the token's org (and group).
--
-- The secret is shown once when the token is made; only its SHA-256 (hex) is stored.

CREATE TABLE enrollment_tokens (
    id           TEXT PRIMARY KEY,
    org_id       TEXT NOT NULL REFERENCES orgs (id) ON DELETE CASCADE,
    label        TEXT NOT NULL,
    token_sha256 TEXT NOT NULL UNIQUE,
    -- Computers enrolled with it join this group. A group of the same org (checked when
    -- the token is made and again when it is used); deleting the group leaves the token
    -- ungrouped.
    group_id     TEXT REFERENCES host_groups (id) ON DELETE SET NULL,
    expires_at   INTEGER NOT NULL,
    -- NULL: unlimited.
    max_uses     INTEGER CHECK (max_uses IS NULL OR max_uses >= 1),
    uses         INTEGER NOT NULL DEFAULT 0,
    -- User id of the admin who made it (informational, like hosts.enrolled_by).
    created_by   TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    UNIQUE (org_id, id)
);
CREATE INDEX enrollment_tokens_org ON enrollment_tokens (org_id);
