use crate::app::{App, Pane, PopKind};
use crate::view::{Col, EditKind};
use crate::filter::{Group, MY_WORK};
use crate::github::{Checks, Pr, Review, age};
use crate::jira::{Cat, Detail};
use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Cell, Clear, List, ListItem, Paragraph, Row, Table, Wrap},
};

const ACCENT: Color = Color::Cyan;
const MARK: Color = Color::Magenta;
const CHANGED: Color = Color::Yellow;

fn block(title: String, focused: bool) -> Block<'static> {
    let b = Block::bordered().border_type(BorderType::Rounded).title(title);
    if focused { b.border_style(Style::new().fg(ACCENT)) } else { b.border_style(Style::new().dark_gray()) }
}

fn cat_color(c: &Cat) -> Color {
    match c {
        Cat::Todo => Color::Gray,
        Cat::Doing => Color::Blue,
        Cat::Done => Color::Green,
    }
}

fn prio_color(p: &str) -> Color {
    match p.to_lowercase().as_str() {
        "highest" | "blocker" | "critical" => Color::Red,
        "high" | "major" => Color::LightRed,
        "medium" => Color::Yellow,
        "low" | "minor" => Color::Green,
        "lowest" | "trivial" => Color::DarkGray,
        _ => Color::Gray,
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let mut cons = vec![];
    if app.show_projects {
        cons.push(Constraint::Length(32));
    }
    cons.push(Constraint::Fill(3));
    if app.show_detail {
        cons.push(Constraint::Fill(2));
    }
    let areas = Layout::horizontal(cons).split(main);
    let mut it = areas.iter().copied();
    if app.show_projects {
        draw_projects(f, app, it.next().unwrap());
    }
    let mid = it.next().unwrap();
    if app.show_prs() {
        let [top, bottom] = Layout::vertical([Constraint::Fill(3), Constraint::Fill(2)]).areas(mid);
        draw_issues(f, app, top);
        draw_prs(f, app, bottom);
    } else {
        draw_issues(f, app, mid);
    }
    if app.show_detail {
        let r = it.next().unwrap();
        if app.focus == Pane::Prs { draw_pr_detail(f, app, r) } else { draw_detail(f, app, r) }
    }
    draw_footer(f, app, footer);
    if app.popup.is_some() {
        draw_popup(f, app);
    }
    if app.fpanel.is_some() {
        draw_filter(f, app);
    }
    if app.order_edit.is_some() {
        draw_order_edit(f, app);
    }
    if app.show_help {
        draw_help(f);
    }
}

fn draw_projects(f: &mut Frame, app: &mut App, area: Rect) {
    // inner width minus borders (2) and highlight symbol (1)
    let width = area.width.saturating_sub(3) as usize;
    let items: Vec<ListItem> = app
        .projects
        .iter()
        .map(|p| {
            let count = match app.issues.get(&p.key) {
                Some(v) => v.len().to_string(),
                None if app.loading.contains_key(&p.key) => "…".into(),
                None => String::new(),
            };
            let changed = app.changed_count(&p.key);
            let changed = if changed > 0 { format!("+{changed} ") } else { String::new() };
            let my_work = p.key == MY_WORK;
            let pin = match app.pinned.iter().position(|k| *k == p.key) {
                _ if my_work => "◎ ".into(),
                Some(i) if i < 9 => format!("{} ", i + 1),
                Some(_) => "★ ".into(),
                None => "  ".into(),
            };
            let key = if my_work { String::new() } else { format!("{:<8}", p.key) };
            let used = 2 + key.chars().count() + 1 + changed.chars().count() + count.chars().count();
            let room = width.saturating_sub(used);
            let mut name: String = p.name.chars().take(room).collect();
            if p.name.chars().count() > room && room > 0 {
                name.pop();
                name.push('…');
            }
            let pad = room.saturating_sub(name.chars().count()) + 1;
            let name_style = if my_work { Style::new().fg(ACCENT).bold() } else { Style::new() };
            ListItem::new(Line::from(vec![
                Span::styled(pin, Style::new().fg(if my_work { ACCENT } else { Color::Yellow })),
                Span::styled(key, Style::new().bold()),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(pad)),
                Span::styled(changed, Style::new().fg(CHANGED).bold()),
                Span::styled(count, Style::new().fg(Color::Yellow)),
            ]))
        })
        .collect();
    let title =
        if app.loading_projects { " Projects (loading…) ".into() } else { format!(" Projects ({}) ", app.projects.len()) };
    let list = List::new(items)
        .block(block(title, app.focus == Pane::Projects))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, area, &mut app.proj_state);
}

