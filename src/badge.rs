//! Badge SVGs, self-contained the way Camo serves them: no webfonts, no external refs.
//!
//! Geometry is pinned with `textLength` at shields' 10x scale so a client that
//! substitutes DejaVu lays the badge out identically. The golden tests hold this
//! renderer to the approved reference SVGs byte for byte, so the arithmetic below
//! reproduces the reference generator's rounding exactly.

use crate::widths::{HELVETICA_BOLD_20, VERDANA_11, VERDANA_11_KERN};

/// Octicon `star-fill`, unmodified.
const STAR: &str = "M8 .25a.75.75 0 0 1 .673.418l1.882 3.815 4.21.612a.75.75 0 0 1 .416 1.279l-3.046 2.97.719 4.192a.751.751 0 0 1-1.088.791L8 12.347l-3.766 1.98a.75.75 0 0 1-1.088-.79l.72-4.194L.818 6.374a.75.75 0 0 1 .416-1.28l4.21-.611L7.327.668A.75.75 0 0 1 8 .25Z";

const AMBER: &str = "#d4a72c";
const PROXY_GREY: &str = "#9f9f9f";
const LABEL_BG: &str = "#555555";
const VALUE_FG: &str = "#24292f";
const PILL_STAR: &str = "#eac54f";

const MEASURING: &str = "measuring · first reading tomorrow";
const NOT_TRACKED: &str = "not tracked";

/// 5px pad + 12px octicon + 3px gap.
const TEXT_LEFT: f64 = 20.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoBadge {
    pub full_name: String,
    pub stars: i64,
    pub tracked_since: String,
    pub state: BadgeState,
    /// Sparkline points in local coords of the 388x40 region; drawn only when measured.
    pub spark: Vec<(f64, f64)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BadgeState {
    Measured {
        velocity: i64,
        top_percent: f64,
    },
    /// Young repo, lifetime average until a measured delta exists.
    Proxy {
        avg: i64,
    },
    Measuring,
    /// Enrollment day: history begins now.
    Enrolled,
    Paused,
}

struct Palette {
    canvas: &'static str,
    border: &'static str,
    fg: &'static str,
    muted: &'static str,
    accent: &'static str,
    green: &'static str,
    gold: &'static str,
    spark: &'static str,
}

const LIGHT: Palette = Palette {
    canvas: "#ffffff",
    border: "#d0d7de",
    fg: "#1f2328",
    muted: "#59636e",
    accent: "#0969da",
    green: "#1a7f37",
    gold: "#eac54f",
    spark: "#d4a72c",
};

const DARK: Palette = Palette {
    canvas: "#0d1117",
    border: "#30363d",
    fg: "#e6edf3",
    muted: "#8b949e",
    accent: "#58a6ff",
    green: "#3fb950",
    gold: "#e3b341",
    spark: "#e3b341",
};

impl Theme {
    fn palette(self) -> &'static Palette {
        match self {
            Theme::Light => &LIGHT,
            Theme::Dark => &DARK,
        }
    }
}

pub fn pill(b: &RepoBadge) -> String {
    let count = commas(b.stars);
    let since = xml_escape(&b.tracked_since);
    let (value, bg, aria) = match b.state {
        BadgeState::Measured { velocity, .. } => (
            delta(velocity),
            AMBER,
            format!("afterglow: {count} stars, {}", gain_phrase(velocity)),
        ),
        BadgeState::Proxy { avg } => (
            format!("~{} avg", commas(avg)),
            PROXY_GREY,
            format!(
                "afterglow: {count} stars, about {} per day lifetime average (proxy)",
                commas(avg)
            ),
        ),
        BadgeState::Measuring => (
            MEASURING.to_string(),
            PROXY_GREY,
            format!("afterglow: {count} stars, measuring — first reading tomorrow"),
        ),
        BadgeState::Enrolled => (
            format!("tracking started {since} — history begins now"),
            PROXY_GREY,
            format!("afterglow: tracking started {since} — history begins now"),
        ),
        BadgeState::Paused => (
            "tracking paused".to_string(),
            PROXY_GREY,
            format!("afterglow: {count} stars, tracking paused"),
        ),
    };
    render_pill(&count, &value, bg, &aria)
}

