use crate::config::Config;
use anyhow::{Result, bail};
use reqwest::Method;
use reqwest::blocking::{Client as Http, Response};
use serde_json::{Value, json};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Client {
    http: Http,
    cfg: Config,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Cat {
    Todo,
    Doing,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub key: String,
    pub summary: String,
    pub status: String,
    pub cat: Cat,
    pub assignee: String,
    pub priority: String,
    /// Raw Jira timestamp; used to detect changes.
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub issuetype: String,
    /// accountId ("" = unassigned)
    #[serde(default)]
    pub assignee_id: String,
}

// --- raw API shapes ---
#[derive(Deserialize)]
struct ProjectPage {
    values: Vec<Project>,
    #[serde(rename = "isLast", default = "yes")]
    is_last: bool,
}
fn yes() -> bool {
    true
}

#[derive(Deserialize)]
struct SearchPage {
    issues: Vec<RawIssue>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}
#[derive(Deserialize)]
struct RawIssue {
    key: String,
    fields: RawFields,
}
#[derive(Deserialize)]
struct RawFields {
    summary: Option<String>,
    status: Option<RawStatus>,
    assignee: Option<Named>,
    priority: Option<NameOnly>,
    updated: Option<String>,
    issuetype: Option<NameOnly>,
}
#[derive(Deserialize)]
struct NameOnly {
    name: String,
}
#[derive(Deserialize)]
struct RawStatus {
    name: String,
    #[serde(rename = "statusCategory")]
    category: Option<RawCat>,
}
#[derive(Deserialize)]
struct RawCat {
    key: String,
}
#[derive(Deserialize)]
struct Named {
    #[serde(rename = "displayName")]
    display_name: String,
    #[serde(rename = "accountId", default)]
    account_id: String,
}

impl From<RawIssue> for Issue {
    fn from(r: RawIssue) -> Self {
        let f = r.fields;
        let (status, cat) = match f.status {
            Some(s) => {
                let cat = match s.category.map(|c| c.key).as_deref() {
                    Some("indeterminate") => Cat::Doing,
                    Some("done") => Cat::Done,
                    _ => Cat::Todo,
                };
                (s.name, cat)
            }
            None => (String::new(), Cat::Todo),
        };
        Issue {
            key: r.key,
            summary: f.summary.unwrap_or_default(),
            status,
            cat,
            assignee_id: f.assignee.as_ref().map(|a| a.account_id.clone()).unwrap_or_default(),
            assignee: f.assignee.map(|a| a.display_name).unwrap_or_else(|| "—".into()),
            issuetype: f.issuetype.map(|t| t.name).unwrap_or_default(),
            priority: f.priority.map(|p| p.name).unwrap_or_default(),
            updated: f.updated.unwrap_or_default(),
        }
    }
}

impl Client {
    pub fn new(cfg: Config) -> Result<Self> {
        let http = Http::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { http, cfg })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn base_url(&self) -> &str {
        &self.cfg.base_url
    }

    pub fn refresh_minutes(&self) -> u64 {
        self.cfg.refresh_minutes
    }

    pub fn browse_url(&self, key: &str) -> String {
        format!("{}/browse/{key}", self.cfg.base_url)
    }

    fn req(&self, method: Method, path: &str) -> reqwest::blocking::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.cfg.base_url))
            .basic_auth(&self.cfg.email, Some(&self.cfg.api_token))
            .header("Accept", "application/json")
    }

    fn check(res: Response) -> Result<Response> {
        let status = res.status();
        if status.is_success() {
            return Ok(res);
        }
        let body = res.text().unwrap_or_default();
        // Prefer Jira's own error messages when present.
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .map(|v| {
                let mut m: Vec<String> = v["errorMessages"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                if let Some(o) = v["errors"].as_object() {
                    m.extend(o.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or(""))));
                }
                m.join("; ")
            })
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| body.chars().take(200).collect());
        match status.as_u16() {
            401 => bail!("401 Unauthorized: check email/api_token"),
            _ => bail!("HTTP {status}: {msg}"),
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str, query: &[(&str, String)]) -> Result<T> {
        Ok(Self::check(self.req(Method::GET, path).query(query).send()?)?.json()?)
    }

    fn send(&self, method: Method, path: &str, body: Value) -> Result<()> {
        Self::check(self.req(method, path).json(&body).send()?)?;
        Ok(())
    }

    pub fn projects(&self) -> Result<Vec<Project>> {
        let mut out = Vec::new();
        loop {
            let page: ProjectPage = self.get(
                "/rest/api/3/project/search",
                &[("startAt", out.len().to_string()), ("maxResults", "100".into()), ("orderBy", "key".into())],
            )?;
            let n = page.values.len();
            out.extend(page.values);
            if page.is_last || n == 0 {
                break;
            }
        }
        if !self.cfg.projects.is_empty() {
            let pinned = &self.cfg.projects;
            out.retain(|p| pinned.iter().any(|k| k.eq_ignore_ascii_case(&p.key)));
            out.sort_by_key(|p| pinned.iter().position(|k| k.eq_ignore_ascii_case(&p.key)));
        }
        Ok(out)
    }

    /// Issues matching `jql`, capped at `filter::MAX_ISSUES`.
    pub fn issues(&self, jql: &str) -> Result<Vec<Issue>> {
        let jql = jql.to_string();
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("jql", jql.clone()),
                ("fields", "summary,status,assignee,priority,updated,issuetype".into()),
                ("maxResults", "100".into()),
            ];
            if let Some(t) = &token {
                q.push(("nextPageToken", t.clone()));
            }
            let page: SearchPage = self.get("/rest/api/3/search/jql", &q)?;
            out.extend(page.issues.into_iter().map(Issue::from));
            if out.len() >= crate::filter::MAX_ISSUES {
                out.truncate(crate::filter::MAX_ISSUES);
                break;
            }
            match page.next_page_token {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => break,
            }
        }
        Ok(out)
    }

    // ---------- detail ----------
    pub fn detail(&self, key: &str) -> Result<Detail> {
        let v: Value = self.get(
            &format!("/rest/api/3/issue/{key}"),
            &[(
                "fields",
                "summary,status,issuetype,priority,assignee,reporter,labels,created,updated,parent,description,comment"
                    .into(),
            )],
        )?;
        Ok(Detail::from_json(&v))
    }

    // ---------- filter options ----------
    /// (status name, is Done category) used in a project, ordered To Do → In Progress → Done.
    pub fn statuses(&self, project: &str) -> Result<Vec<(String, bool)>> {
        let v: Vec<Value> = self.get(&format!("/rest/api/3/project/{project}/statuses"), &[])?;
        let mut out: Vec<(u8, String)> = vec![];
        for t in &v {
            for st in t["statuses"].as_array().into_iter().flatten() {
                let name = s(&st["name"]);
                let rank = match st["statusCategory"]["key"].as_str() {
                    Some("indeterminate") => 1,
                    Some("done") => 2,
                    _ => 0,
                };
                if !out.iter().any(|(_, n)| *n == name) {
                    out.push((rank, name));
                }
            }
        }
        out.sort_by_key(|(r, _)| *r);
        Ok(out.into_iter().map(|(r, n)| (n, r == 2)).collect())
    }

    /// All statuses on the site: (name, is Done category), To Do → Done.
    pub fn all_statuses(&self) -> Result<Vec<(String, bool)>> {
        let v: Vec<Value> = self.get("/rest/api/3/status", &[])?;
        let mut out: Vec<(u8, String)> = vec![];
        for st in &v {
            let name = s(&st["name"]);
            let rank = match st["statusCategory"]["key"].as_str() {
                Some("indeterminate") => 1,
                Some("done") => 2,
                _ => 0,
            };
            if !out.iter().any(|(_, n)| *n == name) {
                out.push((rank, name));
            }
        }
        out.sort_by_key(|(r, _)| *r);
        Ok(out.into_iter().map(|(r, n)| (n, r == 2)).collect())
    }

    /// All issue type names on the site.
    pub fn all_issue_types(&self) -> Result<Vec<String>> {
        let v: Vec<Value> = self.get("/rest/api/3/issuetype", &[])?;
        let mut out: Vec<String> = vec![];
        for t in &v {
            let n = s(&t["name"]);
            if !out.contains(&n) {
                out.push(n);
            }
        }
        out.sort();
        Ok(out)
    }

    /// Issue type names of a project.
    pub fn issue_types(&self, project: &str) -> Result<Vec<String>> {
        let v: Value = self.get(&format!("/rest/api/3/project/{project}"), &[])?;
        let mut out: Vec<String> = vec![];
        for t in v["issueTypes"].as_array().into_iter().flatten() {
            let n = s(&t["name"]);
            if !out.contains(&n) {
                out.push(n);
            }
        }
        Ok(out)
    }

    // ---------- edits ----------
    /// (transition id, transition name, target status name)
    pub fn transitions(&self, key: &str) -> Result<Vec<(String, String, String)>> {
        let v: Value = self.get(&format!("/rest/api/3/issue/{key}/transitions"), &[])?;
        Ok(v["transitions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|t| (s(&t["id"]), s(&t["name"]), s(&t["to"]["name"])))
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn transition(&self, key: &str, id: &str) -> Result<()> {
        self.send(
            Method::POST,
            &format!("/rest/api/3/issue/{key}/transitions"),
            json!({ "transition": { "id": id } }),
        )
    }

    /// (accountId, displayName)
    pub fn myself(&self) -> Result<(String, String)> {
        let v: Value = self.get("/rest/api/3/myself", &[])?;
        Ok((s(&v["accountId"]), s(&v["displayName"])))
    }

    /// (accountId, displayName) of users assignable in a project.
    pub fn assignable(&self, project: &str) -> Result<Vec<(String, String)>> {
        let v: Vec<Value> = self.get(
            "/rest/api/3/user/assignable/search",
            &[("project", project.into()), ("maxResults", "1000".into())],
        )?;
        let mut out: Vec<_> = v
            .iter()
            .filter(|u| u["active"].as_bool().unwrap_or(true))
            .map(|u| (s(&u["accountId"]), s(&u["displayName"])))
            .collect();
        out.sort_by_key(|(_, n)| n.to_lowercase());
        Ok(out)
    }


    pub fn assign(&self, key: &str, account_id: Option<&str>) -> Result<()> {
        self.send(
            Method::PUT,
            &format!("/rest/api/3/issue/{key}/assignee"),
            json!({ "accountId": account_id }),
        )
    }

    /// (id, name)
    pub fn priorities(&self) -> Result<Vec<(String, String)>> {
        let v: Value = self.get("/rest/api/3/priority/search", &[("maxResults", "100".into())])?;
        Ok(v["values"]
            .as_array()
            .map(|a| a.iter().map(|p| (s(&p["id"]), s(&p["name"]))).collect())
            .unwrap_or_default())
    }

    pub fn set_priority(&self, key: &str, id: &str) -> Result<()> {
        self.send(
            Method::PUT,
            &format!("/rest/api/3/issue/{key}"),
            json!({ "fields": { "priority": { "id": id } } }),
        )
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn date(v: &Value) -> String {
    v.as_str().map(|d| d.chars().take(16).collect::<String>().replace('T', " ")).unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub created: String,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detail {
    pub key: String,
    pub summary: String,
    pub issuetype: String,
    pub status: String,
    pub priority: String,
    pub assignee: String,
    pub reporter: String,
    pub labels: Vec<String>,
    pub created: String,
    pub updated: String,
    pub parent: Option<String>,
    pub description: String,
    pub comments: Vec<Comment>,
}

impl Detail {
    pub fn from_json(v: &Value) -> Self {
        let f = &v["fields"];
        let name_or = |x: &Value, k: &str, d: &str| x[k].as_str().unwrap_or(d).to_string();
        let parent = f["parent"]["key"].as_str().map(|k| {
            let sum = f["parent"]["fields"]["summary"].as_str().unwrap_or("");
            format!("{k} {sum}")
        });
        let mut comments: Vec<Comment> = f["comment"]["comments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| Comment {
                        author: name_or(&c["author"], "displayName", "?"),
                        created: date(&c["created"]),
                        body: adf_to_text(&c["body"]),
                    })
                    .collect()
            })
            .unwrap_or_default();
        comments.reverse(); // newest first
        Detail {
            key: s(&v["key"]),
            summary: s(&f["summary"]),
            issuetype: name_or(&f["issuetype"], "name", ""),
            status: name_or(&f["status"], "name", ""),
            priority: name_or(&f["priority"], "name", "—"),
            assignee: name_or(&f["assignee"], "displayName", "Unassigned"),
            reporter: name_or(&f["reporter"], "displayName", "—"),
            labels: f["labels"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            created: date(&f["created"]),
            updated: date(&f["updated"]),
            parent,
            description: adf_to_text(&f["description"]),
            comments,
        }
    }
}

/// Convert Atlassian Document Format to readable plain text.
pub fn adf_to_text(v: &Value) -> String {
    if let Some(t) = v.as_str() {
        return t.to_string(); // already plain (older API / null-safe)
    }
    let mut out = String::new();
    walk(v, &mut out, 0);
    let mut s = out.trim().to_string();
    while s.contains("\n\n\n") {
        s = s.replace("\n\n\n", "\n\n");
    }
    s
}

fn children(v: &Value, out: &mut String, depth: usize) {
    if let Some(c) = v["content"].as_array() {
        for n in c {
            walk(n, out, depth);
        }
    }
}

fn walk(v: &Value, out: &mut String, depth: usize) {
    let a = &v["attrs"];
    match v["type"].as_str().unwrap_or("") {
        "text" => out.push_str(v["text"].as_str().unwrap_or("")),
        "hardBreak" => out.push('\n'),
        "mention" => out.push_str(a["text"].as_str().unwrap_or("@user")),
        "emoji" => out.push_str(a["text"].as_str().or(a["shortName"].as_str()).unwrap_or("")),
        "inlineCard" | "blockCard" | "embedCard" => out.push_str(a["url"].as_str().unwrap_or("[link]")),
        "status" => out.push_str(&format!("[{}]", a["text"].as_str().unwrap_or(""))),
        "date" => out.push_str(a["timestamp"].as_str().unwrap_or("")),
        "mediaSingle" | "mediaGroup" | "media" => out.push_str("[attachment]\n"),
        "rule" => out.push_str("────────\n"),
        "heading" => {
            out.push_str("\n## ");
            children(v, out, depth);
            out.push_str("\n\n");
        }
        "paragraph" => {
            children(v, out, depth);
            out.push('\n');
            if depth == 0 {
                out.push('\n');
            }
        }
        "codeBlock" => {
            out.push_str("```\n");
            children(v, out, depth);
            out.push_str("\n```\n\n");
        }
        "blockquote" => {
            let mut inner = String::new();
            children(v, &mut inner, depth + 1);
            for l in inner.trim_end().lines() {
                out.push_str(&format!("│ {l}\n"));
            }
            out.push('\n');
        }
        "bulletList" | "orderedList" => {
            let ordered = v["type"] == "orderedList";
            for (i, item) in v["content"].as_array().into_iter().flatten().enumerate() {
                out.push_str(&"  ".repeat(depth));
                out.push_str(&if ordered { format!("{}. ", i + 1) } else { "• ".into() });
                children(item, out, depth + 1);
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            if depth == 0 {
                out.push('\n');
            }
        }
        "table" => {
            for row in v["content"].as_array().into_iter().flatten() {
                let cells: Vec<String> = row["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|c| {
                        let mut t = String::new();
                        children(c, &mut t, 1);
                        t.trim().replace('\n', " ")
                    })
                    .collect();
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
            out.push('\n');
        }
        _ => children(v, out, depth),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_search_page() {
        let json = r#"{
          "issues": [
            {"key":"ABC-1","fields":{"summary":"Fix login","updated":"2026-11-20T10:00:00.000+0000",
              "status":{"name":"In Progress","statusCategory":{"key":"indeterminate"}},
              "assignee":{"displayName":"Ana"}}},
            {"key":"ABC-2","fields":{"summary":"Docs","status":{"name":"To Do","statusCategory":{"key":"new"}},"assignee":null}}
          ],
          "isLast": true
        }"#;
        let page: SearchPage = serde_json::from_str(json).unwrap();
        assert!(page.next_page_token.is_none());
        let issues: Vec<Issue> = page.issues.into_iter().map(Issue::from).collect();
        assert_eq!(issues[0].key, "ABC-1");
        assert_eq!(issues[0].cat, Cat::Doing);
        assert_eq!(issues[0].assignee, "Ana");
        assert_eq!(issues[1].cat, Cat::Todo);
        assert_eq!(issues[1].assignee, "—");
    }

    #[test]
    fn adf_renders() {
        let v: Value = serde_json::from_str(r#"{"type":"doc","content":[
          {"type":"paragraph","content":[{"type":"text","text":"Hello "},{"type":"mention","attrs":{"text":"@Ana"}}]},
          {"type":"bulletList","content":[
            {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"one"}]}]},
            {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"two"}]}]}]}
        ]}"#).unwrap();
        assert_eq!(adf_to_text(&v), "Hello @Ana\n\n• one\n• two");
        assert_eq!(adf_to_text(&Value::Null), "");
    }
}