fn draw_issues(f: &mut Frame, app: &mut App, area: Rect) {
    let baselined = app.current_project().is_some_and(|p| app.cache.baselined.contains(&p.key));
    let issues = app.visible_issues();
    let show_pr = app.has_prs();
    let cols: Vec<Col> = app.view.cols().into_iter().filter(|c| *c != Col::Pr || show_pr).collect();
    let by_issue = app.prs_by_issue();
    let pr_cell = |key: &str| -> Cell<'static> {
        let Some(ps) = by_issue.get(key) else { return Cell::from("") };
        let p = ps[0];
        let more = if ps.len() > 1 { format!("+{}", ps.len() - 1) } else { String::new() };
        let style = review_span(p).style;
        Cell::from(Line::from(vec![Span::styled(format!("#{}", p.number), style), Span::raw(more)]))
    };
    let rows: Vec<Row> = issues
        .iter()
        .map(|i| {
            let marked = app.marked.contains(&i.key);
            let changed = baselined && app.is_changed(i);
            let (sym, sym_color) = if marked {
                ("●", MARK)
            } else if changed {
                ("◆", CHANGED)
            } else {
                (" ", MARK)
            };
            let mut cells = vec![Cell::from(sym).fg(sym_color)];
            cells.extend(cols.iter().map(|c| match c {
                Col::Key => {
                    let k = Cell::from(i.key.clone()).bold();
                    if changed { k.fg(CHANGED) } else { k }
                }
                Col::Pr => pr_cell(&i.key),
                Col::Status => Cell::from(i.status.clone()).fg(cat_color(&i.cat)),
                Col::Priority => Cell::from(i.priority.clone()).fg(prio_color(&i.priority)),
                Col::Assignee => Cell::from(i.assignee.clone()),
                Col::Type => Cell::from(i.issuetype.clone()),
                Col::Labels => label_cell(&app.view.sorted_labels(&i.labels)),
                Col::Summary => Cell::from(i.summary.clone()),
            }));
            let row = Row::new(cells);
            if marked { row.fg(MARK) } else { row }
        })
        .collect();
    let n = issues.len();
    let (pkey, loading) = match app.current_project() {
        Some(p) => (p.key.clone(), app.loading.contains_key(&p.key)),
        None => (String::new(), false),
    };
    let fdesc = app.filter_for(&pkey).short();
    let label = if pkey == MY_WORK { "My work" } else { pkey.as_str() };
    let more = if n >= crate::filter::MAX_ISSUES { "+" } else { "" };
    let mut title = format!(" {label} issues ({n}{more}) · {fdesc} ");
    if !app.fresh.contains(&pkey)
        && let Some(at) = app.fetched_at.get(&pkey)
    {
        title.push_str(&format!("· cached {} ", crate::cache::ago(*at)));
    }
    if !app.marked.is_empty() {
        title.push_str(&format!("· {} marked ", app.marked.len()));
    }
    if loading {
        title.push_str("· loading… ");
    }
    if !app.filter.is_empty() || app.filtering {
        title.push_str(&format!("· filter: {}{} ", app.filter, if app.filtering { "▏" } else { "" }));
    }
    let arrow = if app.sort_desc { " ▼" } else { " ▲" };
    let header: Vec<String> = std::iter::once(String::new())
        .chain(cols.iter().map(|c| {
            if *c == app.sort_col { format!("{}{arrow}", c.name()) } else { c.name().to_string() }
        }))
        .collect();
    let widths: Vec<Constraint> = std::iter::once(Constraint::Length(1))
        .chain(cols.iter().map(|c| match c {
            Col::Key => Constraint::Length(10),
            Col::Pr => Constraint::Length(7),
            Col::Status => Constraint::Length(13),
            Col::Priority => Constraint::Length(8),
            Col::Assignee => Constraint::Length(14),
            Col::Type => Constraint::Length(10),
            Col::Labels => Constraint::Length(LABEL_W as u16),
            Col::Summary => Constraint::Min(15),
        }))
        .collect();
    let table = Table::new(rows, widths)
    .header(Row::new(header).style(Style::new().fg(ACCENT).bold()))
    .block(block(title, app.focus == Pane::Issues))
    .row_highlight_style(Style::new().bg(Color::DarkGray))
    .highlight_symbol("▌")
    .column_spacing(1);
    f.render_stateful_widget(table, area, &mut app.issue_state);
}

