use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::HeaderMap;
use serde::Deserialize;

use crate::time::now_unix;

const API: &str = "https://api.github.com";
const USER_AGENT: &str = "afterglow";
const MAX_ATTEMPTS: u32 = 6;
const MAX_BACKOFF: Duration = Duration::from_secs(900);

/// Aliased repos per GraphQL query. Still one rate-limit point, and now under
/// GitHub's per-query resource limit: measured against the live API on
/// 2026-08-05, 100 aliases took ~10s and came back with the last 14-17 nodes
/// nulled as RESOURCE_LIMITS_EXCEEDED, while 25 took 2.7s clean. Those nulls
/// are not NOT_FOUND, so the sweep read them as `Undecided` and silently lost
/// the day's reading for every repo in the tail (100 of 117 due on 2026-08-04).
pub const GRAPHQL_BATCH: usize = 25;

/// `stargazers_count` carries no default on purpose: a response we cannot read a
/// star count out of is a decode error, never a snapshot row claiming zero stars.
#[derive(Debug, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub full_name: String,
    pub created_at: String,
    pub stargazers_count: i64,
    #[serde(default)]
    pub forks_count: Option<i64>,
    #[serde(default)]
    pub open_issues_count: Option<i64>,
    #[serde(default)]
    pub subscribers_count: Option<i64>,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchItem {
    pub id: i64,
    pub full_name: String,
    pub stargazers_count: i64,
    pub created_at: String,
    #[serde(default)]
    pub forks_count: i64,
    #[serde(default)]
    pub open_issues_count: i64,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Deserialize)]
struct SearchPage {
    items: Vec<SearchItem>,
}

/// One repo's verdict out of a batched probe. `Gone` is a definite NOT_FOUND
/// and nothing else: any other per-repo error (a DMCA block, a transient
/// resolver failure) stays `Undecided`, because retiring a series needs a
/// definite answer.
#[derive(Debug)]
pub enum Probe {
    Found(Repo),
    Gone,
    Undecided,
}

pub struct GitHub {
    http: Client,
    token: String,
    base: String,
}

