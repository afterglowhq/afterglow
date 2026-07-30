# ADR-001: Append-only SQLite store

Accepted, 2026-07-30.

## Context

The product is the time series.
Snapshots that exist can never be recomputed, so the store's first job is to make losing or corrupting them hard.
Everything runs on one host, and will for a long time; the write load is a few tens of thousands of rows a day.

## Decision

One SQLite database via rusqlite, replicated continuously off-host with Litestream.
Postgres is a non-goal until there is more than one host.

Snapshots are append-only, enforced in the schema, not by convention:

```sql
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
```

Identity is GitHub's numeric repo id.
`full_name` is display only and gets overwritten on rename or transfer, so a renamed repo keeps one unbroken series.
A repo that becomes unobservable (deleted, private, blocked) is marked `inactive`; its series stays.

The prehistory tables hold the pre-2026 gross star history reconstructed from archived events.
That data was harvested keyed by `owner/name`, and GitHub's id for those names is not always recoverable, so `repo_id` is resolved lazily: when a tracked repo's name matches, the link is made; unresolved rows stay name-keyed.
Renames that happened before the harvest can split a series; accepted, and covered by the fidelity label the whole lane carries (see ADR-003).
`capture_month` stores the per-month correction factors for the archive's degraded final year.

Schema changes run as embedded numbered migrations keyed off `PRAGMA user_version`, applied at startup.
Later tables (enrollment queue, opt-out list) arrive by migration; this file is not edited to add them.

## Consequences

- Retention is unbounded on purpose.
  At roughly 50 bytes a row, decades of daily snapshots for 50k repos fit in single-digit gigabytes.
- The importer for the bootstrap data (the TSV the launchd collector has been writing since 2026-07-30, plus the prehistory harvest) resolves legacy name-keyed rows to repo ids once, at import time.
- Anything that wants to "fix" a bad snapshot must instead record a new observation.
  If a systematic collection bug ever demands redaction, that is a migration with its own ADR, not an UPDATE.
