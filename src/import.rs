use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use rusqlite::{OptionalExtension, params};

use crate::github::GitHub;
use crate::plural;
use crate::store::{APPEND_SNAPSHOT, REPO_ID_BY_NAME, Store, UPSERT_REPO};

const PREHISTORY_BATCH: usize = 50_000;

const INSERT_PREHISTORY: &str = "\
INSERT OR IGNORE INTO prehistory_monthly (full_name, month, gross_watch_events)
VALUES (?1, ?2, ?3)";

const UPSERT_CAPTURE_MONTH: &str = "\
INSERT INTO capture_month (month, ratio) VALUES (?1, ?2)
ON CONFLICT (month) DO UPDATE SET ratio = excluded.ratio";

#[derive(Debug, PartialEq, Eq)]
pub struct SnapshotRow {
    pub ts: String,
    pub full_name: String,
    pub stars: i64,
    pub created_at: Option<String>,
    pub forks: Option<i64>,
    pub open_issues: Option<i64>,
    pub pushed_at: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct PrehistoryRow<'a> {
    pub full_name: &'a str,
    pub month: &'a str,
    pub gross_watch_events: i64,
}

/// `ts repo stars created [forks open_issues pushed_at]`; the collector wrote
/// the short form before it learned the other three columns.
pub fn parse_snapshot_line(line: &str) -> Result<Option<SnapshotRow>> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return Ok(None);
    }
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 4 && f.len() != 7 {
        bail!("expected 4 or 7 tab-separated fields, got {}", f.len());
    }
    Ok(Some(SnapshotRow {
        ts: required(f[0], "ts")?.to_string(),
        full_name: required(f[1], "repo")?.to_string(),
        stars: required(f[2], "stars")?.parse().context("stars")?,
        created_at: present(f[3]).map(str::to_string),
        forks: optional_int(f.get(4).copied()).context("forks")?,
        open_issues: optional_int(f.get(5).copied()).context("open_issues")?,
        pushed_at: f.get(6).copied().and_then(present).map(str::to_string),
    }))
}

/// `repo_name month watch_events`, month as first-of-month; the store keeps `YYYY-MM`.
pub fn parse_prehistory_line(line: &str) -> Result<Option<PrehistoryRow<'_>>> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.is_empty() {
        return Ok(None);
    }
    let f: Vec<&str> = line.split('\t').collect();
    if f.len() != 3 {
        bail!("expected 3 tab-separated fields, got {}", f.len());
    }
    let full_name = required(f[0], "repo_name")?;
    let month = f[1]
        .get(..7)
        .filter(|m| is_month(m))
        .filter(|_| f[1].len() == 7 || f[1].as_bytes().get(7) == Some(&b'-'))
        .with_context(|| format!("month {:?} is not YYYY-MM or YYYY-MM-DD", f[1]))?;
    Ok(Some(PrehistoryRow {
        full_name,
        month,
        gross_watch_events: f[2].trim().parse().context("watch_events")?,
    }))
}

/// The manifest's capture table, `| month | WatchEvents | capture vs baseline |`,
/// as fractions. These numbers are measurements; they only ever come from the file.
pub fn parse_capture_ratios(markdown: &str) -> Result<Vec<(String, f64)>> {
    let mut months = Vec::new();
    for line in markdown.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() < 3 {
            continue;
        }
        let (Some(month), Some(percent)) = (
            cells[0].get(..7).filter(|m| is_month(m)),
            cells[2].strip_suffix('%'),
        ) else {
            continue;
        };
        let percent: f64 = percent
            .trim_start_matches('~')
            .trim()
            .replace(',', "")
            .parse()
            .with_context(|| format!("capture ratio for {month}"))?;
        months.push((month.to_string(), percent / 100.0));
    }
    ensure!(
        !months.is_empty(),
        "no capture-ratio rows found; expected a table of | YYYY-MM | events | NN% |"
    );
    Ok(months)
}

