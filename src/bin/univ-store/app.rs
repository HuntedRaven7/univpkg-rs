use std::collections::VecDeque;
use std::sync::mpsc::TryRecvError;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::cmd;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Installed,
    Browse,
    Log,
}

impl View {
    pub fn index(self) -> usize {
        match self {
            View::Installed => 0,
            View::Browse => 1,
            View::Log => 2,
        }
    }

    pub fn from_index(i: usize) -> View {
        match i % 3 {
            1 => View::Browse,
            2 => View::Log,
            _ => View::Installed,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            View::Installed => "Installed",
            View::Browse => "Browse",
            View::Log => "Log",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    FilterBox,
    SearchBox,
}

#[derive(Clone, Copy)]
pub enum Confirm {
    Install(usize),
    Uninstall(usize),
}

pub struct App {
    pub view: View,
    pub focus: Focus,
    pub installed: Vec<cmd::Package>,
    pub installed_error: Option<String>,
    pub installed_filter: String,
    pub installed_cursor: usize,
    pub search_query: String,
    pub search_results: Vec<cmd::Package>,
    pub search_error: Option<String>,
    pub search_cursor: usize,
    pub log: VecDeque<String>,
    pub log_offset_from_bottom: usize,
    pub log_follow: bool,
    pub task: Option<cmd::Task>,
    pub confirm: Option<Confirm>,
    pub status: String,
    pub frame: u64,
    pub quit: bool,
}

impl App {
    pub fn new() -> App {
        let mut app = App {
            view: View::Installed,
            focus: Focus::List,
            installed: Vec::new(),
            installed_error: None,
            installed_filter: String::new(),
            installed_cursor: 0,
            search_query: String::new(),
            search_results: Vec::new(),
            search_error: None,
            search_cursor: 0,
            log: VecDeque::new(),
            log_offset_from_bottom: 0,
            log_follow: true,
            task: None,
            confirm: None,
            status: String::new(),
            frame: 0,
            quit: false,
        };
        app.log.push_back("univ store started".to_string());
        app.refresh_installed();
        app
    }

    pub fn busy(&self) -> bool {
        self.task.is_some()
    }

    pub fn filtered(&self) -> Vec<usize> {
        let q = self.installed_filter.trim().to_lowercase();
        self.installed
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                q.is_empty()
                    || p.name.to_lowercase().contains(&q)
                    || p.description.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }

        if let Some(c) = self.confirm {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    self.confirm = None;
                    self.confirm_action(c);
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.confirm = None,
                _ => {}
            }
            return;
        }

        match self.focus {
            Focus::FilterBox => {
                self.box_key(key, Focus::FilterBox);
                return;
            }
            Focus::SearchBox => {
                self.box_key(key, Focus::SearchBox);
                return;
            }
            Focus::List => {}
        }

        match key.code {
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            KeyCode::Tab => {
                self.focus = Focus::List;
                self.view = View::from_index(self.view.index() + 1);
            }
            KeyCode::Char('1') => {
                self.focus = Focus::List;
                self.view = View::Installed;
            }
            KeyCode::Char('2') => {
                self.focus = Focus::List;
                self.view = View::Browse;
            }
            KeyCode::Char('3') => {
                self.focus = Focus::List;
                self.view = View::Log;
            }
            KeyCode::Esc if self.view == View::Log => self.view = View::Installed,
            _ => match self.view {
                View::Installed => self.installed_key(key),
                View::Browse => self.browse_key(key),
                View::Log => self.log_key(key),
            },
        }
    }

    fn box_key(&mut self, key: KeyEvent, focus: Focus) {
        match key.code {
            KeyCode::Char(c) => {
                if focus == Focus::FilterBox {
                    self.installed_filter.push(c);
                    self.installed_cursor = 0;
                } else {
                    self.search_query.push(c);
                }
            }
            KeyCode::Backspace => {
                if focus == Focus::FilterBox {
                    self.installed_filter.pop();
                    self.installed_cursor = 0;
                } else {
                    self.search_query.pop();
                }
            }
            KeyCode::Enter => {
                self.focus = Focus::List;
                match focus {
                    Focus::FilterBox => self.installed_cursor = 0,
                    _ => self.start_search(),
                }
            }
            KeyCode::Esc | KeyCode::Tab => self.focus = Focus::List,
            _ => {}
        }
    }

