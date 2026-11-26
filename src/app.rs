use crate::cache::{self, Cache};
use crate::filter::{self, Choice, Filter, FilterOpts, FilterPanel, Group, MY_WORK};
use crate::github::{GitHub, Pr};
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
pub type TransMap = HashMap<String, HashMap<String, String>>;

pub enum Msg {
    Projects(Res<Vec<Project>>),
    /// project, jql used, result
    Issues(String, String, Res<Vec<Issue>>),
    FilterOpts(String, Res<(FilterOpts, Refs)>),
    /// popup id (0 = none), shared reference data
    Refs(u64, Res<Refs>),
    Prs(String, Res<Vec<Pr>>),
    Detail(String, Res<Box<Detail>>),
    /// popup id, options cache key, result
    Options(u64, String, Res<(Vec<Opt>, TransMap)>),
    Me(Res<(String, String)>),
    Applied { keys: Vec<String>, ok_keys: Vec<String>, edit: Edit, errs: Vec<String> },
}

/// Reference data shared by the filter panel and the edit popups.
#[derive(Default)]
pub struct Refs {
    priorities: Option<Vec<(String, String)>>,
    /// project -> assignable users
    users: Vec<(String, Vec<(String, String)>)>,
    me: Option<(String, String)>,
}

/// What a background reference-data fetch should load.
#[derive(Clone, Default)]
struct RefNeeds {
    priorities: bool,
    users: Vec<String>,
    me: bool,
}

impl RefNeeds {
    fn any(&self) -> bool {
        self.priorities || self.me || !self.users.is_empty()
    }
}

#[derive(PartialEq, Clone, Copy)]
pub enum Pane {
    Projects,
    Issues,
    Prs,
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

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Opt {
    pub label: String,
    /// Status: transition name · Assignee: accountId ("" = unassigned) · Priority: id
    pub value: String,
    /// Status: the status the transition leads to
    #[serde(default)]
    pub extra: String,
}

impl Opt {
    fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into(), extra: String::new() }
    }
}