pub fn not_tracked_pill(full_name: &str) -> String {
    let name = xml_escape(full_name);
    render_pill(
        "afterglow",
        NOT_TRACKED,
        PROXY_GREY,
        &format!("afterglow: {name} is not tracked"),
    )
}

pub fn card(b: &RepoBadge, theme: Theme) -> String {
    let t = theme.palette();
    let repo = xml_escape(&b.full_name);
    let since = xml_escape(&b.tracked_since);
    let count = commas(b.stars);
    let parts = match b.state {
        BadgeState::Measured {
            velocity,
            top_percent,
        } => CardParts {
            aria: format!(
                "afterglow: {repo} — {count} stars, {}, top {}% velocity",
                gain_phrase(velocity),
                percent(top_percent)
            ),
            repo: &repo,
            name_fill: t.accent,
            // Green is the mark of measured gain; a decline is a plain fact in
            // the foreground, neither celebrated nor dressed as degraded.
            nums: nums_row(
                &count,
                &delta(velocity),
                if velocity < 0 { t.fg } else { t.green },
                "600",
                t,
            ),
            pct: pct_el(
                &format!("top {}% velocity · all tracked repos", percent(top_percent)),
                t,
            ),
            spark: spark_el(&b.spark, t),
            footer: footer_el(&format!("last 30 days · tracked since {since}"), "400", t),
        },
        BadgeState::Proxy { avg } => CardParts {
            aria: format!(
                "afterglow: {repo} — {count} stars, about {} per day lifetime average (proxy)",
                commas(avg)
            ),
            repo: &repo,
            name_fill: t.accent,
            nums: nums_row(&count, &format!("~{} avg", commas(avg)), t.muted, "600", t),
            pct: pct_el(MEASURING, t),
            spark: dashed(t),
            footer: footer_el(&format!("tracked since {since}"), "400", t),
        },
        BadgeState::Measuring => CardParts {
            aria: format!("afterglow: {repo} — {count} stars, measuring, first reading tomorrow"),
            repo: &repo,
            name_fill: t.accent,
            nums: nums_row(&count, MEASURING, t.muted, "400", t),
            pct: String::new(),
            spark: dashed(t),
            footer: footer_el(&format!("tracked since {since}"), "400", t),
        },
        BadgeState::Enrolled => CardParts {
            aria: format!("afterglow: {repo} — tracking started {since}, history begins now"),
            repo: &repo,
            name_fill: t.accent,
            nums: nums_row(&count, MEASURING, t.muted, "400", t),
            pct: String::new(),
            // No amber on day one: glow is earned.
            spark: dashed(t),
            footer: footer_el(
                &format!("tracking started {since} — history begins now"),
                "600",
                t,
            ),
        },
        BadgeState::Paused => CardParts {
            aria: format!("afterglow: {repo} — {count} stars, tracking paused"),
            repo: &repo,
            name_fill: t.accent,
            nums: nums_row(&count, "tracking paused", t.muted, "400", t),
            pct: String::new(),
            spark: dashed(t),
            footer: footer_el(&format!("tracked since {since}"), "400", t),
        },
    };
    render_card(&parts, t)
}

pub fn not_tracked_card(full_name: &str, theme: Theme) -> String {
    let t = theme.palette();
    let repo = xml_escape(full_name);
    render_card(
        &CardParts {
            aria: format!("afterglow: {repo} is not tracked"),
            repo: &repo,
            // Muted, not accent: there is nothing to link to.
            name_fill: t.muted,
            nums: format!(
                r#"<text x="16" y="56" font-size="13" font-weight="400" fill="{}">{NOT_TRACKED}</text>"#,
                t.muted
            ),
            pct: String::new(),
            spark: dashed(t),
            footer: String::new(),
        },
        t,
    )
}

/// Direction changes the glyph, never the colour: measured is measured.
fn delta(v: i64) -> String {
    format!(
        "{} {}/day",
        if v < 0 { "▼" } else { "▲" },
        commas(v.saturating_abs())
    )
}

fn gain_phrase(v: i64) -> String {
    format!(
        "{} {} per day",
        if v < 0 { "losing" } else { "gaining" },
        commas(v.saturating_abs())
    )
}

/// Sub-1% percentiles keep a decimal and never round down to a bare 0.
fn percent(p: f64) -> String {
    if p >= 1.0 {
        return round_int(p).to_string();
    }
    let r = round1(p.max(0.1));
    if r >= 1.0 {
        "1".to_string()
    } else {
        format!("{r:.1}")
    }
}