const LABEL_W: usize = 16;

/// Most important label, then "+N" for the rest; the label is cut so "+N" always shows.
fn label_cell(labels: &[&String]) -> Cell<'static> {
    let Some(first) = labels.first() else { return Cell::from("") };
    let more = if labels.len() > 1 { format!(" +{}", labels.len() - 1) } else { String::new() };
    let room = LABEL_W - more.chars().count();
    let mut l: String = first.chars().take(room).collect();
    if first.chars().count() > room {
        l.pop();
        l.push('…');
    }
    Cell::from(Line::from(vec![Span::styled(l, Style::new().fg(Color::Yellow)), Span::styled(more, Style::new().dark_gray())]))
}

fn kv(k: &str, v: String, color: Option<Color>) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<9}"), Style::new().dark_gray()),
        Span::styled(v, color.map(|c| Style::new().fg(c)).unwrap_or_default()),
    ])
}

fn section(t: &str) -> Line<'static> {
    Line::from(Span::styled(format!("── {t} "), Style::new().fg(ACCENT).bold()))
}

fn detail_lines(d: &Detail) -> Vec<Line<'static>> {
    let mut l = vec![
        Line::from(vec![
            Span::styled(d.key.clone(), Style::new().bold().fg(ACCENT)),
            Span::raw("  "),
            Span::styled(d.issuetype.clone(), Style::new().dark_gray()),
        ]),
        Line::from(Span::styled(d.summary.clone(), Style::new().bold())),
        Line::raw(""),
        kv("Status", d.status.clone(), Some(Color::Blue)),
        kv("Priority", d.priority.clone(), Some(prio_color(&d.priority))),
        kv("Assignee", d.assignee.clone(), None),
        kv("Reporter", d.reporter.clone(), None),
    ];
    if let Some(p) = &d.parent {
        l.push(kv("Parent", p.clone(), None));
    }
    if !d.labels.is_empty() {
        l.push(kv("Labels", d.labels.join(", "), Some(Color::Yellow)));
    }
    l.push(kv("Created", d.created.clone(), None));
    l.push(kv("Updated", d.updated.clone(), None));
    l.push(Line::raw(""));
    l.push(section("Description"));
    if d.description.is_empty() {
        l.push(Line::from("No description".dark_gray().italic()));
    } else {
        l.extend(d.description.lines().map(|s| Line::raw(s.to_string())));
    }
    l.push(Line::raw(""));
    l.push(section(&format!("Comments ({})", d.comments.len())));
    for c in &d.comments {
        l.push(Line::from(vec![
            Span::styled(c.author.clone(), Style::new().bold()),
            Span::styled(format!("  {}", c.created), Style::new().dark_gray()),
        ]));
        l.extend(c.body.lines().map(|s| Line::raw(format!("  {s}"))));
        l.push(Line::raw(""));
    }
    l
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let key = app.current_issue_key();
    let focused = app.focus == Pane::Detail;
    let (title, body) = match key.as_ref().and_then(|k| app.details.get(k)) {
        Some(d) => (format!(" {} ", d.key), detail_lines(d)),
        None => {
            let msg = if key.is_some() { "Loading…" } else { "No issue selected" };
            (" Detail ".into(), vec![Line::from(msg.dark_gray())])
        }
    };
    let max = body.len().saturating_sub(1) as u16;
    app.detail_scroll = app.detail_scroll.min(max);
    let p = Paragraph::new(body)
        .block(block(title, focused))
        .wrap(Wrap { trim: false })
        .scroll((app.detail_scroll, 0));
    f.render_widget(p, area);
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let hints = if app.fpanel.is_some() {
        "↑/↓ move · ←/→ column · Space tick · type to search · Ctrl+x untick all · Ctrl+r reset group · Enter apply · Esc cancel"
    } else if app.order_edit.is_some() {
        "j/k move · ←/→, J/K or Shift+↑/↓ reorder · Space show/hide · Enter save · Esc cancel"
    } else if app.popup.is_some() {
        "type to search · ↑/↓ choose · Enter apply · Esc cancel"
    } else if app.filtering {
        "type to filter · Enter keep · Esc clear"
    } else {
        "? help · Ctrl+c quit"
    };
    let line = if app.status.is_empty() {
        Line::from(hints.dark_gray())
    } else {
        let st = if app.status.starts_with("Error") {
            Style::new().fg(Color::Red)
        } else if app.busy {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().fg(Color::Green)
        };
        Line::from(vec![Span::styled(app.status.clone(), st), Span::raw("  "), hints.dark_gray()])
    };
    let ver = concat!("v", env!("CARGO_PKG_VERSION"));
    let [left, right] =
        Layout::horizontal([Constraint::Min(1), Constraint::Length(ver.len() as u16 + 1)]).areas(area);
    f.render_widget(Paragraph::new(line), left);
    f.render_widget(Paragraph::new(Line::from(ver.dark_gray()).right_aligned()), right);
}

