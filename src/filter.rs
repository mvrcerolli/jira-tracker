//! Per-project issue filters: model, JQL builder, persistence and panel state.
use anyhow::Result;
use ratatui::widgets::ListState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

pub const ME: &str = "@me";
pub const NOBODY: &str = "@none";
/// Max issues fetched per project (Done can be huge).
pub const MAX_ISSUES: usize = 1000;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Choice {
    pub value: String,
    pub label: String,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, PartialEq, Debug)]
#[serde(from = "RawFilter", into = "RawFilter")]
pub struct Filter {
    /// `None` = default: every status except those in the Done category.
    /// `Some(list)` = exactly these statuses (empty = all statuses).
    pub statuses: Option<Vec<Choice>>,
    pub assignees: Vec<Choice>,
    pub priorities: Vec<Choice>,
    pub types: Vec<Choice>,
    pub jql: String,
}

/// On-disk shape. `hide_done = true` with no statuses means the default.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct RawFilter {
    hide_done: Option<bool>,
    statuses: Vec<Choice>,
    assignees: Vec<Choice>,
    priorities: Vec<Choice>,
    types: Vec<Choice>,
    jql: String,
}

impl From<RawFilter> for Filter {
    fn from(r: RawFilter) -> Self {
        let statuses = if r.statuses.is_empty() && r.hide_done != Some(false) { None } else { Some(r.statuses) };
        Filter { statuses, assignees: r.assignees, priorities: r.priorities, types: r.types, jql: r.jql }
    }
}

