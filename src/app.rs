use crate::filter::{self, Choice, Filter, FilterOpts, FilterPanel, Group};
use crate::jira::{Client, Detail, Issue, Project};
use std::collections::BTreeMap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::{ListState, TableState};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

type Res<T> = Result<T, String>;
/// issue key -> (transition name -> transition id)
type TransMap = HashMap<String, HashMap<String, String>>;

pub enum Msg {
    Projects(Res<Vec<Project>>),
    /// project, jql used, result
    Issues(String, String, Res<Vec<Issue>>),
    FilterOpts(String, Res<FilterOpts>),
    Detail(String, Res<Box<Detail>>),
    Options(u64, Res<(Vec<Opt>, TransMap)>),
    Applied { project: String, keys: Vec<String>, ok: usize, errs: Vec<String> },
}

#[derive(PartialEq, Clone, Copy)]
pub enum Pane {
    Projects,
    Issues,
    Detail,
}

#[derive(PartialEq, Clone, Copy)]
pub enum PopKind {
    Status,
    Assignee,
    Priority,
    Sort,
}

#[derive(PartialEq, Clone, Copy)]
pub enum SortCol {
    Key,
    Status,
    Priority,
    Assignee,
    Summary,
}

impl SortCol {
    pub const ALL: [SortCol; 5] = [SortCol::Key, SortCol::Status, SortCol::Priority, SortCol::Assignee, SortCol::Summary];
    pub fn name(self) -> &'static str {
        match self {
            SortCol::Key => "Key",
            SortCol::Status => "Status",
            SortCol::Priority => "Priority",
            SortCol::Assignee => "Assignee",
            SortCol::Summary => "Summary",
        }
    }
}

fn key_num(k: &str) -> u64 {
    k.rsplit('-').next().and_then(|n| n.parse().ok()).unwrap_or(0)
}

fn cat_rank(c: &crate::jira::Cat) -> u8 {
    match c {
        crate::jira::Cat::Todo => 0,
        crate::jira::Cat::Doing => 1,
        crate::jira::Cat::Done => 2,
    }
}

/// Higher number = more important.
fn prio_rank(p: &str) -> u8 {
    match p.to_lowercase().as_str() {
        "highest" | "blocker" => 6,
        "critical" => 5,
        "high" | "major" => 4,
        "medium" => 3,
        "low" | "minor" => 2,
        "lowest" | "trivial" => 1,
        _ => 0,
    }
}

#[derive(Clone)]
pub struct Opt {
    pub label: String,
    /// Status: transition name · Assignee: accountId ("" = unassigned) · Priority: id
    pub value: String,
}

pub struct Popup {
    id: u64,
    pub kind: PopKind,
    pub targets: Vec<String>,
    pub opts: Vec<Opt>,
    trans: TransMap,
    pub filter: String,
    pub state: ListState,
    pub loading: bool,
    pub error: Option<String>,
}

impl Popup {
    pub fn visible(&self) -> Vec<&Opt> {
        let f = self.filter.to_lowercase();
        self.opts.iter().filter(|o| o.label.to_lowercase().contains(&f)).collect()
    }
}

pub struct App {
    client: Client,
    tx: Sender<Msg>,
    pub projects: Vec<Project>,
    pub proj_state: ListState,
    pub issues: HashMap<String, Vec<Issue>>,
    /// project -> jql being fetched
    pub loading: HashMap<String, String>,
    pub filters: BTreeMap<String, Filter>,
    pub filter_opts: HashMap<String, FilterOpts>,
    filter_opts_loading: HashSet<String>,
    pub fpanel: Option<FilterPanel>,
    pub loading_projects: bool,
    pub issue_state: TableState,
    pub marked: HashSet<String>,
    pub details: HashMap<String, Detail>,
    pub detail_loading: HashSet<String>,
    pub detail_scroll: u16,
    last_key: Option<String>,
    key_changed_at: Instant,
    pub show_projects: bool,
    pub show_detail: bool,
    pub show_help: bool,
    pub sort_col: SortCol,
    pub sort_desc: bool,
    pub pinned: Vec<String>,
    pub popup: Option<Popup>,
    popup_seq: u64,
    pub focus: Pane,
    pub filter: String,
    pub filtering: bool,
    pub status: String,
    pub busy: bool,
    pub quit: bool,
}