fn draw_popup(f: &mut Frame, app: &mut App) {
    let Some(p) = app.popup.as_mut() else { return };
    let what = match p.kind {
        PopKind::Status => "status",
        PopKind::Assignee => "assignee",
        PopKind::Priority => "priority",
        PopKind::Sort => "sort",
    };
    let target = if p.targets.len() == 1 { p.targets[0].clone() } else { format!("{} issues", p.targets.len()) };
    let title = if p.kind == PopKind::Sort { " Sort by ".to_string() } else { format!(" Set {what} · {target} ") };

    let area = f.area();
    let w = (area.width * 2 / 3).clamp(30, 60).min(area.width);
    let h = (area.height * 2 / 3).clamp(8, 20).min(area.height);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    let outer = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(title);
    let inner = outer.inner(r);
    f.render_widget(outer, r);

    let [search, list_area] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::styled("› ", Style::new().fg(ACCENT)), Span::raw(format!("{}▏", p.filter))])),
        search,
    );
    if p.loading {
        f.render_widget(Paragraph::new("Loading…".dark_gray()), list_area);
        return;
    }
    if let Some(e) = &p.error {
        f.render_widget(Paragraph::new(e.clone().red()).wrap(Wrap { trim: true }), list_area);
        return;
    }
    let items: Vec<ListItem> = p.visible().iter().map(|o| ListItem::new(o.label.clone())).collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, list_area, &mut p.state);
}

