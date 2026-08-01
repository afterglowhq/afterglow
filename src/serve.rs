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
use axum::middleware::map_response;
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
/// The least a board-scaled spark may rise, so movement never draws as none.
const SPARK_MIN_RISE: f64 = 1.0;

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

/// The identity mark, as the tab icon. Straight from assets/ rather than a copy
/// under static/, so the favicon cannot drift from the avatar it is. The mark
/// carries its own dark canvas, so one render answers both themes.
const ICON_SVG: &[u8] = include_bytes!("../assets/afterglow-avatar.svg");
const ICON_PNG: &[u8] = include_bytes!("../assets/afterglow-avatar-96.png");
const TOUCH_ICON: &[u8] = include_bytes!("../assets/afterglow-avatar-512.png");
/// Served under /static/, which is the prefix the edge cache rule covers.
pub const ICON_SVG_URL: &str = "/static/favicon.svg";
pub const ICON_PNG_URL: &str = "/static/favicon-96.png";
pub const TOUCH_ICON_URL: &str = "/static/apple-touch-icon.png";
/// A tab icon moves when the brand does, which is not on any schedule.
const ICON_CACHE: &str = "public, max-age=86400";

/// April 2025 replayed through the rankings: the board fully enriched from the
/// archived public record, rendered once, checked in, and served verbatim. The
/// page's whole claim is that it does not move, so no handler rebuilds it.
const APRIL_2025: &str = include_str!("../static/april-2025.html");
pub const APRIL_2025_URL: &str = "/april-2025";

/// Asks AI crawlers to keep off the rankings; the rest of the site is theirs
/// to read. Advisory by nature; anything stricter is a Cloudflare rule in the
/// dashboard, not here.
const ROBOTS: &str = include_str!("../static/robots.txt");

/// Two years of HTTPS-only, and deliberately no `preload` token: that token is a
/// claim that the domain is submitted to the browser preload list, and it is not.
const HSTS: &str = "max-age=63072000; includeSubDomains";

/// What a page actually loads: its own images, its own font, the stylesheet that
/// ships inline in a `<style>` element, and a form that posts back to us. No
/// script anywhere, so `default-src 'none'` is the floor everything else sits on.
const PAGE_CSP: &str = "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; \
     font-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";

/// Everything else we serve fetches nothing: the badge SVGs carry no `<style>`
/// element and no `style` attribute, and the font, the icons and the 404 are bytes.
const STRICT_CSP: &str = "default-src 'none'";

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
SELECT s.repo_id, r.full_name, r.created_at, r.language, r.lane, s.ts, s.stars
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
        .route(APRIL_2025_URL, get(april_2025))
        .route("/enroll", post(enroll_form))
        .route("/badge/{owner}/{repo}", get(canonical_badge))
        .route("/svg", get(compat_badge))
        .route(FONT_URL, get(font))
        .route(
            ICON_SVG_URL,
            get(|| async { icon(ICON_SVG, "image/svg+xml") }),
        )
        .route(ICON_PNG_URL, get(|| async { icon(ICON_PNG, "image/png") }))
        .route(
            TOUCH_ICON_URL,
            get(|| async { icon(TOUCH_ICON, "image/png") }),
        )
        .route("/robots.txt", get(robots))
        .fallback(missing)
        .layer(map_response(secure))
        .with_state(state)
}

/// The security headers, on everything the router answers with.
///
/// At the origin rather than at the edge, so they travel with the app, survive a
/// CDN change, and can be held to by a test. Only the CSP and the framing rule
/// differ by kind, and both read the content type the handler already set.
async fn secure(mut response: Response) -> Response {
    let page = response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|kind| kind.as_bytes().starts_with(b"text/html"));
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::STRICT_TRANSPORT_SECURITY,
        HeaderValue::from_static(HSTS),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(if page { PAGE_CSP } else { STRICT_CSP }),
    );
    if page {
        // Pages only. A badge is embedded cross-origin as an image by design, and
        // while this header does not govern `<img>`, keeping it off that path
        // leaves nothing to wonder about.
        headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    response
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

/// The replay is one static string, so its handler is the font's, in HTML.
async fn april_2025() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(PAGE_CACHE)),
        ],
        APRIL_2025,
    )
        .into_response()
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