impl App {
    pub fn new(client: Client, tx: Sender<Msg>) -> Self {
        let mut app = Self {
            client,
            tx,
            projects: Vec::new(),
            proj_state: ListState::default(),
            issues: HashMap::new(),
            loading: HashMap::new(),
            filters: filter::load(),
            filter_opts: HashMap::new(),
            filter_opts_loading: HashSet::new(),
            fpanel: None,
            loading_projects: false,
            issue_state: TableState::default(),
            marked: HashSet::new(),
            details: HashMap::new(),
            detail_loading: HashSet::new(),
            detail_scroll: 0,
            last_key: None,
            key_changed_at: Instant::now(),
            show_projects: true,
            show_detail: false,
            show_help: false,
            sort_col: SortCol::Key,
            sort_desc: true,
            pinned: crate::config::load_pins(),
            popup: None,
            popup_seq: 0,
            focus: Pane::Projects,
            filter: String::new(),
            filtering: false,
            status: String::new(),
            busy: false,
            quit: false,
        };
        app.fetch_projects();
        app
    }

    pub fn current_project(&self) -> Option<&Project> {
        self.proj_state.selected().and_then(|i| self.projects.get(i))
    }

    pub fn visible_issues(&self) -> Vec<&Issue> {
        let Some(p) = self.current_project() else { return vec![] };
        let Some(list) = self.issues.get(&p.key) else { return vec![] };
        let f = self.filter.to_lowercase();
        let mut v: Vec<&Issue> = list
            .iter()
            .filter(|i| {
                f.is_empty()
                    || [&i.key, &i.summary, &i.status, &i.assignee, &i.priority]
                        .iter()
                        .any(|s| s.to_lowercase().contains(&f))
            })
            .collect();
        v.sort_by(|a, b| {
            let o = match self.sort_col {
                SortCol::Key => key_num(&a.key).cmp(&key_num(&b.key)),
                SortCol::Status => (cat_rank(&a.cat), a.status.to_lowercase())
                    .cmp(&(cat_rank(&b.cat), b.status.to_lowercase())),
                SortCol::Priority => prio_rank(&a.priority).cmp(&prio_rank(&b.priority)),
                SortCol::Assignee => a.assignee.to_lowercase().cmp(&b.assignee.to_lowercase()),
                SortCol::Summary => a.summary.to_lowercase().cmp(&b.summary.to_lowercase()),
            };
            // ties: newest key first
            let o = if self.sort_desc { o.reverse() } else { o };
            o.then_with(|| key_num(&b.key).cmp(&key_num(&a.key)))
        });
        v
    }

    pub fn current_issue_key(&self) -> Option<String> {
        self.issue_state
            .selected()
            .and_then(|i| self.visible_issues().get(i).map(|x| x.key.clone()))
    }

    /// Marked issues, or the selected one if nothing is marked.
    fn targets(&self) -> Vec<String> {
        if !self.marked.is_empty() {
            // keep list order
            let p = self.current_project().map(|p| p.key.clone()).unwrap_or_default();
            return self
                .issues
                .get(&p)
                .map(|l| l.iter().filter(|i| self.marked.contains(&i.key)).map(|i| i.key.clone()).collect())
                .unwrap_or_default();
        }
        self.current_issue_key().into_iter().collect()
    }