fn draw_order_edit(f: &mut Frame, app: &mut App) {
    let Some(e) = app.order_edit.as_mut() else { return };
    let (title, hint) = match e.kind {
        EditKind::Columns => (" Columns ", " ✓ shown · top = leftmost "),
        EditKind::Labels => (" Label priority ", " top = shown first · ✗ never shown "),
    };
    let area = f.area();
    let longest = e.items.iter().map(|(n, _)| n.chars().count()).max().unwrap_or(0) as u16;
    let w = (longest + 12).max(40).min(area.width);
    let h = (e.items.len() as u16 + 2).clamp(5, 24).min(area.height);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(title)
        .title_bottom(Line::from(hint.dark_gray()).right_aligned());
    let labels = e.kind == EditKind::Labels;
    let items: Vec<ListItem> = e
        .items
        .iter()
        .enumerate()
        .map(|(i, (n, shown))| {
            let (mark, st) = match (shown, labels) {
                (true, _) => ("✓", Style::new().fg(Color::Green)),
                (false, true) => ("✗", Style::new().fg(Color::Red)),
                (false, false) => (" ", Style::new()),
            };
            let name = if *shown { Style::new() } else { Style::new().dark_gray() };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:>3} ", i + 1), Style::new().dark_gray()),
                Span::styled(format!("{mark} "), st),
                Span::styled(n.clone(), name),
            ]))
        })
        .collect();
    let list = List::new(items)
        .block(b)
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, r, &mut e.state);
}

type HelpGroup = (&'static str, &'static [(&'static str, &'static str)]);

const HELP_LEFT: &[HelpGroup] = &[
    ("Navigate", &[
        ("j k  ↑ ↓", "up / down"),
        ("Home End", "top / bottom"),
        ("PgUp PgDn", "page"),
        ("Tab  h l", "switch pane"),
        ("1-9", "jump to pinned project"),
    ]),
    ("Panels", &[
        ("Enter", "issue detail"),
        ("J K", "scroll detail"),
        ("z", "projects sidebar"),
        ("g", "pull requests"),
        ("o", "open issue / PR in browser"),
    ]),
    ("Find", &[
        ("/", "search text"),
        ("f", "filters (status, type, JQL…)"),
        ("t", "sort by column (again flips)"),
        ("Esc", "clear search, then marks"),
    ]),
];

const HELP_RIGHT: &[HelpGroup] = &[
    ("Edit", &[
        ("Space", "mark / unmark issue"),
        ("Ctrl+a", "mark / unmark all"),
        ("s", "status"),
        ("a", "assignee"),
        ("p", "priority"),
    ]),
    ("View", &[
        ("C", "columns: order / show"),
        ("L", "label priority"),
    ]),
    ("Project", &[
        ("*", "pin / unpin"),
        ("r", "refresh project + PRs"),
        ("R", "reload everything"),
        ("c", "mark listed issues as seen"),
        ("Ctrl+c", "quit"),
    ]),
];

fn help_lines(groups: &[HelpGroup]) -> Vec<Line<'static>> {
    let mut lines = vec![];
    for (i, (group, keys)) in groups.iter().enumerate() {
        if i > 0 {
            lines.push(Line::raw(""));
        }
        lines.push(section(group));
        for (k, d) in *keys {
            lines.push(Line::from(vec![
                Span::styled(format!(" {k:<11}"), Style::new().fg(Color::Yellow)),
                Span::raw(*d),
            ]));
        }
        if *group == "Edit" {
            lines.push(Line::from(" acts on marked issues, else the selected one".dark_gray()));
        }
    }
    lines
}

fn legend_lines() -> Vec<Line<'static>> {
    let sym = |s: &'static str, c: Color, d: &'static str| {
        Line::from(vec![Span::styled(format!(" {s:<11}"), Style::new().fg(c)), Span::raw(d)])
    };
    vec![
        Line::raw(""),
        section("Legend"),
        sym("●", MARK, "marked"),
        sym("◆", CHANGED, "new / changed since last look"),
        sym("◎ My work", ACCENT, "assigned to you, all projects"),
        Line::from(vec![
            Span::styled(" #123       ", Style::new().fg(Color::Yellow)),
            Span::raw("PR: "),
            Span::styled("review", Style::new().fg(Color::Yellow)),
            Span::raw(" · "),
            Span::styled("approved", Style::new().fg(Color::Green)),
            Span::raw(" · "),
            Span::styled("changes", Style::new().fg(Color::Red)),
        ]),
    ]
}

