//! On-disk cache so jt opens instantly with the last known data.
use crate::app::{Opt, TransMap};
use crate::filter::FilterOpts;
use crate::github::Pr;
use crate::jira::Detail;
use crate::jira::{Issue, Project};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone)]
pub struct CachedList {
    /// view (project key or My work) the list belongs to
    #[serde(default)]
    pub view: String,
    /// JQL the list was fetched with; ignored if the filter changed since.
    pub jql: String,
    pub issues: Vec<Issue>,
    /// Unix seconds.
    pub at: u64,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Cache {
    /// Jira site the data belongs to.
    pub base_url: String,
    pub projects: Vec<Project>,
    /// jql -> list (one entry per filter combination used)
    pub lists: HashMap<String, CachedList>,
    /// issue key -> `updated` value when the user last looked at it
    pub seen: HashMap<String, String>,
    /// views whose issues have a "seen" baseline (avoids highlighting everything on first load)
    pub baselined: HashSet<String>,
    /// view -> open pull requests
    pub prs: HashMap<String, Vec<Pr>>,
    /// filter panel choices per view
    pub filter_opts: HashMap<String, FilterOpts>,
    /// issue key -> detail
    pub details: HashMap<String, Detail>,
    /// status popup choices: "status:KEY@status,..." -> (options, transitions)
    pub options: HashMap<String, (Vec<Opt>, TransMap)>,
    /// shared by filter panel and edit popups: (id, name)
    pub priorities: Vec<(String, String)>,
    /// project -> assignable users (accountId, name), shared by filter panel and edit popups
    pub users: HashMap<String, Vec<(String, String)>>,
    /// (accountId, displayName) of the logged-in Jira user
    pub me: Option<(String, String)>,
}

/// Keep at most this many cached lists (newest first).
const MAX_LISTS: usize = 40;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn path() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("jt/cache.json")
}

pub fn load(base_url: &str) -> Cache {
    let mut c: Cache = std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    if c.base_url != base_url {
        return Cache { base_url: base_url.into(), ..Default::default() };
    }
    // older caches were keyed by view: re-key by jql
    c.lists = c
        .lists
        .into_iter()
        .map(|(k, mut l)| {
            if l.view.is_empty() {
                l.view = k;
            }
            (l.jql.clone(), l)
        })
        .collect();
    c
}

pub fn save(c: &mut Cache) {
    if c.lists.len() > MAX_LISTS {
        let mut ats: Vec<u64> = c.lists.values().map(|l| l.at).collect();
        ats.sort_unstable_by(|a, b| b.cmp(a));
        let cutoff = ats[MAX_LISTS - 1];
        c.lists.retain(|_, l| l.at >= cutoff);
    }
    let p = path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(b) = serde_json::to_vec(c) {
        // write then rename, so a crash never leaves a half-written cache
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, b).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }
}

/// "just now", "5m ago", "3h ago", "2d ago"
pub fn ago(at: u64) -> String {
    let s = now().saturating_sub(at);
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}
