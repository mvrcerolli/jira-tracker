//! Read-only GitHub client: open pull requests of the repos mapped to each Jira project.
use anyhow::{Context, Result, anyhow, bail};
use reqwest::blocking::Client as Http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Review {
    Required,
    Approved,
    Changes,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Checks {
    Pass,
    Fail,
    Pending,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pr {
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub branch: String,
    pub draft: bool,
    pub created_at: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    pub review: Review,
    pub checks: Checks,
    /// A review is requested from me (directly, not via team).
    pub wants_me: bool,
    /// I opened it.
    pub mine: bool,
    /// Jira issue key found in the branch name or title.
    pub issue: Option<String>,
    pub body: String,
}

#[derive(Clone)]
pub struct GitHub {
    http: Http,
    config_token: Option<String>,
    token: Arc<OnceLock<Result<String, String>>>,
}

const QUERY: &str = r#"
query($owner: String!, $name: String!) {
  viewer { login }
  repository(owner: $owner, name: $name) {
    pullRequests(states: OPEN, first: 100, orderBy: {field: UPDATED_AT, direction: DESC}) {
      nodes {
        number title url isDraft createdAt headRefName additions deletions changedFiles
        bodyText
        author { login }
        reviewDecision
        reviewRequests(first: 20) { nodes { requestedReviewer { ... on User { login } } } }
        commits(last: 1) { nodes { commit { statusCheckRollup { state } } } }
      }
    }
  }
}"#;

impl GitHub {
    pub fn new(config_token: Option<String>) -> Result<Self> {
        let http = Http::builder().timeout(std::time::Duration::from_secs(30)).user_agent("jt").build()?;
        Ok(Self { http, config_token, token: Arc::new(OnceLock::new()) })
    }

    /// config `github_token` → env GITHUB_TOKEN → `gh auth token` (resolved once).
    fn token(&self) -> Result<String> {
        self.token
            .get_or_init(|| {
                if let Some(t) = self.config_token.clone().filter(|t| !t.is_empty()) {
                    return Ok(t);
                }
                if let Ok(t) = std::env::var("GITHUB_TOKEN")
                    && !t.is_empty()
                {
                    return Ok(t);
                }
                let out = std::process::Command::new("gh")
                    .args(["auth", "token"])
                    .output()
                    .map_err(|_| "no GitHub token: set github_token in the config or install/login gh".to_string())?;
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if t.is_empty() {
                    return Err("no GitHub token: `gh auth token` returned nothing (run `gh auth login`)".into());
                }
                Ok(t)
            })
            .clone()
            .map_err(|e| anyhow!(e))
    }

    /// Open PRs of `repos` ("owner/name"), newest activity first.
    /// `known_keys` are Jira project keys used to recognise issue keys.
    pub fn prs(&self, repos: &[String], known_keys: &[String]) -> Result<Vec<Pr>> {
        let token = self.token()?;
        let mut out = vec![];
        for repo in repos {
            let (owner, name) = repo.split_once('/').ok_or_else(|| anyhow!("bad repo \"{repo}\" (use owner/name)"))?;
            let res = self
                .http
                .post("https://api.github.com/graphql")
                .bearer_auth(&token)
                .json(&json!({ "query": QUERY, "variables": { "owner": owner, "name": name } }))
                .send()?;
            let status = res.status();
            if status.as_u16() == 401 {
                bail!("GitHub 401: token rejected");
            }
            let v: Value = res.json().with_context(|| format!("GitHub HTTP {status}"))?;
            if let Some(errs) = v["errors"].as_array() {
                let msg: Vec<&str> = errs.iter().filter_map(|e| e["message"].as_str()).collect();
                bail!("{repo}: {}", msg.join("; "));
            }
            let me = v["data"]["viewer"]["login"].as_str().unwrap_or("").to_string();
            for n in v["data"]["repository"]["pullRequests"]["nodes"].as_array().into_iter().flatten() {
                out.push(parse_pr(repo, n, &me, known_keys));
            }
        }
        Ok(out)
    }
}

fn st(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn parse_pr(repo: &str, n: &Value, me: &str, known: &[String]) -> Pr {
    let author = n["author"]["login"].as_str().unwrap_or("ghost").to_string();
    let branch = st(&n["headRefName"]);
    let title = st(&n["title"]);
    let wants_me = n["reviewRequests"]["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|r| r["requestedReviewer"]["login"].as_str() == Some(me));
    let review = match n["reviewDecision"].as_str() {
        Some("APPROVED") => Review::Approved,
        Some("CHANGES_REQUESTED") => Review::Changes,
        Some("REVIEW_REQUIRED") => Review::Required,
        _ => Review::None,
    };
    let checks = match n["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["state"].as_str() {
        Some("SUCCESS") => Checks::Pass,
        Some("FAILURE") | Some("ERROR") => Checks::Fail,
        Some("PENDING") | Some("EXPECTED") => Checks::Pending,
        _ => Checks::None,
    };
    let issue = find_issue_key(&branch, known).or_else(|| find_issue_key(&title, known));
    Pr {
        repo: repo.to_string(),
        number: n["number"].as_u64().unwrap_or(0),
        url: st(&n["url"]),
        draft: n["isDraft"].as_bool().unwrap_or(false),
        created_at: st(&n["createdAt"]),
        additions: n["additions"].as_u64().unwrap_or(0),
        deletions: n["deletions"].as_u64().unwrap_or(0),
        files: n["changedFiles"].as_u64().unwrap_or(0),
        body: st(&n["bodyText"]).chars().take(2000).collect(),
        mine: author == me,
        author,
        branch,
        title,
        review,
        checks,
        wants_me,
        issue,
    }
}

/// First "KEY-123" in `s` (case-insensitive) whose KEY is a known Jira project.
pub fn find_issue_key(s: &str, known: &[String]) -> Option<String> {
    let up: Vec<char> = s.to_uppercase().chars().collect();
    let mut i = 0;
    while i < up.len() {
        let start_ok = i == 0 || !up[i - 1].is_ascii_alphanumeric();
        if start_ok && up[i].is_ascii_alphabetic() {
            let mut j = i;
            while j < up.len() && up[j].is_ascii_alphanumeric() {
                j += 1;
            }
            if j < up.len() && up[j] == '-' {
                let mut k = j + 1;
                while k < up.len() && up[k].is_ascii_digit() {
                    k += 1;
                }
                let key: String = up[i..j].iter().collect();
                if k > j + 1 && known.iter().any(|p| p.eq_ignore_ascii_case(&key)) {
                    return Some(format!("{key}-{}", up[j + 1..k].iter().collect::<String>()));
                }
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// Whole days since an RFC 3339 UTC timestamp, as "3d", "5h", "12m".
pub fn age(ts: &str) -> String {
    let p = |a: usize, b: usize| ts.get(a..b).and_then(|x| x.parse::<i64>().ok());
    let (Some(y), Some(mo), Some(d), Some(h), Some(mi)) = (p(0, 4), p(5, 7), p(8, 10), p(11, 13), p(14, 16)) else {
        return String::new();
    };
    // days from civil (Howard Hinnant)
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if mo > 2 { mo - 3 } else { mo + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let then = days * 86400 + h * 3600 + mi * 60;
    let s = (crate::cache::now() as i64 - then).max(0);
    match s {
        0..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_keys() {
        let k = vec!["TSM".to_string(), "TX".to_string()];
        assert_eq!(find_issue_key("feat/tsm-220-cache", &k), Some("TSM-220".into()));
        assert_eq!(find_issue_key("TX-12: fix", &k), Some("TX-12".into()));
        assert_eq!(find_issue_key("bump UTF-8 lib", &k), None);
        assert_eq!(find_issue_key("ATSM-1", &k), None);
        assert_eq!(find_issue_key("fix/ci", &k), None);
    }

    #[test]
    fn parses_pr() {
        let n: Value = serde_json::from_str(r#"{
          "number": 7, "title": "Add cache", "url": "u", "isDraft": false, "createdAt": "2026-11-20T10:00:00Z",
          "headRefName": "feat/TSM-9-cache", "additions": 10, "deletions": 2, "changedFiles": 3, "bodyText": "b",
          "author": {"login": "ana"}, "reviewDecision": "CHANGES_REQUESTED",
          "reviewRequests": {"nodes": [{"requestedReviewer": {"login": "me"}}, {"requestedReviewer": {}}]},
          "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}
        }"#).unwrap();
        let p = parse_pr("o/r", &n, "me", &["TSM".into()]);
        assert!(p.wants_me && !p.mine);
        assert_eq!(p.review, Review::Changes);
        assert_eq!(p.checks, Checks::Fail);
        assert_eq!(p.issue.as_deref(), Some("TSM-9"));
        assert!(age("2026-11-20T10:00:00Z").ends_with('d'));
    }
}