fn draw_help(f: &mut Frame) {
    let left = help_lines(HELP_LEFT);
    let mut right = help_lines(HELP_RIGHT);
    right.extend(legend_lines());

    let area = f.area();
    let two = area.width >= 100;
    let (w, h) = if two {
        (100, left.len().max(right.len()) as u16 + 2)
    } else {
        (area.width.min(50), (left.len() + right.len() + 1) as u16 + 2)
    };
    let [r] = Layout::horizontal([Constraint::Length(w.min(area.width))]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h.min(area.height))]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    let b = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(" Keys ")
        .title_bottom(Line::from(" any key closes ".dark_gray()).right_aligned());
    let inner = b.inner(r).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(b, r);
    if two {
        let [a, _, c] =
            Layout::horizontal([Constraint::Fill(1), Constraint::Length(3), Constraint::Fill(1)]).areas(inner);
        f.render_widget(Paragraph::new(left), a);
        f.render_widget(Paragraph::new(right), c);
    } else {
        let mut all = left;
        all.push(Line::raw(""));
        all.extend(right);
        f.render_widget(Paragraph::new(all), inner);
    }
}

fn draw_filter(f: &mut Frame, app: &mut App) {
    let Some(fp) = app.fpanel.as_mut() else { return };
    let opts = app.filter_opts.get(&fp.project);

    let area = f.area();
    let w = (area.width * 4 / 5).clamp(40, 110).min(area.width);
    let h = (area.height * 3 / 4).clamp(10, 30).min(area.height);
    let [r] = Layout::horizontal([Constraint::Length(w)]).flex(Flex::Center).areas(area);
    let [r] = Layout::vertical([Constraint::Length(h)]).flex(Flex::Center).areas(r);
    f.render_widget(Clear, r);
    let outer = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(format!(" Filters · {} ", if fp.project == MY_WORK { "My work" } else { fp.project.as_str() }))
        .title_bottom(Line::from(" Enter apply · Esc cancel ".dark_gray()).right_aligned());
    let inner = outer.inner(r);
    f.render_widget(outer, r);
    let [left, right] = Layout::horizontal([Constraint::Length(34), Constraint::Min(10)]).areas(inner);

    // groups + summaries
    let gw = left.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = Group::ALL
        .iter()
        .map(|g| {
            let sum: String = fp.draft.summary(*g).chars().take(gw).collect();
            let active = !fp.draft.is_group_default(*g);
            ListItem::new(vec![
                Line::from(Span::styled(g.name(), Style::new().bold())),
                Line::from(Span::styled(
                    format!("  {sum}"),
                    if active { Style::new().fg(Color::Yellow) } else { Style::new().dark_gray() },
                )),
            ])
        })
        .collect();
    let mut gstate = ratatui::widgets::ListState::default();
    gstate.select(Some(fp.group));
    let list = List::new(items)
        .block(block(" Group ".into(), !fp.in_opts))
        .highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, left, &mut gstate);

    let g = fp.current_group();
    if g == Group::Jql {
        let cursor = if fp.in_opts { "▏" } else { "" };
        let lines = vec![
            Line::from(vec![Span::styled("› ", Style::new().fg(ACCENT)), Span::raw(format!("{}{cursor}", fp.draft.jql))]),
            Line::raw(""),
            Line::from("Extra JQL, combined with the other groups using AND.".dark_gray()),
            Line::from("Examples:".dark_gray()),
            Line::from("  labels = backend".dark_gray()),
            Line::from("  created >= -30d".dark_gray()),
            Line::from("  text ~ \"login\"".dark_gray()),
            Line::from("Don't add ORDER BY.".dark_gray()),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(block(" JQL ".into(), fp.in_opts)),
            right,
        );
        return;
    }

    let b = block(format!(" {} ", g.name()), fp.in_opts);
    let ri = b.inner(right);
    f.render_widget(b, right);
    let [search, list_area, hint] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(ri);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("› ", Style::new().fg(ACCENT)),
            Span::raw(format!("{}{}", fp.search, if fp.in_opts { "▏" } else { "" })),
        ])),
        search,
    );
    let none_hint = match g {
        Group::Status => "Default: all except Done · none ticked = all",
        Group::Assignee => "None ticked = anyone",
        _ => "None ticked = any",
    };
    f.render_widget(Paragraph::new(none_hint.dark_gray()), hint);
    if opts.is_none() {
        f.render_widget(Paragraph::new("Loading…".dark_gray()), list_area);
        return;
    }
    let vis = fp.visible(opts);
    if fp.state.selected().is_none_or(|i| i >= vis.len()) {
        fp.state.select(if vis.is_empty() { None } else { Some(0) });
    }
    let items: Vec<ListItem> = vis
        .iter()
        .map(|c| {
            let on = fp.draft.is_ticked(g, &c.value, opts);
            ListItem::new(Line::from(vec![
                Span::styled(if on { "[x] " } else { "[ ] " }, Style::new().fg(if on { MARK } else { Color::DarkGray })),
                Span::styled(c.label.clone(), if on { Style::new().fg(MARK).bold() } else { Style::new() }),
            ]))
        })
        .collect();
    let list = List::new(items)
        .highlight_style(Style::new().bg(if fp.in_opts { Color::DarkGray } else { Color::Reset }))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, list_area, &mut fp.state);
}