impl GitHub {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .context("GITHUB_TOKEN is not set; the GitHub API needs an authenticated identity")?;
        Ok(GitHub {
            http: Self::client()?,
            token,
            base: API.to_string(),
        })
    }

    /// Points the client at a local stub so tests exercise the real request path.
    #[cfg(test)]
    pub fn at(base: &str) -> Self {
        GitHub {
            http: Self::client().expect("building the test client"),
            token: "test-token".to_string(),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    fn client() -> Result<Client> {
        Ok(Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()?)
    }

    /// `None` means the repo is gone from our vantage point: deleted, renamed, or private.
    pub fn repo(&self, full_name: &str) -> Result<Option<Repo>> {
        let url = format!("{}/repos/{full_name}", self.base);
        match self.get(&url, &[])? {
            Some(resp) => Ok(Some(
                resp.json().with_context(|| format!("decoding {url}"))?,
            )),
            None => Ok(None),
        }
    }

    pub fn search_repositories(&self, query: &str, page: u32) -> Result<Vec<SearchItem>> {
        let url = format!("{}/search/repositories", self.base);
        let params = [
            ("q", query.to_string()),
            // Ascending, so a bucket past the Search API's 1000-result cap
            // sheds its top (repos about to rise into the next bucket's
            // fully-scanned bottom) instead of its floor, where a riser
            // could sit unseen for weeks.
            ("sort", "stars".to_string()),
            ("order", "asc".to_string()),
            ("per_page", "100".to_string()),
            ("page", page.to_string()),
        ];
        let Some(resp) = self.get(&url, &params)? else {
            bail!("search/repositories returned 404 for q={query}");
        };
        let page: SearchPage = resp.json().context("decoding search/repositories")?;
        Ok(page.items)
    }

    /// Up to [`GRAPHQL_BATCH`] repos resolved in one GraphQL query for one
    /// rate-limit point, where REST would spend a GET each. Verdicts come back
    /// in input order. Renames resolve through their old name here exactly as
    /// they do through REST's 301.
    pub fn repos_batch(&self, names: &[&str]) -> Result<Vec<Probe>> {
        let mut query = String::from("query {\n");
        for (i, name) in names.iter().enumerate() {
            // Tracked names all came back from GitHub, but this one is about to
            // be spliced into a query string: refuse anything off-charset.
            let legal = |c: char| c.is_ascii_alphanumeric() || "-._".contains(c);
            let Some((owner, repo)) = name.split_once('/').filter(|(o, r)| {
                !o.is_empty() && !r.is_empty() && o.chars().chain(r.chars()).all(legal)
            }) else {
                bail!("{name} is not a legal owner/name, refusing to query it");
            };
            query.push_str(&format!(
                "r{i}: repository(owner: \"{owner}\", name: \"{repo}\") {{ \
                 databaseId nameWithOwner createdAt pushedAt stargazerCount forkCount \
                 issues(states: OPEN) {{ totalCount }} pullRequests(states: OPEN) {{ totalCount }} \
                 watchers {{ totalCount }} primaryLanguage {{ name }} }}\n"
            ));
        }
        query.push('}');

        let url = format!("{}/graphql", self.base);
        let Some(resp) = self.send(&format!("POST {url}"), || {
            self.http
                .post(&url)
                .json(&serde_json::json!({ "query": query }))
        })?
        else {
            bail!("POST {url} -> 404");
        };
        let payload: serde_json::Value = resp.json().with_context(|| format!("decoding {url}"))?;

        let errors = payload["errors"].as_array().cloned().unwrap_or_default();
        let data = &payload["data"];
        if data.is_null() {
            // The whole query was refused (RATE_LIMITED and friends): no
            // verdict on anyone, the caller tries again another day.
            let why = errors
                .first()
                .and_then(|e| e["message"].as_str())
                .unwrap_or("no error given");
            bail!("POST {url}: no data ({why})");
        }
        let verdicts: HashMap<&str, &str> = errors
            .iter()
            .filter_map(|e| Some((e["path"][0].as_str()?, e["type"].as_str()?)))
            .collect();

        (0..names.len())
            .map(|i| {
                let alias = format!("r{i}");
                let node = &data[alias.as_str()];
                if !node.is_null() {
                    Ok(Probe::Found(repo_from_node(node)?))
                } else if verdicts.get(alias.as_str()) == Some(&"NOT_FOUND") {
                    Ok(Probe::Gone)
                } else {
                    Ok(Probe::Undecided)
                }
            })
            .collect()
    }

    fn get(&self, url: &str, params: &[(&str, String)]) -> Result<Option<Response>> {
        self.send(&format!("GET {url}"), || self.http.get(url).query(params))
    }

    fn send(
        &self,
        what: &str,
        build: impl Fn() -> reqwest::blocking::RequestBuilder,
    ) -> Result<Option<Response>> {
        for attempt in 0..MAX_ATTEMPTS {
            let resp = build()
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .with_context(|| what.to_string())?;

            let status = resp.status();
            if status.is_success() {
                return Ok(Some(resp));
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if let Some(wait) = backoff(status, resp.headers(), attempt, now_unix()) {
                eprintln!(
                    "github: {status} on {what}, waiting {}s",
                    wait.as_secs().max(1)
                );
                std::thread::sleep(wait);
                continue;
            }
            let body = resp.text().unwrap_or_default();
            bail!(
                "{what} -> {status}: {}",
                body.trim().chars().take(300).collect::<String>()
            );
        }
        bail!("{what}: still rate-limited after {MAX_ATTEMPTS} attempts")
    }
}

/// A GraphQL repository node reshaped into the REST [`Repo`], preserving REST's
/// meanings: `open_issues_count` counts open issues plus open PRs, and
/// `subscribers_count` is what GraphQL calls watchers.
fn repo_from_node(node: &serde_json::Value) -> Result<Repo> {
    let text = |field: &str| -> Result<String> {
        node[field]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("graphql node has no {field}: {node}"))
    };
    let count = |field: &str| node[field]["totalCount"].as_i64();
    Ok(Repo {
        // Repository only offers the Int-typed databaseId; ids sat at ~1.2e9 in
        // mid-2026, so the 2^31 ceiling is years off. When GitHub adds
        // fullDatabaseId to Repository (Issue and PullRequest have it), take it.
        id: node["databaseId"]
            .as_i64()
            .with_context(|| format!("graphql node has no databaseId: {node}"))?,
        full_name: text("nameWithOwner")?,
        created_at: text("createdAt")?,
        // Same no-default rule as the REST decode: no star count is an error,
        // never a zero.
        stargazers_count: node["stargazerCount"]
            .as_i64()
            .with_context(|| format!("graphql node has no stargazerCount: {node}"))?,
        forks_count: node["forkCount"].as_i64(),
        open_issues_count: match (count("issues"), count("pullRequests")) {
            (Some(issues), Some(prs)) => Some(issues + prs),
            _ => None,
        },
        subscribers_count: count("watchers"),
        pushed_at: node["pushedAt"].as_str().map(str::to_string),
        language: node["primaryLanguage"]["name"].as_str().map(str::to_string),
    })
}

/// How long to wait before retrying, or `None` if the response is a hard failure.
///
/// GitHub signals three separate things here: `Retry-After` on the secondary
/// rate limit, `x-ratelimit-remaining: 0` plus a reset epoch on the primary one,
/// and plain 5xx. Everything else is the caller's problem.
fn backoff(status: StatusCode, headers: &HeaderMap, attempt: u32, now: i64) -> Option<Duration> {
    let header = |name: &str| headers.get(name)?.to_str().ok()?.trim().parse::<i64>().ok();
    let exponential = || Duration::from_secs(1 << attempt.min(9));

    if let Some(secs) = header("retry-after") {
        return Some(Duration::from_secs(
            secs.clamp(1, MAX_BACKOFF.as_secs() as i64) as u64,
        ));
    }
    let throttled = status == StatusCode::FORBIDDEN || status == StatusCode::TOO_MANY_REQUESTS;
    if throttled && header("x-ratelimit-remaining") == Some(0) {
        let reset = header("x-ratelimit-reset").unwrap_or(now);
        let secs = (reset - now + 1).clamp(1, MAX_BACKOFF.as_secs() as i64);
        return Some(Duration::from_secs(secs as u64));
    }
    if throttled || status.is_server_error() {
        return Some(exponential().min(MAX_BACKOFF));
    }
    None
}

/// A canned GitHub: one response per connection, in order. When the list runs out
/// the port closes, which is how a test proves no further call was made.
#[cfg(test)]
pub fn stub(
    responses: Vec<(u16, String)>,
) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let listener = TcpListener::bind("127.0.0.1:0").expect("binding the stub");
    let base = format!("http://{}", listener.local_addr().expect("stub address"));
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        let mut canned = responses.into_iter();
        while let Ok((mut sock, _)) = listener.accept() {
            counter.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            let mut byte = [0u8; 1];
            while !request.ends_with(b"\r\n\r\n") && matches!(sock.read(&mut byte), Ok(1)) {
                request.push(byte[0]);
            }
            // Drain a POST body too, or closing early resets the client mid-write.
            let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
            let body_len = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|len| len.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let _ = sock.read_exact(&mut vec![0u8; body_len]);
            let Some((status, body)) = canned.next() else {
                // Out of responses: hang up, so an unexpected call fails at once
                // instead of hanging the test on a timeout.
                let _ = sock.shutdown(std::net::Shutdown::Both);
                continue;
            };
            let _ = write!(
                sock,
                "HTTP/1.1 {status} stub\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, hits)
}

#[cfg(test)]
pub fn repo_json(id: i64, full_name: &str, stars: i64, created_at: &str) -> String {
    format!(
        r#"{{"id":{id},"full_name":"{full_name}","created_at":"{created_at}","stargazers_count":{stars},"forks_count":3,"open_issues_count":4,"subscribers_count":5,"pushed_at":"{created_at}","language":"Rust"}}"#
    )
}

/// The GraphQL twin of [`repo_json`]: same repo, same derived counts (1 issue +
/// 3 PRs is REST's 4), so tests can swap transports without moving assertions.
#[cfg(test)]
pub fn graphql_node(id: i64, full_name: &str, stars: i64, created_at: &str) -> String {
    format!(
        r#"{{"databaseId":{id},"nameWithOwner":"{full_name}","createdAt":"{created_at}","pushedAt":"{created_at}","stargazerCount":{stars},"forkCount":3,"issues":{{"totalCount":1}},"pullRequests":{{"totalCount":3}},"watchers":{{"totalCount":5}},"primaryLanguage":{{"name":"Rust"}}}}"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn retry_after_wins() {
        let h = headers(&[("retry-after", "37")]);
        assert_eq!(
            backoff(StatusCode::FORBIDDEN, &h, 0, 1000),
            Some(Duration::from_secs(37))
        );
    }

    #[test]
    fn primary_limit_waits_for_the_reset() {
        let h = headers(&[
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset", "1060"),
        ]);
        assert_eq!(
            backoff(StatusCode::FORBIDDEN, &h, 0, 1000),
            Some(Duration::from_secs(61))
        );
    }

    #[test]
    fn secondary_limit_backs_off_exponentially() {
        let h = headers(&[("x-ratelimit-remaining", "42")]);
        assert_eq!(
            backoff(StatusCode::FORBIDDEN, &h, 3, 1000),
            Some(Duration::from_secs(8))
        );
        assert_eq!(
            backoff(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), 0, 1000),
            Some(Duration::from_secs(1))
        );
    }

    #[test]
    fn server_errors_retry_and_client_errors_do_not() {
        assert!(backoff(StatusCode::BAD_GATEWAY, &HeaderMap::new(), 1, 0).is_some());
        assert!(backoff(StatusCode::UNAUTHORIZED, &HeaderMap::new(), 1, 0).is_none());
        assert!(backoff(StatusCode::UNPROCESSABLE_ENTITY, &HeaderMap::new(), 1, 0).is_none());
    }

    #[test]
    fn the_stub_speaks_enough_http_for_the_real_client() {
        let (base, hits) = stub(vec![
            (200, repo_json(9, "a/b", 12, "2026-01-02T03:04:05Z")),
            (404, "{}".to_string()),
        ]);
        let gh = GitHub::at(&base);

        let repo = gh.repo("a/b").unwrap().expect("a body");
        assert_eq!((repo.id, repo.stargazers_count), (9, 12));
        assert_eq!(repo.subscribers_count, Some(5));
        assert_eq!(repo.pushed_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(repo.language.as_deref(), Some("Rust"));
        assert!(gh.repo("a/b").unwrap().is_none());
        // Out of canned responses: the call is seen and then refused, never hung.
        assert!(gh.repo("a/b").is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[test]
    fn a_batch_probe_sorts_found_gone_and_blocked() {
        let body = format!(
            r#"{{"data":{{"r0":{},"r1":null,"r2":null}},"errors":[{{"type":"NOT_FOUND","path":["r1"],"message":"gone"}},{{"type":"FORBIDDEN","path":["r2"],"message":"dmca"}}]}}"#,
            graphql_node(9, "a/renamed", 12, "2026-01-02T03:04:05Z")
        );
        let (base, hits) = stub(vec![(200, body)]);
        let gh = GitHub::at(&base);

        let probes = gh.repos_batch(&["a/b", "c/d", "e/f"]).unwrap();

        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        let Probe::Found(repo) = &probes[0] else {
            panic!("expected Found, got {:?}", probes[0]);
        };
        assert_eq!((repo.id, repo.stargazers_count), (9, 12));
        assert_eq!(repo.full_name, "a/renamed");
        // REST's open_issues_count means issues plus PRs; the reshape keeps that.
        assert_eq!(repo.open_issues_count, Some(4));
        assert_eq!(repo.subscribers_count, Some(5));
        assert_eq!(repo.pushed_at.as_deref(), Some("2026-01-02T03:04:05Z"));
        assert_eq!(repo.language.as_deref(), Some("Rust"));
        assert!(matches!(probes[1], Probe::Gone));
        // Blocked is not gone: no verdict, never a retirement.
        assert!(matches!(probes[2], Probe::Undecided));
    }

    #[test]
    fn a_refused_query_and_an_illegal_name_are_errors_not_verdicts() {
        let refused = r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"slow down"}]}"#;
        let (base, hits) = stub(vec![(200, refused.to_string())]);
        let gh = GitHub::at(&base);

        // An off-charset name is refused before any request is built.
        assert!(gh.repos_batch(&[r#"evil"} x: viewer {login"#]).is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

        let err = gh.repos_batch(&["a/b"]).unwrap_err();
        assert!(err.to_string().contains("slow down"), "{err}");
    }

    #[test]
    fn waits_are_clamped() {
        let h = headers(&[("retry-after", "99999")]);
        assert_eq!(backoff(StatusCode::FORBIDDEN, &h, 0, 0), Some(MAX_BACKOFF));
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "5")]);
        assert_eq!(
            backoff(StatusCode::FORBIDDEN, &h, 0, 1000),
            Some(Duration::from_secs(1))
        );
    }
}
