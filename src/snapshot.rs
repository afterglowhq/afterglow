use std::collections::HashMap;

use anyhow::{Result, bail};
use rusqlite::params;

use crate::github::{GRAPHQL_BATCH, GitHub, Probe, SearchItem};
use crate::plural;
use crate::serve::{EMBED_DAILY_BUDGET, MANUAL_DAILY_BUDGET};
use crate::store::{
    APPEND_SNAPSHOT, DEQUEUE_ENROLLMENT, MARK_INACTIVE, QUEUED_ENROLLMENTS, SET_LANGUAGE, Store,
    UPSERT_REPO, opted_out, record_repo,
};
use crate::time::{SECONDS_PER_DAY, date_utc, iso8601_utc, now_unix};

/// The Search API caps any one query at 1000 results, so the scan is split by
/// star range to keep every query under the cap. Populations on 2026-08-01
/// were 919, 882, 662, 609, 176, 46, 5, so every bucket has headroom, and the
/// ascending sort in `search_repositories` means an overflowing bucket sheds
/// its top, not its floor.
///
/// The 500 bar is ADR-002's "much lower star bar" for the young scan: below
/// the board floor the store is quietly wider than the site, and a repo that
/// suddenly takes off is already mid-series by the time anyone looks
/// (ticket 024).
const BUCKETS: [&str; 7] = [
    "500..700",
    "700..1100",
    "1100..2000",
    "2000..7000",
    "7000..25000",
    "25000..100000",
    ">100000",
];
const MAX_AGE_DAYS: i64 = 180;
/// The Search API's own ceiling: ten pages of 100 is the 1000-result cap.
const PAGES: u32 = 10;

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
/// silently (ADR-002).
///
/// Batched GraphQL: a whole batch a query for one rate-limit point of 5,000 an
/// hour, so the whole pass stays a handful of points a day well past any
/// population the enrollment budgets can reach.
///
/// A due repo that ends the pass without a reading fails the run, because the
/// unit's `OnFailure=afterglow-alert@` hook is the only way out of this process
/// that anyone reads. No tolerance fraction: a standing block already sorts
/// itself out as `Probe::Blocked` and costs nothing, so whatever is left is a
/// run that went wrong, and one repo's missed day is as unrecoverable as a
/// hundred (ADR-002).
fn sweep(store: &mut Store, gh: &GitHub, now: i64, ts: &str) -> Result<()> {
    let cutoff = iso8601_utc(now - STALE_AFTER_HOURS * 3600);
    let stale: Vec<(i64, String, String)> = {
        let mut stmt = store.conn.prepare(STALE_REPOS)?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let (mut swept, mut inactive, mut blocked) = (0usize, 0usize, 0usize);
    for chunk in stale.chunks(GRAPHQL_BATCH) {
        let names: Vec<&str> = chunk.iter().map(|(_, name, _)| name.as_str()).collect();
        let probes = match gh.repos_batch(&names) {
            Ok(probes) => probes,
            // The unswept stay stale and are due again tomorrow (ADR-002), but
            // this batch and every one behind it lost today, so the run fails.
            Err(e) => {
                eprintln!("sweep stopped at the batch holding {}: {e:#}", names[0]);
                break;
            }
        };
        for ((id, full_name, lane), probe) in chunk.iter().zip(probes) {
            match probe {
                // Renames heal here: the row is keyed by id, so the name just moves.
                Probe::Found(repo) => {
                    record_repo(&store.conn, &repo, lane, ts)?;
                    swept += 1;
                }
                Probe::Gone => {
                    store.conn.execute(MARK_INACTIVE, params![id])?;
                    inactive += 1;
                }
                Probe::Blocked => {
                    blocked += 1;
                    eprintln!("sweep: {full_name} is blocked, kept as due");
                }
                Probe::Failed(why) => eprintln!("sweep: no reading for {full_name} ({why})"),
            }
        }
    }
    // Whatever a due repo did not become is a reading this pass lost: a failed
    // probe, or a batch the break above never reached.
    let unread = stale.len() - swept - inactive - blocked;
    println!(
        "sweep: {} refreshed, {} now inactive, {} blocked, {} due",
        plural(swept, "repo"),
        plural(inactive, "repo"),
        blocked,
        stale.len()
    );
    if unread > 0 {
        bail!(
            "sweep left {unread} of {} due repos unread; those days cannot be backfilled",
            stale.len()
        );
    }
    Ok(())
}

/// Young repos that already have real traction, across star buckets.
fn scan(gh: &GitHub, now: i64) -> Result<HashMap<i64, SearchItem>> {
    let cutoff = date_utc(now - MAX_AGE_DAYS * SECONDS_PER_DAY);
    let mut found = HashMap::new();
    for bucket in BUCKETS {
        let query = format!("created:>{cutoff} stars:{bucket}");
        for page in 1..=PAGES {
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
    use crate::github::{graphql_node, repo_json, stub};
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
            (200, search_page(&[])),
            (200, search_page(&[])),
            (200, search_page(&[])),
            // What the queue drain gets when it asks about the name it parked.
            (200, repo_json(2, "b/renamed", 5_000, created)),
        ]);

        run(&mut s, &GitHub::at(&base)).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 8);
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

        // Both stale repos ride one batched request; a/stale is r0 and z/gone
        // is r1 because the stale list is ordered by name.
        let body = format!(
            r#"{{"data":{{"r0":{},"r1":null}},"errors":[{{"type":"NOT_FOUND","path":["r1"],"message":"gone"}}]}}"#,
            graphql_node(1, "a/renamed", 140, "2026-01-01T00:00:00Z")
        );
        let (base, hits) = stub(vec![(200, body)]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap();

        // One request for the two due repos; the fresh one was not worth asking about.
        assert_eq!(hits.load(Ordering::SeqCst), 1);
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

    /// One repo past a full batch takes two requests, and every one gets its row.
    #[test]
    fn the_sweep_chunks_past_one_batch() {
        let last = GRAPHQL_BATCH as i64 + 1;
        let mut s = store();
        for i in 1..=last {
            track(&s, i, &format!("o{i:03}/x"), "scan");
        }
        let node = |i: i64| graphql_node(i, &format!("o{i:03}/x"), 10 * i, "2026-01-01T00:00:00Z");
        let first: Vec<String> = (1..last)
            .map(|i| format!(r#""r{}":{}"#, i - 1, node(i)))
            .collect();
        let (base, hits) = stub(vec![
            (200, format!(r#"{{"data":{{{}}}}}"#, first.join(","))),
            (200, format!(r#"{{"data":{{"r0":{}}}}}"#, node(last))),
        ]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap();

        assert_eq!(hits.load(Ordering::SeqCst), 2);
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots"), last);
        // The repo that fell into the second chunk still got its reading.
        assert_eq!(
            scalar::<i64>(
                &s,
                &format!("SELECT stars FROM snapshots WHERE repo_id = {last}")
            ),
            10 * last
        );
    }

    /// A DMCA block answers without deciding anything: the repo is neither
    /// refreshed nor retired, and comes up due again tomorrow. The unwrap is the
    /// point. A block is a standing condition of the repo, so it must not page
    /// anyone every morning for as long as the repo is tracked.
    #[test]
    fn a_blocked_repo_is_neither_refreshed_nor_retired() {
        let mut s = store();
        track(&s, 7, "d/dmca", "scan");
        let body = r#"{"data":{"r0":null},"errors":[{"type":"FORBIDDEN","path":["r0"],"message":"dmca"}]}"#;
        let (base, _) = stub(vec![(200, body.to_string())]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap();

        assert_eq!(
            scalar::<String>(&s, "SELECT status FROM repos WHERE id = 7"),
            "active"
        );
        assert_eq!(scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots"), 0);
    }

    /// A nulled tail with a resource limit on it. Those readings are gone
    /// whatever we do next, so the run exits non-zero and the alert hook fires.
    #[test]
    fn a_repo_left_unread_fails_the_run() {
        let mut s = store();
        track(&s, 1, "a/answered", "scan");
        track(&s, 2, "b/nulled", "embed");
        let body = format!(
            r#"{{"data":{{"r0":{},"r1":null}},"errors":[{{"type":"RESOURCE_LIMITS_EXCEEDED","path":["r1"],"message":"too much"}}]}}"#,
            graphql_node(1, "a/answered", 140, "2026-01-01T00:00:00Z")
        );
        let (base, _) = stub(vec![(200, body)]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        let err = sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap_err();

        assert!(err.to_string().contains("1 of 2"), "{err}");
        // Failing is a signal, not a rollback: the repo that answered keeps its
        // row, and the one that did not is still active and still due.
        assert_eq!(
            scalar::<i64>(&s, "SELECT COUNT(*) FROM snapshots WHERE repo_id = 1"),
            1
        );
        assert_eq!(
            scalar::<String>(&s, "SELECT status FROM repos WHERE id = 2"),
            "active"
        );
    }

    /// A batch that never answered is the same lost day as one that answered
    /// with nulls, so breaking out of the sweep fails the run too.
    #[test]
    fn a_batch_the_sweep_never_reached_fails_the_run() {
        let mut s = store();
        track(&s, 1, "a/one", "scan");
        track(&s, 2, "b/two", "scan");
        let (base, _) = stub(vec![]);
        let now = parse_iso8601_utc(NOW).expect("a fixed now");

        let err = sweep(&mut s, &GitHub::at(&base), now, NOW).unwrap_err();

        assert!(err.to_string().contains("2 of 2"), "{err}");
    }
}