// ---------- GitHub pull requests ----------
fn review_span(p: &Pr) -> Span<'static> {
    if p.draft {
        return Span::styled("draft", Style::new().dark_gray());
    }
    match p.review {
        Review::Required | Review::None => Span::styled("review", Style::new().fg(Color::Yellow)),
        Review::Approved => Span::styled("approved", Style::new().fg(Color::Green)),
        Review::Changes => Span::styled("changes", Style::new().fg(Color::Red)),
    }
}

fn checks_span(c: Checks) -> Span<'static> {
    match c {
        Checks::Pass => Span::styled("✓", Style::new().fg(Color::Green)),
        Checks::Fail => Span::styled("✗", Style::new().fg(Color::Red)),
        Checks::Pending => Span::styled("◌", Style::new().fg(Color::Yellow)),
        Checks::None => Span::styled("·", Style::new().dark_gray()),
    }
}

fn draw_prs(f: &mut Frame, app: &mut App, area: Rect) {
    let key = app.current_project().map(|p| p.key.clone()).unwrap_or_default();
    let focused = app.focus == Pane::Prs;
    let cur_issue = app.current_issue_key();
    let repos = app.repos_for(&key);
    let multi = repos.len() > 1;
    let prs = app.visible_prs();
    let waiting = prs.iter().filter(|p| p.wants_me).count();
    let label = if key == MY_WORK { "My review / my PRs".to_string() } else { repos.join(", ") };
    let mut title = format!(" Pull requests ({}) · {label} ", prs.len());
    if waiting > 0 {
        title.push_str(&format!("· {waiting} waiting for you "));
    }
    if app.prs_loading.contains(&key) {
        title.push_str("· loading… ");
    }
    let rows: Vec<Row> = prs
        .iter()
        .map(|p| {
            let linked = p.issue.is_some() && p.issue == cur_issue;
            let num = if multi || key == MY_WORK {
                format!("{}#{}", p.repo.rsplit('/').next().unwrap_or(""), p.number)
            } else {
                format!("#{}", p.number)
            };
            let row = Row::new(vec![
                Cell::from(if p.wants_me { Span::styled("●", Style::new().fg(MARK).bold()) } else { Span::raw("") }),
                Cell::from(num).bold(),
                Cell::from(p.issue.clone().unwrap_or_else(|| "—".into())).fg(ACCENT),
                Cell::from(p.title.clone()),
                Cell::from(p.author.clone()),
                Cell::from(review_span(p)),
                Cell::from(checks_span(p.checks)),
                Cell::from(age(&p.created_at)).dark_gray(),
            ]);
            if linked { row.bold().bg(Color::Rgb(30, 45, 60)) } else { row }
        })
        .collect();
    let num_w = if multi || key == MY_WORK {
        prs.iter().map(|p| p.repo.rsplit('/').next().unwrap_or("").len() + 1 + p.number.to_string().len()).max().unwrap_or(6).min(26) as u16
    } else {
        6
    };
    let table = Table::new(
        rows,
        [
            Constraint::Length(1),
            Constraint::Length(num_w),
            Constraint::Length(9),
            Constraint::Min(15),
            Constraint::Length(12),
            Constraint::Length(8),
            Constraint::Length(1),
            Constraint::Length(4),
        ],
    )
    .header(Row::new(["", "#", "Issue", "Title", "Author", "Review", "CI", "Age"]).style(Style::new().fg(ACCENT).bold()))
    .block(block(title.clone(), focused))
    .row_highlight_style(Style::new().bg(if focused { Color::DarkGray } else { Color::Reset }))
    .highlight_symbol(if focused { "▌" } else { " " })
    .column_spacing(1);
    if let Some(e) = app.pr_errors.get(&key).cloned()
        && prs.is_empty()
    {
        let p = Paragraph::new(Line::from(format!("GitHub: {e}").red())).wrap(Wrap { trim: true }).block(block(title, focused));
        f.render_widget(p, area);
        return;
    }
    f.render_stateful_widget(table, area, &mut app.pr_state);
}

