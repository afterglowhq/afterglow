use std::collections::HashMap;

use anyhow::Result;
use rusqlite::params;

use crate::github::{GitHub, SearchItem};
use crate::plural;
use crate::serve::{EMBED_DAILY_BUDGET, MANUAL_DAILY_BUDGET};
use crate::store::{
    APPEND_SNAPSHOT, DEQUEUE_ENROLLMENT, MARK_INACTIVE, QUEUED_ENROLLMENTS, SET_LANGUAGE, Store,
    UPSERT_REPO, opted_out, record_repo,
};
use crate::time::{SECONDS_PER_DAY, date_utc, iso8601_utc, now_unix};

/// The Search API caps any one query at 1000 results, so the sweep is split by
/// star range to keep every query under the cap.
const BUCKETS: [&str; 4] = ["2000..7000", "7000..25000", "25000..100000", ">100000"];
const MAX_AGE_DAYS: i64 = 180;
const PAGES: u32 = 2;

/// A repo the scan did not just refresh is due again after this long.
const STALE_AFTER_HOURS: i64 = 20;

const STALE_REPOS: &str = "\
SELECT r.id, r.full_name, r.lane FROM repos r
WHERE r.status = 'active'
  AND COALESCE((SELECT MAX(s.ts) FROM snapshots s WHERE s.repo_id = r.id), '') < ?1
ORDER BY r.full_name";

const QUEUE_DEPTH: &str = "SELECT COUNT(*) FROM enroll_queue";

pub fn run(store: &mut Store, gh: &GitHub) -> Result<()> {
    let now = now_unix();
    let found = scan(gh, now)?;
    eprintln!("scanned {} candidates", found.len());
    let ts = iso8601_utc(now);

    let mut items: Vec<&SearchItem> = found.values().collect();
    items.sort_unstable_by_key(|it| &it.full_name);

    let tx = store.conn.transaction()?;
    let (mut appended, mut declined) = (0usize, 0usize);
    {
        let mut upsert = tx.prepare(UPSERT_REPO)?;
        let mut append = tx.prepare(APPEND_SNAPSHOT)?;
        let mut set_language = tx.prepare(SET_LANGUAGE)?;
        for it in &items {
            // The search does not know who asked to be left alone (ToS rule 4).
            if opted_out(&tx, it.id)? {
                declined += 1;
                continue;
            }
            upsert.execute(params![it.id, it.full_name, "scan", ts, it.created_at])?;
            set_language.execute(params![it.id, it.language])?;
            appended += append.execute(params![
                it.id,
                ts,
                it.stargazers_count,
                it.forks_count,
                it.open_issues_count,
                it.pushed_at,
                None::<i64>,
            ])?;
        }
    }
    tx.commit()?;

    println!(
        "{ts}  {} seen, {} appended, {} opted out",
        plural(items.len(), "repo"),
        plural(appended, "snapshot row"),
        declined
    );

    drain_queue(store, gh, &ts)?;
    sweep(store, gh, now, &ts)
}