fn icon(bytes: &'static [u8], mime: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CACHE_CONTROL, HeaderValue::from_static(ICON_CACHE)),
        ],
        bytes,
    )
        .into_response()
}

async fn robots() -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static(ICON_CACHE)),
        ],
        ROBOTS,
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
    // An empty entry is not the repo anybody meant, so a leading comma falls
    // through to the next one rather than drawing a badge about nothing.
    let first = q
        .get("repos")
        .and_then(|repos| repos.split(',').map(str::trim).find(|r| !r.is_empty()))
        .unwrap_or_default()
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
    // A tracked repo's name came back from GitHub and is already legal; the one in
    // the URL is whatever was typed at us, and only the untracked badge prints it.
    let shown = display_name(&name);
    let (svg, max_age) = match (&badge, shape) {
        (Some(b), Shape::Pill) => (badge::pill(b), max_age_for(b.state)),
        (Some(b), Shape::Card(theme)) => (badge::card(b, theme), max_age_for(b.state)),
        (None, Shape::Pill) => (badge::not_tracked_pill(&shown), FRESH_MAX_AGE),
        (None, Shape::Card(theme)) => (badge::not_tracked_card(&shown, theme), FRESH_MAX_AGE),
    };
    svg_response(svg, max_age)
}

/// GitHub's charset, applied to a name we are about to print rather than look up.
///
/// `resolve` refuses an illegal name before it reaches GitHub or the store, but
/// the refused name still reaches the SVG, and a URL can carry bidi controls or
/// C0 bytes that reorder the line they land in. Escaping keeps the markup intact;
/// this keeps the display intact. A merely wrong ASCII name survives unchanged,
/// because reading it back is how a maintainer finds the typo.
fn display_name(full_name: &str) -> String {
    let keep = |s: &str, extra: &str| -> String {
        s.chars()
            .filter(|&c| c.is_ascii_alphanumeric() || extra.contains(c))
            .collect()
    };
    // No separator means there is no owner segment to tell apart, so what is left
    // is held to the looser of the two charsets.
    let (owner, name) = full_name.split_once('/').unwrap_or(("", full_name));
    let (owner, name) = (keep(owner, "-"), keep(name, "-._"));
    if owner.is_empty() || name.is_empty() {
        // One empty segment leaves no separator behind, and two leave nothing.
        owner + &name
    } else {
        format!("{owner}/{name}")
    }
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
    language: Option<String>,
    /// How the repo entered (ADR-002); requested lanes sit on the board at any size.
    lane: String,
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
            ts: row.get(5)?,
            stars: row.get(6)?,
        };
        match out.last_mut() {
            Some(series) if series.id == id => series.readings.push(reading),
            _ => out.push(Series {
                id,
                full_name: row.get(1)?,
                created_at: row.get(2)?,
                language: row.get(3)?,
                lane: row.get(4)?,
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
    Ok(spark_points(&readings, now, &CARD_SPARK, None))
}

/// One point a day inside the window: the last reading of each UTC day.
fn daily_points(readings: &[Reading], start: i64) -> Vec<(i64, i64)> {
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
    daily
}

/// How far a series moved inside the window, for a board's shared scale.
fn spark_range(readings: &[Reading], now: i64) -> i64 {
    let daily = daily_points(readings, now - SPARK_DAYS * SECONDS_PER_DAY);
    if daily.len() < 2 {
        return 0;
    }
    let (min, max) = daily
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &(_, s)| {
            (lo.min(s), hi.max(s))
        });
    max - min
}

/// The last 30 days of a series as points in a region. x is real time, so a young
/// series occupies only the right and the empty left is the whole point.
///
/// `shared_range` is the largest 30-day movement on the board this series sits
/// on: every row is drawn in that row's stars-per-pixel, so a steeper line is a
/// faster repo, comparable row to row. A series drawn alone (the card) passes
/// None and takes the whole region, because a lone spark has nothing to be
/// comparable with and its magnitude is printed beside it.
fn spark_points(
    readings: &[Reading],
    now: i64,
    region: &Region,
    shared_range: Option<i64>,
) -> Vec<(f64, f64)> {
    let start = now - SPARK_DAYS * SECONDS_PER_DAY;
    let daily = daily_points(readings, start);
    if daily.len() < 2 {
        return Vec::new();
    }

    let (min, max) = daily
        .iter()
        .fold((i64::MAX, i64::MIN), |(lo, hi), &(_, s)| {
            (lo.min(s), hi.max(s))
        });
    let range = max - min;
    let height = region.bottom - region.top;
    // A scaled row keeps at least a visible tilt: dead flat stays the mark of a
    // series that did not move, never of one that moved less than the leader.
    let extent = match shared_range {
        Some(shared) if shared > range => {
            (height * range as f64 / shared as f64).max(SPARK_MIN_RISE)
        }
        _ => height,
    };
    let span = (now - start) as f64;
    daily
        .iter()
        .map(|&(at, stars)| {
            let x = ((at - start) as f64 / span * region.width).clamp(0.0, region.width);
            let y = if range == 0 {
                region.flat
            } else {
                region.bottom - (stars - min) as f64 / range as f64 * extent
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
    /// Linguist's call at last sight; None renders as nothing.
    pub language: Option<String>,
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
    /// What the row ranks on within its tier. A proxy average is the best claim
    /// a repo has, but it is a guess about a rate, not a rate somebody measured,
    /// so it never outranks one (ticket 024).
    fn per_day(&self) -> i64 {
        match *self {
            RowVelocity::Measured { per_day, .. } => per_day,
            RowVelocity::Proxy { avg } => avg,
        }
    }

    fn is_proxy(&self) -> bool {
        matches!(self, RowVelocity::Proxy { .. })
    }
}

/// The board shows the slice above this many stars; a repo somebody asked for
/// (the embed and manual lanes) sits on it at any size. A display line only,
/// not ADR-002's coverage floor: moving it changes what the page prints, never
/// what is collected. The percentile pool stays every tracked repo, so the
/// card's "all tracked repos" stays true (ticket 024).
const BOARD_FLOOR: i64 = 2000;

fn board(conn: &Connection, now: i64) -> Result<Board> {
    let series = recent_series(conn, now)?;
    let on_board = |s: &Series| {
        matches!(s.lane.as_str(), "embed" | "manual")
            || s.readings.last().is_some_and(|r| r.stars >= BOARD_FLOOR)
    };
    let measured: Vec<Option<i64>> = series
        .iter()
        .map(|s| series_velocity(&s.readings))
        .collect();
    let fleet: Vec<i64> = measured.iter().flatten().copied().collect();
    // The fastest mover sets one vertical scale for every minispark, so the
    // angle of a row means the same thing as the angle of the row above it.
    // Rows only: a sub-floor rocket the page never draws must not flatten it.
    let shared_range = series
        .iter()
        .zip(&measured)
        .filter(|(s, per_day)| per_day.is_some() && on_board(s))
        .map(|(s, _)| spark_range(&s.readings, now))
        .max();

    let mut through: Option<&str> = None;
    let mut rows: Vec<BoardRow> = Vec::new();
    for (s, per_day) in series.iter().zip(&measured) {
        let Some(latest) = s.readings.last() else {
            continue;
        };
        // Tracked wider than ranked: a scan repo under the floor feeds the
        // store and the percentile pool, not the page (ticket 024).
        if !on_board(s) {
            continue;
        }
        through = through.max(Some(latest.ts.as_str()));
        let (velocity, spark) = match *per_day {
            Some(per_day) => (
                RowVelocity::Measured {
                    per_day,
                    top_percent: percentile_of(&fleet, per_day),
                },
                spark_points(&s.readings, now, &MINI_SPARK, shared_range),
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
            language: s.language.clone(),
            stars: latest.stars,
            velocity,
            spark,
        });
    }

    // Measured rows first, whatever their rate: a repo enrolled this morning
    // does not top the board on a lifetime guess, it earns its rank tomorrow
    // when it measures.
    rows.sort_by(|a, b| {
        a.velocity
            .is_proxy()
            .cmp(&b.velocity.is_proxy())
            .then_with(|| b.velocity.per_day().cmp(&a.velocity.per_day()))
            .then_with(|| a.full_name.cmp(&b.full_name))
    });
    // Every row above the floor, not a top hundred: a card reading "top 80%
    // velocity" belongs to a repo no short list would ever print. SPEC §5's
    // checkability now stops at the floor: the pool counts repos the page
    // does not show, and the ledes say so out loud (ticket 024).
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
    use axum::http::{HeaderMap, Request};
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
        headers: HeaderMap,
        /// Every text response the server makes is UTF-8; the font is bytes, and
        /// the test that cares about it compares them.
        body: String,
        bytes: Vec<u8>,
    }

    impl Fetched {
        /// A header by name, or the empty string for one that is not there, so a
        /// test can state both what is set and what deliberately is not.
        fn header(&self, name: &str) -> &str {
            self.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
        }
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
        /// The embed lane, so fixtures sit on the board whatever their stars; the
        /// board-floor test seeds the scan lane itself.
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
            self.track_full(id, name, enrolled_ago, created_ago, status, "embed");
        }

        fn track_lane(&self, id: i64, name: &str, lane: &str) {
            self.track_full(id, name, 24 * 30, None, "active", lane);
        }

        fn track_full(
            &self,
            id: i64,
            name: &str,
            enrolled_ago: i64,
            created_ago: Option<i64>,
            status: &str,
            lane: &str,
        ) {
            self.state
                .store()
                .conn
                .execute(
                    "INSERT INTO repos (id, full_name, status, lane, enrolled_at, created_at)
                     VALUES (?1, ?2, ?3, ?6, ?4, ?5)",
                    params![
                        id,
                        name,
                        status,
                        iso8601_utc(self.now - enrolled_ago * HOUR),
                        created_ago.map(|h| iso8601_utc(self.now - h * HOUR)),
                        lane,
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
                    let headers = response.headers().clone();
                    let head = |name: &str| {
                        headers
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
                        headers,
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

    /// The sentence a badge speaks, which is also the one its `<title>` carries.
    fn aria_of(svg: &str) -> &str {
        svg.split(r#"aria-label=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("every badge states an aria-label")
    }

    #[test]
    fn an_untracked_name_prints_only_what_github_allows() {
        let h = harness(vec![]);
        // U+202E reorders everything after it; U+0001 is a control byte a terminal
        // or a reader may do anything at all with. Both arrive through the path and
        // through the compat query, and neither belongs in a rendered line.
        for uri in [
            "/badge/o%E2%80%AEwner/repo",
            "/badge/o%E2%80%AEwner/repo?style=card",
            "/badge/owner/re%01po?style=card",
            "/svg?repos=o%E2%80%AEwner/re%01po",
        ] {
            let got = h.get(uri);
            assert_eq!(got.status, StatusCode::OK, "{uri}");
            assert!(got.body.contains("not tracked"), "{uri}: {}", got.body);
            assert!(!got.body.contains('\u{202e}'), "{uri}: {}", got.body);
            assert!(!got.body.contains('\u{1}'), "{uri}: {}", got.body);
            // The aria-label and the title carry the same sentence, so both are
            // checked rather than trusting one to stand for the other.
            assert_eq!(aria_of(&got.body), "afterglow: owner/repo is not tracked");
            assert!(
                got.body
                    .contains("<title>afterglow: owner/repo is not tracked</title>"),
                "{uri}: {}",
                got.body
            );
        }

        // A name that is only wrong prints exactly as it was asked for, because
        // reading it back is how a maintainer finds the typo. This one is refused
        // for its length, so nothing about it reaches GitHub either.
        let long = "a".repeat(40);
        let got = h.get(&format!("/badge/{long}/clim_dg.rs"));
        assert_eq!(
            aria_of(&got.body),
            format!("afterglow: {long}/clim_dg.rs is not tracked")
        );
        assert_eq!(h.calls(), 0);
        assert_eq!(h.scalar("SELECT COUNT(*) FROM repos"), 0);
    }

    #[test]
    fn an_empty_repo_token_leaves_no_hole_in_the_badge() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        // Nothing named at all: still a badge, and one without a gap in its line.
        for uri in ["/svg?repos=", "/svg?repos=,", "/svg?repos=%20"] {
            let got = h.get(uri);
            assert_eq!(got.status, StatusCode::OK, "{uri}");
            assert!(got.body.contains("not tracked"), "{uri}: {}", got.body);
            assert_eq!(aria_of(&got.body), "afterglow: not tracked", "{uri}");
            assert!(
                got.body.contains("<title>afterglow: not tracked</title>"),
                "{uri}: {}",
                got.body
            );
        }

        // An empty owner segment leaves no separator behind either.
        let got = h.get("/badge//climdg");
        assert_eq!(aria_of(&got.body), "afterglow: climdg is not tracked");

        let got = h.get("/svg?repos=,o/r");
        assert!(got.body.contains(">o/r<"), "{}", got.body);
        assert_eq!(h.calls(), 0);
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
    fn robots_txt_turns_away_ai_crawlers_and_no_one_else() {
        let h = harness(vec![]);
        let got = h.get("/robots.txt");
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.content_type, "text/plain; charset=utf-8");
        assert_eq!(got.cache_control, ICON_CACHE);
        // Named AI bots share one disallow group scoped to the rankings, the
        // catch-all allow sits after. Crawlers match by most-specific agent,
        // not first-match; the order asserts just pin the file's shape.
        let block = got.body.find("User-agent: GPTBot").unwrap();
        let deny = got.body.find("Disallow: /rankings").unwrap();
        let open = got.body.find("User-agent: *\nAllow: /").unwrap();
        assert!(block < deny && deny < open, "{}", got.body);
        // The block never widens to the whole site by accident.
        assert!(!got.body.contains("Disallow: /\n"), "{}", got.body);
        assert!(got.body.contains("User-agent: ClaudeBot"), "{}", got.body);
        assert!(
            got.body.contains("User-agent: Google-Extended"),
            "{}",
            got.body
        );
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn every_response_carries_its_security_headers() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        let pages = ["/", "/rankings", APRIL_2025_URL];
        let badges = ["/badge/o/r", "/badge/o/r?style=card", "/svg?repos=o/r"];
        let rest = [
            FONT_URL,
            ICON_SVG_URL,
            ICON_PNG_URL,
            "/robots.txt",
            "/nowhere",
        ];
        for uri in pages.iter().chain(&badges).chain(&rest) {
            let got = h.get(uri);
            assert_eq!(got.header("x-content-type-options"), "nosniff", "{uri}");
            assert_eq!(got.header("referrer-policy"), "no-referrer", "{uri}");
            assert_eq!(
                got.header("strict-transport-security"),
                "max-age=63072000; includeSubDomains",
                "{uri}"
            );
        }

        for uri in pages {
            let page = h.get(uri);
            assert_eq!(
                page.header("content-security-policy"),
                "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; \
                 font-src 'self'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'",
                "{uri}"
            );
            assert_eq!(page.header("x-frame-options"), "DENY", "{uri}");
        }
        // The form's answer is a page too, however short-lived.
        let form = h.post_form("/enroll", "repo=not+a+repo");
        assert_eq!(form.header("x-frame-options"), "DENY");
        assert_eq!(form.header("x-content-type-options"), "nosniff");

        for uri in badges {
            let badge = h.get(uri);
            assert_eq!(
                badge.header("content-security-policy"),
                "default-src 'none'",
                "{uri}"
            );
            // Not on a badge: a README embeds it cross-origin as an image on
            // purpose, and the header has no business anywhere near that.
            assert_eq!(badge.header("x-frame-options"), "", "{uri}");
        }
        for uri in rest {
            assert_eq!(
                h.get(uri).header("content-security-policy"),
                "default-src 'none'",
                "{uri}"
            );
        }
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn the_replay_is_a_fixed_page_and_the_rankings_lede_points_to_it() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        let replay = h.get(APRIL_2025_URL);
        assert_eq!(replay.status, StatusCode::OK);
        assert_eq!(replay.content_type, "text/html; charset=utf-8");
        assert_eq!(replay.cache_control, PAGE_CACHE);
        assert!(replay.body.contains("Velocity, April 2025"));
        // Grey is the page's whole claim: neither theme's live gold appears.
        assert!(!replay.body.contains("#eac54f") && !replay.body.contains("#e3b341"));
        // Its font and icons are the site's own, inside the page CSP.
        assert!(replay.body.contains(FONT_URL) && replay.body.contains(ICON_SVG_URL));
        assert!(!replay.body.contains("data:"));

        let rankings = h.get("/rankings");
        assert!(
            rankings
                .body
                .contains(&format!("href=\"{APRIL_2025_URL}\""))
        );
        assert_eq!(h.calls(), 0);
    }

    #[test]
    fn the_mark_is_the_tab_icon_and_the_footer_says_where_we_are() {
        let h = harness(vec![]);
        measured(&h, 1, "o/r");

        for (uri, mime, bytes) in [
            (ICON_SVG_URL, "image/svg+xml", ICON_SVG),
            (ICON_PNG_URL, "image/png", ICON_PNG),
            (TOUCH_ICON_URL, "image/png", TOUCH_ICON),
        ] {
            let icon = h.get(uri);
            assert_eq!(icon.status, StatusCode::OK, "{uri}");
            assert_eq!(icon.content_type, mime, "{uri}");
            assert_eq!(icon.cache_control, "public, max-age=86400", "{uri}");
            // The bytes out of assets/, so the tab cannot show a stale mark.
            assert_eq!(icon.bytes, bytes, "{uri}");
            // Under /static/, which is what the edge cache rule matches on.
            assert!(uri.starts_with("/static/"), "{uri}");
        }

        for uri in ["/", "/rankings"] {
            let page = h.get(uri).body;
            // Safari ignores an SVG favicon, so the PNG is not a nicety.
            assert!(
                page.contains(
                    r#"<link rel="icon" type="image/svg+xml" href="/static/favicon.svg">"#
                ),
                "{uri}: {page}"
            );
            assert!(
                page.contains(
                    r#"<link rel="icon" type="image/png" sizes="96x96" href="/static/favicon-96.png">"#
                ),
                "{uri}: {page}"
            );
            assert!(
                page.contains(
                    r#"<link rel="apple-touch-icon" href="/static/apple-touch-icon.png">"#
                ),
                "{uri}: {page}"
            );
            // The footer is drawn by the shell, so both pages carry it.
            assert!(
                page.contains(
                    r#"<a href="https://github.com/afterglowhq/afterglow">source on GitHub</a> · <a href="https://x.com/afterglowwatch">@afterglowwatch on X</a> · <a href="https://bsky.app/profile/afterglow.watch">@afterglow.watch on Bluesky</a>"#
                ),
                "{uri}: {page}"
            );
        }
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

    /// The store is wider than the board (ticket 024): a scan repo under the
    /// floor gets no row but keeps its weight in every percentile, and a repo
    /// somebody asked for sits on the board at any size.
    #[test]
    fn the_board_floor_hides_scan_repos_but_not_their_pool_weight() {
        let h = harness(vec![]);
        // A scan repo above the floor: on the board.
        h.track_lane(1, "big/scan", "scan");
        h.snapshot(1, 24, 2_500);
        h.snapshot(1, 0, 2_600);
        // A scan repo under the floor, faster than everything: pool only.
        h.track_lane(2, "small/rocket", "scan");
        h.snapshot(2, 24, 500);
        h.snapshot(2, 0, 900);
        // An embed repo under the floor: somebody asked, so it has a row.
        h.track_lane(3, "small/asked", "embed");
        h.snapshot(3, 24, 100);
        h.snapshot(3, 0, 150);

        let board = h.get("/rankings").body;
        assert!(board.contains(">big/scan</a>"), "{board}");
        assert!(board.contains(">small/asked</a>"), "{board}");
        assert!(!board.contains("small/rocket"), "{board}");
        // Two of three in a pool the page only half shows: only the hidden
        // rocket's weight makes big/scan "top 67%" instead of top 50%.
        assert!(
            board.contains(r#"top 67%<span class="sr-only"> velocity</span>"#),
            "{board}"
        );
        // The rocket's card still measures, floor or no floor.
        let card = h.get("/badge/small/rocket?style=card").body;
        assert!(card.contains("top 33% velocity"), "{card}");
        assert_eq!(h.calls(), 0);
    }

    /// A lifetime average is a guess about a rate, not a rate somebody measured,
    /// so a repo enrolled this morning sits under every measured row however
    /// large its average. It earns its rank when it measures (ticket 024).
    #[test]
    fn a_proxy_row_never_tops_a_measured_one() {
        let h = harness(vec![]);
        // Measured and modest: 100 a day.
        measured(&h, 1, "old/steady");
        // Enrolled today, three days old, 9,000 stars: a 3,000-a-day average.
        h.track(2, "new/rocket", 0, Some(24 * 3));
        h.snapshot(2, 0, 9_000);

        let board = h.get("/rankings").body;
        let rank = |name: &str| board.find(name).expect(name);
        assert!(rank("old/steady") < rank("new/rocket"), "{board}");

        // The stars order stays star counts, no tiers.
        let stars = h.get("/rankings?sort=stars").body;
        let rank = |name: &str| stars.find(name).expect(name);
        assert!(rank("new/rocket") < rank("old/steady"), "{stars}");
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
    fn the_language_is_on_the_row_when_github_has_named_one() {
        let h = harness(vec![]);
        measured(&h, 1, "o/coded");
        measured(&h, 2, "o/prose");
        measured(&h, 3, "o/obscure");
        for (id, language) in [(1, "Rust"), (3, "Fable")] {
            h.state
                .store()
                .conn
                .execute(
                    "UPDATE repos SET language = ?2 WHERE id = ?1",
                    params![id, language],
                )
                .expect("naming a language");
        }

        let board = h.get("/rankings").body;
        // The dot wears Linguist's hex for the language, then the muted name.
        let coded = row_of(&board, "o/coded");
        assert!(coded.contains(r##"fill="#dea584""##), "{coded}");
        assert!(coded.contains(">Rust</span>"), "{coded}");
        // A language Linguist has no color for still gets its name, on the
        // neutral dot.
        let obscure = row_of(&board, "o/obscure");
        assert!(obscure.contains(r#"fill="var(--border)""#), "{obscure}");
        assert!(obscure.contains(">Fable</span>"), "{obscure}");
        // No verdict, no claim: the cell ends at the repo name.
        assert!(!row_of(&board, "o/prose").contains("c-lang"), "{board}");
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

    #[test]
    fn board_minisparks_share_one_vertical_scale() {
        let h = harness(vec![]);
        // Three measured repos over the same two days: rises of 100, 10, and 1.
        for (id, name, latest) in [(1, "o/fast", 300), (2, "o/slow", 210), (3, "o/crawl", 201)] {
            h.track(id, name, 24 * 40, None);
            h.snapshot(id, 48, 200);
            h.snapshot(id, 0, latest);
        }

        let store = h.state.store();
        let b = board(&store.conn, h.now).unwrap();
        let rise = |row: &BoardRow| {
            let ys = row.spark.iter().map(|&(_, y)| y);
            let rise = ys.clone().fold(f64::MIN, f64::max) - ys.fold(f64::MAX, f64::min);
            (rise * 10.0).round() / 10.0
        };
        assert_eq!(
            b.rows
                .iter()
                .map(|r| r.full_name.as_str())
                .collect::<Vec<_>>(),
            ["o/fast", "o/slow", "o/crawl"]
        );
        // The fastest row spans the region; the rest rise in its stars-per-pixel,
        // down to the floor that keeps movement from drawing as none.
        assert_eq!(rise(&b.rows[0]), 16.0);
        assert_eq!(rise(&b.rows[1]), 1.6);
        assert_eq!(rise(&b.rows[2]), SPARK_MIN_RISE);
        assert_eq!(h.calls(), 0);
    }
}
