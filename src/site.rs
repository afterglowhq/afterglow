//! The public HTML: the site page, the leaderboard, and what the form says back.
//!
//! Server-rendered, no client JS, one stylesheet inline. Every number on these
//! pages is formatted by the badge's own code, so a card's percentile and its row
//! on the leaderboard cannot disagree about the same repo.

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::badge::{self, STAR};
use crate::language;
use crate::serve::{
    Board, BoardRow, FONT_URL, ICON_PNG_URL, ICON_SVG_URL, MINI_SPARK, MINI_SPARK_HEIGHT, Outcome,
    RowVelocity, Sort, TOUCH_ICON_URL,
};

const HOST: &str = "https://afterglow.watch";
const CONTACT: &str = "hello@afterglow.watch";
const SOURCE_REPO: &str = "https://github.com/afterglowhq/afterglow";
const DESCRIPTION: &str = "Daily star snapshots for GitHub repos: a README badge with measured velocity, \
     and a public leaderboard.";

/// Mona Sans carries the display type and nothing else; body text is the reader's
/// own system stack, which is already the voice of every page they read code on.
///
/// The form's two controls take --muted for a border where everything else takes
/// --border. A control is an edge somebody has to find, and --border is a rule
/// between rows: quiet enough to sit under text, too quiet to be a boundary.
const CSS: &str = r#"
@font-face {
  font-family: "Mona Sans";
  src: url("/static/mona-sans.woff2") format("woff2");
  font-weight: 200 900;
  font-display: swap;
}
:root {
  color-scheme: light dark;
  --canvas: #ffffff;
  --canvas-subtle: #f6f8fa;
  --border: #d0d7de;
  --fg: #1f2328;
  --muted: #59636e;
  --accent: #0969da;
  --green: #1a7f37;
  --gold: #eac54f;
  --spark: #d4a72c;
  --body: -apple-system, BlinkMacSystemFont, "Segoe UI", "Noto Sans", Helvetica, Arial, sans-serif;
  --mono: ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: dark) {
  :root {
    --canvas: #0d1117;
    --canvas-subtle: #21262d;
    --border: #30363d;
    --fg: #e6edf3;
    --muted: #8b949e;
    --accent: #58a6ff;
    --green: #3fb950;
    --gold: #e3b341;
    --spark: #e3b341;
  }
}
*, *::before, *::after { box-sizing: border-box; }
body {
  margin: 0;
  background: var(--canvas);
  color: var(--fg);
  font: 1rem/1.6 var(--body);
  -webkit-text-size-adjust: 100%;
}
.page { max-width: 52rem; margin: 0 auto; padding: 24px 16px 48px; }
h1, h2, .wordmark { font-family: "Mona Sans", var(--body); font-weight: 600; letter-spacing: -0.011em; }
h1 { font-size: 2rem; line-height: 1.2; margin: 0 0 8px; }
h2 { font-size: 1.125rem; margin: 32px 0 8px; }
p { margin: 0 0 12px; }
a { color: var(--accent); text-decoration: none; }
a:hover { text-decoration: underline; }
:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  padding: 0;
  margin: -1px;
  overflow: hidden;
  clip: rect(0, 0, 0, 0);
  white-space: nowrap;
  border: 0;
}
.lede { color: var(--muted); font-size: 1.0625rem; margin-bottom: 24px; }
.note { color: var(--muted); font-size: 0.875rem; }
code { font-family: var(--mono); font-size: 0.875em; }
pre {
  font-family: var(--mono);
  font-size: 0.8125rem;
  background: var(--canvas-subtle);
  border: 1px solid var(--border);
  border-radius: 6px;
  padding: 12px;
  margin: 0 0 12px;
  overflow-x: auto;
}
.masthead {
  display: flex;
  align-items: baseline;
  justify-content: space-between;
  gap: 16px;
  padding-bottom: 16px;
  margin-bottom: 24px;
  border-bottom: 1px solid var(--border);
}
.wordmark { font-size: 1.125rem; color: var(--fg); }
nav { display: flex; gap: 16px; }
.mark { height: 1.25rem; width: auto; margin-right: 8px; vertical-align: -0.3em; }
.mark .line { fill: none; stroke: var(--spark); stroke-width: 22; stroke-linecap: round; stroke-linejoin: round; }
.mark .glyph { fill: var(--gold); }
.sprite { position: absolute; width: 0; height: 0; overflow: hidden; }
stop { stop-color: var(--spark); }
.row {
  display: grid;
  grid-template-columns: 2.5rem minmax(0, 1fr) 100px 5.5rem 7.5rem 5.5rem;
  align-items: center;
  gap: 12px;
}
.head {
  padding-bottom: 8px;
  color: var(--muted);
  font-size: 0.75rem;
  letter-spacing: 0.04em;
}
.head a { color: inherit; }
.board { list-style: none; margin: 0; padding: 0; counter-reset: rank; }
.board li { counter-increment: rank; padding: 10px 0; border-top: 1px solid var(--border); }
.board li::before {
  content: counter(rank);
  color: var(--muted);
  text-align: right;
  font-variant-numeric: tabular-nums;
}
.num { text-align: right; font-variant-numeric: tabular-nums lining-nums; }
.c-repo { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.c-lang { color: var(--muted); font-size: 0.8125rem; margin-left: 8px; white-space: nowrap; }
.c-lang .dot { width: 8px; height: 8px; margin-right: 4px; }
.c-stars { display: flex; align-items: center; justify-content: flex-end; gap: 4px; }
.star { width: 12px; height: 12px; fill: var(--gold); flex: none; }
.gain { color: var(--green); font-weight: 600; }
.drop { color: var(--fg); font-weight: 600; }
.proxy { color: var(--muted); }
.c-pct { color: var(--muted); font-size: 0.8125rem; }
.spark polyline { fill: none; stroke: var(--spark); stroke-width: 1.5; }
form { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; margin: 0 0 12px; }
label { flex-basis: 100%; margin-bottom: 4px; color: var(--muted); font-size: 0.875rem; }
input[type="text"] {
  flex: 1 1 18rem;
  min-width: 0;
  padding: 8px 12px;
  border: 1px solid var(--muted);
  border-radius: 6px;
  background: var(--canvas);
  color: var(--fg);
  font: inherit;
  font-size: 0.9375rem;
}
button {
  padding: 8px 16px;
  border: 1px solid var(--muted);
  border-radius: 6px;
  background: var(--canvas-subtle);
  color: var(--fg);
  font: inherit;
  font-size: 0.9375rem;
  font-weight: 600;
  cursor: pointer;
}
button:hover { border-color: var(--fg); }
.examples { display: flex; flex-wrap: wrap; align-items: flex-start; gap: 16px; margin: 16px 0; }
.examples img { max-width: 100%; height: auto; }
.attribution {
  margin-top: 48px;
  padding-top: 16px;
  border-top: 1px solid var(--border);
  color: var(--muted);
  font-size: 0.8125rem;
}
.attribution p { margin: 0 0 4px; }
.attribution a { color: inherit; }
@media (max-width: 40rem) {
  h1 { font-size: 1.625rem; }
  .row { grid-template-columns: 1.75rem minmax(0, 1fr) 4.75rem 6.25rem; gap: 8px; }
  .c-spark, .c-pct, .c-lang { display: none; }
}
"#;

pub fn index(board: &Board) -> Markup {
    let example = board.rows.first().map(|r| r.full_name.as_str());
    let slug = example.unwrap_or("OWNER/REPO");
    let body = html! {
        h1 { "Star velocity, measured daily." }
        p.lede {
            "Afterglow reads the public star count of every repo it tracks, once a day, and keeps "
            "every reading. The numbers come back as a README badge and as a leaderboard."
        }

        h2 { "What this is" }
        p {
            "GitHub restricted stargazer lists on 30 June 2026, and star history went with them. "
            "Aggregate counts are still public, so those are what we snapshot, daily, and a "
            "snapshot is never deleted."
        }
        p {
            "History cannot be backfilled. Nobody can go back and observe what a star count was "
            "last Tuesday, which is what makes this the only comprehensive, cross-ecosystem "
            "accumulating series, and makes every tracked day a day nobody can catch up on."
        }
        p {
            "Every number here carries its fidelity. A measured velocity comes from two readings "
            "at least half a day apart. Anything weaker is labeled and stays grey: a young repo "
            "shows its lifetime average marked " code { "~avg" } ", a repo on its first reading "
            "says it is still measuring. We would rather print less than print a number we would "
            "have to retract."
        }

        h2 { "Put it in your README" }
        p { "One URL. No token, and it works for repos you do not own." }
        pre { code { "![stars](" (HOST) "/badge/" (slug) ")" } }
        p {
            "Add " code { "?style=card" } " for the card with its 30-day sparkline, and "
            code { "?theme=dark" } " if the README is dark. The pill is one render everywhere, "
            "the way the rest of the badge row is."
        }
        @if let Some(name) = example {
            .examples {
                img src=(format!("/badge/{name}")) alt=(format!("afterglow pill for {name}")) height="20";
                // The card the way a themed README should ask for it, which is
                // also the pattern this page is documenting.
                picture {
                    source media="(prefers-color-scheme: dark)"
                        srcset=(format!("/badge/{name}?style=card&theme=dark"));
                    img src=(format!("/badge/{name}?style=card"))
                        alt=(format!("afterglow card for {name}")) width="420" height="150";
                }
            }
        }
        p.note {
            "The card above follows this page's theme: a "
            code { "<picture>" }
            " with a "
            code { "prefers-color-scheme" }
            " source, which is what a README wants if it has to read well in both."
        }
        p {
            "If your README still points at the dead star-history embed, edit the hostname and "
            "change nothing else:"
        }
        pre { code { (HOST) "/svg?repos=" (slug) "&type=Date" } }
        p {
            "A badge request for a repo we have not seen before is what starts it being tracked. "
            "The first render says tracking has just started, and the series builds from there."
        }
        p.note {
            "If your repo is mostly Markdown, a skills collection say, GitHub files it under "
            "whatever scripts it finds, or under nothing. "
            code { "*.md linguist-detectable" } " in " code { ".gitattributes" }
            " fixes the label, on your repo page and on our rankings both."
        }

        h2 { "Track a repo" }
        form method="post" action="/enroll" {
            label for="repo" { "A public repo, as owner/name or its GitHub URL" }
            input #repo type="text" name="repo" placeholder="owner/name"
                autocapitalize="off" autocorrect="off" spellcheck="false" required;
            button type="submit" { "Track it" }
        }
        p.note {
            "We enroll a fixed number of repos a day. Past that a submission goes into the queue "
            "and enrolls over the next days, rather than being dropped."
        }

        h2 { "The leaderboard" }
        p {
            "Every tracked repo we can measure, ranked by stars per day. A card's percentile line "
            "is checkable there: same pool, same arithmetic, same day. "
            a href="/rankings" { "Open the rankings" } "."
        }

        h2 { "Opting out" }
        p {
            "If you maintain a repo and you would rather it was not here, write to "
            a href=(format!("mailto:{CONTACT}")) { (CONTACT) }
            " from an address that ties you to the repo. A person checks it by hand. After that "
            "the badge reads not tracked and the repo is gone from the rankings."
        }
        p {
            "Readings already taken stay in the store, because nothing is ever deleted from it. "
            "Nothing further is collected, and nothing is shown."
        }
    };
    shell("Afterglow", "/", body, board.through.as_deref())
}

pub fn rankings(board: &Board, sort: Sort) -> Markup {
    let body = html! {
        h1 { "Velocity" }
        p.lede {
            "Every tracked repo we can measure, by stars per day. Measured rows carry a percentile "
            "against the whole tracked set. Grey rows are young repos ranked on a lifetime "
            "average, which is a weaker claim and looks like one."
        }
        @if board.rows.is_empty() {
            p {
                "Nothing measured yet. A velocity needs two readings at least half a day apart, "
                "so the board fills in as the fleet reports."
            }
        } @else {
            // The other ordering is the same board, so the way to it is the header
            // of the column it sorts on. The one in force is not a link to itself.
            .row.head {
                // The rank column is drawn by a CSS counter, so its header has
                // nothing to show and everything to say. The span stays a grid
                // item, because an sr-only one would leave the column.
                span { span.sr-only { "rank" } }
                span.c-repo { "repo" }
                span.c-spark { "30 days" }
                span.c-stars.num {
                    @match sort {
                        Sort::Velocity => {
                            a href="/rankings?sort=stars" aria-label="sort by stars" { "stars" }
                        }
                        Sort::Stars => { "stars" }
                    }
                }
                span.num {
                    @match sort {
                        Sort::Velocity => { "velocity" }
                        Sort::Stars => {
                            a href="/rankings" aria-label="sort by velocity" { "velocity" }
                        }
                    }
                }
                span.c-pct.num { "percentile" }
            }
            ol.board {
                @for row in &board.rows {
                    li.row { (board_row(row)) }
                }
            }
            p.note {
                "Ranks move week to week. The badge never prints one, because a number that "
                "swings is a number we would end up retracting."
            }
            p.note {
                "The language next to a repo is GitHub's call, and GitHub does not count "
                "Markdown. A mostly-Markdown repo shows whatever scripts are left, or nothing. "
                "Putting " code { "*.md linguist-detectable" } " in its " code { ".gitattributes" }
                " makes Markdown count, here and on the repo's own page."
            }
        }
    };
    shell(
        "Afterglow velocity rankings",
        "/rankings",
        body,
        board.through.as_deref(),
    )
}

pub fn enrolled(outcome: &Outcome) -> Markup {
    let body = match outcome {
        Outcome::Malformed => html! {
            h1 { "That is not a repo name" }
            p { "Use owner/name, or paste the repo's GitHub URL." }
        },
        Outcome::Tracking { full_name, fresh } => html! {
            h1 {
                @if *fresh { "Tracking " (full_name) } @else { (full_name) " is already tracked" }
            }
            @if *fresh {
                p {
                    "The first reading is in. Its badge says tracking started today, and a "
                    "measured velocity appears once a second reading lands at least half a day "
                    "later."
                }
            } @else {
                p { "Nothing to do, then. Here is its badge as it stands." }
            }
            .examples {
                img src=(format!("/badge/{full_name}")) alt=(format!("afterglow pill for {full_name}")) height="20";
            }
            pre { code { "![stars](" (HOST) "/badge/" (full_name) ")" } }
        },
        // Queued covers a spent budget and a GitHub we could not reach, so the
        // wording claims neither.
        Outcome::Queued { full_name } => html! {
            h1 { (full_name) " is queued" }
            p {
                "We could not enroll it just now, so its name is parked. The queue is worked "
                "through over the next days, in the order it was asked for, and nothing is "
                "dropped out of it."
            }
        },
        Outcome::Nothing { full_name } => html! {
            h1 { "Nothing to track for " (full_name) }
            p {
                "Either there is no public repo by that name, or its maintainer asked us not to "
                "track it."
            }
        },
    };
    shell(
        "Afterglow",
        "",
        html! { (body) p { a href="/" { "Back to afterglow.watch" } } },
        None,
    )
}

fn shell(title: &str, here: &str, body: Markup, through: Option<&str>) -> Markup {
    let current = |path: &str| (here == path).then_some("page");
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                meta name="description" content=(DESCRIPTION);
                link rel="preload" href=(FONT_URL) as="font" type="font/woff2" crossorigin;
                // Safari ignores an SVG favicon, so the PNG is the one it uses.
                link rel="icon" type="image/svg+xml" href=(ICON_SVG_URL);
                link rel="icon" type="image/png" sizes="96x96" href=(ICON_PNG_URL);
                link rel="apple-touch-icon" href=(TOUCH_ICON_URL);
                style { (PreEscaped(CSS)) }
            }
            body {
                (sprite())
                .page {
                    header.masthead {
                        // The nav says where we are. The wordmark is the way home and
                        // nothing else, or the page announces itself twice.
                        a.wordmark href="/" { (mark()) "afterglow" }
                        nav {
                            a href="/" aria-current=[current("/")] { "home" }
                            a href="/rankings" aria-current=[current("/rankings")] { "rankings" }
                        }
                    }
                    main { (body) }
                    (attribution(through))
                }
            }
        }
    }
}

