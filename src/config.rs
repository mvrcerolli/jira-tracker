use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Deserialize, Clone)]
pub struct Config {
    pub base_url: String,
    pub email: String,
    #[serde(default)]
    pub api_token: String,
    /// Optional: only show these project keys (in this order).
    #[serde(default)]
    pub projects: Vec<String>,
    /// Auto-refresh interval in minutes (0 = off).
    #[serde(default = "default_refresh")]
    pub refresh_minutes: u64,
    /// GitHub token (optional; falls back to GITHUB_TOKEN, then `gh auth token`).
    #[serde(default)]
    pub github_token: Option<String>,
    /// Jira project key -> GitHub repo(s) "owner/name".
    #[serde(default)]
    pub github: HashMap<String, Repos>,
}

#[derive(Deserialize, Clone)]
#[serde(untagged)]
pub enum Repos {
    One(String),
    Many(Vec<String>),
}

impl Config {
    /// Repos mapped to a Jira project (case-insensitive key).
    pub fn repos_for(&self, project: &str) -> Vec<String> {
        self.github
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(project))
            .flat_map(|(_, r)| match r {
                Repos::One(s) => vec![s.clone()],
                Repos::Many(v) => v.clone(),
            })
            .collect()
    }

    /// Every mapped repo, deduplicated.
    pub fn all_repos(&self) -> Vec<String> {
        let mut out: Vec<String> = vec![];
        for r in self.github.values() {
            for s in match r {
                Repos::One(s) => vec![s.clone()],
                Repos::Many(v) => v.clone(),
            } {
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out.sort();
        out
    }
}

fn default_refresh() -> u64 {
    5
}

pub const EXAMPLE: &str = r#"base_url  = "https://your-site.atlassian.net"
email     = "you@example.com"
api_token = "..."            # or set JIRA_API_TOKEN
# projects = ["ABC", "XYZ"]  # optional: limit/pin projects
# refresh_minutes = 5        # auto-refresh interval, 0 = off

# GitHub pull requests per project (optional, read-only)
# github_token = "..."       # optional; else GITHUB_TOKEN, else `gh auth token`
# [github]
# ABC = "my-org/abc-repo"
# XYZ = ["my-org/xyz-api", "my-org/xyz-web"]
"#;

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("jt/config.toml")
}

pub fn load() -> Result<Config> {
    let p = path();
    if !p.exists() {
        // Create a template so the user only has to fill it in.
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if std::fs::write(&p, EXAMPLE).is_ok() {
            // the file will hold an API token: keep it private
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
            }
            if cfg!(windows) {
                let _ = open::that_detached(&p); // opens in Notepad
            }
            bail!("Created config template at:\n  {}\n\nFill in base_url, email and api_token, save it, then run jt again.", p.display());
        }
        bail!("No config found. Create {} with:\n\n{EXAMPLE}", p.display());
    }
    let raw = std::fs::read_to_string(&p).with_context(|| format!("reading {}", p.display()))?;
    let mut cfg: Config = toml::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?;
    if let Ok(t) = std::env::var("JIRA_API_TOKEN") {
        cfg.api_token = t;
    }
    cfg.base_url = cfg.base_url.trim_end_matches('/').to_string();
    if !cfg.base_url.starts_with("https://") {
        bail!("base_url must start with https:// (the API token would be sent unencrypted)\nEdit: {}", p.display());
    }
    if cfg.base_url.contains("your-site") || cfg.email == "you@example.com" || cfg.api_token == "..." {
        bail!("Config still has the example values. Edit:\n  {}", p.display());
    }
    if cfg.api_token.is_empty() {
        bail!("api_token missing: set it in {} or JIRA_API_TOKEN", p.display());
    }
    Ok(cfg)
}

fn pins_path() -> PathBuf {
    path().with_file_name("pinned.txt")
}

/// Pinned project keys, in pin order.
pub fn load_pins() -> Vec<String> {
    std::fs::read_to_string(pins_path())
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

pub fn save_pins(pins: &[String]) -> Result<()> {
    let p = pins_path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, pins.join("\n") + "\n")?;
    Ok(())
}
