//! The badge server. Two URL shapes, both immortal once public (SPEC §5), and one
//! rule underneath them: a badge is answered from the snapshot store, never from a
//! live API call (ToS rule 6). The only GitHub request on this path is the one that
//! enrolls a repo nobody has asked for before.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use rusqlite::{Connection, OptionalExtension, params};

use crate::badge::{self, BadgeState, RepoBadge, Theme};
use crate::github::GitHub;
use crate::store::{QUEUE_ENROLLMENT, Store, record_repo};
use crate::time::{SECONDS_PER_DAY, date_utc, iso8601_utc, now_unix, parse_iso8601_utc};

/// Enrollments the embed lane spends in one UTC day; overflow queues (ADR-002).
pub const EMBED_DAILY_BUDGET: u32 = 256;

/// Two readings closer together than this are the same day seen twice, not a delta.
const MIN_VELOCITY_SPAN_HOURS: i64 = 12;
/// How far back the percentile pool reads; wide enough to survive a missed day.
const VELOCITY_WINDOW_DAYS: i64 = 35;
const SPARK_DAYS: i64 = 30;
/// Above this age a repo gets no proxy number, only `measuring` (ticket 007).
const PROXY_MAX_AGE_DAYS: f64 = 180.0;

/// The card's spark region: 388 wide, stars mapped between these two rows.
const SPARK_WIDTH: f64 = 388.0;
const SPARK_TOP: f64 = 4.0;
const SPARK_BOTTOM: f64 = 36.0;
const SPARK_FLAT: f64 = 20.0;

/// Day-one and not-tracked states change tomorrow; measured badges can sit in the CDN.
const FRESH_MAX_AGE: u32 = 300;
const SETTLED_MAX_AGE: u32 = 3600;

const INDEX: &str = "\
Afterglow snapshots GitHub star counts once a day and serves them back as a README badge.

Point an image at /badge/{owner}/{repo} for the pill, add ?style=card for the card with a 30-day
sparkline, and ?theme=dark if the README is dark.
Asking for a repo we have not seen before is what starts it being tracked, so its first badge says
tracking just started and the history builds from there.
";

const NOT_FOUND: &str = "Nothing here. Badges are at /badge/{owner}/{repo}.\n";

/// Badge URLs carry whatever case a README author typed.
///
/// NOCASE means the `full_name` index does not help and this scans; free
/// at a few hundred repos. Add a lowercased column when the table gets big.
const REPO_BY_NAME: &str = "\
SELECT id, full_name, status, enrolled_at, created_at FROM repos
WHERE full_name = ?1 COLLATE NOCASE ORDER BY id LIMIT 1";

const LATEST_SNAPSHOT: &str = "\
SELECT ts, stars FROM snapshots WHERE repo_id = ?1 ORDER BY ts DESC LIMIT 1";

const SNAPSHOT_AT_OR_BEFORE: &str = "\
SELECT ts, stars FROM snapshots WHERE repo_id = ?1 AND ts <= ?2 ORDER BY ts DESC LIMIT 1";

const SNAPSHOTS_SINCE: &str = "\
SELECT ts, stars FROM snapshots WHERE repo_id = ?1 AND ts >= ?2 ORDER BY ts";

const RECENT_SNAPSHOTS: &str = "\
SELECT s.repo_id, s.ts, s.stars FROM snapshots s JOIN repos r ON r.id = s.repo_id
WHERE r.status = 'active' AND s.ts >= ?1
ORDER BY s.repo_id, s.ts";

pub struct AppState {
    store: Mutex<Store>,
    github: GitHub,
    embed: Mutex<Budget>,
}

/// In memory, so a restart refills the day's budget. At one host and 256
/// a day that beats a store write on every badge miss; move it into the store when
/// a second host appears.
struct Budget {
    date: String,
    spent: u32,
}

impl AppState {
    pub fn new(store: Store, github: GitHub) -> Self {
        AppState {
            store: Mutex::new(store),
            github,
            embed: Mutex::new(Budget {
                date: String::new(),
                spent: 0,
            }),
        }
    }

    /// One panicking request must not take the store down for every later one.
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts an enrollment attempt, or refuses once the day is spent.
    fn spend_enrollment(&self, today: &str) -> bool {
        let mut budget = self.embed.lock().unwrap_or_else(|e| e.into_inner());
        if budget.date != today {
            budget.date = today.to_string();
            budget.spent = 0;
        }
        if budget.spent >= EMBED_DAILY_BUDGET {
            return false;
        }
        budget.spent += 1;
        true
    }
}

