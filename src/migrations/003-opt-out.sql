-- A maintainer who asks to be left alone becomes a third repo status (ToS rule 4),
-- and widening a CHECK constraint is the one schema change SQLite has no ALTER for:
-- the table is rebuilt and renamed under its own name. The append-only triggers
-- live on snapshots, not here, so this is a plain copy; every column and the
-- full_name index come across, and the ids that snapshots hangs off never move.
--
-- The migration runner holds foreign keys off across this and runs
-- PRAGMA foreign_key_check before the commit, per SQLite's own procedure.
CREATE TABLE repos_rebuilt (
    id          INTEGER PRIMARY KEY,          -- GitHub numeric repo id
    full_name   TEXT NOT NULL,                -- owner/name at last sight; display only
    status      TEXT NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'inactive', 'opted_out')),
    lane        TEXT NOT NULL
                CHECK (lane IN ('scan', 'embed', 'manual', 'import')),
    enrolled_at TEXT NOT NULL,                -- UTC ISO-8601
    created_at  TEXT                          -- repo creation time per GitHub
);

INSERT INTO repos_rebuilt (id, full_name, status, lane, enrolled_at, created_at)
SELECT id, full_name, status, lane, enrolled_at, created_at FROM repos;

DROP TABLE repos;
ALTER TABLE repos_rebuilt RENAME TO repos;
CREATE INDEX repos_full_name ON repos (full_name);

-- The queue is shared by both request lanes, so a row has to say which one put it
-- there or the drain records the wrong lane on the repo (ADR-002). This rides the
-- same migration because 003 is unreleased: production is still at user_version 2
-- and has never seen either half of this file.
--
-- Existing rows are all badge requests, which is what the default backfills.
ALTER TABLE enroll_queue ADD COLUMN lane TEXT NOT NULL DEFAULT 'embed'
    CHECK (lane IN ('scan', 'embed', 'manual', 'import'));