impl From<Filter> for RawFilter {
    fn from(f: Filter) -> Self {
        RawFilter {
            hide_done: Some(f.statuses.is_none()),
            statuses: f.statuses.unwrap_or_default(),
            assignees: f.assignees,
            priorities: f.priorities,
            types: f.types,
            jql: f.jql,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Group {
    Status,
    Assignee,
    Priority,
    Type,
    Jql,
}

impl Group {
    pub const ALL: [Group; 5] = [Group::Status, Group::Assignee, Group::Priority, Group::Type, Group::Jql];
    pub fn name(self) -> &'static str {
        match self {
            Group::Status => "Status",
            Group::Assignee => "Assignee",
            Group::Priority => "Priority",
            Group::Type => "Type",
            Group::Jql => "JQL",
        }
    }
}

/// Available choices for one project.
#[derive(Default)]
pub struct FilterOpts {
    pub groups: HashMap<Group, Vec<Choice>>,
    /// Status names in the Done category.
    pub done: HashSet<String>,
}

impl FilterOpts {
    /// The statuses the default filter shows (everything not Done).
    fn not_done(&self) -> Vec<Choice> {
        self.groups
            .get(&Group::Status)
            .into_iter()
            .flatten()
            .filter(|c| !self.done.contains(&c.value))
            .cloned()
            .collect()
    }
}

fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn list(v: &[Choice]) -> String {
    v.iter().map(|c| q(&c.value)).collect::<Vec<_>>().join(", ")
}

impl Filter {
    fn group(&self, g: Group) -> &[Choice] {
        match g {
            Group::Status => self.statuses.as_deref().unwrap_or(&[]),
            Group::Assignee => &self.assignees,
            Group::Priority => &self.priorities,
            Group::Type => &self.types,
            Group::Jql => &[],
        }
    }

    pub fn is_ticked(&self, g: Group, value: &str, opts: Option<&FilterOpts>) -> bool {
        if g == Group::Status && self.statuses.is_none() {
            return opts.is_some_and(|o| !o.done.contains(value));
        }
        self.group(g).iter().any(|c| c.value == value)
    }

    pub fn toggle(&mut self, g: Group, c: &Choice, opts: Option<&FilterOpts>) {
        let v = match g {
            Group::Status => {
                // First change turns the default into an explicit list.
                self.statuses.get_or_insert_with(|| opts.map(|o| o.not_done()).unwrap_or_default())
            }
            Group::Assignee => &mut self.assignees,
            Group::Priority => &mut self.priorities,
            Group::Type => &mut self.types,
            Group::Jql => return,
        };
        if let Some(i) = v.iter().position(|x| x.value == c.value) {
            v.remove(i);
        } else {
            v.push(c.clone());
        }
    }

    /// Untick everything in a group.
    pub fn clear(&mut self, g: Group) {
        match g {
            Group::Status => self.statuses = Some(vec![]),
            Group::Assignee => self.assignees.clear(),
            Group::Priority => self.priorities.clear(),
            Group::Type => self.types.clear(),
            Group::Jql => self.jql.clear(),
        }
    }

    /// Back to the default for a group.
    pub fn reset(&mut self, g: Group) {
        match g {
            Group::Status => self.statuses = None,
            _ => self.clear(g),
        }
    }

    pub fn is_default(&self) -> bool {
        *self == Filter::default()
    }

    pub fn summary(&self, g: Group) -> String {
        if g == Group::Jql {
            return if self.jql.trim().is_empty() { "—".into() } else { self.jql.trim().into() };
        }
        if g == Group::Status && self.statuses.is_none() {
            return "All except Done".into();
        }
        let v = self.group(g);
        if v.is_empty() {
            return match g {
                Group::Status => "All".into(),
                Group::Assignee => "Anyone".into(),
                _ => "Any".into(),
            };
        }
        v.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ")
    }

    pub fn is_group_default(&self, g: Group) -> bool {
        let mut d = self.clone();
        d.reset(g);
        d == *self
    }

    /// Short description for titles, e.g. "not done, Assignee".
    pub fn short(&self) -> String {
        let mut parts: Vec<&str> = vec![];
        if self.statuses.is_none() {
            parts.push("not done");
        }
        parts.extend(
            Group::ALL
                .iter()
                .filter(|g| if **g == Group::Jql { !self.jql.trim().is_empty() } else { !self.group(**g).is_empty() })
                .map(|g| g.name()),
        );
        if parts.is_empty() { "all".into() } else { parts.join(", ") }
    }

    pub fn to_jql(&self, project: &str) -> String {
        let mut parts = vec![format!("project = {}", q(project))];
        match &self.statuses {
            None => parts.push("statusCategory != Done".into()),
            Some(v) if !v.is_empty() => parts.push(format!("status in ({})", list(v))),
            Some(_) => {}
        }
        if !self.assignees.is_empty() {
            let mut ors = vec![];
            if self.assignees.iter().any(|c| c.value == ME) {
                ors.push("assignee = currentUser()".to_string());
            }
            if self.assignees.iter().any(|c| c.value == NOBODY) {
                ors.push("assignee is EMPTY".to_string());
            }
            let people: Vec<Choice> =
                self.assignees.iter().filter(|c| c.value != ME && c.value != NOBODY).cloned().collect();
            if !people.is_empty() {
                ors.push(format!("assignee in ({})", list(&people)));
            }
            parts.push(format!("({})", ors.join(" OR ")));
        }
        if !self.priorities.is_empty() {
            parts.push(format!("priority in ({})", list(&self.priorities)));
        }
        if !self.types.is_empty() {
            parts.push(format!("issuetype in ({})", list(&self.types)));
        }
        let extra = self.jql.trim();
        if !extra.is_empty() {
            parts.push(format!("({extra})"));
        }
        format!("{} ORDER BY updated DESC", parts.join(" AND "))
    }
}

// ---------- persistence ----------
fn path() -> PathBuf {
    crate::config::path().with_file_name("filters.toml")
}

pub fn load() -> BTreeMap<String, Filter> {
    std::fs::read_to_string(path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(all: &BTreeMap<String, Filter>) -> Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let keep: BTreeMap<_, _> = all.iter().filter(|(_, f)| !f.is_default()).collect();
    std::fs::write(p, toml::to_string(&keep)?)?;
    Ok(())
}

// ---------- panel state ----------
pub struct FilterPanel {
    pub project: String,
    pub draft: Filter,
    pub group: usize,
    pub in_opts: bool,
    pub search: String,
    pub state: ListState,
}

impl FilterPanel {
    pub fn new(project: String, current: Filter) -> Self {
        Self { project, draft: current, group: 0, in_opts: false, search: String::new(), state: ListState::default() }
    }

    pub fn current_group(&self) -> Group {
        Group::ALL[self.group]
    }

    /// Options of the current group, filtered by the search text.
    /// Ticked values missing from `opts` (e.g. a removed user) are still listed.
    pub fn visible(&self, opts: Option<&FilterOpts>) -> Vec<Choice> {
        let g = self.current_group();
        let mut all: Vec<Choice> = opts.and_then(|o| o.groups.get(&g)).cloned().unwrap_or_default();
        for c in self.draft.group(g) {
            if !all.iter().any(|x| x.value == c.value) {
                all.push(c.clone());
            }
        }
        let s = self.search.to_lowercase();
        all.into_iter().filter(|c| c.label.to_lowercase().contains(&s)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> FilterOpts {
        let mut o = FilterOpts::default();
        o.groups.insert(
            Group::Status,
            ["To Do", "In Progress", "Done"].iter().map(|s| Choice::new(*s, *s)).collect(),
        );
        o.done.insert("Done".into());
        o
    }

    #[test]
    fn default_jql() {
        assert_eq!(
            Filter::default().to_jql("EE"),
            r#"project = "EE" AND statusCategory != Done ORDER BY updated DESC"#
        );
    }

    #[test]
    fn default_ticks_all_but_done() {
        let o = opts();
        let mut f = Filter::default();
        assert!(f.is_ticked(Group::Status, "To Do", Some(&o)));
        assert!(!f.is_ticked(Group::Status, "Done", Some(&o)));
        // ticking Done materializes the list: everything ticked
        f.toggle(Group::Status, &Choice::new("Done", "Done"), Some(&o));
        assert_eq!(
            f.to_jql("EE"),
            r#"project = "EE" AND status in ("To Do", "In Progress", "Done") ORDER BY updated DESC"#
        );
        f.clear(Group::Status);
        assert_eq!(f.to_jql("EE"), r#"project = "EE" ORDER BY updated DESC"#);
        assert_eq!(f.short(), "all");
        f.reset(Group::Status);
        assert!(f.is_default());
    }

    #[test]
    fn full_jql() {
        let f = Filter {
            statuses: Some(vec![Choice::new("To Do", "To Do"), Choice::new("Done", "Done")]),
            assignees: vec![Choice::new(ME, "Me"), Choice::new(NOBODY, "Unassigned"), Choice::new("abc", "Ana")],
            priorities: vec![Choice::new("High", "High")],
            types: vec![Choice::new("Bug", "Bug")],
            jql: " labels = x ".into(),
        };
        assert_eq!(
            f.to_jql("EE"),
            r#"project = "EE" AND status in ("To Do", "Done") AND (assignee = currentUser() OR assignee is EMPTY OR assignee in ("abc")) AND priority in ("High") AND issuetype in ("Bug") AND (labels = x) ORDER BY updated DESC"#
        );
        assert_eq!(f.short(), "Status, Assignee, Priority, Type, JQL");
    }

    #[test]
    fn roundtrip_toml() {
        for f in [
            Filter { types: vec![Choice::new("Bug", "Bug")], ..Default::default() },
            Filter { statuses: Some(vec![]), ..Default::default() },
            Filter { statuses: Some(vec![Choice::new("Done", "Done")]), ..Default::default() },
        ] {
            let mut m = BTreeMap::new();
            m.insert("EE".to_string(), f);
            let s = toml::to_string(&m).unwrap();
            let back: BTreeMap<String, Filter> = toml::from_str(&s).unwrap();
            assert_eq!(back, m, "{s}");
        }
        // older files
        let old: Filter = toml::from_str("jql = \"\"\nstatuses = []").unwrap();
        assert!(old.statuses.is_none());
        let old: Filter = toml::from_str("hide_done = false").unwrap();
        assert_eq!(old.statuses, Some(vec![]));
    }
}