pub fn run_tsv(store: &mut Store, path: &Path) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut rows = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let row =
            parse_snapshot_line(line).with_context(|| format!("{}:{}", path.display(), i + 1))?;
        rows.extend(row);
    }

    let mut names: Vec<&str> = Vec::new();
    let mut first_seen: HashMap<&str, &str> = HashMap::new();
    let mut created: HashMap<&str, String> = HashMap::new();
    let mut row_count: HashMap<&str, usize> = HashMap::new();
    for row in &rows {
        let name = row.full_name.as_str();
        let count = row_count.entry(name).or_default();
        if *count == 0 {
            names.push(name);
        }
        *count += 1;
        first_seen
            .entry(name)
            .and_modify(|ts| *ts = (*ts).min(row.ts.as_str()))
            .or_insert(&row.ts);
        if let Some(c) = row.created_at.as_deref() {
            created.entry(name).or_insert_with(|| c.to_string());
        }
    }
    names.sort_unstable();

    let mut resolved: HashMap<&str, (i64, Option<String>)> = HashMap::new();
    let mut unresolved: Vec<&str> = Vec::new();
    {
        let mut stmt = store.conn.prepare(REPO_ID_BY_NAME)?;
        for name in &names {
            match stmt.query_row(params![name], |row| row.get(0)).optional()? {
                Some(id) => {
                    resolved.insert(name, (id, None));
                }
                None => unresolved.push(name),
            }
        }
    }
    let cached = resolved.len();

    let mut missing: Vec<&str> = Vec::new();
    if !unresolved.is_empty() {
        let gh = GitHub::from_env()?;
        eprintln!("resolving {} repo names against the API", unresolved.len());
        for (i, name) in unresolved.iter().enumerate() {
            match gh.repo(name)? {
                Some(repo) => {
                    resolved.insert(name, (repo.id, Some(repo.full_name)));
                    created.insert(name, repo.created_at);
                }
                None => missing.push(name),
            }
            if (i + 1) % 50 == 0 {
                eprintln!("  {}/{}", i + 1, unresolved.len());
            }
        }
    }

    let tx = store.conn.transaction()?;
    {
        let mut upsert = tx.prepare(UPSERT_REPO)?;
        for name in &names {
            let Some((id, fresh_name)) = resolved.get(name) else {
                continue;
            };
            upsert.execute(params![
                id,
                fresh_name.as_deref().unwrap_or(name),
                "import",
                first_seen[name],
                created.get(name),
            ])?;
        }
    }
    let mut appended = 0usize;
    let mut skipped = 0usize;
    {
        let mut append = tx.prepare(APPEND_SNAPSHOT)?;
        for row in &rows {
            let Some((id, _)) = resolved.get(row.full_name.as_str()) else {
                skipped += 1;
                continue;
            };
            appended += append.execute(params![
                id,
                row.ts,
                row.stars,
                row.forks,
                row.open_issues,
                row.pushed_at,
                None::<i64>,
            ])?;
        }
    }
    tx.commit()?;

    println!(
        "{}, {}: {cached} already in the store, {} resolved via the API",
        plural(rows.len(), "data row"),
        plural(names.len(), "distinct repo"),
        resolved.len() - cached
    );
    if !missing.is_empty() {
        let dropped: usize = missing.iter().map(|n| row_count[n]).sum();
        println!(
            "{} no longer visible to the API, {} skipped:",
            plural(missing.len(), "repo"),
            plural(dropped, "row")
        );
        for name in &missing {
            println!("  {name} ({})", plural(row_count[name], "row"));
        }
    }
    println!(
        "{} appended, {} already present, {skipped} skipped",
        plural(appended, "snapshot row"),
        rows.len() - appended - skipped
    );
    Ok(())
}

pub fn run_prehistory(store: &mut Store, path: &Path, manifest: Option<PathBuf>) -> Result<()> {
    let manifest = manifest.unwrap_or_else(|| default_manifest(path));
    let markdown = std::fs::read_to_string(&manifest)
        .with_context(|| format!("reading {}", manifest.display()))?;
    let ratios = parse_capture_ratios(&markdown)
        .with_context(|| format!("parsing capture ratios from {}", manifest.display()))?;

    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = String::new();
    let (mut lineno, mut read, mut appended) = (0usize, 0usize, 0usize);
    let mut eof = false;
    while !eof {
        let tx = store.conn.transaction()?;
        {
            let mut stmt = tx.prepare(INSERT_PREHISTORY)?;
            for _ in 0..PREHISTORY_BATCH {
                line.clear();
                if reader.read_line(&mut line)? == 0 {
                    eof = true;
                    break;
                }
                lineno += 1;
                let parsed = parse_prehistory_line(&line)
                    .with_context(|| format!("{}:{lineno}", path.display()))?;
                let Some(row) = parsed else { continue };
                read += 1;
                appended +=
                    stmt.execute(params![row.full_name, row.month, row.gross_watch_events])?;
            }
        }
        tx.commit()?;
        if read > 0 && read % 1_000_000 < PREHISTORY_BATCH && !eof {
            eprintln!("  {read} rows");
        }
    }

    let tx = store.conn.transaction()?;
    {
        let mut stmt = tx.prepare(UPSERT_CAPTURE_MONTH)?;
        for (month, ratio) in &ratios {
            stmt.execute(params![month, ratio])?;
        }
    }
    tx.commit()?;

    println!(
        "{}, {appended} appended, {} already present",
        plural(read, "data row"),
        read - appended
    );
    println!(
        "{} from {}",
        plural(ratios.len(), "capture ratio"),
        manifest.display()
    );
    Ok(())
}

fn default_manifest(tsv: &Path) -> PathBuf {
    tsv.parent()
        .unwrap_or(Path::new("."))
        .join("prehistory-MANIFEST.md")
}