    // ---------- background work ----------
    fn spawn<F: FnOnce(Client) -> Msg + Send + 'static>(&self, f: F) {
        let (c, tx) = (self.client.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(f(c));
        });
    }

    fn fetch_projects(&mut self) {
        self.loading_projects = true;
        self.spawn(|c| Msg::Projects(c.projects().map_err(|e| e.to_string())));
    }

    pub fn filter_for(&self, project: &str) -> Filter {
        self.filters.get(project).cloned().unwrap_or_default()
    }

    fn fetch_issues(&mut self, key: String) {
        let jql = self.filter_for(&key).to_jql(&key);
        if self.loading.get(&key) == Some(&jql) {
            return;
        }
        self.loading.insert(key.clone(), jql.clone());
        self.spawn(move |c| {
            let r = c.issues(&jql).map_err(|e| e.to_string());
            Msg::Issues(key, jql, r)
        });
    }

    fn fetch_filter_opts(&mut self, project: String) {
        if self.filter_opts.contains_key(&project) || !self.filter_opts_loading.insert(project.clone()) {
            return;
        }
        self.spawn(move |c| {
            let r = load_filter_opts(&c, &project).map_err(|e| e.to_string());
            Msg::FilterOpts(project, r)
        });
    }

    fn fetch_detail(&mut self, key: String) {
        if self.details.contains_key(&key) || !self.detail_loading.insert(key.clone()) {
            return;
        }
        self.spawn(move |c| {
            let r = c.detail(&key).map(Box::new).map_err(|e| e.to_string());
            Msg::Detail(key, r)
        });
    }

    fn ensure_issues(&mut self) {
        if let Some(k) = self.current_project().map(|p| p.key.clone())
            && !self.issues.contains_key(&k)
        {
            self.fetch_issues(k);
        }
    }

    /// Called every loop iteration: lazily loads the detail of the selected issue (debounced).
    pub fn tick(&mut self) {
        let key = self.current_issue_key();
        if key != self.last_key {
            self.last_key = key.clone();
            self.key_changed_at = Instant::now();
            self.detail_scroll = 0;
        }
        if let Some(k) = key
            && self.show_detail
            && self.key_changed_at.elapsed() > Duration::from_millis(120)
        {
            self.fetch_detail(k);
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Projects(Ok(p)) => {
                self.loading_projects = false;
                self.status = format!("{} projects", p.len());
                self.projects = p;
                self.sort_projects(None);
                if !self.projects.is_empty() {
                    self.proj_state.select(Some(0));
                    self.ensure_issues();
                }
            }
            Msg::Projects(Err(e)) => {
                self.loading_projects = false;
                self.status = format!("Error: {e}");
            }
            Msg::Issues(k, jql, r) => {
                // Ignore results of a filter that has since changed.
                if self.loading.get(&k) != Some(&jql) {
                    return;
                }
                self.loading.remove(&k);
                match r {
                    Ok(list) => {
                        let keys: HashSet<_> = list.iter().map(|i| i.key.clone()).collect();
                        self.marked.retain(|m| keys.contains(m));
                        self.issues.insert(k, list);
                        self.clamp_issue();
                    }
                    Err(e) => self.status = format!("Error ({k}): {e}"),
                }
            }
            Msg::FilterOpts(k, r) => {
                self.filter_opts_loading.remove(&k);
                match r {
                    Ok(o) => {
                        self.filter_opts.insert(k, o);
                    }
                    Err(e) => self.status = format!("Error (filters): {e}"),
                }
            }
            Msg::Detail(k, r) => {
                self.detail_loading.remove(&k);
                match r {
                    Ok(d) => {
                        self.details.insert(k, *d);
                    }
                    Err(e) => self.status = format!("Error ({k}): {e}"),
                }
            }
            Msg::Options(id, r) => {
                if let Some(p) = self.popup.as_mut().filter(|p| p.id == id) {
                    p.loading = false;
                    match r {
                        Ok((opts, trans)) => {
                            p.opts = opts;
                            p.trans = trans;
                            p.state.select(if p.opts.is_empty() { None } else { Some(0) });
                            if p.opts.is_empty() {
                                p.error = Some("No options available".into());
                            }
                        }
                        Err(e) => p.error = Some(e),
                    }
                }
            }
            Msg::Applied { project, keys, ok, errs } => {
                self.busy = false;
                for k in &keys {
                    self.details.remove(k);
                }
                self.status = if errs.is_empty() {
                    self.marked.clear();
                    format!("Updated {ok} issue(s)")
                } else {
                    format!("Error: {ok} ok, {} failed — {}", errs.len(), errs.join(" | "))
                };
                self.fetch_issues(project);
                self.last_key = None; // force detail reload
            }
        }
    }

    fn clamp_issue(&mut self) {
        let n = self.visible_issues().len();
        let sel = match (n, self.issue_state.selected()) {
            (0, _) => None,
            (_, Some(i)) => Some(i.min(n - 1)),
            (_, None) => Some(0),
        };
        self.issue_state.select(sel);
    }

    fn move_sel(&mut self, delta: isize) {
        match self.focus {
            Pane::Projects => {
                let n = self.projects.len();
                if n == 0 {
                    return;
                }
                let i = self.proj_state.selected().unwrap_or(0) as isize;
                let new = (i + delta).clamp(0, n as isize - 1) as usize;
                if Some(new) != self.proj_state.selected() {
                    self.proj_state.select(Some(new));
                    self.issue_state.select(None);
                    self.filter.clear();
                    self.marked.clear();
                    self.ensure_issues();
                    self.clamp_issue();
                }
            }
            Pane::Issues => {
                let n = self.visible_issues().len();
                if n == 0 {
                    return;
                }
                let i = self.issue_state.selected().unwrap_or(0) as isize;
                self.issue_state.select(Some((i + delta).clamp(0, n as isize - 1) as usize));
            }
            Pane::Detail => self.scroll_detail(delta),
        }
    }

    fn scroll_detail(&mut self, delta: isize) {
        self.detail_scroll = (self.detail_scroll as isize + delta).clamp(0, u16::MAX as isize) as u16;
    }

    // ---------- edit popups ----------
    // ---------- filter panel ----------
    fn open_filter(&mut self) {
        let Some(project) = self.current_project().map(|p| p.key.clone()) else { return };
        self.fetch_filter_opts(project.clone());
        let current = self.filter_for(&project);
        self.fpanel = Some(FilterPanel::new(project, current));
    }

    fn filter_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let Some(fp) = self.fpanel.as_mut() else { return };
        let g = fp.current_group();
        let opts = self.filter_opts.get(&fp.project);
        let n = if g == Group::Jql { 0 } else { fp.visible(opts).len() };
        match k.code {
            KeyCode::Esc => self.fpanel = None,
            KeyCode::Enter => self.apply_filter(),
            KeyCode::Char('x') if ctrl => fp.draft.clear(g),
            KeyCode::Char('r') if ctrl => fp.draft.reset(g),
            KeyCode::Tab | KeyCode::BackTab => fp.in_opts = !fp.in_opts,
            KeyCode::Right => fp.in_opts = true,
            KeyCode::Left if fp.in_opts && !(g == Group::Jql) => fp.in_opts = false,
            KeyCode::Left if !fp.in_opts => {}
            KeyCode::Up | KeyCode::Down | KeyCode::Char('k') | KeyCode::Char('j')
                if !fp.in_opts =>
            {
                let d: isize = if matches!(k.code, KeyCode::Up | KeyCode::Char('k')) { -1 } else { 1 };
                fp.group = (fp.group as isize + d).rem_euclid(Group::ALL.len() as isize) as usize;
                fp.search.clear();
                fp.state.select(Some(0));
            }
            _ if !fp.in_opts => {}
            // --- right column ---
            _ if g == Group::Jql => match k.code {
                KeyCode::Backspace => {
                    fp.draft.jql.pop();
                }
                KeyCode::Left => fp.in_opts = false,
                KeyCode::Char(c) if !ctrl => fp.draft.jql.push(c),
                _ => {}
            },
            KeyCode::Up => sel_step(&mut fp.state, n, -1),
            KeyCode::Down => sel_step(&mut fp.state, n, 1),
            KeyCode::Char(' ') => {
                if let Some(c) = fp.state.selected().and_then(|i| fp.visible(opts).get(i).cloned()) {
                    fp.draft.toggle(g, &c, opts);
                }
            }
            KeyCode::Backspace => {
                fp.search.pop();
                fp.state.select(Some(0));
            }
            KeyCode::Char(c) if !ctrl => {
                fp.search.push(c);
                fp.state.select(Some(0));
            }
            _ => {}
        }
    }

    fn apply_filter(&mut self) {
        let Some(fp) = self.fpanel.take() else { return };
        if fp.draft == self.filter_for(&fp.project) {
            return;
        }
        if fp.draft.is_default() {
            self.filters.remove(&fp.project);
        } else {
            self.filters.insert(fp.project.clone(), fp.draft);
        }
        if let Err(e) = filter::save(&self.filters) {
            self.status = format!("Error: saving filters: {e}");
        }
        self.issues.remove(&fp.project);
        self.marked.clear();
        self.issue_state.select(None);
        self.fetch_issues(fp.project);
    }

    fn open_sort(&mut self) {
        self.popup_seq += 1;
        let opts: Vec<Opt> = SortCol::ALL
            .iter()
            .map(|c| {
                let label = if *c == self.sort_col {
                    format!("{}  {} (again to flip)", c.name(), if self.sort_desc { "▼" } else { "▲" })
                } else {
                    c.name().to_string()
                };
                Opt { label, value: c.name().to_string() }
            })
            .collect();
        let cur = SortCol::ALL.iter().position(|c| *c == self.sort_col);
        let mut state = ListState::default();
        state.select(cur);
        self.popup = Some(Popup {
            id: self.popup_seq,
            kind: PopKind::Sort,
            targets: vec![],
            opts,
            trans: HashMap::new(),
            filter: String::new(),
            state,
            loading: false,
            error: None,
        });
    }

    fn set_sort(&mut self, name: &str) {
        let Some(col) = SortCol::ALL.into_iter().find(|c| c.name() == name) else { return };
        let keep = self.current_issue_key();
        if col == self.sort_col {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort_col = col;
            // sensible defaults: newest key / most important first, text A→Z
            self.sort_desc = matches!(col, SortCol::Key | SortCol::Priority);
        }
        if let Some(k) = keep {
            let pos = self.visible_issues().iter().position(|i| i.key == k);
            self.issue_state.select(pos);
        }
    }

    fn open_popup(&mut self, kind: PopKind) {
        if self.busy {
            self.status = "Busy: wait for the current update".into();
            return;
        }
        let targets = self.targets();
        if targets.is_empty() {
            return;
        }
        let Some(project) = self.current_project().map(|p| p.key.clone()) else { return };
        self.popup_seq += 1;
        let id = self.popup_seq;
        self.popup = Some(Popup {
            id,
            kind,
            targets: targets.clone(),
            opts: vec![],
            trans: HashMap::new(),
            filter: String::new(),
            state: ListState::default(),
            loading: true,
            error: None,
        });
        self.spawn(move |c| Msg::Options(id, load_options(&c, kind, &targets, &project).map_err(|e| e.to_string())));
    }

    fn popup_key(&mut self, k: KeyEvent) {
        let Some(p) = self.popup.as_mut() else { return };
        let n = p.visible().len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.popup = None,
            KeyCode::Up => sel_step(&mut p.state, n, -1),
            KeyCode::Down | KeyCode::Tab => sel_step(&mut p.state, n, 1),
            KeyCode::Char('p') if ctrl => sel_step(&mut p.state, n, -1),
            KeyCode::Char('n') if ctrl => sel_step(&mut p.state, n, 1),
            KeyCode::Backspace => {
                p.filter.pop();
                p.state.select(if p.visible().is_empty() { None } else { Some(0) });
            }
            KeyCode::Char(c) if !ctrl => {
                p.filter.push(c);
                p.state.select(if p.visible().is_empty() { None } else { Some(0) });
            }
            KeyCode::Enter => {
                let Some(opt) = p.state.selected().and_then(|i| p.visible().get(i).map(|o| (*o).clone())) else {
                    return;
                };
                let p = self.popup.take().unwrap();
                self.apply(p, opt);
            }
            _ => {}
        }
    }

    fn apply(&mut self, p: Popup, opt: Opt) {
        if p.kind == PopKind::Sort {
            self.set_sort(&opt.value);
            return;
        }
        let Some(project) = self.current_project().map(|p| p.key.clone()) else { return };
        self.busy = true;
        self.status = format!("Updating {} issue(s)…", p.targets.len());
        let Popup { kind, targets, trans, .. } = p;
        self.spawn(move |c| {
            let (mut ok, mut errs) = (0, vec![]);
            for key in &targets {
                let r = match kind {
                    PopKind::Status => match trans.get(key).and_then(|m| m.get(&opt.value)) {
                        Some(id) => c.transition(key, id),
                        None => Err(anyhow::anyhow!("transition not available")),
                    },
                    PopKind::Assignee => c.assign(key, (!opt.value.is_empty()).then_some(opt.value.as_str())),
                    PopKind::Priority => c.set_priority(key, &opt.value),
                    PopKind::Sort => Ok(()),
                };
                match r {
                    Ok(()) => ok += 1,
                    Err(e) => errs.push(format!("{key}: {e}")),
                }
            }
            Msg::Applied { project, keys: targets, ok, errs }
        });
    }

    // ---------- keys ----------
    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if self.show_help {
            self.show_help = false; // any key closes help
            return;
        }
        if self.popup.is_some() {
            self.popup_key(k);
            return;
        }
        if self.fpanel.is_some() {
            self.filter_key(k);
            return;
        }
        if self.filtering {
            match k.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                }
                KeyCode::Char(c) => self.filter.push(c),
                _ => {}
            }
            self.issue_state.select(None);
            self.clamp_issue();
            return;
        }
        match k.code {
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.clamp_issue();
                } else {
                    self.marked.clear();
                }
            }
            KeyCode::Char('d') if ctrl => self.scroll_detail(10),
            KeyCode::Char('u') if ctrl => self.scroll_detail(-10),
            KeyCode::Char('J') => self.scroll_detail(3),
            KeyCode::Char('K') => self.scroll_detail(-3),
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Home => self.move_sel(-100_000),
            KeyCode::End => self.move_sel(100_000),
            KeyCode::Tab | KeyCode::BackTab => {
                let mut panes = vec![];
                if self.show_projects {
                    panes.push(Pane::Projects);
                }
                panes.push(Pane::Issues);
                if self.show_detail {
                    panes.push(Pane::Detail);
                }
                let n = panes.len() as isize;
                let i = panes.iter().position(|p| *p == self.focus).unwrap_or(0) as isize;
                let d = if k.code == KeyCode::Tab { 1 } else { -1 };
                self.focus = panes[(i + d).rem_euclid(n) as usize];
            }
            KeyCode::Char('h') | KeyCode::Left => {
                self.focus = match self.focus {
                    Pane::Detail => Pane::Issues,
                    _ if self.show_projects => Pane::Projects,
                    f => f,
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    Pane::Projects => Pane::Issues,
                    _ if self.show_detail => Pane::Detail,
                    f => f,
                }
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('*') => self.toggle_pin(),
            KeyCode::Char(c @ '1'..='9') => self.jump_pinned(c as usize - '1' as usize),
            KeyCode::Char('z') => {
                self.show_projects = !self.show_projects;
                if !self.show_projects && self.focus == Pane::Projects {
                    self.focus = Pane::Issues;
                }
            }
            KeyCode::Char('/') => {
                self.focus = Pane::Issues;
                self.filtering = true;
            }
            KeyCode::Char(' ') => {
                if let Some(key) = self.current_issue_key() {
                    if !self.marked.remove(&key) {
                        self.marked.insert(key);
                    }
                    self.focus = Pane::Issues;
                    self.move_sel(1);
                }
            }
            KeyCode::Char('a') if ctrl => {
                let vis: Vec<String> = self.visible_issues().iter().map(|i| i.key.clone()).collect();
                if vis.iter().all(|k| self.marked.contains(k)) {
                    self.marked.clear();
                } else {
                    self.marked.extend(vis);
                }
            }
            KeyCode::Char('t') => self.open_sort(),
            KeyCode::Char('f') => self.open_filter(),
            KeyCode::Char('s') => self.open_popup(PopKind::Status),
            KeyCode::Char('a') => self.open_popup(PopKind::Assignee),
            KeyCode::Char('p') => self.open_popup(PopKind::Priority),
            KeyCode::Char('r') => {
                if self.projects.is_empty() {
                    self.fetch_projects();
                } else if let Some(k) = self.current_project().map(|p| p.key.clone()) {
                    if let Some(i) = self.current_issue_key() {
                        self.details.remove(&i);
                        self.last_key = None;
                    }
                    self.fetch_issues(k);
                }
            }
            KeyCode::Char('R') => {
                self.issues.clear();
                self.details.clear();
                self.fetch_projects();
            }
            KeyCode::Enter => match self.focus {
                Pane::Projects => self.focus = Pane::Issues,
                _ => self.toggle_detail(),
            },
            KeyCode::Char('o') => self.open_selected(),
            _ => {}
        }
    }

    pub fn is_pinned(&self, key: &str) -> bool {
        self.pinned.iter().any(|k| k == key)
    }

    /// Pinned projects first (in pin order), then the rest; keeps `keep` selected.
    fn sort_projects(&mut self, keep: Option<String>) {
        let pins = &self.pinned;
        self.projects
            .sort_by_key(|p| pins.iter().position(|k| *k == p.key).unwrap_or(usize::MAX));
        if let Some(k) = keep {
            self.proj_state.select(self.projects.iter().position(|p| p.key == k));
        }
    }

    fn toggle_pin(&mut self) {
        let Some(key) = self.current_project().map(|p| p.key.clone()) else { return };
        if self.is_pinned(&key) {
            self.pinned.retain(|k| *k != key);
            self.status = format!("Unpinned {key}");
        } else {
            self.pinned.push(key.clone());
            self.status = format!("Pinned {key}");
        }
        self.sort_projects(Some(key));
        if let Err(e) = crate::config::save_pins(&self.pinned) {
            self.status = format!("Error: saving pins: {e}");
        }
    }

    fn jump_pinned(&mut self, n: usize) {
        let Some(key) = self.pinned.get(n).cloned() else {
            self.status = format!("No project pinned at {}", n + 1);
            return;
        };
        let Some(i) = self.projects.iter().position(|p| p.key == key) else { return };
        let cur = self.proj_state.selected().unwrap_or(0) as isize;
        let prev = self.focus;
        self.focus = Pane::Projects;
        self.move_sel(i as isize - cur);
        self.focus = if prev == Pane::Projects { Pane::Projects } else { Pane::Issues };
    }

    fn toggle_detail(&mut self) {
        self.show_detail = !self.show_detail;
        if !self.show_detail && self.focus == Pane::Detail {
            self.focus = Pane::Issues;
        }
        self.key_changed_at = Instant::now() - Duration::from_secs(1); // load right away
    }

    fn open_selected(&mut self) {
        if let Some(key) = self.current_issue_key() {
            let url = self.client.browse_url(&key);
            self.status = match open::that_detached(&url) {
                Ok(_) => format!("Opened {key}"),
                Err(e) => format!("Error: couldn't open browser: {e}"),
            };
        }
    }
}