/// A successful edit, applied locally right away (before the refetch).
#[derive(Clone)]
pub enum Edit {
    Status(String),
    Assignee { id: String, name: String },
    Priority(String),
    None,
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
    /// Replace the choices, keeping the highlighted one.
    fn set_opts(&mut self, opts: Vec<Opt>, trans: TransMap) {
        let sel = self.state.selected().and_then(|i| self.visible().get(i).map(|o| o.label.clone()));
        self.opts = opts;
        self.trans = trans;
        self.error = if self.opts.is_empty() { Some("No options available".into()) } else { None };
        let vis = self.visible();
        let idx = sel.and_then(|l| vis.iter().position(|o| o.label == l)).or(Some(0));
        self.state.select(if vis.is_empty() { None } else { idx });
    }

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
    filter_opts_fresh: HashSet<String>,
    /// reference data refreshed (or being refreshed) this session: "priority", "users:KEY"
    refs_fresh: HashSet<String>,
    dirty: bool,
    last_save: Instant,
    pub cache: Cache,
    gh: GitHub,
    pub prs: HashMap<String, Vec<Pr>>,
    pub pr_errors: HashMap<String, String>,
    pub prs_loading: HashSet<String>,
    prs_fresh: HashSet<String>,
    pub pr_state: TableState,
    /// views fetched from Jira during this session (others show cached data)
    pub fresh: HashSet<String>,
    /// view -> unix time its data was fetched
    pub fetched_at: HashMap<String, u64>,
    last_refresh: Instant,
    /// issues I just edited: mark as seen once they come back
    just_edited: HashSet<String>,
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
        let gh_token = client.config().github_token.clone();
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
            filter_opts_fresh: HashSet::new(),
            refs_fresh: HashSet::new(),
            dirty: false,
            last_save: Instant::now(),
            cache: Cache::default(),
            gh: GitHub::new(gh_token).expect("http client"),
            prs: HashMap::new(),
            pr_errors: HashMap::new(),
            prs_loading: HashSet::new(),
            prs_fresh: HashSet::new(),
            pr_state: TableState::default(),
            fresh: HashSet::new(),
            fetched_at: HashMap::new(),
            last_refresh: Instant::now(),
            just_edited: HashSet::new(),
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
        app.load_cache();
        app.fetch_projects();
        app.ensure_issues();
        app
    }

    // ---------- cache / seen ----------
    fn load_cache(&mut self) {
        self.cache = cache::load(self.client.base_url());
        self.projects = with_my_work(self.cache.projects.clone());
        self.sort_projects(None);
        self.proj_state.select(Some(0));
        let lists: Vec<cache::CachedList> = self.cache.lists.values().cloned().collect();
        for l in lists {
            if l.jql == self.filter_for(&l.view).to_jql(&l.view) {
                self.fetched_at.insert(l.view.clone(), l.at);
                self.issues.insert(l.view, l.issues);
            }
        }
        self.prs = self.cache.prs.clone();
        self.filter_opts = self.cache.filter_opts.clone();
        // priorities and users now live in the shared store
        self.cache.options.retain(|k, _| k.starts_with("status:"));
        self.sync_refs();
        self.details = self.cache.details.clone();
        if self.cache.me.is_none() {
            self.spawn(|c| Msg::Me(c.myself().map_err(|e| e.to_string())));
        }
        self.clamp_issue();
    }

    pub fn save_cache(&mut self) {
        // forget "seen" entries for issues no longer in any cached list
        let live: HashSet<String> =
            self.cache.lists.values().flat_map(|l| l.issues.iter().map(|i| i.key.clone())).collect();
        let seen: HashMap<String, String> =
            self.cache.seen.iter().filter(|(k, _)| live.contains(*k)).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.cache.seen = seen;
        self.cache.details.retain(|k, _| live.contains(k));
        cache::save(&mut self.cache);
        self.dirty = false;
        self.last_save = Instant::now();
    }

    /// New or updated since the user last looked at it.
    pub fn is_changed(&self, i: &Issue) -> bool {
        self.cache.seen.get(&i.key) != Some(&i.updated)
    }

    pub fn changed_count(&self, view: &str) -> usize {
        if !self.cache.baselined.contains(view) {
            return 0;
        }
        self.issues.get(view).map(|l| l.iter().filter(|i| self.is_changed(i)).count()).unwrap_or(0)
    }

    fn mark_seen(&mut self, keys: &[String]) {
        let Some(view) = self.current_project().map(|p| p.key.clone()) else { return };
        let Some(list) = self.issues.get(&view) else { return };
        for i in list.iter().filter(|i| keys.contains(&i.key)) {
            self.cache.seen.insert(i.key.clone(), i.updated.clone());
        }
    }

    fn refresh_all(&mut self) {
        let mut views: Vec<String> = self.fresh.iter().cloned().collect();
        if let Some(k) = self.current_project().map(|p| p.key.clone())
            && !views.contains(&k)
        {
            views.push(k);
        }
        let pr_views: Vec<String> = self.prs_fresh.iter().cloned().chain(views.iter().cloned()).collect();
        for v in views {
            self.fetch_issues(v);
        }
        for v in pr_views {
            self.fetch_prs(v);
        }
        self.last_refresh = Instant::now();
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
        if self.filter_opts_fresh.contains(&project) || !self.filter_opts_loading.insert(project.clone()) {
            return;
        }
        let users = if project == MY_WORK { vec![] } else { vec![project.clone()] };
        let needs = self.ref_needs(true, &users);
        self.spawn(move |c| {
            let r = load_filter_opts(&c, &project, &needs).map_err(|e| e.to_string());
            Msg::FilterOpts(project, r)
        });
    }

    /// Reference data not yet refreshed this session; marks it as being refreshed.
    fn ref_needs(&mut self, priorities: bool, projects: &[String]) -> RefNeeds {
        RefNeeds {
            priorities: priorities && self.refs_fresh.insert("priority".into()),
            users: projects.iter().filter(|p| self.refs_fresh.insert(format!("users:{p}"))).cloned().collect(),
            me: !projects.is_empty() && self.cache.me.is_none(),
        }
    }

    fn merge_refs(&mut self, r: Refs) {
        if let Some(p) = r.priorities {
            self.cache.priorities = p;
        }
        for (k, u) in r.users {
            self.cache.users.insert(k, u);
        }
        if r.me.is_some() {
            self.cache.me = r.me;
        }
        self.dirty = true;
        self.sync_refs();
        self.refresh_popup();
    }

    /// Refill an open Priority/Assignee popup from the shared store.
    fn refresh_popup(&mut self) {
        let Some(p) = self.popup.as_ref() else { return };
        if let Some(opts) = self.ref_opts(p.kind, &p.targets) {
            let p = self.popup.as_mut().unwrap();
            p.loading = false;
            p.set_opts(opts, TransMap::new());
        }
    }

    /// Rebuild the filter panel's Priority/Assignee groups from the shared store.
    fn sync_refs(&mut self) {
        let me = self.cache.me.as_ref().map(|(id, _)| id.clone());
        for (view, o) in self.filter_opts.iter_mut() {
            if !self.cache.priorities.is_empty() {
                let v = self.cache.priorities.iter().map(|(_, n)| Choice::new(n.clone(), n.clone())).collect();
                o.groups.insert(Group::Priority, v);
            }
            if let Some(users) = self.cache.users.get(view).filter(|_| view != MY_WORK) {
                let mut people = vec![Choice::new(filter::ME, "Me"), Choice::new(filter::NOBODY, "Unassigned")];
                people.extend(
                    users
                        .iter()
                        .filter(|(id, _)| Some(id) != me.as_ref())
                        .map(|(id, name)| Choice::new(id.clone(), name.clone())),
                );
                o.groups.insert(Group::Assignee, people);
            }
        }
    }

    /// Edit-popup choices built from the shared store; None if something is missing.
    fn ref_opts(&self, kind: PopKind, targets: &[String]) -> Option<Vec<Opt>> {
        match kind {
            PopKind::Priority => {
                let p = &self.cache.priorities;
                (!p.is_empty()).then(|| p.iter().map(|(id, n)| Opt::new(n.clone(), id.clone())).collect())
            }
            PopKind::Assignee => {
                let (me_id, me_name) = self.cache.me.as_ref()?;
                let lists = projects_of(targets).iter().map(|p| self.cache.users.get(p)).collect::<Option<Vec<_>>>()?;
                let mut v = vec![Opt::new(format!("Me ({me_name})"), me_id.clone()), Opt::new("Unassigned", "")];
                // only users assignable in every target project
                v.extend(
                    lists[0]
                        .iter()
                        .filter(|(id, _)| id != me_id && lists[1..].iter().all(|l| l.iter().any(|(i, _)| i == id)))
                        .map(|(id, name)| Opt::new(name.clone(), id.clone())),
                );
                Some(v)
            }
            _ => None,
        }
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
        if let Some(k) = self.current_project().map(|p| p.key.clone()) {
            if !self.fresh.contains(&k) {
                self.fetch_issues(k.clone());
            }
            if !self.prs_fresh.contains(&k) {
                self.fetch_prs(k);
            }
        }
    }

    // ---------- GitHub ----------
    pub fn repos_for(&self, view: &str) -> Vec<String> {
        let cfg = self.client.config();
        if view == MY_WORK { cfg.all_repos() } else { cfg.repos_for(view) }
    }

    /// Whether the current view has a PR pane.
    pub fn show_prs(&self) -> bool {
        self.current_project().is_some_and(|p| !self.repos_for(&p.key).is_empty())
    }

    pub fn visible_prs(&self) -> &[Pr] {
        self.current_project().and_then(|p| self.prs.get(&p.key)).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn fetch_prs(&mut self, view: String) {
        let repos = self.repos_for(&view);
        if repos.is_empty() || !self.prs_loading.insert(view.clone()) {
            return;
        }
        let known: Vec<String> = self.projects.iter().map(|p| p.key.clone()).filter(|k| k != MY_WORK).collect();
        let (gh, tx) = (self.gh.clone(), self.tx.clone());
        thread::spawn(move || {
            let r = gh.prs(&repos, &known).map_err(|e| e.to_string()).map(|mut v| {
                if view == MY_WORK {
                    v.retain(|p| p.wants_me || p.mine);
                }
                v.sort_by_key(|p| !p.wants_me); // stable: keeps newest-first within groups
                v
            });
            let _ = tx.send(Msg::Prs(view, r));
        });
    }

    /// Open PRs linked to each Jira issue (by key in the PR title or branch), from every cached view.
    pub fn prs_by_issue(&self) -> HashMap<&str, Vec<&Pr>> {
        let mut m: HashMap<&str, Vec<&Pr>> = HashMap::new();
        let mut seen = HashSet::new();
        for p in self.prs.values().flatten() {
            if let Some(k) = &p.issue
                && seen.insert(p.url.as_str())
            {
                m.entry(k.as_str()).or_default().push(p);
            }
        }
        m
    }

    fn open_pr(&mut self) {
        if let Some(p) = self.pr_state.selected().and_then(|i| self.visible_prs().get(i)).cloned() {
            self.status = match open::that_detached(&p.url) {
                Ok(_) => format!("Opened #{}", p.number),
                Err(e) => format!("Error: couldn't open browser: {e}"),
            };
        }
    }

    /// Called every loop iteration: lazily loads the detail of the selected issue (debounced).
    pub fn tick(&mut self) {
        let key = self.current_issue_key();
        if key != self.last_key {
            self.last_key = key.clone();
            self.key_changed_at = Instant::now();
            self.detail_scroll = 0;
            if let Some(k) = &key {
                self.mark_seen(std::slice::from_ref(k));
            }
        }
        if self.dirty && self.last_save.elapsed() > Duration::from_secs(3) {
            self.save_cache();
        }
        let mins = self.client.refresh_minutes();
        if mins > 0 && self.last_refresh.elapsed() >= Duration::from_secs(mins * 60) {
            self.refresh_all();
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
                let keep = self.current_project().map(|p| p.key.clone());
                self.cache.projects = p.clone();
                self.projects = with_my_work(p);
                self.sort_projects(keep);
                if self.proj_state.selected().is_none() {
                    self.proj_state.select(Some(0));
                }
                self.ensure_issues();
                self.dirty = true;
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
                        if self.current_project().is_some_and(|p| p.key == k) {
                            self.marked.retain(|m| keys.contains(m));
                        }
                        // first load of a view: everything counts as seen
                        let first = self.cache.baselined.insert(k.clone());
                        for i in &list {
                            if first || self.just_edited.remove(&i.key) {
                                self.cache.seen.insert(i.key.clone(), i.updated.clone());
                            }
                        }
                        // drop cached details of issues that changed on the server
                        if let Some(old) = self.issues.get(&k) {
                            let before: HashMap<&String, &String> = old.iter().map(|i| (&i.key, &i.updated)).collect();
                            for i in &list {
                                if before.get(&i.key) != Some(&&i.updated) {
                                    self.details.remove(&i.key);
                                    self.cache.details.remove(&i.key);
                                }
                            }
                        }
                        let at = cache::now();
                        self.cache.lists.insert(jql.clone(), cache::CachedList { view: k.clone(), jql, issues: list.clone(), at });
                        self.fetched_at.insert(k.clone(), at);
                        self.fresh.insert(k.clone());
                        self.issues.insert(k, list);
                        self.clamp_issue();
                        self.dirty = true;
                    }
                    Err(e) => self.status = format!("Error ({k}): {e}"),
                }
            }
            Msg::Prs(k, r) => {
                self.prs_loading.remove(&k);
                self.prs_fresh.insert(k.clone());
                match r {
                    Ok(v) => {
                        self.pr_errors.remove(&k);
                        self.cache.prs.insert(k.clone(), v.clone());
                        self.prs.insert(k, v);
                        let n = self.visible_prs().len();
                        let sel = self.pr_state.selected().map(|i| i.min(n.saturating_sub(1)));
                        self.pr_state.select(if n == 0 { None } else { sel.or(Some(0)) });
                        self.dirty = true;
                    }
                    Err(e) => {
                        self.pr_errors.insert(k, e);
                    }
                }
            }
            Msg::FilterOpts(k, r) => {
                self.filter_opts_loading.remove(&k);
                match r {
                    Ok((o, refs)) => {
                        self.cache.filter_opts.insert(k.clone(), o.clone());
                        self.filter_opts_fresh.insert(k.clone());
                        self.filter_opts.insert(k, o);
                        self.merge_refs(refs);
                    }
                    Err(e) => {
                        self.refs_fresh.clear();
                        self.status = format!("Error (filters): {e}");
                    }
                }
            }
            Msg::Refs(id, r) => {
                match r {
                    Ok(refs) => self.merge_refs(refs),
                    Err(e) => {
                        self.refs_fresh.clear();
                        match self.popup.as_mut().filter(|p| p.id == id) {
                            Some(p) if p.loading => {
                                p.loading = false;
                                p.error = Some(e);
                            }
                            _ => self.status = format!("Error: couldn't refresh options: {e}"),
                        }
                    }
                }
            }
            Msg::Detail(k, r) => {
                self.detail_loading.remove(&k);
                match r {
                    Ok(d) => {
                        self.cache.details.insert(k.clone(), (*d).clone());
                        self.details.insert(k, *d);
                        self.dirty = true;
                    }
                    Err(e) => self.status = format!("Error ({k}): {e}"),
                }
            }
            Msg::Options(id, key, r) => {
                if let Ok(v) = &r {
                    self.cache.options.insert(key, v.clone());
                    self.dirty = true;
                }
                if let Some(p) = self.popup.as_mut().filter(|p| p.id == id) {
                    let had_cached = !p.loading;
                    p.loading = false;
                    match r {
                        Ok((opts, trans)) => p.set_opts(opts, trans),
                        Err(e) if had_cached => self.status = format!("Error: couldn't refresh options: {e}"),
                        Err(e) => p.error = Some(e),
                    }
                }
            }
            Msg::Me(r) => {
                if let Ok(me) = r {
                    self.cache.me = Some(me);
                    self.dirty = true;
                }
            }
            Msg::Applied { keys, ok_keys, edit, errs } => {
                self.busy = false;
                let ok = ok_keys.len();
                self.apply_local(&ok_keys, &edit);
                for k in &keys {
                    self.details.remove(k);
                }
                self.status = if errs.is_empty() {
                    self.marked.clear();
                    format!("Updated {ok} issue(s)")
                } else {
                    format!("Error: {ok} ok, {} failed — {}", errs.len(), errs.join(" | "))
                };
                self.just_edited.extend(keys);
                self.refresh_all();
                self.last_key = None; // force detail reload
            }
        }
    }

    /// Reflect a successful edit in every loaded list immediately.
    fn apply_local(&mut self, keys: &[String], edit: &Edit) {
        let done: HashSet<String> = self.filter_opts.values().flat_map(|o| o.done.iter().cloned()).collect();
        let known: HashSet<String> = self
            .filter_opts
            .values()
            .flat_map(|o| o.groups.get(&Group::Status).into_iter().flatten().map(|c| c.value.clone()))
            .collect();
        for list in self.issues.values_mut() {
            for i in list.iter_mut().filter(|i| keys.contains(&i.key)) {
                match edit {
                    Edit::Status(to) if !to.is_empty() => {
                        i.status = to.clone();
                        if done.contains(to) {
                            i.cat = crate::jira::Cat::Done;
                        } else if known.contains(to) && i.cat == crate::jira::Cat::Done {
                            i.cat = crate::jira::Cat::Todo;
                        }
                    }
                    Edit::Assignee { id, name } => {
                        i.assignee_id = id.clone();
                        i.assignee = name.clone();
                    }
                    Edit::Priority(p) => i.priority = p.clone(),
                    _ => {}
                }
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
                    self.pr_state.select(Some(0));
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
            Pane::Prs => {
                let n = self.visible_prs().len();
                if n == 0 {
                    return;
                }
                let i = self.pr_state.selected().unwrap_or(0) as isize;
                self.pr_state.select(Some((i + delta).clamp(0, n as isize - 1) as usize));
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
        let view = fp.project;
        let jql = self.filter_for(&view).to_jql(&view);
        if let Some(l) = self.cache.lists.get(&jql).cloned() {
            // used this filter before: show that list right away
            self.fetched_at.insert(view.clone(), l.at);
            self.issues.insert(view.clone(), l.issues);
        } else if let Some(preview) = self.local_preview(&view) {
            self.issues.insert(view.clone(), preview);
        } // else: keep the current list until Jira answers
        self.fresh.remove(&view);
        self.marked.clear();
        self.issue_state.select(None);
        self.clamp_issue();
        self.fetch_issues(view);
    }

    /// Everything cached for a view, filtered locally with its current filter.
    fn local_preview(&self, view: &str) -> Option<Vec<Issue>> {
        let f = self.filter_for(view);
        let me = self.cache.me.as_ref().map(|(id, _)| id.as_str());
        let mut pool: HashMap<String, (u64, Issue)> = HashMap::new();
        for l in self.cache.lists.values().filter(|l| l.view == view) {
            for i in &l.issues {
                if pool.get(&i.key).is_none_or(|(at, _)| *at < l.at) {
                    pool.insert(i.key.clone(), (l.at, i.clone()));
                }
            }
        }
        let mut out = vec![];
        for (_, i) in pool.into_values() {
            if f.matches(&i, me)? {
                out.push(i);
            }
        }
        Some(out)
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
                Opt::new(label, c.name())
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
        if self.current_project().is_none() {
            return;
        }
        self.popup_seq += 1;
        let id = self.popup_seq;
        if matches!(kind, PopKind::Priority | PopKind::Assignee) {
            let opts = self.ref_opts(kind, &targets);
            let needs = match kind {
                PopKind::Priority => self.ref_needs(true, &[]),
                _ => self.ref_needs(false, &projects_of(&targets)),
            };
            let mut state = ListState::default();
            if opts.as_ref().is_some_and(|o| !o.is_empty()) {
                state.select(Some(0));
            }
            self.popup = Some(Popup {
                id,
                kind,
                targets,
                loading: opts.is_none(),
                error: None,
                opts: opts.unwrap_or_default(),
                trans: TransMap::new(),
                filter: String::new(),
                state,
            });
            // anything already in flight fills the popup via refresh_popup when it lands
            if needs.any() {
                self.spawn(move |c| Msg::Refs(id, load_refs(&c, &needs).map_err(|e| e.to_string())));
            }
            return;
        }
        let key = self.options_key(&targets);
        let cached = self.cache.options.get(&key).cloned();
        let mut state = ListState::default();
        if cached.as_ref().is_some_and(|(o, _)| !o.is_empty()) {
            state.select(Some(0));
        }
        let (opts, trans) = cached.clone().unwrap_or_default();
        self.popup = Some(Popup {
            id,
            kind,
            targets: targets.clone(),
            opts,
            trans,
            filter: String::new(),
            state,
            loading: cached.is_none(),
            error: None,
        });
        // always refresh in the background; cached choices are shown meanwhile
        self.spawn(move |c| Msg::Options(id, key, load_options(&c, kind, &targets).map_err(|e| e.to_string())));
    }

    /// Cache key for status transitions, which depend on each issue's current status.
    fn options_key(&self, targets: &[String]) -> String {
        let status = |k: &String| {
            self.issues.values().flatten().find(|i| &i.key == k).map(|i| i.status.clone()).unwrap_or_default()
        };
        let mut parts: Vec<String> = targets.iter().map(|k| format!("{k}@{}", status(k))).collect();
        parts.sort();
        format!("status:{}", parts.join(","))
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
        if self.current_project().is_none() {
            return;
        }
        self.busy = true;
        self.status = format!("Updating {} issue(s)…", p.targets.len());
        let Popup { kind, targets, trans, .. } = p;
        let edit = match kind {
            PopKind::Status => Edit::Status(opt.extra.clone()),
            PopKind::Assignee => {
                let name = if opt.value.is_empty() {
                    "—".to_string()
                } else if let Some(n) = opt.label.strip_prefix("Me (").and_then(|l| l.strip_suffix(')')) {
                    n.to_string()
                } else {
                    opt.label.clone()
                };
                Edit::Assignee { id: opt.value.clone(), name }
            }
            PopKind::Priority => Edit::Priority(opt.label.clone()),
            PopKind::Sort => Edit::None,
        };
        self.spawn(move |c| {
            let (mut ok_keys, mut errs) = (vec![], vec![]);
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
                    Ok(()) => ok_keys.push(key.clone()),
                    Err(e) => errs.push(format!("{key}: {e}")),
                }
            }
            Msg::Applied { keys: targets, ok_keys, edit, errs }
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
                if self.show_prs() {
                    panes.push(Pane::Prs);
                }
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
                    _ => Pane::Issues,
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                self.focus = match self.focus {
                    Pane::Projects => Pane::Issues,
                    _ if self.show_detail => Pane::Detail,
                    f => f,
                }
            }
            KeyCode::Char('g') => {
                self.focus = if self.focus == Pane::Prs || !self.show_prs() { Pane::Issues } else { Pane::Prs };
            }
            KeyCode::Char(' ' | 's' | 'a' | 'p') if self.focus == Pane::Prs => {
                self.status = "Pull requests are read-only here (o opens in browser)".into();
            }
            KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('*') => self.toggle_pin(),
            KeyCode::Char(c @ '1'..='9') => self.jump_pinned(c as usize - '1' as usize),
            KeyCode::Char('z') => {
                self.show_projects = !self.show_projects;
                if self.show_projects {
                    self.focus = Pane::Projects;
                } else if self.focus == Pane::Projects {
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
                    self.fetch_issues(k.clone());
                    self.fetch_prs(k);
                }
            }
            KeyCode::Char('R') => {
                self.details.clear();
                self.fresh.clear();
                self.fetch_projects();
                self.refresh_all();
            }
            KeyCode::Char('c') => {
                let keys: Vec<String> = self.visible_issues().iter().map(|i| i.key.clone()).collect();
                self.mark_seen(&keys);
                self.dirty = true;
            }
            KeyCode::Enter => match self.focus {
                Pane::Projects => self.focus = Pane::Issues,
                _ => self.toggle_detail(),
            },
            KeyCode::Char('o') if self.focus == Pane::Prs => self.open_pr(),
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
        self.projects.sort_by_key(|p| {
            if p.key == MY_WORK { 0 } else { pins.iter().position(|k| *k == p.key).map_or(usize::MAX, |i| i + 1) }
        });
        if let Some(k) = keep {
            self.proj_state.select(self.projects.iter().position(|p| p.key == k));
        }
    }

    fn toggle_pin(&mut self) {
        let Some(key) = self.current_project().map(|p| p.key.clone()) else { return };
        if key == MY_WORK {
            return;
        }
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

fn load_options(c: &Client, kind: PopKind, targets: &[String]) -> anyhow::Result<(Vec<Opt>, TransMap)> {
    let mut trans = TransMap::new();
    let opts = match kind {
        PopKind::Status => {
            // Only offer transitions available for every target issue.
            let mut common: Option<Vec<(String, String, String)>> = None; // (name, label, to)
            for key in targets {
                let ts = c.transitions(key)?;
                let names: HashSet<&String> = ts.iter().map(|t| &t.1).collect();
                common = Some(match common {
                    None => ts
                        .iter()
                        .map(|(_, name, to)| {
                            let label = if name == to { name.clone() } else { format!("{name}  → {to}") };
                            (name.clone(), label, to.clone())
                        })
                        .collect(),
                    Some(prev) => prev.into_iter().filter(|(n, _, _)| names.contains(n)).collect(),
                });
                trans.insert(key.clone(), ts.into_iter().map(|(id, name, _)| (name, id)).collect());
            }
            common
                .unwrap_or_default()
                .into_iter()
                .map(|(value, label, to)| Opt { label, value, extra: to })
                .collect()
        }
        _ => vec![],
    };
    Ok((opts, trans))
}

fn load_filter_opts(c: &Client, project: &str, needs: &RefNeeds) -> anyhow::Result<(FilterOpts, Refs)> {
    let plain = |v: Vec<String>| v.into_iter().map(|n| Choice::new(n.clone(), n)).collect::<Vec<_>>();
    let mut o = FilterOpts::default();
    let my_work = project == MY_WORK;
    let statuses = if my_work { c.all_statuses()? } else { c.statuses(project)? };
    o.done = statuses.iter().filter(|(_, done)| *done).map(|(n, _)| n.clone()).collect();
    o.groups.insert(Group::Status, plain(statuses.into_iter().map(|(n, _)| n).collect()));
    o.groups.insert(Group::Type, plain(if my_work { c.all_issue_types()? } else { c.issue_types(project)? }));
    Ok((o, load_refs(c, needs)?))
}

fn load_refs(c: &Client, needs: &RefNeeds) -> anyhow::Result<Refs> {
    let mut r = Refs::default();
    if needs.priorities {
        r.priorities = Some(c.priorities()?);
    }
    for p in &needs.users {
        r.users.push((p.clone(), c.assignable(p)?));
    }
    if needs.me {
        r.me = Some(c.myself()?);
    }
    Ok(r)
}

/// Prepend the "My work" pseudo-project.
fn with_my_work(mut v: Vec<Project>) -> Vec<Project> {
    v.retain(|p| p.key != MY_WORK);
    v.insert(0, Project { key: MY_WORK.into(), name: "My work".into() });
    v
}

/// Distinct project keys of issue keys ("ABC-12" -> "ABC").
fn projects_of(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for k in keys {
        let p = k.rsplit_once('-').map_or(k.as_str(), |(p, _)| p).to_string();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}
