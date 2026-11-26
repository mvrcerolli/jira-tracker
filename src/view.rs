//! Table layout the user chooses: visible columns, their order, and label priority.
//! Stored next to the config in `view.toml` (hand-editable).
use ratatui::widgets::ListState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum Col {
    Key,
    Pr,
    Status,
    Priority,
    Assignee,
    Type,
    Labels,
    Summary,
}

impl Col {
    pub const ALL: [Col; 8] =
        [Col::Key, Col::Pr, Col::Status, Col::Priority, Col::Assignee, Col::Type, Col::Labels, Col::Summary];

    pub fn name(self) -> &'static str {
        match self {
            Col::Key => "Key",
            Col::Pr => "PR",
            Col::Status => "Status",
            Col::Priority => "Priority",
            Col::Assignee => "Assignee",
            Col::Type => "Type",
            Col::Labels => "Labels",
            Col::Summary => "Summary",
        }
    }

    pub fn parse(s: &str) -> Option<Col> {
        Col::ALL.into_iter().find(|c| c.name().eq_ignore_ascii_case(s.trim()))
    }

    pub fn sortable(self) -> bool {
        self != Col::Pr
    }
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct View {
    /// visible columns, in order (columns not listed are hidden)
    pub columns: Vec<String>,
    /// labels, most important first; unlisted labels come after, A→Z
    pub labels: Vec<String>,
    /// labels never shown in the Labels column
    pub hidden_labels: Vec<String>,
}

impl Default for View {
    fn default() -> Self {
        let cols = [Col::Key, Col::Pr, Col::Status, Col::Priority, Col::Assignee, Col::Labels, Col::Summary];
        Self { columns: cols.iter().map(|c| c.name().to_string()).collect(), labels: vec![], hidden_labels: vec![] }
    }
}

impl View {
    pub fn cols(&self) -> Vec<Col> {
        let mut out = vec![];
        for c in self.columns.iter().filter_map(|s| Col::parse(s)) {
            if !out.contains(&c) {
                out.push(c);
            }
        }
        if out.is_empty() { vec![Col::Key, Col::Summary] } else { out }
    }

    fn rank(&self, label: &str) -> (usize, String) {
        let pos = self.labels.iter().position(|l| l.eq_ignore_ascii_case(label)).unwrap_or(usize::MAX);
        (pos, label.to_lowercase())
    }

    pub fn is_hidden(&self, label: &str) -> bool {
        self.hidden_labels.iter().any(|l| l.eq_ignore_ascii_case(label))
    }

    /// The issue's shown labels, most important first.
    pub fn sorted_labels<'a>(&self, labels: &'a [String]) -> Vec<&'a String> {
        let mut v: Vec<&String> = labels.iter().filter(|l| !self.is_hidden(l)).collect();
        v.sort_by_key(|l| self.rank(l));
        v
    }

    /// Sort key for the Labels column: the best label (none sorts last).
    pub fn label_key(&self, labels: &[String]) -> (usize, String) {
        self.sorted_labels(labels).first().map(|l| self.rank(l)).unwrap_or((usize::MAX, "\u{10FFFF}".into()))
    }
}

fn path() -> PathBuf {
    crate::config::path().with_file_name("view.toml")
}

pub fn load() -> View {
    std::fs::read_to_string(path()).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(v: &View) -> anyhow::Result<()> {
    let p = path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, toml::to_string(v)?)?;
    Ok(())
}

#[derive(PartialEq, Clone, Copy)]
pub enum EditKind {
    Columns,
    Labels,
}

/// Reorder / show-hide list (`C` columns, `L` labels).
pub struct OrderEdit {
    pub kind: EditKind,
    /// (name, shown)
    pub items: Vec<(String, bool)>,
    pub state: ListState,
}

impl OrderEdit {
    pub fn columns(v: &View) -> Self {
        let shown = v.cols();
        let mut items: Vec<(String, bool)> = shown.iter().map(|c| (c.name().to_string(), true)).collect();
        items.extend(Col::ALL.iter().filter(|c| !shown.contains(c)).map(|c| (c.name().to_string(), false)));
        Self::new(EditKind::Columns, items)
    }

    /// `seen`: labels on the loaded issues.
    pub fn labels(v: &View, seen: Vec<String>) -> Self {
        let mut names: Vec<String> = vec![];
        for l in v.labels.iter().chain(&v.hidden_labels).chain(&seen) {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(l)) {
                names.push(l.clone());
            }
        }
        names.sort_by_key(|l| v.rank(l));
        let items = names.into_iter().map(|l| { let shown = !v.is_hidden(&l); (l, shown) }).collect();
        Self::new(EditKind::Labels, items)
    }

    fn new(kind: EditKind, items: Vec<(String, bool)>) -> Self {
        let mut state = ListState::default();
        state.select((!items.is_empty()).then_some(0));
        Self { kind, items, state }
    }

    pub fn step(&mut self, d: isize) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let i = self.state.selected().unwrap_or(0) as isize;
        self.state.select(Some((i + d).clamp(0, n as isize - 1) as usize));
    }

    /// Move the selected item up (-1) or down (+1).
    pub fn shift(&mut self, d: isize) {
        let Some(i) = self.state.selected() else { return };
        let j = i as isize + d;
        if j < 0 || j >= self.items.len() as isize {
            return;
        }
        self.items.swap(i, j as usize);
        self.state.select(Some(j as usize));
    }

    pub fn toggle(&mut self) {
        let Some(i) = self.state.selected() else { return };
        let shown = self.items.iter().filter(|(_, s)| *s).count();
        // keep at least one column on screen
        if self.kind == EditKind::Columns && self.items[i].1 && shown == 1 {
            return;
        }
        self.items[i].1 = !self.items[i].1;
    }

    pub fn apply(&self, v: &mut View) {
        match self.kind {
            EditKind::Columns => {
                v.columns = self.items.iter().filter(|(_, s)| *s).map(|(n, _)| n.clone()).collect();
            }
            EditKind::Labels => {
                v.labels = self.items.iter().map(|(n, _)| n.clone()).collect();
                v.hidden_labels = self.items.iter().filter(|(_, s)| !*s).map(|(n, _)| n.clone()).collect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_priority() {
        let v = View {
            labels: vec!["urgent".into(), "bug".into()],
            hidden_labels: vec!["noise".into()],
            ..Default::default()
        };
        let l: Vec<String> = ["zeta", "Bug", "noise", "alpha", "URGENT"].iter().map(|s| s.to_string()).collect();
        let got: Vec<&str> = v.sorted_labels(&l).iter().map(|s| s.as_str()).collect();
        assert_eq!(got, ["URGENT", "Bug", "alpha", "zeta"]);
    }

    #[test]
    fn columns_parse_and_dedupe() {
        let v = View { columns: vec!["summary".into(), "KEY".into(), "bogus".into(), "Key".into()], ..Default::default() };
        assert_eq!(v.cols(), [Col::Summary, Col::Key]);
    }
}
