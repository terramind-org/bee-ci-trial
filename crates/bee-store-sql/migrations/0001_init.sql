-- bee metadata schema, version 1 (SPEC §6.1; logical tables in bee_store::types).
--
-- One file serves Postgres and SQLite. The runner replaces these tokens before applying it:
--   {KEY}           a 64-bit surrogate key that is the whole primary key
--   {BLOB}          bytes (BYTEA / BLOB)
--   {TEXT}          text compared bytewise (TEXT COLLATE "C" / TEXT)
--   {WITHOUT_ROWID} clusters SQLite tables on their primary key (empty on Postgres)
--
-- Rules: every key is org first, then repo; every integer is 64-bit; there are no foreign keys
-- (GC deletes in reverse write order and fsck checks the references instead).
-- Object states: 0 live, 1 quarantined, 2 purged.

CREATE TABLE orgs (
    org_key {KEY},
    id {BLOB} NOT NULL UNIQUE,
    name {TEXT} NOT NULL UNIQUE,
    created_at_ms BIGINT NOT NULL,
    -- Number of GC marks recorded so far; writers read it under a shared lock.
    gc_seq BIGINT NOT NULL DEFAULT 0
);

CREATE TABLE repos (
    repo_key {KEY},
    org_key BIGINT NOT NULL,
    id {BLOB} NOT NULL UNIQUE,
    name {TEXT} NOT NULL,
    created_at_ms BIGINT NOT NULL,
    draft_retention_days BIGINT NOT NULL,
    UNIQUE (org_key, name)
);

CREATE TABLE objects (
    org_key BIGINT NOT NULL,
    object {BLOB} NOT NULL,
    size BIGINT NOT NULL,
    state BIGINT NOT NULL,
    inline_bytes {BLOB},
    created_at_ms BIGINT NOT NULL,
    -- gc_seq of the org when a write last touched the row (GC defers on newer touches).
    touched_gen BIGINT NOT NULL,
    PRIMARY KEY (org_key, object)
) {WITHOUT_ROWID};

CREATE TABLE object_uploads (
    org_key BIGINT NOT NULL,
    object {BLOB} NOT NULL,
    principal {TEXT} NOT NULL,
    repo_key BIGINT,
    at_ms BIGINT NOT NULL,
    PRIMARY KEY (org_key, object, principal)
) {WITHOUT_ROWID};
CREATE INDEX object_uploads_by_time ON object_uploads (org_key, at_ms);

CREATE TABLE changesets (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    id {BLOB} NOT NULL,
    change_id {BLOB} NOT NULL,
    bytes {BLOB} NOT NULL,
    created_at_ms BIGINT NOT NULL,
    last_used_ms BIGINT NOT NULL,
    touched_gen BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key, id)
) {WITHOUT_ROWID};
CREATE INDEX changesets_by_change_id ON changesets (org_key, repo_key, change_id, id);

CREATE TABLE parent_edges (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    child {BLOB} NOT NULL,
    idx BIGINT NOT NULL,
    parent {BLOB} NOT NULL,
    PRIMARY KEY (org_key, repo_key, child, idx)
) {WITHOUT_ROWID};
CREATE INDEX parent_edges_by_parent ON parent_edges (org_key, repo_key, parent, child);

CREATE TABLE object_paths (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    object {BLOB} NOT NULL,
    -- Empty for the repository root.
    path {BLOB} NOT NULL,
    PRIMARY KEY (org_key, repo_key, object, path)
) {WITHOUT_ROWID};
CREATE INDEX object_paths_by_object ON object_paths (org_key, object, repo_key);

CREATE TABLE derived (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    dkey {BLOB} NOT NULL,
    type_name {TEXT} NOT NULL,
    version BIGINT NOT NULL,
    value {BLOB} NOT NULL,
    PRIMARY KEY (org_key, repo_key, dkey, type_name, version)
) {WITHOUT_ROWID};

CREATE TABLE refs (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    name {TEXT} NOT NULL,
    target {BLOB},
    version BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key, name)
) {WITHOUT_ROWID};
CREATE INDEX refs_by_target ON refs (org_key, repo_key, target);

CREATE TABLE ref_log (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    name {TEXT} NOT NULL,
    version BIGINT NOT NULL,
    old_target {BLOB},
    new_target {BLOB},
    principal {TEXT} NOT NULL,
    at_ms BIGINT NOT NULL,
    -- Landed entries: change_id (16) || source (34) || result (34), repeated.
    landed {BLOB} NOT NULL,
    PRIMARY KEY (org_key, repo_key, name, version)
) {WITHOUT_ROWID};

CREATE TABLE landed (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    ref_name {TEXT} NOT NULL,
    change_id {BLOB} NOT NULL,
    source {BLOB} NOT NULL,
    result {BLOB} NOT NULL,
    version BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key, ref_name, change_id)
) {WITHOUT_ROWID};

CREATE TABLE event_counters (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    allocated BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key)
) {WITHOUT_ROWID};

CREATE TABLE events (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    kind {TEXT} NOT NULL,
    payload {BLOB} NOT NULL,
    at_ms BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key, seq)
) {WITHOUT_ROWID};

CREATE TABLE redactions (
    org_key BIGINT NOT NULL,
    object {BLOB} NOT NULL,
    principal {TEXT} NOT NULL,
    reason {TEXT} NOT NULL,
    at_ms BIGINT NOT NULL,
    PRIMARY KEY (org_key, object)
) {WITHOUT_ROWID};

CREATE TABLE redaction_epochs (
    org_key BIGINT NOT NULL,
    repo_key BIGINT NOT NULL,
    epoch BIGINT NOT NULL,
    PRIMARY KEY (org_key, repo_key)
) {WITHOUT_ROWID};

CREATE TABLE gc_marks (
    org_key BIGINT NOT NULL,
    seq BIGINT NOT NULL,
    at_ms BIGINT NOT NULL,
    PRIMARY KEY (org_key, seq)
) {WITHOUT_ROWID};
