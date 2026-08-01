use ratatui::prelude::*;
use ratatui::widgets::*;

use crate::app::{App, Confirm, Focus, View};

const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(2),
    ])
    .split(f.area());

    draw_header(f, chunks[0], app);
    draw_tabs(f, chunks[1], app);

    if app.view == View::Log {
        draw_log(f, chunks[2], app);
    } else {
        let panes = Layout::horizontal([
            Constraint::Percentage(58),
            Constraint::Percentage(42),
        ])
        .split(chunks[2]);
        draw_list_pane(f, panes[0], app);
        draw_detail_pane(f, panes[1], app);
    }

    draw_footer(f, chunks[3], app);

    if app.confirm.is_some() {
        draw_confirm(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(
            " univ ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    if let Some(task) = &app.task {
        let frame = SPINNER[app.frame as usize % SPINNER.len()];
        spans.push(Span::styled(
            format!("{frame} {}", task.title),
            Style::default().fg(Color::Yellow),
        ));
    } else {
        spans.push(Span::styled(
            "store manager",
            Style::default().fg(Color::DarkGray),
        ));
    }
    let block = Block::default().borders(Borders::BOTTOM);
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_tabs(f: &mut Frame, area: Rect, app: &App) {
    let tabs = Tabs::new(vec![
        Line::from(format!(" {} ", View::Installed.label())),
        Line::from(format!(" {} ", View::Browse.label())),
        Line::from(format!(" {} ", View::Log.label())),
    ])
    .select(app.view.index())
    .divider("│")
    .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Yellow));
    f.render_widget(tabs, area);
}

fn draw_list_pane(f: &mut Frame, area: Rect, app: &App) {
    let (title, box_label, box_text, focused) = match app.view {
        View::Installed => (
            " Installed ",
            "filter",
            &app.installed_filter,
            app.focus == Focus::FilterBox,
        ),
        View::Browse => (
            " Browse ",
            "search",
            &app.search_query,
            app.focus == Focus::SearchBox,
        ),
        View::Log => unreachable!(),
    };

    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(title);
    f.render_widget(block, area);
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).split(inner);

    let input_style = if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(format!("{box_label}: "), input_style),
            Span::raw(box_text.to_string()),
        ])),
        chunks[0],
    );
    if focused && chunks[0].width > 0 {
        f.set_cursor_position((chunks[0].x + box_label.len() as u16 + 2, chunks[0].y));
    }

    let (items, selected) = match app.view {
        View::Installed => {
            let idxs = app.filtered();
            let sel = if idxs.is_empty() {
                None
            } else {
                Some(app.installed_cursor.min(idxs.len() - 1))
            };
            let items: Vec<ListItem> = idxs
                .iter()
                .map(|&i| {
                    let p = &app.installed[i];
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{:<24} ", cut(&p.name, 24))),
                        Span::raw(format!("{:<20} ", cut(&p.version, 20))),
                        Span::raw(format!("{:<10} ", cut(&p.architecture, 10))),
                        Span::styled(
                            first_line(&p.description),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            (items, sel)
        }
        View::Browse => {
            let sel = if app.search_results.is_empty() {
                None
            } else {
                Some(app.search_cursor.min(app.search_results.len() - 1))
            };
            let items: Vec<ListItem> = app
                .search_results
                .iter()
                .map(|p| {
                    let kind_style = if p.kind == "rpm" {
                        Color::Yellow
                    } else {
                        Color::Cyan
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(format!("{:<24} ", cut(&p.name, 24))),
                        Span::raw(format!("{:<20} ", cut(&p.version, 20))),
                        Span::styled(
                            format!("[{:<5}] ", p.kind),
                            Style::default().fg(kind_style),
                        ),
                        Span::raw(format!("{:<14} ", cut(&p.repo, 14))),
                        Span::styled(
                            first_line(&p.description),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            (items, sel)
        }
        View::Log => unreachable!(),
    };

    let list = List::new(items)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .bg(Color::Rgb(0x2a, 0x4a, 0x4f)),
        )
        .highlight_symbol("▸ ")
        .block(Block::default().borders(Borders::TOP));
    let mut state = ListState::default();
    state.select(selected);
    f.render_stateful_widget(list, chunks[1], &mut state);

    let msg = match app.view {
        View::Installed => {
            if let Some(err) = &app.installed_error {
                Some(Line::from(Span::styled(
                    format!("! {err}"),
                    Style::default().fg(Color::Red),
                )))
            } else if app.filtered().is_empty() {
                let hint = if app.installed.is_empty() {
                    "no packages installed — Tab to Browse and search"
                } else {
                    "no packages match the filter"
                };
                Some(Line::from(Span::styled(
                    hint,
                    Style::default().fg(Color::DarkGray),
                )))
            } else {
                None
            }
        }
        View::Browse => {
            if let Some(err) = &app.search_error {
                Some(Line::from(Span::styled(
                    format!("! {err}"),
                    Style::default().fg(Color::Red),
                )))
            } else if app.search_results.is_empty() && app.search_query.trim().is_empty() {
                Some(Line::from(Span::styled(
                    "type a search above and press Enter",
                    Style::default().fg(Color::DarkGray),
                )))
            } else if app.search_results.is_empty() {
                Some(Line::from(Span::styled(
                    "no matches",
                    Style::default().fg(Color::DarkGray),
                )))
            } else {
                None
            }
        }
        View::Log => None,
    };
    if let Some(line) = msg {
        f.render_widget(Paragraph::new(line), chunks[1]);
    }
}

fn draw_detail_pane(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL).border_type(BorderType::Rounded)
        .title(" details ");
    f.render_widget(block, area);
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });

    let (pkg, action) = match app.view {
        View::Installed => {
            let idxs = app.filtered();
            let sel = idxs.get(app.installed_cursor.min(idxs.len().saturating_sub(1)));
            match sel {
                Some(&i) => (Some(&app.installed[i]), "u uninstall   r refresh"),
                None => (None, ""),
            }
        }
        View::Browse => {
            if app.search_results.is_empty() {
                (None, "")
            } else {
                let i = app.search_cursor.min(app.search_results.len() - 1);
                (
                    Some(&app.search_results[i]),
                    "i install   n new search",
                )
            }
        }
        View::Log => (None, ""),
    };

    let Some(pkg) = pkg else {
        let msg = Paragraph::new(Line::from(Span::styled(
            "select a package on the left",
            Style::default().fg(Color::DarkGray),
        )))
        .alignment(Alignment::Center);
        f.render_widget(msg, inner);
        return;
    };

    let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(inner);
    let mut lines = vec![
        Line::from(Span::styled(
            pkg.name.clone(),
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan),
        )),
        Line::from(vec![
            Span::styled("version  ", Style::default().fg(Color::DarkGray)),
            Span::raw(pkg.version.clone()),
        ]),
        Line::from(vec![
            Span::styled("arch     ", Style::default().fg(Color::DarkGray)),
            Span::raw(pkg.architecture.clone()),
        ]),
    ];
    if !pkg.kind.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("kind     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                pkg.kind.clone(),
                Style::default().fg(if pkg.kind == "rpm" {
                    Color::Yellow
                } else {
                    Color::Cyan
                }),
            ),
        ]));
    }
    if !pkg.repo.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("repo     ", Style::default().fg(Color::DarkGray)),
            Span::raw(pkg.repo.clone()),
        ]));
    }
    if !pkg.depends.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("depends  ", Style::default().fg(Color::DarkGray)),
            Span::raw(pkg.depends.clone()),
        ]));
    }
    if !pkg.description.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(pkg.description.clone()));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), chunks[0]);
    if !action.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                action,
                Style::default().fg(Color::DarkGray),
            ))),
            chunks[1],
        );
    }
}