    fn installed_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('/') => self.focus = Focus::FilterBox,
            KeyCode::Down | KeyCode::Char('j') => self.move_installed(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_installed(-1),
            KeyCode::Home | KeyCode::Char('g') => self.installed_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.installed_cursor = self.filtered().len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('u') if !self.busy() => {
                if let Some(&i) = self.filtered().get(self.installed_cursor) {
                    self.confirm = Some(Confirm::Uninstall(i));
                }
            }
            KeyCode::Char('r') if !self.busy() => self.refresh_installed(),
            _ => {}
        }
    }

    fn browse_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('/') | KeyCode::Char('n') => self.focus = Focus::SearchBox,
            KeyCode::Down | KeyCode::Char('j') => {
                self.search_cursor = (self.search_cursor + 1)
                    .min(self.search_results.len().saturating_sub(1));
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.search_cursor = self.search_cursor.saturating_sub(1);
            }
            KeyCode::Home | KeyCode::Char('g') => self.search_cursor = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.search_cursor = self.search_results.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char('i')
                if !self.busy() && !self.search_results.is_empty() =>
            {
                self.confirm = Some(Confirm::Install(self.search_cursor));
            }
            _ => {}
        }
    }

    fn log_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                self.log_offset_from_bottom = self.log_offset_from_bottom.saturating_add(1);
                self.log_follow = false;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.log_offset_from_bottom = self.log_offset_from_bottom.saturating_sub(1);
                if self.log_offset_from_bottom == 0 {
                    self.log_follow = true;
                }
            }
            KeyCode::Char('f') | KeyCode::End | KeyCode::Char('G') => {
                self.log_follow = true;
                self.log_offset_from_bottom = 0;
            }
            _ => {}
        }
    }

    fn move_installed(&mut self, delta: i32) {
        let len = self.filtered().len();
        if len == 0 {
            self.installed_cursor = 0;
            return;
        }
        if delta > 0 {
            self.installed_cursor = (self.installed_cursor + delta as usize).min(len - 1);
        } else {
            self.installed_cursor =
                self.installed_cursor.saturating_sub(delta.unsigned_abs() as usize);
        }
    }

    fn confirm_action(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::Install(i) => {
                let Some(pkg) = self.search_results.get(i).cloned() else {
                    return;
                };
                let args = if pkg.kind == "rpm" {
                    vec!["install-rpm".to_string(), pkg.name.clone()]
                } else {
                    vec!["install".to_string(), pkg.name.clone()]
                };
                self.start_task(format!("installing {} ({})", pkg.name, pkg.kind), args);
            }
            Confirm::Uninstall(i) => {
                let Some(pkg) = self.installed.get(i).cloned() else {
                    return;
                };
                self.start_task(
                    format!("uninstalling {}", pkg.name),
                    vec!["uninstall".to_string(), pkg.name],
                );
            }
        }
    }

    fn start_task(&mut self, title: String, args: Vec<String>) {
        if self.busy() {
            return;
        }
        self.log.push_back(format!("> {title}"));
        self.status = format!("{title}...");
        self.task = Some(cmd::Task::spawn(title, args));
    }

    pub fn start_search(&mut self) {
        let query = self.search_query.trim();
        if query.is_empty() {
            return;
        }
        self.status = format!("searching for '{query}'...");
        match cmd::search(query) {
            Ok(results) => {
                self.search_results = results;
                self.search_error = None;
                self.search_cursor = 0;
                self.status = format!(
                    "{} result(s) for '{query}'",
                    self.search_results.len()
                );
            }
            Err(e) => {
                self.search_results.clear();
                self.search_error = Some(e.to_string());
                self.status = format!("search failed: {e}");
            }
        }
    }

    pub fn refresh_installed(&mut self) {
        match cmd::list() {
            Ok(pkgs) => {
                self.installed = pkgs;
                self.installed_error = None;
                self.installed_cursor = 0;
                self.status = format!("{} package(s) installed", self.installed.len());
            }
            Err(e) => {
                self.installed.clear();
                self.installed_error = Some(e.to_string());
                self.status = format!("list failed: {e}");
            }
        }
    }

    pub fn tick(&mut self) {
        self.frame += 1;

        let finished = if let Some(task) = &self.task {
            let mut done = false;
            loop {
                match task.rx.try_recv() {
                    Ok(line) => self.log.push_back(line),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        done = true;
                        break;
                    }
                }
            }
            done
        } else {
            false
        };

        if finished {
            let title = self
                .task
                .as_ref()
                .map(|t| t.title.clone())
                .unwrap_or_default();
            self.task = None;
            self.status = format!("finished {title}");
            self.log.push_back(format!("[finished {title}]"));
            self.refresh_installed();
        }

        while self.log.len() > 2000 {
            self.log.pop_front();
        }
        if self.log_follow {
            self.log_offset_from_bottom = 0;
        }
    }
}