fn commas(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = if n < 0 {
        "-".to_string()
    } else {
        String::new()
    };
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// full_name arrives from a URL; nothing reaches an SVG text node or attribute unescaped.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

fn render_pill(label: &str, value: &str, value_bg: &str, aria: &str) -> String {
    let lw = verdana(label);
    let vw = verdana(value);
    let label_w = round1(TEXT_LEFT + lw + 5.0);
    let value_w = round1(5.0 + vw + 5.0);
    let w = round1(label_w + value_w);
    let lx = round_int((TEXT_LEFT + lw / 2.0) * 10.0);
    let vx = round_int((label_w + value_w / 2.0) * 10.0);
    let ltl = round_int(lw * 10.0);
    let vtl = round_int(vw * 10.0);
    // Flat fills, no shields gloss overlay: the afterglow fade is the only gradient.
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{w:.1}" height="20" role="img" aria-label="{aria}">
<title>{aria}</title>
<clipPath id="r"><rect width="{w:.1}" height="20" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)">
<rect width="{label_w:.1}" height="20" fill="{LABEL_BG}"/>
<rect x="{label_w:.1}" width="{value_w:.1}" height="20" fill="{value_bg}"/>
</g>
<path transform="translate(5,4) scale(0.75)" fill="{PILL_STAR}" d="{STAR}"/>
<g text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="110" text-rendering="geometricPrecision">
<text aria-hidden="true" x="{lx}" y="150" fill="#010101" fill-opacity=".3" transform="scale(.1)" textLength="{ltl}">{label}</text>
<text x="{lx}" y="140" fill="#fff" transform="scale(.1)" textLength="{ltl}">{label}</text>
<text aria-hidden="true" x="{vx}" y="150" fill="#ccc" fill-opacity=".3" transform="scale(.1)" textLength="{vtl}">{value}</text>
<text x="{vx}" y="140" fill="{VALUE_FG}" transform="scale(.1)" textLength="{vtl}">{value}</text>
</g>
</svg>"##
    )
}

/// The per-state pieces of the card; the box around them is fixed in every state.
struct CardParts<'a> {
    aria: String,
    repo: &'a str,
    name_fill: &'static str,
    nums: String,
    pct: String,
    spark: String,
    footer: String,
}

fn render_card(p: &CardParts<'_>, t: &Palette) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="420" height="150" role="img" aria-label="{aria}">
<title>{aria}</title>
<rect x=".5" y=".5" width="419" height="149" rx="6" fill="{canvas}" stroke="{border}"/>
<g font-family="Helvetica,Arial,sans-serif">
<text x="16" y="27" font-size="13" font-weight="600" fill="{name_fill}">{repo}</text>
{nums}
{pct}
{spark}
{footer}
<text x="404" y="140" font-size="10.5" text-anchor="end" fill="{muted}">afterglow</text>
</g>
</svg>"#,
        aria = p.aria,
        canvas = t.canvas,
        border = t.border,
        name_fill = p.name_fill,
        repo = p.repo,
        nums = p.nums,
        pct = p.pct,
        spark = p.spark,
        footer = p.footer,
        muted = t.muted,
    )
}

/// Star, count, then the value 12px after the count's measured width.
fn nums_row(count: &str, value: &str, value_fill: &str, weight: &str, t: &Palette) -> String {
    let value_x = round1(38.0 + text_width(count, HELVETICA_BOLD_20, &[]) + 12.0);
    format!(
        r#"<path transform="translate(16,41)" fill="{gold}" d="{STAR}"/>
<text x="38" y="56" font-size="20" font-weight="600" fill="{fg}">{count}</text>
<text x="{value_x:.1}" y="56" font-size="13" font-weight="{weight}" fill="{value_fill}">{value}</text>"#,
        gold = t.gold,
        fg = t.fg,
    )
}

fn pct_el(text: &str, t: &Palette) -> String {
    format!(
        r#"<text x="16" y="74" font-size="11" fill="{}">{text}</text>"#,
        t.muted
    )
}

fn footer_el(text: &str, weight: &str, t: &Palette) -> String {
    format!(
        r#"<text x="16" y="140" font-size="10.5" font-weight="{weight}" fill="{}">{text}</text>"#,
        t.muted
    )
}

