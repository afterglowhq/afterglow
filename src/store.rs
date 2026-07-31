use std::path::Path;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};

use crate::github::Repo;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001-store.sql"),
    include_str!("migrations/002-enroll-queue.sql"),
    include_str!("migrations/003-opt-out.sql"),
    include_str!("migrations/004-language.sql"),
];

/// `enrolled_at` only ever moves earlier: it is the first sight, and importing
/// older history is a sighting we did not have before.
pub const UPSERT_REPO: &str = "\
INSERT INTO repos (id, full_name, lane, enrolled_at, created_at)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT (id) DO UPDATE SET
    full_name   = excluded.full_name,
    enrolled_at = MIN(repos.enrolled_at, excluded.enrolled_at),
    created_at  = COALESCE(excluded.created_at, repos.created_at)";

/// `subscribers` is only ever non-null where a per-repo GET happened; the search
/// sweep passes NULL rather than inventing a count.
pub const APPEND_SNAPSHOT: &str = "\
INSERT OR IGNORE INTO snapshots (repo_id, ts, stars, forks, open_issues, pushed_at, subscribers)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)";

pub const REPO_ID_BY_NAME: &str = "SELECT id FROM repos WHERE full_name = ?1 ORDER BY id LIMIT 1";

/// The lane rides along because it is how the repo entered, and only the request
/// knows it. A name queued twice keeps the lane that first asked for it.
pub const QUEUE_ENROLLMENT: &str = "\
INSERT OR IGNORE INTO enroll_queue (full_name, requested_at, lane) VALUES (?1, ?2, ?3)";

pub const QUEUED_ENROLLMENTS: &str = "\
SELECT full_name, lane FROM enroll_queue ORDER BY requested_at, full_name LIMIT ?1";

pub const DEQUEUE_ENROLLMENT: &str = "DELETE FROM enroll_queue WHERE full_name = ?1";

/// Series are retained when a repo goes away; only the badge changes.
pub const MARK_INACTIVE: &str = "UPDATE repos SET status = 'inactive' WHERE id = ?1";

/// The newest verdict wins, NULL included: a repo whose detectable code goes away
/// stops claiming a language. Kept out of UPSERT_REPO so the import path, which
/// never asked GitHub, cannot blank what a real request observed.
pub const SET_LANGUAGE: &str = "UPDATE repos SET language = ?2 WHERE id = ?1";

const REPO_BY_NAME_NOCASE: &str = "\
SELECT id, full_name FROM repos WHERE full_name = ?1 COLLATE NOCASE ORDER BY id LIMIT 1";

const SET_STATUS: &str = "UPDATE repos SET status = ?2 WHERE id = ?1";

const IS_OPTED_OUT: &str = "SELECT 1 FROM repos WHERE id = ?1 AND status = 'opted_out'";

/// Nothing is collected for an opted-out repo and no surface shows it, so every
/// lane asks this before it writes a row (ToS rule 4).
pub fn opted_out(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn
        .prepare(IS_OPTED_OUT)?
        .query_row(params![id], |_| Ok(()))
        .optional()?
        .is_some())
}