fn draw_pr_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(p) = app.pr_state.selected().and_then(|i| app.visible_prs().get(i)).cloned() else {
        f.render_widget(Paragraph::new("No pull request selected".dark_gray()).block(block(" Pull request ".into(), false)), area);
        return;
    };
    let mut l = vec![
        Line::from(vec![
            Span::styled(format!("{}#{}", p.repo, p.number), Style::new().bold().fg(ACCENT)),
            Span::raw("  "),
            review_span(&p),
        ]),
        Line::from(Span::styled(p.title.clone(), Style::new().bold())),
        Line::raw(""),
        kv("Author", p.author.clone(), None),
        kv("Branch", p.branch.clone(), None),
        kv("Issue", p.issue.clone().unwrap_or_else(|| "—".into()), Some(ACCENT)),
        Line::from(vec![Span::styled(format!("{:<9}", "CI"), Style::new().dark_gray()), checks_span(p.checks)]),
        Line::from(vec![
            Span::styled(format!("{:<9}", "Changes"), Style::new().dark_gray()),
            Span::styled(format!("+{}", p.additions), Style::new().fg(Color::Green)),
            Span::raw(" "),
            Span::styled(format!("-{}", p.deletions), Style::new().fg(Color::Red)),
            Span::raw(format!(" in {} files", p.files)),
        ]),
        kv("Opened", format!("{} ago", age(&p.created_at)), None),
    ];
    if p.wants_me {
        l.push(Line::from(Span::styled("● Your review is requested", Style::new().fg(MARK).bold())));
    }
    l.push(Line::raw(""));
    l.push(section("Description"));
    if p.body.trim().is_empty() {
        l.push(Line::from("No description".dark_gray().italic()));
    } else {
        l.extend(p.body.lines().map(|s| Line::raw(s.to_string())));
    }
    let max = l.len().saturating_sub(1) as u16;
    app.detail_scroll = app.detail_scroll.min(max);
    f.render_widget(
        Paragraph::new(l).wrap(Wrap { trim: false }).scroll((app.detail_scroll, 0)).block(block(" Pull request ".into(), true)),
        area,
    );
}