fn sel_step(state: &mut ListState, n: usize, d: isize) {
    if n == 0 {
        return;
    }
    let i = state.selected().unwrap_or(0) as isize;
    state.select(Some((i + d).rem_euclid(n as isize) as usize));
}

fn load_options(c: &Client, kind: PopKind, targets: &[String], project: &str) -> anyhow::Result<(Vec<Opt>, TransMap)> {
    let mut trans = TransMap::new();
    let opts = match kind {
        PopKind::Status => {
            // Only offer transitions available for every target issue.
            let mut common: Option<Vec<(String, String)>> = None; // (name, label)
            for key in targets {
                let ts = c.transitions(key)?;
                let names: HashSet<&String> = ts.iter().map(|t| &t.1).collect();
                common = Some(match common {
                    None => ts
                        .iter()
                        .map(|(_, name, to)| {
                            let label = if name == to { name.clone() } else { format!("{name}  → {to}") };
                            (name.clone(), label)
                        })
                        .collect(),
                    Some(prev) => prev.into_iter().filter(|(n, _)| names.contains(n)).collect(),
                });
                trans.insert(key.clone(), ts.into_iter().map(|(id, name, _)| (name, id)).collect());
            }
            common
                .unwrap_or_default()
                .into_iter()
                .map(|(value, label)| Opt { label, value })
                .collect()
        }
        PopKind::Assignee => {
            let (me_id, me_name) = c.myself()?;
            let mut v = vec![
                Opt { label: format!("Me ({me_name})"), value: me_id.clone() },
                Opt { label: "Unassigned".into(), value: String::new() },
            ];
            v.extend(
                c.assignable(project)?
                    .into_iter()
                    .filter(|(id, _)| *id != me_id)
                    .map(|(id, name)| Opt { label: name, value: id }),
            );
            v
        }
        PopKind::Sort => vec![],
        PopKind::Priority => c
            .priorities()?
            .into_iter()
            .map(|(id, name)| Opt { label: name, value: id })
            .collect(),
    };
    Ok((opts, trans))
}

fn load_filter_opts(c: &Client, project: &str) -> anyhow::Result<FilterOpts> {
    let plain = |v: Vec<String>| v.into_iter().map(|n| Choice::new(n.clone(), n)).collect::<Vec<_>>();
    let mut o = FilterOpts::default();
    let statuses = c.statuses(project)?;
    o.done = statuses.iter().filter(|(_, done)| *done).map(|(n, _)| n.clone()).collect();
    o.groups.insert(Group::Status, plain(statuses.into_iter().map(|(n, _)| n).collect()));
    let (me_id, _) = c.myself()?;
    let mut people = vec![Choice::new(filter::ME, "Me"), Choice::new(filter::NOBODY, "Unassigned")];
    people.extend(
        c.assignable(project)?
            .into_iter()
            .filter(|(id, _)| *id != me_id)
            .map(|(id, name)| Choice::new(id, name)),
    );
    o.groups.insert(Group::Assignee, people);
    o.groups.insert(Group::Priority, plain(c.priorities()?.into_iter().map(|(_, n)| n).collect()));
    o.groups.insert(Group::Type, plain(c.issue_types(project)?));
    Ok(o)
}