/// Enrollment overflow from both request lanes, oldest first, capped at a day's
/// worth of budget. A GitHub failure stops the drain with the rest still queued:
/// they enroll on a later day, never dropped (ADR-002).
fn drain_queue(store: &mut Store, gh: &GitHub, ts: &str) -> Result<()> {
    let queued: Vec<(String, String)> = {
        let mut stmt = store.conn.prepare(QUEUED_ENROLLMENTS)?;
        let budget = EMBED_DAILY_BUDGET + MANUAL_DAILY_BUDGET;
        let rows = stmt.query_map(params![budget], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let (mut enrolled, mut dropped) = (0usize, 0usize);
    for (name, lane) in &queued {
        match gh.repo(name) {
            // A queued name resolving onto an opted-out id is the rename case:
            // the id decides, and it leaves the queue without a row (ToS rule 4).
            Ok(Some(repo)) if !opted_out(&store.conn, repo.id)? => {
                // The lane the request arrived through, not the lane that drains it.
                record_repo(&store.conn, &repo, lane, ts)?;
                store.conn.execute(DEQUEUE_ENROLLMENT, params![name])?;
                enrolled += 1;
            }
            // A name nobody can resolve would otherwise be retried forever.
            Ok(_) => {
                store.conn.execute(DEQUEUE_ENROLLMENT, params![name])?;
                dropped += 1;
            }
            Err(e) => {
                eprintln!("queue drain stopped at {name}: {e:#}");
                break;
            }
        }
    }
    let left: i64 = store.conn.query_row(QUEUE_DEPTH, [], |row| row.get(0))?;
    println!(
        "queue: {} enrolled, {} dropped as unresolvable or opted out, {} still waiting",
        plural(enrolled, "repo"),
        plural(dropped, "name"),
        left
    );
    Ok(())
}

/// Everything active the scan did not just refresh: enrolled, imported and manual
/// repos, plus scan repos that aged out of the search window. No series ends
/// silently (ADR-002), so each one costs a per-repo GET.
///
/// Per-repo GETs are the entire cost of this pass and fine at hundreds
/// of repos; the 50k star-floor tier will take most of this over once it's built.
fn sweep(store: &mut Store, gh: &GitHub, now: i64, ts: &str) -> Result<()> {
    let cutoff = iso8601_utc(now - STALE_AFTER_HOURS * 3600);
    let stale: Vec<(i64, String, String)> = {
        let mut stmt = store.conn.prepare(STALE_REPOS)?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let (mut swept, mut inactive) = (0usize, 0usize);
    for (id, full_name, lane) in &stale {
        match gh.repo(full_name) {
            // Renames heal here: the row is keyed by id, so the name just moves.
            Ok(Some(repo)) => {
                record_repo(&store.conn, &repo, lane, ts)?;
                swept += 1;
            }
            Ok(None) => {
                store.conn.execute(MARK_INACTIVE, params![id])?;
                inactive += 1;
            }
            Err(e) => {
                eprintln!("sweep stopped at {full_name}: {e:#}");
                break;
            }
        }
    }
    println!(
        "sweep: {} refreshed, {} now inactive, {} due",
        plural(swept, "repo"),
        plural(inactive, "repo"),
        stale.len()
    );
    Ok(())
}

/// Young repos that already have real traction, across star buckets.
fn scan(gh: &GitHub, now: i64) -> Result<HashMap<i64, SearchItem>> {
    let cutoff = date_utc(now - MAX_AGE_DAYS * SECONDS_PER_DAY);
    let mut found = HashMap::new();
    for bucket in BUCKETS {
        let query = format!("created:>{cutoff} stars:{bucket}");
        for page in 1..=PAGES {
            // 200 a bucket is plenty; the tail is noise.
            let items = gh.search_repositories(&query, page)?;
            let full = items.len() == 100;
            for item in items {
                found.insert(item.id, item);
            }
            if !full {
                break;
            }
        }
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{repo_json, stub};
    use crate::store::QUEUE_ENROLLMENT;
    use crate::time::parse_iso8601_utc;
    use std::sync::atomic::Ordering;

    const NOW: &str = "2026-07-31T00:00:00Z";

    fn store() -> Store {
        Store::open_in_memory().expect("in-memory store")
    }

    fn track(store: &Store, id: i64, name: &str, lane: &str) {
        track_as(store, id, name, lane, "active");
    }

    fn track_as(store: &Store, id: i64, name: &str, lane: &str, status: &str) {
        store
            .conn
            .execute(
                "INSERT INTO repos (id, full_name, status, lane, enrolled_at)
                 VALUES (?1, ?2, ?3, ?4, '2026-01-01T00:00:00Z')",
                params![id, name, status, lane],
            )
            .expect("seeding a repo");
    }

    fn search_page(items: &[String]) -> String {
        format!(r#"{{"items":[{}]}}"#, items.join(","))
    }

    fn snapshot(store: &Store, id: i64, ts: &str, stars: i64) {
        store
            .conn
            .execute(
                "INSERT INTO snapshots (repo_id, ts, stars) VALUES (?1, ?2, ?3)",
                params![id, ts, stars],
            )
            .expect("seeding a snapshot");
    }

    fn queue(store: &Store, name: &str, at: &str) {
        queue_as(store, name, at, "embed");
    }

    fn queue_as(store: &Store, name: &str, at: &str, lane: &str) {
        store
            .conn
            .execute(QUEUE_ENROLLMENT, params![name, at, lane])
            .expect("queueing");
    }

    fn lane_of(store: &Store, name: &str) -> String {
        store
            .conn
            .query_row(
                "SELECT lane FROM repos WHERE full_name = ?1",
                params![name],
                |row| row.get(0),
            )
            .expect(name)
    }

    fn scalar<T: rusqlite::types::FromSql>(store: &Store, sql: &str) -> T {
        store.conn.query_row(sql, [], |row| row.get(0)).expect(sql)
    }

    #[test]
    fn the_queue_drains_oldest_first_and_drops_names_that_do_not_resolve() {
        let mut s = store();
        queue(&s, "b/second", "2026-07-30T02:00:00Z");
        queue(&s, "a/first", "2026-07-30T01:00:00Z");
        let (base, hits) = stub(vec![
            (200, repo_json(1, "a/first", 10, "2026-01-01T00:00:00Z")),
            (404, "{}".to_string()),
        ]);

        drain_queue(&mut s, &GitHub::at(&base), NOW).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM enroll_queue"), 0);
        assert_eq!(
            scalar::<String>(&s, "SELECT full_name FROM repos"),
            "a/first"
        );
        assert_eq!(scalar::<String>(&s, "SELECT lane FROM repos"), "embed");
        assert_eq!(
            scalar::<i64>(&s, "SELECT stars FROM snapshots WHERE repo_id = 1"),
            10
        );
        // A per-repo GET is where subscriber counts come from.
        assert_eq!(
            scalar::<i64>(&s, "SELECT subscribers FROM snapshots WHERE repo_id = 1"),
            5
        );
        assert_eq!(
            scalar::<String>(&s, "SELECT language FROM repos WHERE id = 1"),
            "Rust"
        );
    }

    #[test]
    fn a_drained_name_is_recorded_under_the_lane_that_asked_for_it() {
        let mut s = store();
        queue_as(&s, "a/badge", "2026-07-30T01:00:00Z", "embed");
        queue_as(&s, "b/typed", "2026-07-30T02:00:00Z", "manual");
        let (base, _) = stub(vec![
            (200, repo_json(1, "a/badge", 10, "2026-01-01T00:00:00Z")),
            (200, repo_json(2, "b/typed", 20, "2026-01-01T00:00:00Z")),
        ]);

        drain_queue(&mut s, &GitHub::at(&base), NOW).unwrap();

        // The lane is how a repo entered (ADR-002). Overflow means it
        // entered a day late, not that it entered through the drain.
        assert_eq!(lane_of(&s, "a/badge"), "embed");
        assert_eq!(lane_of(&s, "b/typed"), "manual");
    }

    #[test]
    fn a_github_failure_leaves_the_rest_of_the_queue_for_a_later_day() {
        let mut s = store();
        queue(&s, "a/one", "2026-07-30T01:00:00Z");
        queue(&s, "b/two", "2026-07-30T02:00:00Z");
        let (base, hits) = stub(vec![]);

        drain_queue(&mut s, &GitHub::at(&base), NOW).unwrap();

        // Stopped at the first failure rather than burning the rest of the queue.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM enroll_queue"), 2);
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM repos"), 0);
    }

    /// Every collecting path has to agree that an opted-out repo is left alone:
    /// the search sweep that never asked, and the queue holding a name from
    /// before the opt-out (ToS rule 4).
    #[test]
    fn nothing_collects_for_a_repo_that_opted_out() {
        let mut s = store();
        track_as(&s, 2, "b/old-name", "scan", "opted_out");
        let created = "2026-06-01T00:00:00Z";
        queue(&s, "b/renamed", "2026-07-30T01:00:00Z");
        let (base, hits) = stub(vec![
            (
                200,
                search_page(&[
                    repo_json(1, "a/keep", 100, created),
                    repo_json(2, "b/renamed", 5_000, created),
                ]),
            ),
            (200, search_page(&[])),
            (200, search_page(&[])),
            (200, search_page(&[])),
            // What the queue drain gets when it asks about the name it parked.
            (200, repo_json(2, "b/renamed", 5_000, created)),
        ]);

        run(&mut s, &GitHub::at(&base)).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 5);
        // The scan found it and wrote nothing; the name in the store did not move.
        assert_eq!(
            scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots WHERE repo_id = 2"),
            0
        );
        assert_eq!(
            scalar::<String>(&s, "SELECT full_name FROM repos WHERE id = 2"),
            "b/old-name"
        );
        // The queued name resolved onto the opted-out id and left the queue.
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM enroll_queue"), 0);
        // The repo that never opted out is collected as usual.
        assert_eq!(
            scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots WHERE repo_id = 1"),
            1
        );
        // Search items carry the language too, so the scan lane sets it for free.
        assert_eq!(
            scalar::<String>(&s, "SELECT language FROM repos WHERE id = 1"),
            "Rust"
        );
    }

    #[test]
    fn the_sweep_refreshes_the_stale_and_retires_the_missing() {
        let mut s = store();
        track(&s, 1, "a/stale", "scan");
        snapshot(&s, 1, "2026-07-29T00:00:00Z", 100);
        track(&s, 2, "m/fresh", "scan");
        snapshot(&s, 2, "2026-07-30T23:00:00Z", 50);
        track(&s, 3, "z/gone", "embed");

        let (base, hits) = stub(vec![
            (200, repo_json(1, "a/renamed", 140, "2026-01-01T00:00:00Z")),
            (404, "{}".to_string()),
        ]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap();

        // The one snapshotted an hour ago was not worth an API call.
        assert_eq!(hits.load(Ordering::SeqCst), 2);
        // A rename heals onto the same id and the series just continues.
        assert_eq!(
            scalar::<String>(&s, "SELECT full_name FROM repos WHERE id = 1"),
            "a/renamed"
        );
        assert_eq!(
            scalar::<String>(&s, "SELECT lane FROM repos WHERE id = 1"),
            "scan"
        );
        assert_eq!(
            scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots WHERE repo_id = 1"),
            2
        );
        assert_eq!(
            scalar::<i64>(
                &s,
                "SELECT stars FROM snapshots WHERE repo_id = 1 AND ts = '2026-07-31T00:00:00Z'"
            ),
            140
        );
        // Gone from our vantage point: status flips, the series is kept.
        assert_eq!(
            scalar::<String>(&s, "SELECT status FROM repos WHERE id = 3"),
            "inactive"
        );
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots"), 3);
    }
}