/// The identity mark: the sparkline rising into the star
/// through its bottom notch. Same geometry as assets/afterglow-avatar.svg, minus
/// the canvas, themed via --gold/--spark and the shared #glow gradient.
fn mark() -> Markup {
    html! {
        svg.mark viewBox="93 155 325 265" aria-hidden="true" {
            path.fade d="M93 344 L104 344 L184 312 L248 328 L358 211 L358 430 L93 430 Z" fill="url(#glow)" {}
            path.line d="M104 344 L184 312 L248 328 L328 243" {}
            g transform="translate(368,200) rotate(43.2) scale(8) translate(-8,-7.6)" { path.glyph d=(STAR) {} }
        }
    }
}

/// The star and the afterglow fade, defined once and pointed at from every row.
fn sprite() -> Markup {
    html! {
        svg.sprite aria-hidden="true" width="0" height="0" {
            defs {
                linearGradient #glow x1="0" y1="0" x2="0" y2="1" {
                    stop offset="0" stop-opacity=".3" {}
                    stop offset="1" stop-opacity="0" {}
                }
                symbol #star viewBox="0 0 16 16" { path d=(STAR) {} }
            }
        }
    }
}

fn board_row(row: &BoardRow) -> Markup {
    html! {
        span.c-repo {
            a href=(format!("https://github.com/{}", row.full_name)) { (row.full_name) }
            // GitHub's call, not ours, and it shares the repo cell rather than
            // taking a column: information, never an ordering. The dot wears the
            // hex Linguist gives the language, the same one GitHub's bar shows;
            // fill is an attribute, so the CSP never notices.
            @if let Some(name) = &row.language {
                span.c-lang {
                    svg.dot aria-hidden="true" viewBox="0 0 8 8" {
                        circle cx="4" cy="4" r="4"
                            fill=(language::color(name).unwrap_or("var(--border)")) {}
                    }
                    (name)
                }
            }
        }
        (minispark(&row.spark))
        // Every unit on the row is drawn as a glyph or a column header, which a
        // screen reader reading one cell has neither of. The sr-only words are
        // what the eye already gets from the layout.
        span.c-stars.num {
            svg.star aria-hidden="true" viewBox="0 0 16 16" { use href="#star" {} }
            (badge::commas(row.stars))
            span.sr-only { " stars" }
        }
        @match row.velocity {
            // Green is the mark of measured gain. A decline is a plain foreground
            // fact, and a proxy average never glows at all.
            RowVelocity::Measured { per_day, .. } => {
                span.num.gain[per_day >= 0].drop[per_day < 0] {
                    span.sr-only { (badge::direction(per_day)) " " }
                    span aria-hidden="true" { (badge::arrow(per_day)) " " }
                    (badge::rate(per_day))
                }
            }
            RowVelocity::Proxy { avg } => span.num.proxy { (badge::proxy_avg(avg)) }
        }
        @match row.velocity {
            RowVelocity::Measured { top_percent, .. } => {
                span.c-pct.num {
                    "top " (badge::percent(top_percent)) "%"
                    span.sr-only { " velocity" }
                }
            }
            RowVelocity::Proxy { .. } => span.c-pct {}
        }
    }
}