fn required<'a>(field: &'a str, name: &str) -> Result<&'a str> {
    present(field).with_context(|| format!("{name} is empty"))
}

fn present(field: &str) -> Option<&str> {
    Some(field.trim()).filter(|f| !f.is_empty())
}

fn optional_int(field: Option<&str>) -> Result<Option<i64>> {
    field
        .and_then(present)
        .map(str::parse)
        .transpose()
        .map_err(Into::into)
}

fn is_month(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 7 && b[4] == b'-' && b[..4].iter().chain(&b[5..]).all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: &str) -> SnapshotRow {
        parse_snapshot_line(line).unwrap().unwrap()
    }

    #[test]
    fn parses_the_current_seven_column_row() {
        let r = row(
            "2026-07-30T17:04:20Z\tzhongerxin/Cowart\t5143\t2026-06-18T16:08:30Z\t395\t13\t2026-07-17T05:52:22Z",
        );
        assert_eq!(
            r,
            SnapshotRow {
                ts: "2026-07-30T17:04:20Z".into(),
                full_name: "zhongerxin/Cowart".into(),
                stars: 5143,
                created_at: Some("2026-06-18T16:08:30Z".into()),
                forks: Some(395),
                open_issues: Some(13),
                pushed_at: Some("2026-07-17T05:52:22Z".into()),
            }
        );
    }

    #[test]
    fn parses_the_legacy_four_column_row() {
        let r = row("2026-07-30T15:25:13Z\ta/b\t12\t2026-01-02T03:04:05Z");
        assert_eq!(r.stars, 12);
        assert_eq!(r.created_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!((r.forks, r.open_issues, r.pushed_at), (None, None, None));
    }

    #[test]
    fn empty_trailing_fields_become_null() {
        let r = row("2026-07-30T15:25:13Z\ta/b\t12\t2026-01-02T03:04:05Z\t0\t0\t");
        assert_eq!(r.forks, Some(0));
        assert_eq!(r.open_issues, Some(0));
        assert_eq!(r.pushed_at, None);
    }

    #[test]
    fn rejects_other_widths_and_junk() {
        assert!(parse_snapshot_line("a\tb\tc").is_err());
        assert!(parse_snapshot_line("a\tb\tc\td\te").is_err());
        assert!(
            parse_snapshot_line("2026-07-30T15:25:13Z\ta/b\tmany\t2026-01-02T03:04:05Z").is_err()
        );
        assert!(parse_snapshot_line("\n").unwrap().is_none());
        assert!(parse_snapshot_line("").unwrap().is_none());
    }

    #[test]
    fn parses_prehistory_rows() {
        assert_eq!(
            parse_prehistory_line("0xGF/boneyard\t2026-04-01\t1046\n")
                .unwrap()
                .unwrap(),
            PrehistoryRow {
                full_name: "0xGF/boneyard",
                month: "2026-04",
                gross_watch_events: 1046,
            }
        );
        // The floor harvest carries one degenerate name; it is still a key.
        assert_eq!(
            parse_prehistory_line("/\t2011-02-01\t1216")
                .unwrap()
                .unwrap()
                .full_name,
            "/"
        );
        assert_eq!(
            parse_prehistory_line("a/b\t2011-02\t1")
                .unwrap()
                .unwrap()
                .month,
            "2011-02"
        );
        assert!(parse_prehistory_line("").unwrap().is_none());
        assert!(parse_prehistory_line("a/b\t2011-02-01").is_err());
        assert!(parse_prehistory_line("a/b\tFeb 2011\t1").is_err());
        assert!(parse_prehistory_line("a/b\t2011-0201\t1").is_err());
        assert!(parse_prehistory_line("a/b\t2011-02-01\tlots").is_err());
    }

    #[test]
    fn reads_capture_ratios_from_the_manifest_table() {
        let md = "\
| file | rows | bytes | columns |
|---|---|---|---|
| `prehistory-tracked-monthly.tsv` | 1,688 | 63,756 | repo_name, month, watch_events |

| repo | harvested sum | 005 reference | match |
|---|---|---|---|
| google/deepdream | 14,912 | 14,912 | yes |

| month | WatchEvents | capture vs baseline |
|---|---|---|
| 2025-01 | 6,206,143 | 101% |
| 2026-06 | 310,744 | 5% |
| 2026-07 (to the 30th) | 78,796 | ~1% |
";
        assert_eq!(
            parse_capture_ratios(md).unwrap(),
            vec![
                ("2025-01".to_string(), 1.01),
                ("2026-06".to_string(), 0.05),
                ("2026-07".to_string(), 0.01),
            ]
        );
        assert!(parse_capture_ratios("nothing here").is_err());
    }

    #[test]
    fn manifest_defaults_to_the_tsv_directory() {
        assert_eq!(
            default_manifest(Path::new("data/prehistory-floor500-monthly.tsv")),
            Path::new("data/prehistory-MANIFEST.md")
        );
    }
}
