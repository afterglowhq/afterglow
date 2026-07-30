use std::collections::HashMap;

use anyhow::Result;
use rusqlite::params;

use crate::github::{GitHub, SearchItem};
use crate::plural;
use crate::store::{APPEND_SNAPSHOT, Store, UPSERT_REPO};
use crate::time::{SECONDS_PER_DAY, date_utc, iso8601_utc, now_unix};

/// The Search API caps any one query at 1000 results, so the sweep is split by
/// star range to keep every query under the cap.
const BUCKETS: [&str; 4] = ["2000..7000", "7000..25000", "25000..100000", ">100000"];
const MAX_AGE_DAYS: i64 = 180;
const PAGES: u32 = 2;

pub fn run(store: &mut Store, gh: &GitHub) -> Result<()> {
    let now = now_unix();
    let found = scan(gh, now)?;
    eprintln!("scanned {} candidates", found.len());
    let ts = iso8601_utc(now);

    let mut items: Vec<&SearchItem> = found.values().collect();
    items.sort_unstable_by_key(|it| &it.full_name);

    let tx = store.conn.transaction()?;
    let mut appended = 0usize;
    {
        let mut upsert = tx.prepare(UPSERT_REPO)?;
        let mut append = tx.prepare(APPEND_SNAPSHOT)?;
        for it in &items {
            upsert.execute(params![it.id, it.full_name, "scan", ts, it.created_at])?;
            appended += append.execute(params![
                it.id,
                ts,
                it.stargazers_count,
                it.forks_count,
                it.open_issues_count,
                it.pushed_at,
            ])?;
        }
    }
    tx.commit()?;

    println!(
        "{ts}  {} seen, {} appended",
        plural(items.len(), "repo"),
        plural(appended, "snapshot row")
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
