use std::path::Path;

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};

use crate::github::Repo;

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/001-store.sql"),
    include_str!("migrations/002-enroll-queue.sql"),
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

pub const QUEUE_ENROLLMENT: &str = "\
INSERT OR IGNORE INTO enroll_queue (full_name, requested_at) VALUES (?1, ?2)";

pub const QUEUED_ENROLLMENTS: &str = "\
SELECT full_name FROM enroll_queue ORDER BY requested_at, full_name LIMIT ?1";

pub const DEQUEUE_ENROLLMENT: &str = "DELETE FROM enroll_queue WHERE full_name = ?1";

/// Series are retained when a repo goes away; only the badge changes.
pub const MARK_INACTIVE: &str = "UPDATE repos SET status = 'inactive' WHERE id = ?1";

/// The repo row and the snapshot that a per-repo GET just paid for. `lane` is only
/// read when the row is new, so a sweep passes the lane the repo already has.
pub fn record_repo(conn: &Connection, repo: &Repo, lane: &str, ts: &str) -> Result<usize> {
    conn.execute(
        UPSERT_REPO,
        params![repo.id, repo.full_name, lane, ts, repo.created_at],
    )?;
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
        conn.pragma_update(None, "foreign_keys", true)?;
        // The badge server and the snapshot timer are separate processes on one
        // db; a write landing mid-transaction should wait, not fail SQLITE_BUSY.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let mut store = Store { conn };
        store.migrate()?;
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
    /// without touching what is already there.
    #[test]
    fn migrations_apply_on_top_of_an_older_store() {
        let path = temp_path("additive");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.execute(
                "INSERT INTO repos (id, full_name, lane, enrolled_at) VALUES (7, 'a/b', 'scan', '2026-07-30T00:00:00Z')",
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
        let name: String = store
            .conn
            .query_row("SELECT full_name FROM repos WHERE id = 7", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(name, "a/b");
        store
            .conn
            .execute(QUEUE_ENROLLMENT, params!["c/d", "2026-07-31T00:00:00Z"])
            .unwrap();

        drop(store);
        let _ = std::fs::remove_file(&path);
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
