-- tfex index, schema version 1.
--
-- The index is a cache: everything except `state_mappings` is rebuilt by
-- `tfex scan`, so changing this file only requires bumping SCHEMA_VERSION
-- (older indexes are then rejected and can simply be deleted).

CREATE TABLE repositories (
    repo_key   TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    remote     TEXT,
    checkout   TEXT NOT NULL,
    head       TEXT,
    indexed_at TEXT NOT NULL
) STRICT;

-- A directory with Terraform configuration: a root, or a reusable module.
CREATE TABLE roots (
    id             TEXT PRIMARY KEY,
    repo_key       TEXT NOT NULL REFERENCES repositories (repo_key),
    relative_path  TEXT NOT NULL,
    abs_path       TEXT NOT NULL,
    classification TEXT NOT NULL,
    reason         TEXT NOT NULL,
    backend_type   TEXT NOT NULL,
    -- JSON object holding the allowlisted backend settings only.
    backend_config TEXT NOT NULL,
    indexed_at     TEXT NOT NULL
) STRICT;

CREATE TABLE resources (
    root_id TEXT    NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    address TEXT    NOT NULL,
    kind    TEXT    NOT NULL,
    type    TEXT    NOT NULL,
    name    TEXT    NOT NULL,
    file    TEXT    NOT NULL,
    line    INTEGER NOT NULL,
    PRIMARY KEY (root_id, address, file, line)
) STRICT;

CREATE INDEX resources_address ON resources (address);

CREATE TABLE module_calls (
    root_id TEXT    NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    name    TEXT    NOT NULL,
    source  TEXT,
    file    TEXT    NOT NULL,
    line    INTEGER NOT NULL,
    PRIMARY KEY (root_id, name, file, line)
) STRICT;

-- State locations given explicitly with `tfex map-state`.
CREATE TABLE state_mappings (
    root_id      TEXT NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    workspace    TEXT NOT NULL,
    location     TEXT NOT NULL,
    backend_type TEXT NOT NULL,
    PRIMARY KEY (root_id, workspace)
) STRICT;

-- State metadata only: never resource attributes or output values.
CREATE TABLE states (
    root_id           TEXT NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    workspace         TEXT NOT NULL,
    backend_type      TEXT NOT NULL,
    location          TEXT,
    status            TEXT NOT NULL,
    serial            INTEGER,
    lineage           TEXT,
    terraform_version TEXT,
    resource_count    INTEGER,
    fetched_at        TEXT,
    error             TEXT,
    PRIMARY KEY (root_id, workspace)
) STRICT;