fn spark_el(points: &[(f64, f64)], t: &Palette) -> String {
    if points.is_empty() {
        return dashed(t);
    }
    let pts: Vec<String> = points
        .iter()
        .map(|&(x, y)| format!("{},{}", coord(x + 16.0), coord(y + 82.0)))
        .collect();
    let area = format!(
        "M{} L{},122 L{},122 Z",
        pts.join(" L"),
        coord(points[points.len() - 1].0 + 16.0),
        coord(points[0].0 + 16.0)
    );
    format!(
        r#"<defs><linearGradient id="glow" x1="0" y1="0" x2="0" y2="1">
<stop offset="0" stop-color="{spark}" stop-opacity=".3"/><stop offset="1" stop-color="{spark}" stop-opacity="0"/>
</linearGradient></defs>
<path d="{area}" fill="url(#glow)"/>
<polyline points="{points}" fill="none" stroke="{spark}" stroke-width="1.5"/>"#,
        spark = t.spark,
        points = pts.join(" "),
    )
}

/// Degraded states show an honest void where the sparkline will live.
fn dashed(t: &Palette) -> String {
    format!(
        r#"<line x1="16" y1="112" x2="404" y2="112" stroke="{}" stroke-width="1" stroke-dasharray="3,5" opacity=".6"/>"#,
        t.muted
    )
}

