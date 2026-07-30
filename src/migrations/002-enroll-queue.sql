-- Enrollment overflow: a badge request past the daily lane budget parks the name
-- here and the next snapshot run enrolls it. Never silently dropped (ADR-002).
-- Keyed by owner/name because the numeric id is exactly what we do not know yet.
CREATE TABLE enroll_queue (
    full_name    TEXT PRIMARY KEY,             -- owner/name as requested
    requested_at TEXT NOT NULL                 -- UTC ISO-8601
) WITHOUT ROWID;