/// The operator's half of the opt-out lane: a maintainer emails, a human verifies
/// they own the repo, and this flips the row. The series already collected stays
/// where it is, because a snapshot is never deleted; it just stops being shown.
///
/// Undo puts the repo back in the active set. It does not re-decide whether the
/// repo is still observable: the next sweep does that, the way it does for
/// everything else.
pub fn opt_out(store: &Store, full_name: &str, undo: bool) -> Result<()> {
    let (id, name) = store
        .conn
        .prepare(REPO_BY_NAME_NOCASE)?
        .query_row(params![full_name], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .optional()?
        .with_context(|| format!("{full_name} is not tracked, so there is nothing to opt out"))?;

    let status = if undo { "active" } else { "opted_out" };
    store.conn.execute(SET_STATUS, params![id, status])?;
    println!(
        "{name} ({id}) {}",
        if undo {
            "is tracked again; its badge and its rankings row are back"
        } else {
            "opted out; its badge reads not tracked and it leaves the rankings"
        }
    );
    Ok(())
}

/// The repo row and the snapshot that a per-repo GET just paid for. `lane` is only
/// read when the row is new, so a sweep passes the lane the repo already has.
pub fn record_repo(conn: &Connection, repo: &Repo, lane: &str, ts: &str) -> Result<usize> {
    conn.execute(
        UPSERT_REPO,
        params![repo.id, repo.full_name, lane, ts, repo.created_at],
    )?;
    conn.execute(SET_LANGUAGE, params![repo.id, repo.language])?;
    Ok(conn.execute(
        APPEND_SNAPSHOT,
        params![
            repo.id,
            ts,
            repo.stargazers_count,
            repo.forks_count,
            repo.open_issues_count,
            repo.pushed_at,
            repo.subscribers_count,
        ],
    )?)
}

pub struct Store {
    pub conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating store directory {}", dir.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening store at {}", path.display()))?;
        Self::prepare(conn)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "wal")?;
        conn.pragma_update(None, "synchronous", "normal")?;
        // The badge server and the snapshot timer are separate processes on one
        // db; a write landing mid-transaction should wait, not fail SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        // Foreign keys go on after the migrations, not before: rebuilding a table
        // means dropping and renaming under the rows that reference it, which
        // SQLite only allows with the constraint off. This is step one of its own
        // ALTER TABLE procedure; step ten is the foreign_key_check in migrate().
        conn.pragma_update(None, "foreign_keys", false)?;
        let mut store = Store { conn };
        store.migrate()?;
        store.conn.pragma_update(None, "foreign_keys", true)?;
        Ok(store)
    }

    fn migrate(&mut self) -> Result<()> {
        let applied: usize = self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?
            as usize;
        ensure!(
            applied <= MIGRATIONS.len(),
            "store is at schema version {applied}, this binary knows {}",
            MIGRATIONS.len()
        );
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(applied) {
            let version = i + 1;
            let tx = self.conn.transaction()?;
            tx.execute_batch(sql)
                .with_context(|| format!("applying migration {version}"))?;
            // A rebuilt table that lost its children would take the moat with it,
            // and enforcement is off while migrations run, so this is the check
            // that a migration left every reference resolvable.
            ensure!(
                !tx.prepare("PRAGMA foreign_key_check")?.exists([])?,
                "migration {version} left dangling references"
            );
            tx.pragma_update(None, "user_version", version as i64)?;
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn temp_path(tag: &str) -> std::path::PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir()
            .join("afterglow-tests")
            .join(format!("{tag}-{}-{nonce}.db", std::process::id()))
    }

    #[test]
    fn migration_creates_schema_and_advances_user_version() {
        let path = temp_path("migrate");
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);

        let mut names: Vec<String> = store
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        names.retain(|n: &String| !n.starts_with("sqlite_"));
        assert_eq!(
            names,
            [
                "capture_month",
                "enroll_queue",
                "prehistory_monthly",
                "repos",
                "snapshots"
            ]
        );

        drop(store);
        // Reopening an already-migrated store is a no-op, not a re-apply.
        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The deployed store is mid-list; later migrations have to land on top of it
    /// without touching what is already there. Migration 3 rebuilds `repos`, so
    /// this is also the test that the rebuild keeps the rows the moat is made of.
    #[test]
    fn migrations_apply_on_top_of_an_older_store() {
        let path = temp_path("additive");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute(
                "INSERT INTO repos (id, full_name, status, lane, enrolled_at, created_at)
                 VALUES (7, 'a/b', 'inactive', 'scan', '2026-07-30T00:00:00Z', '2020-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO snapshots (repo_id, ts, stars) VALUES (7, '2026-07-30T00:00:00Z', 42)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO prehistory_monthly (full_name, month, gross_watch_events, repo_id)
                 VALUES ('a/b', '2020-01', 9, 7)",
                [],
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }

        let store = Store::open(&path).unwrap();
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
        // Every column came across the rebuild, not just the key.
        let row: (String, String, String, String, String) = store
            .conn
            .query_row(
                "SELECT full_name, status, lane, enrolled_at, created_at FROM repos WHERE id = 7",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "a/b".to_string(),
                "inactive".to_string(),
                "scan".to_string(),
                "2026-07-30T00:00:00Z".to_string(),
                "2020-01-01T00:00:00Z".to_string(),
            )
        );
        // The snapshot that hung off it is still there and still points at it.
        let stars: i64 = store
            .conn
            .query_row(
                "SELECT s.stars FROM snapshots s JOIN repos r ON r.id = s.repo_id",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stars, 42);
        assert!(
            store
                .conn
                .prepare("PRAGMA index_list(repos)")
                .unwrap()
                .exists([])
                .unwrap()
        );
        store
            .conn
            .execute(
                QUEUE_ENROLLMENT,
                params!["c/d", "2026-07-31T00:00:00Z", "manual"],
            )
            .unwrap();

        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    /// The migration guard is only worth having if it can fail, and a check that
    /// silently reports nothing would be indistinguishable from a clean store.
    #[test]
    fn the_migration_guard_notices_a_dangling_reference() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .pragma_update(None, "foreign_keys", false)
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO prehistory_monthly (full_name, month, gross_watch_events, repo_id)
                 VALUES ('a/b', '2020-01', 5, 999)",
                [],
            )
            .unwrap();
        assert!(
            store
                .conn
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .exists([])
                .unwrap()
        );
    }

    #[test]
    fn the_rebuilt_status_check_takes_opted_out_and_nothing_else() {
        let store = Store::open_in_memory().unwrap();
        let insert = "INSERT INTO repos (id, full_name, status, lane, enrolled_at)
                      VALUES (?1, 'a/b', ?2, 'scan', '2026-07-30T00:00:00Z')";
        for (id, status) in [(1, "active"), (2, "inactive"), (3, "opted_out")] {
            store.conn.execute(insert, params![id, status]).unwrap();
        }
        let err = store
            .conn
            .execute(insert, params![4, "shadowbanned"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHECK"), "{err}");
    }

    #[test]
    fn opting_out_flips_the_status_and_undo_puts_it_back() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO repos (id, full_name, lane, enrolled_at)
                 VALUES (1, 'Owner/Repo', 'scan', '2026-07-30T00:00:00Z')",
                [],
            )
            .unwrap();
        let status = || -> String {
            store
                .conn
                .query_row("SELECT status FROM repos WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };

        // Whatever case the maintainer's email used.
        opt_out(&store, "owner/repo", false).unwrap();
        assert_eq!(status(), "opted_out");
        assert!(opted_out(&store.conn, 1).unwrap());

        opt_out(&store, "Owner/Repo", true).unwrap();
        assert_eq!(status(), "active");
        assert!(!opted_out(&store.conn, 1).unwrap());

        // A name we do not have is an error, not a quiet success.
        let err = opt_out(&store, "who/knows", false).unwrap_err().to_string();
        assert!(err.contains("not tracked"), "{err}");
    }

    #[test]
    fn the_language_follows_the_newest_verdict_even_to_null() {
        let store = Store::open_in_memory().unwrap();
        let repo = |language: Option<&str>| Repo {
            id: 1,
            full_name: "a/b".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            stargazers_count: 10,
            forks_count: None,
            open_issues_count: None,
            subscribers_count: None,
            pushed_at: None,
            language: language.map(str::to_string),
        };
        let language = || -> Option<String> {
            store
                .conn
                .query_row("SELECT language FROM repos WHERE id = 1", [], |row| {
                    row.get(0)
                })
                .unwrap()
        };

        record_repo(
            &store.conn,
            &repo(Some("Shell")),
            "scan",
            "2026-07-30T00:00:00Z",
        )
        .unwrap();
        assert_eq!(language().as_deref(), Some("Shell"));

        // The last script left the repo and GitHub now says nothing. So do we.
        record_repo(&store.conn, &repo(None), "scan", "2026-07-31T00:00:00Z").unwrap();
        assert_eq!(language(), None);
    }

    #[test]
    fn snapshots_reject_update_and_delete() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO repos (id, full_name, lane, enrolled_at) VALUES (1, 'a/b', 'scan', ?1)",
                params!["2026-07-30T00:00:00Z"],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO snapshots (repo_id, ts, stars) VALUES (1, ?1, 10)",
                params!["2026-07-30T00:00:00Z"],
            )
            .unwrap();

        let err = store
            .conn
            .execute("UPDATE snapshots SET stars = 11", [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("append-only"), "{err}");

        let err = store
            .conn
            .execute("DELETE FROM snapshots", [])
            .unwrap_err()
            .to_string();
        assert!(err.contains("append-only"), "{err}");

        let stars: i64 = store
            .conn
            .query_row("SELECT stars FROM snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(stars, 10);
    }

    #[test]
    fn snapshots_are_keyed_by_repo_and_ts() {
        let store = Store::open_in_memory().unwrap();
        store
            .conn
            .execute(
                "INSERT INTO repos (id, full_name, lane, enrolled_at) VALUES (1, 'a/b', 'import', ?1)",
                params!["2026-07-30T00:00:00Z"],
            )
            .unwrap();
        let insert = "INSERT OR IGNORE INTO snapshots (repo_id, ts, stars) VALUES (1, ?1, ?2)";
        assert_eq!(
            store
                .conn
                .execute(insert, params!["2026-07-30T00:00:00Z", 10])
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .conn
                .execute(insert, params!["2026-07-30T00:00:00Z", 99])
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .conn
                .execute(insert, params!["2026-07-31T00:00:00Z", 11])
                .unwrap(),
            1
        );
    }

    #[test]
    fn repos_reject_unknown_lanes() {
        let store = Store::open_in_memory().unwrap();
        let err = store
            .conn
            .execute(
                "INSERT INTO repos (id, full_name, lane, enrolled_at) VALUES (1, 'a/b', 'guess', '2026-07-30T00:00:00Z')",
                [],
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHECK"), "{err}");
    }
}