/// Whole coordinates print bare, the rest keep their one decimal.
fn coord(v: f64) -> String {
    let r = round1(v);
    if r.fract() == 0.0 {
        format!("{r:.0}")
    } else {
        format!("{r:.1}")
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round_ties_even() / 10.0
}

fn round_int(x: f64) -> i64 {
    x.round_ties_even() as i64
}

fn verdana(s: &str) -> f64 {
    text_width(s, VERDANA_11, VERDANA_11_KERN)
}

/// CoreText's whole-string width: summed advances plus the kern of every adjacent pair.
fn text_width(s: &str, table: &[(char, f64)], kern: &[(char, char, f64)]) -> f64 {
    let mut w = 0.0;
    let mut prev: Option<char> = None;
    for c in s.chars() {
        w += advance(table, c);
        if let Some(p) = prev {
            w += kern
                .iter()
                .find(|&&(a, b, _)| a == p && b == c)
                .map_or(0.0, |&(_, _, k)| k);
        }
        prev = Some(c);
    }
    w
}

fn advance(table: &[(char, f64)], c: char) -> f64 {
    let find = |c: char| table.iter().find(|&&(t, _)| t == c).map(|&(_, w)| w);
    // Widths are only ever taken over digits and the contract wording; a digit is the safe fallback.
    find(c).or_else(|| find('0')).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reference generator derives the gradient id from the output filename.
    fn normalize(fixture: &str) -> String {
        let mut out = String::with_capacity(fixture.len());
        let mut rest = fixture;
        while let Some(i) = rest.find("glow-") {
            out.push_str(&rest[..i]);
            out.push_str("glow");
            let tail = &rest[i + "glow-".len()..];
            let end = tail
                .find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'))
                .unwrap_or(tail.len());
            rest = &tail[end..];
        }
        out.push_str(rest);
        out
    }

    macro_rules! golden {
        ($name:ident, $file:literal, $svg:expr) => {
            #[test]
            fn $name() {
                assert_eq!(
                    $svg,
                    normalize(include_str!(concat!("../tests/golden/", $file)))
                );
            }
        };
    }

    const SPARK: [(f64, f64); 30] = [
        (0.0, 36.0),
        (13.0, 35.0),
        (27.0, 35.0),
        (40.0, 34.0),
        (54.0, 33.0),
        (67.0, 33.0),
        (80.0, 32.0),
        (94.0, 31.0),
        (107.0, 31.0),
        (121.0, 30.0),
        (134.0, 29.0),
        (147.0, 28.0),
        (161.0, 27.0),
        (174.0, 27.0),
        (187.0, 26.0),
        (201.0, 25.0),
        (214.0, 23.0),
        (228.0, 22.0),
        (241.0, 21.0),
        (254.0, 20.0),
        (268.0, 18.0),
        (281.0, 17.0),
        (294.0, 15.0),
        (308.0, 14.0),
        (321.0, 12.0),
        (334.0, 11.0),
        (348.0, 9.0),
        (361.0, 8.0),
        (375.0, 6.0),
        (388.0, 5.0),
    ];

    const MEASURED: BadgeState = BadgeState::Measured {
        velocity: 807,
        top_percent: 0.4,
    };

    fn caveman(state: BadgeState) -> RepoBadge {
        RepoBadge {
            full_name: "JuliusBrussee/caveman".to_string(),
            stars: 94_590,
            tracked_since: "2026-07-30".to_string(),
            state,
            spark: SPARK.to_vec(),
        }
    }

    fn kimi() -> RepoBadge {
        RepoBadge {
            full_name: "MoonshotAI/Kimi-K3".to_string(),
            stars: 31_038,
            tracked_since: "2026-07-30".to_string(),
            state: BadgeState::Proxy { avg: 2_217 },
            spark: Vec::new(),
        }
    }

    golden!(pill_measured, "pill-measured.svg", pill(&caveman(MEASURED)));
    golden!(pill_e1_proxy, "pill-e1-proxy.svg", pill(&kimi()));
    golden!(
        pill_e2_measuring,
        "pill-e2-measuring.svg",
        pill(&caveman(BadgeState::Measuring))
    );
    golden!(
        pill_e3_enrollment,
        "pill-e3-enrollment.svg",
        pill(&caveman(BadgeState::Enrolled))
    );

    golden!(
        card_measured_light,
        "card-measured-light.svg",
        card(&caveman(MEASURED), Theme::Light)
    );
    golden!(
        card_measured_dark,
        "card-measured-dark.svg",
        card(&caveman(MEASURED), Theme::Dark)
    );
    golden!(
        card_e1_proxy_light,
        "card-e1-proxy-light.svg",
        card(&kimi(), Theme::Light)
    );
    golden!(
        card_e1_proxy_dark,
        "card-e1-proxy-dark.svg",
        card(&kimi(), Theme::Dark)
    );
    golden!(
        card_e2_measuring_light,
        "card-e2-measuring-light.svg",
        card(&caveman(BadgeState::Measuring), Theme::Light)
    );
    golden!(
        card_e2_measuring_dark,
        "card-e2-measuring-dark.svg",
        card(&caveman(BadgeState::Measuring), Theme::Dark)
    );
    golden!(
        card_e3_enrollment_light,
        "card-e3-enrollment-light.svg",
        card(&caveman(BadgeState::Enrolled), Theme::Light)
    );
    golden!(
        card_e3_enrollment_dark,
        "card-e3-enrollment-dark.svg",
        card(&caveman(BadgeState::Enrolled), Theme::Dark)
    );

    #[test]
    fn percentiles_keep_a_decimal_under_one() {
        assert_eq!(percent(0.04), "0.1");
        assert_eq!(percent(0.0), "0.1");
        assert_eq!(percent(0.4), "0.4");
        assert_eq!(percent(0.94), "0.9");
        assert_eq!(percent(0.95), "1");
        assert_eq!(percent(1.0), "1");
        assert_eq!(percent(1.4), "1");
        assert_eq!(percent(12.0), "12");
        assert_eq!(percent(12.5), "12");
        assert_eq!(percent(13.5), "14");
    }

    #[test]
    fn thousands_are_separated() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(7), "7");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(94_590), "94,590");
        assert_eq!(commas(1_234_567), "1,234,567");
        assert_eq!(commas(-1_500), "-1,500");
    }

    #[test]
    fn falling_velocity_flips_the_glyph_but_keeps_the_amber() {
        let svg = pill(&caveman(BadgeState::Measured {
            velocity: -1_042,
            top_percent: 0.4,
        }));
        assert!(svg.contains(">▼ 1,042/day<"), "{svg}");
        assert!(svg.contains(r##"fill="#d4a72c""##), "{svg}");
        assert!(
            svg.contains(r#"aria-label="afterglow: 94,590 stars, losing 1,042 per day""#),
            "{svg}"
        );

        let svg = card(
            &caveman(BadgeState::Measured {
                velocity: -1_042,
                top_percent: 12.0,
            }),
            Theme::Light,
        );
        assert!(svg.contains(">▼ 1,042/day<"), "{svg}");
        // A decline is a plain foreground fact: no green, and no degraded grey.
        assert!(
            svg.contains(r##"font-weight="600" fill="#1f2328">▼ 1,042/day"##),
            "{svg}"
        );
        assert!(!svg.contains(r##"fill="#1a7f37""##), "{svg}");
        assert!(
            svg.contains("top 12% velocity · all tracked repos"),
            "{svg}"
        );
        assert!(
            svg.contains(
                "afterglow: JuliusBrussee/caveman — 94,590 stars, losing 1,042 per day, top 12% velocity"
            ),
            "{svg}"
        );
    }

    #[test]
    fn flat_velocity_still_reads_as_measured() {
        let svg = pill(&caveman(BadgeState::Measured {
            velocity: 0,
            top_percent: 50.0,
        }));
        assert!(svg.contains(">▲ 0/day<"), "{svg}");
        assert!(svg.contains("gaining 0 per day"), "{svg}");
        assert!(svg.contains(r##"fill="#d4a72c""##), "{svg}");
    }

    #[test]
    fn paused_and_not_tracked_carry_their_wording() {
        let svg = pill(&caveman(BadgeState::Paused));
        assert!(svg.contains(">tracking paused<"), "{svg}");
        assert!(
            svg.contains("afterglow: 94,590 stars, tracking paused"),
            "{svg}"
        );

        let svg = card(&caveman(BadgeState::Paused), Theme::Dark);
        assert!(svg.contains(">tracking paused<"), "{svg}");
        assert!(svg.contains("stroke-dasharray"), "{svg}");
        assert!(svg.contains(">tracked since 2026-07-30<"), "{svg}");

        let svg = not_tracked_pill("JuliusBrussee/caveman");
        assert!(svg.contains(">afterglow<"), "{svg}");
        assert!(svg.contains(">not tracked<"), "{svg}");
        assert!(svg.contains(r##"fill="#9f9f9f""##), "{svg}");
        assert!(
            svg.contains("afterglow: JuliusBrussee/caveman is not tracked"),
            "{svg}"
        );

        let svg = not_tracked_card("JuliusBrussee/caveman", Theme::Light);
        assert!(
            svg.contains(
                r##"<text x="16" y="56" font-size="13" font-weight="400" fill="#59636e">not tracked</text>"##
            ),
            "{svg}"
        );
        assert!(
            svg.contains(r##"y="27" font-size="13" font-weight="600" fill="#59636e""##),
            "{svg}"
        );
        assert!(!svg.contains("#0969da"), "{svg}");
        assert!(!svg.contains(STAR), "{svg}");
        assert!(svg.contains("stroke-dasharray"), "{svg}");
        assert!(svg.contains(r##"<text x="404" y="140" font-size="10.5" text-anchor="end" fill="#59636e">afterglow</text>"##), "{svg}");
    }

    #[test]
    fn hostile_names_are_escaped() {
        let hostile = r#"a&b<c>"d'/e"#;
        for svg in [
            card(
                &RepoBadge {
                    full_name: hostile.to_string(),
                    stars: 1,
                    tracked_since: "2026-07-30".to_string(),
                    state: BadgeState::Measuring,
                    spark: Vec::new(),
                },
                Theme::Light,
            ),
            not_tracked_card(hostile, Theme::Dark),
            not_tracked_pill(hostile),
        ] {
            assert!(svg.contains("a&amp;b&lt;c&gt;&quot;d&apos;/e"), "{svg}");
            assert!(!svg.contains("<c>"), "{svg}");
            assert!(!svg.contains(hostile), "{svg}");
        }
    }

    #[test]
    fn spark_coordinates_drop_whole_decimals_and_close_under_the_curve() {
        let svg = card(
            &RepoBadge {
                spark: vec![(0.0, 35.5), (10.0, 36.0), (24.4, 20.0)],
                ..caveman(MEASURED)
            },
            Theme::Light,
        );
        assert!(
            svg.contains(r#"<polyline points="16,117.5 26,118 40.4,102""#),
            "{svg}"
        );
        assert!(
            svg.contains(r#"<path d="M16,117.5 L26,118 L40.4,102 L40.4,122 L16,122 Z""#),
            "{svg}"
        );

        // A measured repo with no series yet still gets the honest void, not a broken path.
        let svg = card(
            &RepoBadge {
                spark: Vec::new(),
                ..caveman(MEASURED)
            },
            Theme::Light,
        );
        assert!(svg.contains("stroke-dasharray"), "{svg}");
        assert!(!svg.contains("polyline"), "{svg}");
    }
}
