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

#[derive(Debug, Deserialize)]
pub struct Repo {
    pub id: i64,
    pub full_name: String,
    pub created_at: String,
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
}

#[derive(Deserialize)]
struct SearchPage {
    items: Vec<SearchItem>,
}

pub struct GitHub {
    http: Client,
    token: String,
}

impl GitHub {
    pub fn from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .context("GITHUB_TOKEN is not set; the GitHub API needs an authenticated identity")?;
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(GitHub { http, token })
    }

    /// `None` means the repo is gone from our vantage point: deleted, renamed, or private.
    pub fn repo(&self, full_name: &str) -> Result<Option<Repo>> {
        let url = format!("{API}/repos/{full_name}");
        match self.get(&url, &[])? {
            Some(resp) => Ok(Some(
                resp.json().with_context(|| format!("decoding {url}"))?,
            )),
            None => Ok(None),
        }
    }

    pub fn search_repositories(&self, query: &str, page: u32) -> Result<Vec<SearchItem>> {
        let url = format!("{API}/search/repositories");
        let params = [
            ("q", query.to_string()),
            ("sort", "stars".to_string()),
            ("order", "desc".to_string()),
            ("per_page", "100".to_string()),
            ("page", page.to_string()),
        ];
        let Some(resp) = self.get(&url, &params)? else {
            bail!("search/repositories returned 404 for q={query}");
        };
        let page: SearchPage = resp.json().context("decoding search/repositories")?;
        Ok(page.items)
    }

    fn get(&self, url: &str, params: &[(&str, String)]) -> Result<Option<Response>> {
        for attempt in 0..MAX_ATTEMPTS {
            let resp = self
                .http
                .get(url)
                .query(params)
                .bearer_auth(&self.token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .send()
                .with_context(|| format!("GET {url}"))?;

            let status = resp.status();
            if status.is_success() {
                return Ok(Some(resp));
            }
            if status == StatusCode::NOT_FOUND {
                return Ok(None);
            }
            if let Some(wait) = backoff(status, resp.headers(), attempt, now_unix()) {
                eprintln!(
                    "github: {status} on {url}, waiting {}s",
                    wait.as_secs().max(1)
                );
                std::thread::sleep(wait);
                continue;
            }
            let body = resp.text().unwrap_or_default();
            bail!(
                "GET {url} -> {status}: {}",
                body.trim().chars().take(300).collect::<String>()
            );
        }
        bail!("GET {url}: still rate-limited after {MAX_ATTEMPTS} attempts")
    }
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
