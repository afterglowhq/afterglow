//! The badge server, and the two HTML pages that make a badge checkable. The badge
//! URL shapes are immortal once public (SPEC §5), and one rule sits under all of
//! it: what we show is answered from the snapshot store, never from a live API
//! call (ToS rule 6). The only GitHub request on these paths is the one that
//! enrolls a repo nobody has asked for before.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_server::tls_rustls::RustlsConfig;
use maud::Markup;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::badge::{self, BadgeState, RepoBadge, Theme};
use crate::github::GitHub;
use crate::site;
use crate::store::{QUEUE_ENROLLMENT, Store, opted_out, record_repo};
use crate::time::{SECONDS_PER_DAY, date_utc, iso8601_utc, now_unix, parse_iso8601_utc};

/// Enrollments the embed lane spends in one UTC day; overflow queues (ADR-002).
pub const EMBED_DAILY_BUDGET: u32 = 256;
/// The site form's own budget. Smaller than the embed lane's: a person typing a
/// name is rarer than a README loading, and it queues rather than refuses.
pub const MANUAL_DAILY_BUDGET: u32 = 64;

/// Two readings closer together than this are the same day seen twice, not a delta.
const MIN_VELOCITY_SPAN_HOURS: i64 = 12;
/// How far back the percentile pool reads; wide enough to survive a missed day.
const VELOCITY_WINDOW_DAYS: i64 = 35;
const SPARK_DAYS: i64 = 30;
/// Above this age a repo gets no proxy number, only `measuring` (ticket 007).
const PROXY_MAX_AGE_DAYS: f64 = 180.0;

/// Where a series is drawn: x across the width, stars between the two rows, a
/// flat series along the middle one.
pub struct Region {
    pub width: f64,
    top: f64,
    bottom: f64,
    flat: f64,
}

/// The card's spark region, 388 wide, and the leaderboard's minispark, the same
/// shape at row height.
const CARD_SPARK: Region = Region {
    width: 388.0,
    top: 4.0,
    bottom: 36.0,
    flat: 20.0,
};
pub const MINI_SPARK: Region = Region {
    width: 100.0,
    top: 4.0,
    bottom: 20.0,
    flat: 12.0,
};
/// The box the minispark is drawn in; its area fill closes on the bottom edge.
pub const MINI_SPARK_HEIGHT: f64 = 24.0;

/// Day-one and not-tracked states change tomorrow; measured badges can sit in the CDN.
const FRESH_MAX_AGE: u32 = 300;
const SETTLED_MAX_AGE: u32 = 3600;

/// Both pages are a snapshot of a store that moves once a day.
const PAGE_CACHE: &str = "public, max-age=300";
/// What the form says back is about one request and belongs to nobody else.
const NO_STORE: &str = "no-store";
/// The font is content-addressed by its path; a new cut would get a new one.
const FONT_CACHE: &str = "public, max-age=31536000, immutable";

/// Mona Sans, GitHub's own open font, self-hosted: no font CDN, and one fewer
/// party watching who reads the page.
const MONA_SANS: &[u8] = include_bytes!("../static/mona-sans.woff2");
pub const FONT_URL: &str = "/static/mona-sans.woff2";

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

/// The fleet's recent readings in one pass. `status = 'active'` is what keeps an
/// opted-out repo out of both the percentile pool and the leaderboard.
const RECENT_SNAPSHOTS: &str = "\
SELECT s.repo_id, r.full_name, r.created_at, s.ts, s.stars
FROM snapshots s JOIN repos r ON r.id = s.repo_id
WHERE r.status = 'active' AND s.ts >= ?1
ORDER BY s.repo_id, s.ts";

pub struct AppState {
    store: Mutex<Store>,
    github: GitHub,
    embed: Mutex<Budget>,
    manual: Mutex<Budget>,
}

/// Which lane an enrollment is spending, and what the repo row will record.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    Embed,
    Manual,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Lane::Embed => "embed",
            Lane::Manual => "manual",
        }
    }

    fn daily_budget(self) -> u32 {
        match self {
            Lane::Embed => EMBED_DAILY_BUDGET,
            Lane::Manual => MANUAL_DAILY_BUDGET,
        }
    }
}

/// In memory, so a restart refills the day's budget. At one host and 256
/// a day that beats a store write on every badge miss; move it into the store when
/// a second host appears.
struct Budget {
    date: String,
    spent: u32,
}

impl Budget {
    fn fresh() -> Mutex<Budget> {
        Mutex::new(Budget {
            date: String::new(),
            spent: 0,
        })
    }
}

impl AppState {
    pub fn new(store: Store, github: GitHub) -> Self {
        AppState {
            store: Mutex::new(store),
            github,
            embed: Budget::fresh(),
            manual: Budget::fresh(),
        }
    }

