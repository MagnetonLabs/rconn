-- Session policies: each company's default (for ungrouped computers) and one per
-- computer group. `policy` is the API's Policy object as JSON, validated on write.

CREATE TABLE org_policies (
    org_id     TEXT PRIMARY KEY REFERENCES orgs (id) ON DELETE CASCADE,
    policy     TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT
);

CREATE TABLE host_group_policies (
    org_id     TEXT NOT NULL,
    group_id   TEXT NOT NULL,
    policy     TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    updated_by TEXT,
    PRIMARY KEY (org_id, group_id),
    FOREIGN KEY (org_id, group_id) REFERENCES host_groups (org_id, id) ON DELETE CASCADE
);

-- Companies that exist already keep what their computers got so far (no indicator);
-- new companies start from the portal's default.
INSERT INTO org_policies (org_id, policy, updated_at)
SELECT id,
       '{"clipboard":true,"file_transfer":true,"audio":true,"require_approval":false,'
       || '"show_indicator":false,"allow_access_password":true,"allowed_hours":null}',
       CAST(strftime('%s', 'now') AS INTEGER)
FROM orgs;

-- Newest first, per company (the audit page's order).
CREATE INDEX audit_org_id ON audit (org_id, id);