/// The row's 30 days, drawn the way the card draws them, one order smaller.
fn minispark(points: &[(f64, f64)]) -> Markup {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return html! { span.c-spark {} };
    };
    let plot: Vec<String> = points
        .iter()
        .map(|&(x, y)| format!("{},{}", badge::coord(x), badge::coord(y)))
        .collect();
    let area = format!(
        "M{} L{},{h} L{},{h} Z",
        plot.join(" L"),
        badge::coord(last.0),
        badge::coord(first.0),
        h = badge::coord(MINI_SPARK_HEIGHT),
    );
    html! {
        svg.c-spark.spark aria-hidden="true"
            width=(MINI_SPARK.width) height=(MINI_SPARK_HEIGHT)
            viewBox=(format!("0 0 {} {}", MINI_SPARK.width, MINI_SPARK_HEIGHT)) {
            path d=(area) fill="url(#glow)" {}
            polyline points=(plot.join(" ")) {}
        }
    }
}

/// Attribution without affiliation, plus the moment the page's numbers are from
/// (ToS rule 5). No GitHub mark, no borrowed trade dress.
fn attribution(through: Option<&str>) -> Markup {
    html! {
        footer.attribution {
            p {
                "Source: GitHub public API"
                @if let Some(stamp) = through.and_then(stamp) {
                    " · snapshots through " (stamp)
                }
            }
            p { "Afterglow is an independent project and is not affiliated with GitHub." }
            p {
                a href=(SOURCE_REPO) { "source on GitHub" }
                " · "
                a href="https://x.com/afterglowwatch" { "@afterglowwatch on X" }
                " · "
                a href="https://bsky.app/profile/afterglow.watch" { "@afterglow.watch on Bluesky" }
            }
        }
    }
}

/// `2026-07-31T05:31:47Z` as `2026-07-31 05:31 UTC`.
fn stamp(ts: &str) -> Option<String> {
    let (date, clock) = ts.split_once('T')?;
    Some(format!("{date} {} UTC", clock.get(..5)?))
}