    /// One panicking request must not take the store down for every later one.
    fn store(&self) -> MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Counts an enrollment attempt against its lane, or refuses once the day is
    /// spent. The lanes hold separate budgets so a busy README cannot eat the
    /// allowance the site form runs on.
    fn spend_enrollment(&self, lane: Lane, today: &str) -> bool {
        let mut budget = match lane {
            Lane::Embed => &self.embed,
            Lane::Manual => &self.manual,
        }
        .lock()
        .unwrap_or_else(|e| e.into_inner());
        if budget.date != today {
            budget.date = today.to_string();
            budget.spent = 0;
        }
        if budget.spent >= lane.daily_budget() {
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
        .route("/rankings", get(rankings))
        .route("/enroll", post(enroll_form))
        .route("/badge/{owner}/{repo}", get(canonical_badge))
        .route("/svg", get(compat_badge))
        .route(FONT_URL, get(font))
        .fallback(missing)
        .with_state(state)
}

#[derive(Clone, Copy)]
enum Shape {
    Pill,
    Card(Theme),
}

/// Velocity is what the page claims, so it stays the default order. Stars is the
/// same rows in another order, carrying the same numbers.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    Velocity,
    Stars,
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    html(site::index(&load_board(&state).await), PAGE_CACHE)
}

async fn rankings(
    State(state): State<Arc<AppState>>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    // An unreadable sort is the default board, the way an unreadable style is the pill.
    let sort = match q.get("sort").map(String::as_str) {
        Some("stars") => Sort::Stars,
        _ => Sort::Velocity,
    };
    let mut board = load_board(&state).await;
    if sort == Sort::Stars {
        board.rows.sort_by(|a, b| {
            b.stars
                .cmp(&a.stars)
                .then_with(|| a.full_name.cmp(&b.full_name))
        });
    }
    html(site::rankings(&board, sort), PAGE_CACHE)
}

/// The form's own answer, about one submission: never cached, never shared.
async fn enroll_form(
    State(state): State<Arc<AppState>>,
    form: Result<Form<Submission>, FormRejection>,
) -> Response {
    let raw = form.map(|Form(f)| f.repo).unwrap_or_default();
    let Some(full_name) = repo_name(&raw) else {
        return html(site::enrolled(&Outcome::Malformed), NO_STORE);
    };
    let outcome = tokio::task::spawn_blocking(move || submit(&state, &full_name, now_unix()))
        .await
        .unwrap_or_else(|e| {
            eprintln!("enroll form: {e}");
            Outcome::Nothing {
                full_name: String::new(),
            }
        });
    html(site::enrolled(&outcome), NO_STORE)
}

async fn font() -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("font/woff2")),
            (header::CACHE_CONTROL, HeaderValue::from_static(FONT_CACHE)),
        ],
        MONA_SANS,
    )
        .into_response()
}

async fn missing() -> Response {
    text(StatusCode::NOT_FOUND, NOT_FOUND)
}

