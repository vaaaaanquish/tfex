-- tfex index, schema version 2.
--
-- The index is a cache: everything except `state_mappings` and `plans` is
-- rebuilt by `tfex scan`, so changing this file only requires bumping
-- SCHEMA_VERSION (older indexes are then rejected and can simply be deleted).

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
    has_provider   INTEGER NOT NULL,
    -- Digest of the names, sizes and modification times of the configuration
    -- files that the rows below were read from. NULL when the directory has
    -- to be read again by the next scan regardless.
    fingerprint    TEXT,
    indexed_at     TEXT NOT NULL
) STRICT;

CREATE INDEX roots_abs_path ON roots (abs_path);

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
    -- The directory called, when the source is a local path. It is joined
    -- with `roots.abs_path`: the module may be indexed by another scan.
    target_path TEXT,
    file    TEXT    NOT NULL,
    line    INTEGER NOT NULL,
    PRIMARY KEY (root_id, name, file, line)
) STRICT;

CREATE INDEX module_calls_target_path ON module_calls (target_path);

-- References between the declarations of one directory: `source` cannot be
-- evaluated without `target`. Both are addresses as written in expressions
-- (`aws_vpc.main`, `data.x.y`, `module.x`, `local.x`, `var.x`, `output.x`).
CREATE TABLE dependencies (
    root_id TEXT    NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    source  TEXT    NOT NULL,
    target  TEXT    NOT NULL,
    -- Where the first such reference is.
    file    TEXT    NOT NULL,
    line    INTEGER NOT NULL,
    PRIMARY KEY (root_id, source, target)
) STRICT;

CREATE INDEX dependencies_target ON dependencies (root_id, target);

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

-- The outcome of the last `tfex plan` of each kind for a workspace: counts
-- and addresses only, never planned values.
CREATE TABLE plans (
    root_id           TEXT NOT NULL REFERENCES roots (id) ON DELETE CASCADE,
    workspace         TEXT NOT NULL,
    -- `plan` or `refresh-only`.
    mode              TEXT NOT NULL,
    status            TEXT NOT NULL,
    terraform_version TEXT,
    to_add            INTEGER NOT NULL,
    to_change         INTEGER NOT NULL,
    to_remove         INTEGER NOT NULL,
    to_import         INTEGER NOT NULL,
    drifted           INTEGER NOT NULL,
    planned_at        TEXT NOT NULL,
    error             TEXT,
    PRIMARY KEY (root_id, workspace, mode)
) STRICT;