pub fn run(db: &FsPath, listen: SocketAddr, tls: Option<(PathBuf, PathBuf)>) -> Result<()> {
    let state = Arc::new(AppState::new(Store::open(db)?, GitHub::from_env()?));
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?
        .block_on(listen_and_serve(router(state), listen, tls))
}

async fn listen_and_serve(
    app: Router,
    listen: SocketAddr,
    tls: Option<(PathBuf, PathBuf)>,
) -> Result<()> {
    match tls {
        Some((cert, key)) => {
            let config = RustlsConfig::from_pem_file(&cert, &key)
                .await
                .with_context(|| format!("loading {} and {}", cert.display(), key.display()))?;
            println!("afterglow serving https on {listen}");
            axum_server::bind_rustls(listen, config)
                .serve(app.into_make_service())
                .await
        }
        None => {
            println!("afterglow serving http on {listen}");
            axum_server::bind(listen)
                .serve(app.into_make_service())
                .await
        }
    }
    .context("the badge server stopped")
}

fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/badge/{owner}/{repo}", get(canonical_badge))
        .route("/svg", get(compat_badge))
        .fallback(missing)
        .with_state(state)
}

#[derive(Clone, Copy)]
enum Form {
    Pill,
    Card(Theme),
}

async fn index() -> Response {
    text(StatusCode::OK, INDEX)
}

async fn missing() -> Response {
    text(StatusCode::NOT_FOUND, NOT_FOUND)
}

async fn canonical_badge(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    // An unreadable style falls back to the pill: this URL never gets to error.
    let form = match q.get("style").map(String::as_str) {
        Some("card") => Form::Card(theme_of(&q)),
        _ => Form::Pill,
    };
    render(state, &format!("{owner}/{repo}"), form).await
}

/// star-history's embed shape, so migrating a dead chart is a one-hostname edit.
/// Only the first repo of the list is ours to draw; everything else is ignored.
async fn compat_badge(
    State(state): State<Arc<AppState>>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    let first = q
        .get("repos")
        .and_then(|repos| repos.split(',').next())
        .unwrap_or_default()
        .trim()
        .to_string();
    render(state, &first, Form::Card(theme_of(&q))).await
}

fn theme_of(q: &HashMap<String, String>) -> Theme {
    match q.get("theme").map(String::as_str) {
        Some("dark") => Theme::Dark,
        _ => Theme::Light,
    }
}

async fn render(state: Arc<AppState>, full_name: &str, form: Form) -> Response {
    let name = full_name.to_string();
    let resolved = {
        let (state, name) = (Arc::clone(&state), name.clone());
        tokio::task::spawn_blocking(move || resolve(&state, &name, now_unix())).await
    };
    let badge = resolved.unwrap_or_else(|e| {
        // A panicking handler is a bug to fix, never a broken image in a README.
        eprintln!("badge {name}: {e}");
        None
    });
    let (svg, max_age) = match (&badge, form) {
        (Some(b), Form::Pill) => (badge::pill(b), max_age_for(b.state)),
        (Some(b), Form::Card(theme)) => (badge::card(b, theme), max_age_for(b.state)),
        (None, Form::Pill) => (badge::not_tracked_pill(&name), FRESH_MAX_AGE),
        (None, Form::Card(theme)) => (badge::not_tracked_card(&name, theme), FRESH_MAX_AGE),
    };
    svg_response(svg, max_age)
}

fn max_age_for(state: BadgeState) -> u32 {
    match state {
        BadgeState::Enrolled => FRESH_MAX_AGE,
        _ => SETTLED_MAX_AGE,
    }
}

fn svg_response(body: String, max_age: u32) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("image/svg+xml; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_str(&format!("public, max-age={max_age}"))
                    .expect("max-age is a number"),
            ),
        ],
        body,
    )
        .into_response()
}

