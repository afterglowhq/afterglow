CREATE TABLE repos (
    id          INTEGER PRIMARY KEY,          -- GitHub numeric repo id
    full_name   TEXT NOT NULL,                -- owner/name at last sight; display only
    status      TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'inactive')),
    lane        TEXT NOT NULL
                CHECK (lane IN ('scan', 'embed', 'manual', 'import')),
    enrolled_at TEXT NOT NULL,                -- UTC ISO-8601
    created_at  TEXT                          -- repo creation time per GitHub
);
CREATE INDEX repos_full_name ON repos (full_name);

CREATE TABLE snapshots (
    repo_id     INTEGER NOT NULL REFERENCES repos (id),
    ts          TEXT NOT NULL,                -- UTC ISO-8601
    stars       INTEGER NOT NULL,
    forks       INTEGER,
    open_issues INTEGER,
    pushed_at   TEXT,
    subscribers INTEGER,                      -- only where a per-repo GET happened
    PRIMARY KEY (repo_id, ts)
) WITHOUT ROWID;

CREATE TRIGGER snapshots_no_update BEFORE UPDATE ON snapshots
BEGIN SELECT RAISE(ABORT, 'snapshots are append-only'); END;

CREATE TRIGGER snapshots_no_delete BEFORE DELETE ON snapshots
BEGIN SELECT RAISE(ABORT, 'snapshots are append-only'); END;

CREATE TABLE prehistory_monthly (
    full_name          TEXT NOT NULL,         -- owner/name as harvested
    month              TEXT NOT NULL,         -- YYYY-MM
    gross_watch_events INTEGER NOT NULL,
    repo_id            INTEGER REFERENCES repos (id),
    PRIMARY KEY (full_name, month)
) WITHOUT ROWID;

CREATE TABLE capture_month (
    month TEXT PRIMARY KEY,                   -- YYYY-MM
    ratio REAL NOT NULL                       -- archive capture vs. estimated true gross
) WITHOUT ROWID;