fn draw_log(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default().borders(Borders::ALL).border_type(BorderType::Rounded).title(" log ");
    f.render_widget(block, area);
    let inner = area.inner(Margin { horizontal: 1, vertical: 1 });
    let view_h = inner.height as usize;
    let start = app
        .log
        .len()
        .saturating_sub(app.log_offset_from_bottom + view_h);
    let text: Text = app.log.iter().map(|l| Line::raw(l.as_str())).collect();
    f.render_widget(
        Paragraph::new(text).scroll((start as u16, 0)),
        inner,
    );
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let chunks = Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).split(area);
    let status = if app.status.is_empty() {
        "ready".to_string()
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            status,
            Style::default().fg(Color::Green),
        ))),
        chunks[0],
    );
    let hints = match app.view {
        View::Installed => "j/k move  Enter or u uninstall  r refresh  / filter  Tab view  q quit",
        View::Browse => "j/k move  Enter or i install  / search  Tab view  q quit",
        View::Log => "j/k scroll  f follow  G end  Tab view  q quit",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hints,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );
}

fn draw_confirm(f: &mut Frame, app: &App) {
    let area = centered(50, 20, f.area());
    let msg = match app.confirm {
        Some(Confirm::Install(i)) => app
            .search_results
            .get(i)
            .map(|p| format!("install {} ({})?", p.name, p.kind))
            .unwrap_or_default(),
        Some(Confirm::Uninstall(i)) => app
            .installed
            .get(i)
            .map(|p| format!("uninstall {}?", p.name))
            .unwrap_or_default(),
        None => return,
    };
    f.render_widget(Clear, area);
    let p = Paragraph::new(vec![
        Line::from(Span::styled(
            msg,
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "y = yes    n / Esc = no",
            Style::default().fg(Color::DarkGray),
        )),
    ])
    .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded))
    .alignment(Alignment::Center);
    f.render_widget(p, area);
}

fn centered(w_pct: u16, h_pct: u16, area: Rect) -> Rect {
    let w = (area.width * w_pct / 100).max(30);
    let h = (area.height * h_pct / 100).max(7);
    let x = (area.width - w) / 2;
    let y = (area.height - h) / 2;
    Rect::new(area.x + x, area.y + y, w, h)
}

fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").to_string()
}