fn text(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

// ---------------------------------------------------------------- badge from the store

struct Tracked {
    id: i64,
    full_name: String,
    active: bool,
    enrolled_at: String,
    created_at: Option<String>,
}

struct Reading {
    ts: String,
    stars: i64,
}

/// The badge for a repo, or `None` for anything untracked we could not enroll.
fn resolve(state: &AppState, full_name: &str, now: i64) -> Option<RepoBadge> {
    let (owner, name) = full_name.split_once('/')?;
    if !valid_owner(owner) || !valid_name(name) {
        // Junk reaches neither GitHub nor the store.
        return None;
    }
    let tracked = {
        let store = state.store();
        logged(full_name, find_repo(&store.conn, full_name))?
    };
    match tracked {
        Some(repo) => {
            let store = state.store();
            logged(full_name, build(&store.conn, &repo, now))?
        }
        None => enroll(state, full_name, now),
    }
}

fn logged<T>(what: &str, result: Result<T>) -> Option<T> {
    result.map_err(|e| eprintln!("badge {what}: {e:#}")).ok()
}

/// GitHub's own shapes. Anything else is a typo or a probe.
fn valid_owner(s: &str) -> bool {
    (1..=39).contains(&s.len()) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn valid_name(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn find_repo(conn: &Connection, full_name: &str) -> Result<Option<Tracked>> {
    Ok(conn
        .prepare(REPO_BY_NAME)?
        .query_row(params![full_name], |row| {
            Ok(Tracked {
                id: row.get(0)?,
                full_name: row.get(1)?,
                active: row.get::<_, String>(2)? == "active",
                enrolled_at: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .optional()?)
}

fn build(conn: &Connection, repo: &Tracked, now: i64) -> Result<Option<RepoBadge>> {
    let latest = read_one(conn, LATEST_SNAPSHOT, params![repo.id])?;
    let stars = latest.as_ref().map_or(0, |r| r.stars);
    let measured = match &latest {
        Some(l) => velocity(conn, repo.id, l)?,
        None => None,
    };
    let state = if !repo.active {
        BadgeState::Paused
    } else if let Some(velocity) = measured {
        BadgeState::Measured {
            velocity,
            top_percent: percentile(conn, velocity, now)?,
        }
    } else if repo.enrolled_at.get(..10) == Some(date_utc(now).as_str()) {
        BadgeState::Enrolled
    } else if let Some(avg) = proxy_average(repo.created_at.as_deref(), stars, now) {
        BadgeState::Proxy { avg }
    } else {
        BadgeState::Measuring
    };
    let spark = match state {
        BadgeState::Measured { .. } => spark(conn, repo.id, now)?,
        _ => Vec::new(),
    };
    Ok(Some(RepoBadge {
        full_name: repo.full_name.clone(),
        stars,
        tracked_since: repo.enrolled_at.get(..10).unwrap_or_default().to_string(),
        state,
        spark,
    }))
}

fn read_one(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Option<Reading>> {
    Ok(conn
        .prepare(sql)?
        .query_row(args, |row| {
            Ok(Reading {
                ts: row.get(0)?,
                stars: row.get(1)?,
            })
        })
        .optional()?)
}

/// The measured delta, or `None` until a second reading sits far enough back.
fn velocity(conn: &Connection, repo_id: i64, latest: &Reading) -> Result<Option<i64>> {
    let Some(at) = parse_iso8601_utc(&latest.ts) else {
        return Ok(None);
    };
    let cutoff = iso8601_utc(at - MIN_VELOCITY_SPAN_HOURS * 3600);
    let prior = read_one(conn, SNAPSHOT_AT_OR_BEFORE, params![repo_id, cutoff])?;
    Ok(prior.and_then(|p| pair_velocity(latest, &p)))
}

/// Stars per day between two readings. Negative is a real observation, kept as one.
fn pair_velocity(latest: &Reading, prior: &Reading) -> Option<i64> {
    let (l, p) = (
        parse_iso8601_utc(&latest.ts)?,
        parse_iso8601_utc(&prior.ts)?,
    );
    let days = (l - p) as f64 / SECONDS_PER_DAY as f64;
    if days <= 0.0 {
        return None;
    }
    Some((((latest.stars - prior.stars) as f64) / days).round() as i64)
}

/// A young repo's lifetime average, worn with the proxy label it deserves.
fn proxy_average(created_at: Option<&str>, stars: i64, now: i64) -> Option<i64> {
    let age_days = (now - parse_iso8601_utc(created_at?)?) as f64 / SECONDS_PER_DAY as f64;
    if age_days > PROXY_MAX_AGE_DAYS {
        return None;
    }
    Some((stars as f64 / age_days.max(1.0)).round() as i64)
}

/// Where this velocity sits among every repo we can currently measure.
///
/// One pass over the recent snapshots per request, which is free at
/// hundreds of repos. Precompute it into a table at tens of thousands.
fn percentile(conn: &Connection, velocity: i64, now: i64) -> Result<f64> {
    let since = iso8601_utc(now - VELOCITY_WINDOW_DAYS * SECONDS_PER_DAY);
    let mut stmt = conn.prepare(RECENT_SNAPSHOTS)?;
    let rows = stmt.query_map(params![since], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            Reading {
                ts: row.get(1)?,
                stars: row.get(2)?,
            },
        ))
    })?;

    // Rows arrive grouped by repo and ordered in time, so pairing is a walk.
    let mut fleet: Vec<i64> = Vec::new();
    let mut current: Vec<Reading> = Vec::new();
    let mut current_id = None;
    for row in rows {
        let (id, reading) = row?;
        if current_id != Some(id) {
            fleet.extend(series_velocity(&current));
            current = Vec::new();
            current_id = Some(id);
        }
        current.push(reading);
    }
    fleet.extend(series_velocity(&current));

    if fleet.is_empty() {
        return Ok(100.0);
    }
    // Nobody outranks themselves out of their own pool.
    let ahead = fleet.iter().filter(|&&v| v >= velocity).count().max(1);
    Ok(100.0 * ahead as f64 / fleet.len() as f64)
}

fn series_velocity(series: &[Reading]) -> Option<i64> {
    let latest = series.last()?;
    let cutoff = iso8601_utc(parse_iso8601_utc(&latest.ts)? - MIN_VELOCITY_SPAN_HOURS * 3600);
    let prior = series.iter().rev().find(|r| r.ts <= cutoff)?;
    pair_velocity(latest, prior)
}

/// The last 30 days as points in the card's spark region. x is real time, so a
/// young series occupies only the right and the empty left is the whole point.
fn spark(conn: &Connection, repo_id: i64, now: i64) -> Result<Vec<(f64, f64)>> {
    let start = now - SPARK_DAYS * SECONDS_PER_DAY;
    let mut stmt = conn.prepare(SNAPSHOTS_SINCE)?;
    let rows = stmt.query_map(params![repo_id, iso8601_utc(start)], |row| {
        Ok(Reading {
            ts: row.get(0)?,
            stars: row.get(1)?,
        })
    })?;

    // One point a day: the last reading of each UTC day.
    let mut daily: Vec<(i64, i64)> = Vec::new();
    for row in rows {
        let reading = row?;
        let Some(at) = parse_iso8601_utc(&reading.ts) else {
            continue;
        };
        match daily.last_mut() {
            Some(last) if last.0.div_euclid(SECONDS_PER_DAY) == at.div_euclid(SECONDS_PER_DAY) => {
                *last = (at, reading.stars);
            }
            _ => daily.push((at, reading.stars)),
        }
    }
    if daily.len() < 2 {
        return Ok(Vec::new());
    }

    let (min, max) = daily
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &(_, s)| {
            (lo.min(s), hi.max(s))
        });
    let span = (now - start) as f64;
    Ok(daily
        .iter()
        .map(|&(at, stars)| {
            let x = ((at - start) as f64 / span * SPARK_WIDTH).clamp(0.0, SPARK_WIDTH);
            let y = if max == min {
                SPARK_FLAT
            } else {
                SPARK_BOTTOM
                    - (stars - min) as f64 / (max - min) as f64 * (SPARK_BOTTOM - SPARK_TOP)
            };
            (x, y)
        })
        .collect())
}

// ---------------------------------------------------------------- embed-lane enrollment

/// The first badge request for an untracked repo is what enrolls it (ADR-002).
fn enroll(state: &AppState, full_name: &str, now: i64) -> Option<RepoBadge> {
    let ts = iso8601_utc(now);
    if !state.spend_enrollment(&date_utc(now)) {
        queue(state, full_name, &ts);
        return None;
    }
    // The store lock is never held across this call.
    match state.github.repo(full_name) {
        Ok(Some(repo)) => {
            let stars = repo.stargazers_count;
            let name = repo.full_name.clone();
            logged(full_name, {
                let store = state.store();
                record_repo(&store.conn, &repo, "embed", &ts)
            })?;
            Some(RepoBadge {
                full_name: name,
                stars,
                tracked_since: date_utc(now),
                state: BadgeState::Enrolled,
                spark: Vec::new(),
            })
        }
        // Unknown or private: a neutral badge and no rows, nothing to retry.
        Ok(None) => None,
        Err(e) => {
            // A GitHub outage must never become a broken image; try again tomorrow.
            eprintln!("enrolling {full_name}: {e:#}");
            queue(state, full_name, &ts);
            None
        }
    }
}

fn queue(state: &AppState, full_name: &str, ts: &str) {
    let store = state.store();
    if let Err(e) = store.conn.execute(QUEUE_ENROLLMENT, params![full_name, ts]) {
        eprintln!("queueing {full_name}: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{repo_json, stub};
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    const HOUR: i64 = 3_600;

    struct Harness {
        app: Router,
        state: Arc<AppState>,
        hits: Arc<AtomicUsize>,
        now: i64,
    }

    struct Fetched {
        status: StatusCode,
        content_type: String,
        cache_control: String,
        body: String,
    }

    fn harness(responses: Vec<(u16, String)>) -> Harness {
        let (base, hits) = stub(responses);
        let store = Store::open_in_memory().expect("in-memory store");
        let state = Arc::new(AppState::new(store, GitHub::at(&base)));
        Harness {
            app: router(Arc::clone(&state)),
            state,
            hits,
            now: now_unix(),
        }
    }

    impl Harness {
        /// Ages are hours before now; a `created` of `None` is the unknown-age case.
        fn track(&self, id: i64, name: &str, enrolled_ago: i64, created_ago: Option<i64>) {
            self.track_as(id, name, enrolled_ago, created_ago, "active");
        }

        fn track_as(
            &self,
            id: i64,
            name: &str,
            enrolled_ago: i64,
            created_ago: Option<i64>,
            status: &str,
        ) {
            self.state
                .store()
                .conn
                .execute(
                    "INSERT INTO repos (id, full_name, status, lane, enrolled_at, created_at)
                     VALUES (?1, ?2, ?3, 'scan', ?4, ?5)",
                    params![
                        id,
                        name,
                        status,
                        iso8601_utc(self.now - enrolled_ago * HOUR),
                        created_ago.map(|h| iso8601_utc(self.now - h * HOUR)),
                    ],
                )
                .expect("seeding a repo");
        }

        fn snapshot(&self, id: i64, ago: i64, stars: i64) {
            self.snapshot_at(id, &iso8601_utc(self.now - ago * HOUR), stars);
        }

        fn snapshot_at(&self, id: i64, ts: &str, stars: i64) {
            self.state
                .store()
                .conn
                .execute(
                    "INSERT INTO snapshots (repo_id, ts, stars) VALUES (?1, ?2, ?3)",
                    params![id, ts, stars],
                )
                .expect("seeding a snapshot");
        }

        fn scalar(&self, sql: &str) -> i64 {
            self.state
                .store()
                .conn
                .query_row(sql, [], |row| row.get(0))
                .expect(sql)
        }

        fn queued(&self) -> Vec<String> {
            let store = self.state.store();
            let mut stmt = store
                .conn
                .prepare("SELECT full_name FROM enroll_queue ORDER BY full_name")
                .expect("reading the queue");
            stmt.query_map([], |row| row.get(0))
                .expect("reading the queue")
                .collect::<rusqlite::Result<_>>()
                .expect("reading the queue")
        }

        fn calls(&self) -> usize {
            self.hits.load(Ordering::SeqCst)
        }

        /// One runtime per request: the harness owns a blocking HTTP client, which
        /// must be built and dropped outside any async context.
        fn get(&self, uri: &str) -> Fetched {
            let request = Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("building the request");
            let app = self.app.clone();
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a runtime")
                .block_on(async move {
                    let response = app.oneshot(request).await.expect("the router answers");
                    let status = response.status();
                    let head = |name: &str| {
                        response
                            .headers()
                            .get(name)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_string()
                    };
                    let (content_type, cache_control) =
                        (head("content-type"), head("cache-control"));
                    let body = to_bytes(response.into_body(), usize::MAX)
                        .await
                        .expect("a body");
                    Fetched {
                        status,
                        content_type,
                        cache_control,
                        body: String::from_utf8(body.to_vec()).expect("utf-8"),
                    }
                })
        }
    }

    /// A repo with a day-old reading behind its latest one, so it measures.
    fn measured(h: &Harness, id: i64, name: &str) {
        h.track(id, name, 24 * 30, None);
        h.snapshot(id, 25, 1_000);
        h.snapshot(id, 1, 1_100);
    }

    #[test]
    fn style_defaults_to_the_pill_and_theme_only_moves_the_card() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        let pill = h.get("/badge/o/r");
        assert_eq!(pill.status, StatusCode::OK);
        assert_eq!(pill.content_type, "image/svg+xml; charset=utf-8");
        assert_eq!(pill.cache_control, "public, max-age=3600");
        assert!(pill.body.contains(r#"height="20""#), "{}", pill.body);
        assert!(pill.body.contains("▲ 100/day"), "{}", pill.body);

        // The pill is theme-invariant: one render everywhere.
        assert_eq!(h.get("/badge/o/r?theme=dark").body, pill.body);

        let card = h.get("/badge/o/r?style=card");
        assert!(
            card.body.contains(r#"width="420" height="150""#),
            "{}",
            card.body
        );
        assert!(card.body.contains("#ffffff"), "{}", card.body);
        assert!(card.body.contains("% velocity"), "{}", card.body);

        let dark = h.get("/badge/o/r?style=card&theme=dark");
        assert!(dark.body.contains("#0d1117"), "{}", dark.body);

        // Unknown style, unknown params, junk theme: still the default pill.
        assert_eq!(
            h.get("/badge/o/r?style=poster&type=Date&theme=neon&x").body,
            pill.body
        );
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn compat_svg_draws_the_first_repo_and_ignores_the_rest() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");
        h.track(2, "other/repo", 24, None);
        h.snapshot(2, 1, 5);

        let got = h.get("/svg?repos=o/r,other/repo&type=Date&junk=1");
        assert_eq!(got.status, StatusCode::OK);
        assert!(got.body.contains(">o/r<"), "{}", got.body);
        assert!(!got.body.contains("other/repo"), "{}", got.body);
        // The compat URL always draws the card, whatever else it carries.
        assert!(
            got.body.contains(r#"width="420" height="150""#),
            "{}",
            got.body
        );
        assert!(h.get("/svg?repos=o/r&theme=dark").body.contains("#0d1117"));
    }

    #[test]
    fn every_state_renders_with_its_own_cache_ttl() {
        let h = harness(vec![]);

        measured(&h, 1, "o/measured");
        // Young repo, one reading: a labeled lifetime average.
        h.track(2, "o/young", 24, Some(24 * 10));
        h.snapshot(2, 1, 300);
        // Old repo, one reading: no number at all.
        h.track(3, "o/old", 24, Some(24 * 400));
        h.snapshot(3, 1, 5_000);
        // Two readings three hours apart are one day seen twice, not a delta.
        h.track(4, "o/twice", 24, Some(24 * 400));
        h.snapshot(4, 4, 5_000);
        h.snapshot(4, 1, 5_010);
        h.track(5, "o/fresh", 0, Some(24 * 400));
        // Gone from our vantage point: the series is kept, the badge says so.
        h.track_as(6, "o/paused", 24 * 30, None, "inactive");
        h.snapshot(6, 48, 900);

        for (uri, marker, max_age) in [
            ("/badge/o/measured", "▲ 100/day", 3_600),
            ("/badge/o/young", "~30 avg", 3_600),
            ("/badge/o/old", "measuring · first reading tomorrow", 3_600),
            (
                "/badge/o/twice",
                "measuring · first reading tomorrow",
                3_600,
            ),
            ("/badge/o/fresh", "tracking started", 300),
            ("/badge/o/paused", "tracking paused", 3_600),
            ("/badge/o/untracked", "not tracked", 300),
        ] {
            let got = h.get(uri);
            assert_eq!(got.status, StatusCode::OK, "{uri}");
            assert!(got.body.contains(marker), "{uri}: {}", got.body);
            assert_eq!(
                got.cache_control,
                format!("public, max-age={max_age}"),
                "{uri}"
            );
        }

        // Only a measured card carries a sparkline; the rest show the honest void.
        assert!(
            h.get("/badge/o/measured?style=card")
                .body
                .contains("polyline")
        );
        assert!(
            h.get("/badge/o/old?style=card")
                .body
                .contains("stroke-dasharray")
        );
    }

    #[test]
    fn invalid_names_never_reach_github_or_the_store() {
        let h = harness(vec![]);
        let long = "a".repeat(40);
        for uri in [
            "/badge/o/r%20x".to_string(),
            "/badge/under_score/r".to_string(),
            "/badge/o/a$b".to_string(),
            format!("/badge/{long}/r"),
            "/svg?repos=notarepo".to_string(),
            "/svg?repos=".to_string(),
        ] {
            let got = h.get(&uri);
            assert_eq!(got.status, StatusCode::OK, "{uri}");
            assert!(got.body.contains("not tracked"), "{uri}: {}", got.body);
            assert_eq!(got.cache_control, "public, max-age=300", "{uri}");
        }
        // Nothing was asked of GitHub, and nothing was written down.
        assert_eq!(h.calls(), 0);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM enroll_queue"), 0);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 0);
    }

    #[test]
    fn a_first_request_enrolls_and_the_next_comes_from_the_store() {
        let created = iso8601_utc(now_unix() - 400 * 24 * HOUR);
        let h = harness(vec![(200, repo_json(42, "Owner/Repo", 1_234, &created))]);

        let first = h.get("/badge/owner/repo?style=card");
        assert_eq!(first.cache_control, "public, max-age=300");
        assert!(first.body.contains("tracking started"), "{}", first.body);
        // The canonical name from the API, not the one in the URL.
        assert!(first.body.contains(">Owner/Repo<"), "{}", first.body);
        assert_eq!(h.calls(), 1);
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM repos WHERE lane = 'embed'"),
            1
        );
        assert_eq!(
            h.scalar("SELECT stars FROM snapshots WHERE repo_id = 42"),
            1_234
        );
        assert_eq!(
            h.scalar("SELECT subscribers FROM snapshots WHERE repo_id = 42"),
            5
        );
        assert_eq!(h.scalar("SELECT COUNT(*) FROM enroll_queue"), 0);

        // The stub has nothing left to say, so a second call would fail: the store
        // answers instead, and the lookup shrugs off the case of the URL.
        let second = h.get("/badge/OWNER/REPO");
        assert!(second.body.contains("1,234"), "{}", second.body);
        assert!(second.body.contains("tracking started"), "{}", second.body);
        assert_eq!(h.calls(), 1);
    }

    #[test]
    fn an_exhausted_budget_queues_instead_of_enrolling() {
        let h = harness(vec![(
            200,
            repo_json(42, "owner/repo", 5, "2026-01-01T00:00:00Z"),
        )]);
        let today = date_utc(h.now);
        for _ in 0..EMBED_DAILY_BUDGET {
            assert!(h.state.spend_enrollment(&today));
        }
        assert!(!h.state.spend_enrollment(&today));

        let got = h.get("/badge/owner/repo");
        assert!(got.body.contains("not tracked"), "{}", got.body);
        assert_eq!(got.cache_control, "public, max-age=300");
        assert_eq!(h.calls(), 0);
        assert_eq!(h.queued(), ["owner/repo"]);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 0);

        // Asking twice does not queue it twice.
        h.get("/badge/owner/repo");
        assert_eq!(h.queued(), ["owner/repo"]);

        // Tomorrow the budget is whole again.
        assert!(h.state.spend_enrollment("2099-01-01"));
    }

    #[test]
    fn a_repo_github_will_not_confirm_leaves_no_rows() {
        let h = harness(vec![(404, "{}".to_string())]);
        let got = h.get("/badge/owner/repo?style=card");
        assert!(got.body.contains("not tracked"), "{}", got.body);
        assert!(got.body.contains(">owner/repo<"), "{}", got.body);
        assert_eq!(h.calls(), 1);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 0);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM snapshots"), 0);
        // A 404 is an answer, not an outage: nothing to come back to.
        assert_eq!(h.scalar("SELECT COUNT(*) FROM enroll_queue"), 0);
    }

    #[test]
    fn a_github_outage_queues_the_repo_and_still_draws_a_badge() {
        let h = harness(vec![]);
        let got = h.get("/badge/owner/repo");
        assert_eq!(got.status, StatusCode::OK);
        assert!(got.body.contains("not tracked"), "{}", got.body);
        assert_eq!(h.calls(), 1);
        assert_eq!(h.queued(), ["owner/repo"]);
    }

    #[test]
    fn the_front_page_and_the_fallback_are_plain_text() {
        let h = harness(vec![]);

        let index = h.get("/");
        assert_eq!(index.status, StatusCode::OK);
        assert_eq!(index.content_type, "text/plain; charset=utf-8");
        assert!(
            index.body.contains("/badge/{owner}/{repo}"),
            "{}",
            index.body
        );

        let missing = h.get("/favicon.ico");
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(missing.content_type, "text/plain; charset=utf-8");
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn velocity_is_stars_per_day_between_two_readings() {
        let r = |ts: &str, stars| Reading {
            ts: ts.to_string(),
            stars,
        };
        let over_a_day =
            |a, b| pair_velocity(&r("2026-07-31T00:00:00Z", a), &r("2026-07-30T00:00:00Z", b));
        assert_eq!(over_a_day(1_100, 1_000), Some(100));
        assert_eq!(over_a_day(1_000, 1_000), Some(0));
        // Losing stars is a real observation, kept as one.
        assert_eq!(over_a_day(900, 1_000), Some(-100));
        // Half a day at 50 is still 100 a day.
        assert_eq!(
            pair_velocity(
                &r("2026-07-31T00:00:00Z", 1_050),
                &r("2026-07-30T12:00:00Z", 1_000)
            ),
            Some(100)
        );
        // No span to divide by, and no readable timestamp.
        assert_eq!(
            pair_velocity(&r("2026-07-31T00:00:00Z", 1), &r("2026-07-31T00:00:00Z", 0)),
            None
        );
        assert_eq!(
            pair_velocity(&r("yesterday", 1), &r("2026-07-30T00:00:00Z", 0)),
            None
        );
    }

    #[test]
    fn a_proxy_average_is_only_offered_for_young_repos() {
        let now = 400 * 24 * HOUR;
        let at = |days: i64| iso8601_utc(now - days * 24 * HOUR);
        assert_eq!(proxy_average(Some(&at(10)), 300, now), Some(30));
        assert_eq!(proxy_average(Some(&at(179)), 1_790, now), Some(10));
        assert_eq!(proxy_average(Some(&at(181)), 1_000, now), None);
        // Younger than a day still divides by a day.
        assert_eq!(proxy_average(Some(&at(0)), 40, now), Some(40));
        // Nothing to divide by.
        assert_eq!(proxy_average(None, 1_000, now), None);
        assert_eq!(proxy_average(Some("who knows"), 1_000, now), None);
    }

    #[test]
    fn percentile_counts_the_repo_in_its_own_pool() {
        let h = harness(vec![]);
        for (id, per_day) in [(1, 400), (2, 300), (3, 200), (4, 100)] {
            h.track(id, &format!("o/r{id}"), 24 * 30, None);
            h.snapshot(id, 24, 1_000);
            h.snapshot(id, 0, 1_000 + per_day);
        }
        // A repo that left the fleet is not part of the fleet.
        h.track_as(5, "o/gone", 24 * 30, None, "inactive");
        h.snapshot(5, 24, 1_000);
        h.snapshot(5, 0, 99_000);

        let store = h.state.store();
        assert_eq!(percentile(&store.conn, 400, h.now).unwrap(), 25.0);
        assert_eq!(percentile(&store.conn, 300, h.now).unwrap(), 50.0);
        assert_eq!(percentile(&store.conn, 100, h.now).unwrap(), 100.0);
        // Faster than everyone measured is still one repo out of the pool.
        assert_eq!(percentile(&store.conn, 5_000, h.now).unwrap(), 25.0);
    }

    #[test]
    fn spark_maps_time_across_the_region_and_stars_between_the_rows() {
        let h = harness(vec![]);
        let now = parse_iso8601_utc("2026-07-31T00:00:00Z").expect("a fixed now");
        h.track(1, "o/r", 24 * 40, None);
        for (ts, stars) in [
            ("2026-07-01T00:00:00Z", 100),
            ("2026-07-16T00:00:00Z", 150),
            // Same UTC day as the reading above: only the last of a day draws.
            ("2026-07-16T18:00:00Z", 160),
            ("2026-07-31T00:00:00Z", 200),
            // Older than the window.
            ("2026-06-01T00:00:00Z", 1),
        ] {
            h.snapshot_at(1, ts, stars);
        }
        // One reading is not a line.
        h.track(2, "o/one", 24, None);
        h.snapshot_at(2, "2026-07-30T00:00:00Z", 5);
        // A flat series sits in the middle of the region.
        h.track(3, "o/flat", 24 * 10, None);
        h.snapshot_at(3, "2026-07-29T00:00:00Z", 7);
        h.snapshot_at(3, "2026-07-30T00:00:00Z", 7);

        let store = h.state.store();
        let round1 = |points: Vec<(f64, f64)>| -> Vec<(f64, f64)> {
            points
                .iter()
                .map(|&(x, y)| ((x * 10.0).round() / 10.0, (y * 10.0).round() / 10.0))
                .collect()
        };
        assert_eq!(
            round1(spark(&store.conn, 1, now).unwrap()),
            [(0.0, 36.0), (203.7, 16.8), (388.0, 4.0)]
        );
        assert!(spark(&store.conn, 2, now).unwrap().is_empty());
        assert!(
            spark(&store.conn, 3, now)
                .unwrap()
                .iter()
                .all(|&(_, y)| y == 20.0)
        );
    }
}