/// A page whose board could not be read shows an empty board, the same way a badge
/// that cannot be read renders not-tracked: the numbers are missing, not the site.
async fn load_board(state: &Arc<AppState>) -> Board {
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        let store = state.store();
        logged("rankings", board(&store.conn, now_unix()))
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

async fn canonical_badge(
    State(state): State<Arc<AppState>>,
    Path((owner, repo)): Path<(String, String)>,
    query: Result<Query<HashMap<String, String>>, QueryRejection>,
) -> Response {
    let q = query.map(|Query(q)| q).unwrap_or_default();
    // An unreadable style falls back to the pill: this URL never gets to error.
    let shape = match q.get("style").map(String::as_str) {
        Some("card") => Shape::Card(theme_of(&q)),
        _ => Shape::Pill,
    };
    render(state, &format!("{owner}/{repo}"), shape).await
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
    render(state, &first, Shape::Card(theme_of(&q))).await
}

fn theme_of(q: &HashMap<String, String>) -> Theme {
    match q.get("theme").map(String::as_str) {
        Some("dark") => Theme::Dark,
        _ => Theme::Light,
    }
}

async fn render(state: Arc<AppState>, full_name: &str, shape: Shape) -> Response {
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
    let (svg, max_age) = match (&badge, shape) {
        (Some(b), Shape::Pill) => (badge::pill(b), max_age_for(b.state)),
        (Some(b), Shape::Card(theme)) => (badge::card(b, theme), max_age_for(b.state)),
        (None, Shape::Pill) => (badge::not_tracked_pill(&name), FRESH_MAX_AGE),
        (None, Shape::Card(theme)) => (badge::not_tracked_card(&name, theme), FRESH_MAX_AGE),
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

fn html(body: Markup, cache: &'static str) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body.into_string(),
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
    status: Status,
    enrolled_at: String,
    created_at: Option<String>,
}

/// What the repo row says we may do with a repo.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Active,
    /// Gone from our vantage point: the series is kept, the badge says paused.
    Inactive,
    /// The maintainer asked us to stop (ToS rule 4).
    OptedOut,
}

impl Status {
    fn read(s: &str) -> Status {
        match s {
            "inactive" => Status::Inactive,
            "opted_out" => Status::OptedOut,
            _ => Status::Active,
        }
    }
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
        // An opted-out repo reads exactly like one we never saw. Not paused:
        // paused says we are still watching and cannot see, which
        // is not what a maintainer who asked us to stop agreed to.
        Some(repo) if repo.status == Status::OptedOut => None,
        Some(repo) => {
            let store = state.store();
            logged(full_name, build(&store.conn, &repo, now))?
        }
        None => match enroll(state, Lane::Embed, full_name, now) {
            Enrollment::Started(badge) => Some(badge),
            _ => None,
        },
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
                status: Status::read(&row.get::<_, String>(2)?),
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
    let state = if repo.status != Status::Active {
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

/// One repo's recent readings, with what a leaderboard row needs around them.
struct Series {
    id: i64,
    full_name: String,
    created_at: Option<String>,
    readings: Vec<Reading>,
}

/// Every measurable repo's recent window, in one pass.
///
/// One pass per request, free at hundreds of repos, and it is what
/// keeps a row and a card deriving their numbers from the same rows. Precompute
/// the velocities into a table at tens of thousands.
fn recent_series(conn: &Connection, now: i64) -> Result<Vec<Series>> {
    let since = iso8601_utc(now - VELOCITY_WINDOW_DAYS * SECONDS_PER_DAY);
    let mut stmt = conn.prepare(RECENT_SNAPSHOTS)?;
    let mut rows = stmt.query(params![since])?;

    // Rows arrive grouped by repo and ordered in time, so grouping is a walk and
    // a repo's name is only read off the first row of its group.
    let mut out: Vec<Series> = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let reading = Reading {
            ts: row.get(3)?,
            stars: row.get(4)?,
        };
        match out.last_mut() {
            Some(series) if series.id == id => series.readings.push(reading),
            _ => out.push(Series {
                id,
                full_name: row.get(1)?,
                created_at: row.get(2)?,
                readings: vec![reading],
            }),
        }
    }
    Ok(out)
}

/// Where this velocity sits among every repo we can currently measure.
fn percentile(conn: &Connection, velocity: i64, now: i64) -> Result<f64> {
    let series = recent_series(conn, now)?;
    Ok(percentile_of(&fleet_velocities(&series), velocity))
}

fn fleet_velocities(series: &[Series]) -> Vec<i64> {
    series
        .iter()
        .filter_map(|s| series_velocity(&s.readings))
        .collect()
}

fn percentile_of(fleet: &[i64], velocity: i64) -> f64 {
    if fleet.is_empty() {
        return 100.0;
    }
    // Nobody outranks themselves out of their own pool.
    let ahead = fleet.iter().filter(|&&v| v >= velocity).count().max(1);
    100.0 * ahead as f64 / fleet.len() as f64
}

fn series_velocity(series: &[Reading]) -> Option<i64> {
    let latest = series.last()?;
    let cutoff = iso8601_utc(parse_iso8601_utc(&latest.ts)? - MIN_VELOCITY_SPAN_HOURS * 3600);
    let prior = series.iter().rev().find(|r| r.ts <= cutoff)?;
    pair_velocity(latest, prior)
}

/// The last 30 days as points in the card's spark region.
fn spark(conn: &Connection, repo_id: i64, now: i64) -> Result<Vec<(f64, f64)>> {
    let start = now - SPARK_DAYS * SECONDS_PER_DAY;
    let mut stmt = conn.prepare(SNAPSHOTS_SINCE)?;
    let rows = stmt.query_map(params![repo_id, iso8601_utc(start)], |row| {
        Ok(Reading {
            ts: row.get(0)?,
            stars: row.get(1)?,
        })
    })?;
    let readings: Vec<Reading> = rows.collect::<rusqlite::Result<_>>()?;
    Ok(spark_points(&readings, now, &CARD_SPARK))
}

/// The last 30 days of a series as points in a region. x is real time, so a young
/// series occupies only the right and the empty left is the whole point.
fn spark_points(readings: &[Reading], now: i64, region: &Region) -> Vec<(f64, f64)> {
    let start = now - SPARK_DAYS * SECONDS_PER_DAY;

    // One point a day: the last reading of each UTC day.
    let mut daily: Vec<(i64, i64)> = Vec::new();
    for reading in readings {
        let Some(at) = parse_iso8601_utc(&reading.ts).filter(|&at| at >= start) else {
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
        return Vec::new();
    }

    let (min, max) = daily
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &(_, s)| {
            (lo.min(s), hi.max(s))
        });
    let span = (now - start) as f64;
    daily
        .iter()
        .map(|&(at, stars)| {
            let x = ((at - start) as f64 / span * region.width).clamp(0.0, region.width);
            let y = if max == min {
                region.flat
            } else {
                region.bottom
                    - (stars - min) as f64 / (max - min) as f64 * (region.bottom - region.top)
            };
            (x, y)
        })
        .collect()
}

// ---------------------------------------------------------------- the leaderboard

/// The velocity leaderboard: one list, ranked by the number itself, so the
/// percentile a card claims is checkable against the row above and below it.
#[derive(Default)]
pub struct Board {
    pub rows: Vec<BoardRow>,
    /// The newest reading behind the page, for the attribution line (ToS rule 5).
    pub through: Option<String>,
}

pub struct BoardRow {
    pub full_name: String,
    pub stars: i64,
    pub velocity: RowVelocity,
    /// Points in `MINI_SPARK`; measured rows only, because glow is earned.
    pub spark: Vec<(f64, f64)>,
}

pub enum RowVelocity {
    Measured {
        per_day: i64,
        top_percent: f64,
    },
    /// A young repo with nothing measured yet, ranked on its lifetime average and
    /// wearing the mark that says so.
    Proxy {
        avg: i64,
    },
}

impl RowVelocity {
    /// What the row ranks on. A proxy number is not a measured one, but it is the
    /// best claim that repo has, so it takes its place in the same list.
    fn per_day(&self) -> i64 {
        match *self {
            RowVelocity::Measured { per_day, .. } => per_day,
            RowVelocity::Proxy { avg } => avg,
        }
    }
}

fn board(conn: &Connection, now: i64) -> Result<Board> {
    let series = recent_series(conn, now)?;
    let measured: Vec<Option<i64>> = series
        .iter()
        .map(|s| series_velocity(&s.readings))
        .collect();
    let fleet: Vec<i64> = measured.iter().flatten().copied().collect();

    let mut through: Option<&str> = None;
    let mut rows: Vec<BoardRow> = Vec::new();
    for (s, per_day) in series.iter().zip(&measured) {
        let Some(latest) = s.readings.last() else {
            continue;
        };
        through = through.max(Some(latest.ts.as_str()));
        let (velocity, spark) = match *per_day {
            Some(per_day) => (
                RowVelocity::Measured {
                    per_day,
                    top_percent: percentile_of(&fleet, per_day),
                },
                spark_points(&s.readings, now, &MINI_SPARK),
            ),
            // Nothing measured: a young repo still has an honest average, and an
            // old one has no number at all, so it has no row.
            None => match proxy_average(s.created_at.as_deref(), latest.stars, now) {
                Some(avg) => (RowVelocity::Proxy { avg }, Vec::new()),
                None => continue,
            },
        };
        rows.push(BoardRow {
            full_name: s.full_name.clone(),
            stars: latest.stars,
            velocity,
            spark,
        });
    }

    rows.sort_by(|a, b| {
        b.velocity
            .per_day()
            .cmp(&a.velocity.per_day())
            .then_with(|| a.full_name.cmp(&b.full_name))
    });
    // Every row, not a top hundred: a card reading "top 80% velocity" belongs to a
    // repo no short list would ever print, and SPEC §5 wants that number checkable
    // here.
    //
    // The full board is ~25 KB gzipped at ~430 repos and stops being a
    // page somewhere well short of the 50k-repo coverage floor. Pagination, or a
    // top-N with a per-repo lookup, decided when the size actually bites.
    Ok(Board {
        rows,
        through: through.map(str::to_string),
    })
}

// ---------------------------------------------------------------- enrollment lanes

/// What became of a repo nobody had tracked yet.
enum Enrollment {
    Started(RepoBadge),
    /// Past the day's budget, or GitHub was unreachable: the name is parked and
    /// a later day enrolls it (ADR-002).
    Queued,
    /// Unknown, private, or opted out. No rows either way, nothing to retry.
    Nothing,
}

/// The first request for an untracked repo is what enrolls it, whichever lane it
/// arrived through: a README loading its badge, or a name typed on the site.
fn enroll(state: &AppState, lane: Lane, full_name: &str, now: i64) -> Enrollment {
    let ts = iso8601_utc(now);
    if !state.spend_enrollment(lane, &date_utc(now)) {
        queue(state, lane, full_name, &ts);
        return Enrollment::Queued;
    }
    // The store lock is never held across this call.
    match state.github.repo(full_name) {
        Ok(Some(repo)) => {
            let store = state.store();
            // A rename can hide an opted-out repo behind a name the store has
            // never seen. The numeric id is what decides (ToS rule 4).
            if logged(full_name, opted_out(&store.conn, repo.id)) != Some(false) {
                return Enrollment::Nothing;
            }
            let stars = repo.stargazers_count;
            let name = repo.full_name.clone();
            if logged(full_name, record_repo(&store.conn, &repo, lane.name(), &ts)).is_none() {
                return Enrollment::Nothing;
            }
            Enrollment::Started(RepoBadge {
                full_name: name,
                stars,
                tracked_since: date_utc(now),
                state: BadgeState::Enrolled,
                spark: Vec::new(),
            })
        }
        Ok(None) => Enrollment::Nothing,
        Err(e) => {
            // A GitHub outage must never become a broken image; try again tomorrow.
            eprintln!("enrolling {full_name}: {e:#}");
            queue(state, lane, full_name, &ts);
            Enrollment::Queued
        }
    }
}

fn queue(state: &AppState, lane: Lane, full_name: &str, ts: &str) {
    let store = state.store();
    let queued = store
        .conn
        .execute(QUEUE_ENROLLMENT, params![full_name, ts, lane.name()]);
    if let Err(e) = queued {
        eprintln!("queueing {full_name}: {e:#}");
    }
}

// ---------------------------------------------------------------- the manual lane

#[derive(Deserialize)]
struct Submission {
    repo: String,
}

/// What the site says back about a submitted repo.
pub enum Outcome {
    /// Whatever was typed is not a repo name at all.
    Malformed,
    Tracking {
        full_name: String,
        fresh: bool,
    },
    Queued {
        full_name: String,
    },
    /// No public repo by that name, or its maintainer opted out. One outcome for
    /// both, because an opted-out repo must not be distinguishable from a repo
    /// that was never there (ToS rule 4).
    Nothing {
        full_name: String,
    },
}

fn submit(state: &AppState, full_name: &str, now: i64) -> Outcome {
    let nothing = || Outcome::Nothing {
        full_name: full_name.to_string(),
    };
    let tracked = {
        let store = state.store();
        logged(full_name, find_repo(&store.conn, full_name))
    };
    match tracked {
        Some(Some(repo)) if repo.status == Status::OptedOut => nothing(),
        // Already ours: no budget spent, nothing asked of GitHub, nothing written.
        Some(Some(repo)) => Outcome::Tracking {
            full_name: repo.full_name,
            fresh: false,
        },
        Some(None) => match enroll(state, Lane::Manual, full_name, now) {
            Enrollment::Started(badge) => Outcome::Tracking {
                full_name: badge.full_name,
                fresh: true,
            },
            Enrollment::Queued => Outcome::Queued {
                full_name: full_name.to_string(),
            },
            Enrollment::Nothing => nothing(),
        },
        None => nothing(),
    }
}

/// `owner/name`, out of whatever somebody pasted into the box: the bare name, the
/// repo's URL, the URL of some page inside it, with or without the .git suffix.
fn repo_name(raw: &str) -> Option<String> {
    let s = raw.trim();
    let s = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.strip_prefix("www.").unwrap_or(s);
    let s = s.strip_prefix("github.com").unwrap_or(s);
    let mut parts = s.trim_matches('/').split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let name = name.strip_suffix(".git").unwrap_or(name);
    (valid_owner(owner) && valid_name(name)).then(|| format!("{owner}/{name}"))
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
        /// Every text response the server makes is UTF-8; the font is bytes, and
        /// the test that cares about it compares them.
        body: String,
        bytes: Vec<u8>,
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

        fn get(&self, uri: &str) -> Fetched {
            self.send(
                Request::builder()
                    .uri(uri)
                    .body(Body::empty())
                    .expect("building the request"),
            )
        }

        /// The site's form, shaped the way a browser posts it.
        fn post_form(&self, uri: &str, body: &str) -> Fetched {
            self.send(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(body.to_string()))
                    .expect("building the request"),
            )
        }

        /// One runtime per request: the harness owns a blocking HTTP client, which
        /// must be built and dropped outside any async context.
        fn send(&self, request: Request<Body>) -> Fetched {
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
                        body: String::from_utf8_lossy(&body).into_owned(),
                        bytes: body.to_vec(),
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

    /// The column headers on their own. The nav links to `/rankings` too, and a
    /// test about the head strip must not be answered by the masthead.
    fn head_strip(page: &str) -> &str {
        page.split(r#"<div class="row head">"#)
            .nth(1)
            .and_then(|rest| rest.split("</div>").next())
            .expect("the board prints a head strip")
    }

    /// One repo's row, wherever the page put it.
    fn row_of<'a>(page: &'a str, name: &str) -> &'a str {
        page.split(r#"<li class="row">"#)
            .filter_map(|piece| piece.split("</li>").next())
            .find(|row| row.contains(&format!(">{name}</a>")))
            .unwrap_or_else(|| panic!("the board prints a row for {name}"))
    }

    /// The number the percentile cell shows, stopping at the sr-only span that
    /// says what it is a percentile of.
    fn percentile_of_row(page: &str, name: &str) -> String {
        row_of(page, name)
            .split("top ")
            .nth(1)
            .and_then(|rest| rest.split('<').next())
            .unwrap_or_else(|| panic!("{name} states a percentile"))
            .to_string()
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
            assert!(h.state.spend_enrollment(Lane::Embed, &today));
        }
        assert!(!h.state.spend_enrollment(Lane::Embed, &today));
        // The lanes hold their own allowances; a busy README leaves the form alone.
        assert!(h.state.spend_enrollment(Lane::Manual, &today));

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
        assert!(h.state.spend_enrollment(Lane::Embed, "2099-01-01"));
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
    fn the_pages_are_html_and_the_fallback_is_plain_text() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        for uri in ["/", "/rankings"] {
            let page = h.get(uri);
            assert_eq!(page.status, StatusCode::OK, "{uri}");
            assert_eq!(page.content_type, "text/html; charset=utf-8", "{uri}");
            assert_eq!(page.cache_control, "public, max-age=300", "{uri}");
            // Attribution without affiliation, on both (ToS rule 5).
            assert!(page.body.contains("Source: GitHub public API"), "{uri}");
            assert!(page.body.contains("not affiliated with GitHub"), "{uri}");
            assert!(page.body.contains("snapshots through 20"), "{uri}");
            // Server-rendered, and it stays that way.
            assert!(!page.body.contains("<script"), "{uri}");
        }

        // The moat claim, in the wording the spec pins it to.
        let index = h.get("/").body;
        assert!(
            index.contains("only comprehensive, cross-ecosystem accumulating series"),
            "{index}"
        );
        assert!(index.contains(r#"action="/enroll""#), "{index}");
        assert!(index.contains("hello@afterglow.watch"), "{index}");
        // The badge examples are drawn for a repo that is really tracked.
        assert!(index.contains(r#"src="/badge/o/r""#), "{index}");

        // The font is ours to serve, so no third party learns who reads the page.
        let font = h.get(FONT_URL);
        assert_eq!(font.status, StatusCode::OK);
        assert_eq!(font.content_type, "font/woff2");
        assert_eq!(font.cache_control, "public, max-age=31536000, immutable");
        assert_eq!(font.bytes, MONA_SANS);
        assert!(index.contains(FONT_URL), "{index}");

        let missing = h.get("/favicon.ico");
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(missing.content_type, "text/plain; charset=utf-8");
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn a_row_prints_the_number_the_card_prints() {
        let h = harness(vec![]);
        for (id, per_day) in [(1, 400), (2, 300), (3, 200), (4, 100)] {
            h.track(id, &format!("o/r{id}"), 24 * 30, None);
            h.snapshot(id, 24, 1_000);
            h.snapshot(id, 0, 1_000 + per_day);
        }

        let card = h.get("/badge/o/r2?style=card").body;
        let percentile = card
            .split("top ")
            .nth(1)
            .and_then(|rest| rest.split(" velocity").next())
            .expect("the card states a percentile");
        assert_eq!(percentile, "50%");

        let board = h.get("/rankings").body;
        // Byte for byte the card's percentile, from the same pool and the same
        // rounding, which is the whole reason the page exists.
        assert!(
            board.contains(&format!(
                r#"<span class="c-pct num">top {percentile}<span class="sr-only"> velocity</span></span>"#
            )),
            "{board}"
        );
        assert!(
            board.contains(r#"aria-hidden="true">▲ </span>300/day"#),
            "{board}"
        );
        // Ranked by the number, fastest first.
        let rank = |name: &str| board.find(name).expect(name);
        assert!(rank("o/r1") < rank("o/r2"));
        assert!(rank("o/r2") < rank("o/r3"));
        assert!(rank("o/r3") < rank("o/r4"));
        // A measured row earns its gold: minispark, and the fade under it.
        assert!(board.contains(r##"fill="url(#glow)""##), "{board}");
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn a_slow_repo_is_on_the_board_to_be_checked_against() {
        let h = harness(vec![]);
        // More repos than the board ever used to print.
        for id in 1..=130 {
            h.track(id, &format!("o/r{id:03}"), 24 * 30, None);
            h.snapshot(id, 24, 1_000);
            h.snapshot(id, 0, 1_000 + (131 - id));
        }

        // Rank 110 of 130, nowhere near a leaderboard, and its card still claims a
        // percentile that has to be checkable somewhere.
        let card = h.get("/badge/o/r110?style=card").body;
        let percentile = card
            .split("top ")
            .nth(1)
            .and_then(|rest| rest.split(" velocity").next())
            .expect("the card states a percentile");
        assert_eq!(percentile, "85%");

        let board = h.get("/rankings").body;
        let row = board
            .split(">o/r110</a>")
            .nth(1)
            .and_then(|rest| rest.split("c-repo").next())
            .expect("the board prints the row");
        assert!(
            row.contains(&format!(
                r#"top {percentile}<span class="sr-only"> velocity</span>"#
            )),
            "{row}"
        );
        // The slowest repo of all is down there too.
        assert!(board.contains(">o/r130</a>"), "{board}");
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn stars_is_the_same_board_in_another_order() {
        let h = harness(vec![]);
        // Stars and velocity disagree row for row here, so an order is tellable.
        for (id, name, count, per_day) in [
            (1, "o/steady", 9_000, 100),
            (2, "o/big", 5_000, 200),
            (3, "o/rocket", 1_000, 400),
        ] {
            h.track(id, name, 24 * 30, None);
            h.snapshot(id, 24, count - per_day);
            h.snapshot(id, 0, count);
        }
        let names = ["o/steady", "o/big", "o/rocket"];
        let order = |page: &str| {
            let mut seen = names;
            seen.sort_by_key(|n| page.find(&format!(">{n}</a>")).expect(n));
            seen
        };

        let velocity = h.get("/rankings");
        let stars = h.get("/rankings?sort=stars");
        assert_eq!(stars.status, StatusCode::OK);
        // Two URLs, one page, each cacheable on its own.
        assert_eq!(stars.cache_control, "public, max-age=300");
        assert_eq!(order(&velocity.body), ["o/rocket", "o/big", "o/steady"]);
        assert_eq!(order(&stars.body), ["o/steady", "o/big", "o/rocket"]);

        // Anything but the exact value is the default board, and unknown params
        // are ignored the way the badge URLs ignore them.
        for uri in ["/rankings?sort=STARS", "/rankings?sort=", "/rankings?x=1"] {
            assert_eq!(order(&h.get(uri).body), order(&velocity.body), "{uri}");
        }

        // The percentile still means velocity in the stars view, row for row.
        for name in names {
            assert_eq!(
                percentile_of_row(&velocity.body, name),
                percentile_of_row(&stars.body, name),
                "{name}"
            );
            assert_eq!(row_of(&velocity.body, name), row_of(&stars.body, name));
        }
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn the_head_strip_is_the_way_to_the_other_order() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");
        let default = h.get("/rankings").body;
        let stars = h.get("/rankings?sort=stars").body;

        let head = head_strip(&default);
        assert!(
            head.contains(r#"<a href="/rankings?sort=stars" aria-label="sort by stars">stars</a>"#),
            "{head}"
        );
        // The order in force is not a link to itself.
        assert!(!head.contains(">velocity</a>"), "{head}");

        let head = head_strip(&stars);
        assert!(
            head.contains(r#"<a href="/rankings" aria-label="sort by velocity">velocity</a>"#),
            "{head}"
        );
        assert!(!head.contains(">stars</a>"), "{head}");

        // The rank column is a CSS counter, so its header exists only to be read
        // aloud, and it stays a grid item so the columns do not shift.
        for head in [head_strip(&default), head_strip(&stars)] {
            assert!(
                head.starts_with(r#"<span><span class="sr-only">rank</span></span>"#),
                "{head}"
            );
        }
    }

    #[test]
    fn the_nav_says_where_we_are_and_the_wordmark_only_goes_home() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        let home = r#"<a href="/" aria-current="page">home</a><a href="/rankings">rankings</a>"#;
        let board = r#"<a href="/">home</a><a href="/rankings" aria-current="page">rankings</a>"#;
        for (uri, nav) in [
            ("/", home),
            ("/rankings", board),
            ("/rankings?sort=stars", board),
        ] {
            let page = h.get(uri).body;
            assert!(page.contains(&format!("<nav>{nav}</nav>")), "{uri}: {page}");
            // The wordmark is the way home and says nothing about where we are.
            assert!(page.contains(r#"<a class="wordmark" href="/">"#), "{uri}");
        }
    }

    #[test]
    fn a_decline_keeps_the_glyph_and_loses_the_green() {
        let h = harness(vec![]);
        measured(&h, 1, "o/rising");
        h.track(2, "o/falling", 24 * 30, None);
        h.snapshot(2, 25, 5_000);
        h.snapshot(2, 1, 4_900);
        // Young, one reading: a lifetime average and nothing measured.
        h.track(3, "o/young", 24, Some(24 * 10));
        h.snapshot(3, 1, 300);
        // Old, one reading: no honest number at all, so it has no row.
        h.track(4, "o/quiet", 24 * 40, Some(24 * 400));
        h.snapshot(4, 1, 5_000);

        let board = h.get("/rankings").body;
        assert!(
            board.contains(
                r#"<span class="num drop"><span class="sr-only">losing </span><span aria-hidden="true">▼ </span>100/day</span>"#
            ),
            "{board}"
        );
        assert!(
            board.contains(
                r#"<span class="num gain"><span class="sr-only">gaining </span><span aria-hidden="true">▲ </span>100/day</span>"#
            ),
            "{board}"
        );
        // Proxy rows interleave on their number, muted, with no percentile and no
        // spark: glow is earned.
        assert!(
            board.contains(r#"<span class="num proxy">~30 avg</span>"#),
            "{board}"
        );
        assert!(!board.contains("o/quiet"), "{board}");
    }

    #[test]
    fn a_measured_row_says_out_loud_what_the_layout_shows() {
        let h = harness(vec![]);
        measured(&h, 1, "o/rising");
        // Young, one reading: the proxy cell already says "avg" in its own text.
        h.track(2, "o/young", 24, Some(24 * 10));
        h.snapshot(2, 1, 300);

        let board = h.get("/rankings").body;
        let row = row_of(&board, "o/rising");
        // The count is naked without this: the star is an octicon and aria-hidden.
        assert!(
            row.contains(r#"1,100<span class="sr-only"> stars</span>"#),
            "{row}"
        );
        // The glyph is decoration and the word is what carries it, so the row a
        // screen reader hears is "gaining 100/day", the phrase the pill uses.
        assert!(
            row.contains(
                r#"<span class="sr-only">gaining </span><span aria-hidden="true">▲ </span>100/day"#
            ),
            "{row}"
        );
        // The percentile is the same claim the card's line makes, so it names the
        // number it is a percentile of.
        assert!(
            row.contains(r#"top 100%<span class="sr-only"> velocity</span>"#),
            "{row}"
        );

        // A proxy row reads as it always did, glyphless and unranked.
        let young = row_of(&board, "o/young");
        assert!(
            young.contains(r#"<span class="num proxy">~30 avg</span>"#),
            "{young}"
        );
        assert!(!young.contains("gaining"), "{young}");
    }

    #[test]
    fn an_opted_out_repo_reads_as_never_tracked() {
        let h = harness(vec![]);
        measured(&h, 1, "o/kept");
        h.track_as(2, "o/gone", 24 * 30, None, "opted_out");
        h.snapshot(2, 25, 1_000);
        h.snapshot(2, 1, 99_000);

        // Not paused: paused says we are still watching and cannot see.
        for uri in ["/badge/o/gone", "/badge/o/gone?style=card"] {
            let badge = h.get(uri);
            assert!(badge.body.contains("not tracked"), "{uri}: {}", badge.body);
            assert!(!badge.body.contains("tracking paused"), "{uri}");
            assert_eq!(badge.cache_control, "public, max-age=300", "{uri}");
        }

        // Gone from the board, and gone from the pool the percentile is against.
        let board = h.get("/rankings").body;
        assert!(!board.contains("o/gone"), "{board}");
        assert!(board.contains("o/kept"), "{board}");
        assert!(h.get("/badge/o/kept?style=card").body.contains("top 100%"));

        // The series it already has stays where it is.
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM snapshots WHERE repo_id = 2"),
            2
        );
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn the_form_enrolls_once_and_then_has_nothing_to_do() {
        let created = iso8601_utc(now_unix() - 400 * 24 * HOUR);
        let h = harness(vec![(200, repo_json(42, "Owner/Repo", 1_234, &created))]);

        let first = h.post_form("/enroll", "repo=https%3A%2F%2Fgithub.com%2Fowner%2Frepo");
        assert_eq!(first.status, StatusCode::OK);
        assert_eq!(first.content_type, "text/html; charset=utf-8");
        // One submission, one answer, belonging to nobody else.
        assert_eq!(first.cache_control, "no-store");
        assert!(first.body.contains("Tracking Owner/Repo"), "{}", first.body);
        assert_eq!(h.calls(), 1);
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM repos WHERE lane = 'manual'"),
            1
        );
        assert_eq!(
            h.scalar("SELECT stars FROM snapshots WHERE repo_id = 42"),
            1_234
        );

        // Submitting it again costs nothing: no call, no row, no second budget.
        let again = h.post_form("/enroll", "repo=owner%2Frepo");
        assert!(again.body.contains("is already tracked"), "{}", again.body);
        assert_eq!(h.calls(), 1);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 1);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM snapshots"), 1);

        // Junk in the box is answered, not enrolled.
        let junk = h.post_form("/enroll", "repo=not+a+repo");
        assert!(junk.body.contains("not a repo name"), "{}", junk.body);
        assert_eq!(junk.cache_control, "no-store");
        assert_eq!(h.calls(), 1);
    }

    #[test]
    fn a_spent_manual_budget_queues_the_submission() {
        let h = harness(vec![(
            200,
            repo_json(42, "owner/repo", 5, "2026-01-01T00:00:00Z"),
        )]);
        let today = date_utc(h.now);
        for _ in 0..MANUAL_DAILY_BUDGET {
            assert!(h.state.spend_enrollment(Lane::Manual, &today));
        }

        let got = h.post_form("/enroll", "repo=owner/repo");
        assert!(got.body.contains("is queued"), "{}", got.body);
        assert!(got.body.contains("nothing is dropped"), "{}", got.body);
        assert_eq!(h.queued(), ["owner/repo"]);
        // Which lane asked rides along, so the drain can record it on the repo.
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM enroll_queue WHERE lane = 'manual'"),
            1
        );
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 0);
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn the_form_will_not_re_enroll_a_repo_that_opted_out() {
        let h = harness(vec![]);
        h.track_as(7, "o/gone", 24 * 30, None, "opted_out");

        let got = h.post_form("/enroll", "repo=o/gone");
        // The same answer an unknown repo gets: opting out is not a status anyone
        // can read off the site.
        assert!(got.body.contains("Nothing to track for"), "{}", got.body);
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM repos WHERE status = 'opted_out'"),
            1
        );
        assert_eq!(h.calls(), 0);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM enroll_queue"), 0);
    }

    /// A rename is the one way an opted-out repo reaches the enrol path at all:
    /// the store's name is stale, so only the numeric id gives it away.
    #[test]
    fn a_renamed_opt_out_is_caught_by_its_id() {
        let h = harness(vec![(
            200,
            repo_json(7, "o/renamed", 900, "2026-01-01T00:00:00Z"),
        )]);
        h.track_as(7, "o/old-name", 24 * 30, None, "opted_out");

        let got = h.get("/badge/o/renamed");
        assert!(got.body.contains("not tracked"), "{}", got.body);
        assert_eq!(h.calls(), 1);
        // Asked about, and still not written down.
        assert_eq!(h.scalar("SELECT COUNT(*) FROM snapshots"), 0);
        assert_eq!(
            h.scalar("SELECT COUNT(*) FROM repos WHERE full_name = 'o/old-name'"),
            1
        );
    }

    #[test]
    fn a_pasted_url_is_still_a_repo_name() {
        for raw in [
            "owner/repo",
            "  owner/repo  ",
            "owner/repo/",
            "https://github.com/owner/repo",
            "http://github.com/owner/repo",
            "https://www.github.com/owner/repo.git",
            "github.com/owner/repo",
            "https://github.com/owner/repo/tree/main",
        ] {
            assert_eq!(repo_name(raw).as_deref(), Some("owner/repo"), "{raw}");
        }
        for raw in [
            "",
            "owner",
            "owner/",
            "/repo",
            "not a repo",
            "o/a$b",
            "https://github.com/",
        ] {
            assert_eq!(repo_name(raw), None, "{raw}");
        }
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
