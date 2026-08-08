//! The dashboard: bare `wukong` lands here, inbox first. Four tabs
//! (Inbox, Files, Activity, Packages), number keys or h/l to move,
//! j/k to walk a list, and on the inbox a/r/i to approve, redact, or
//! ignore the selected item. It polls the daemon on a short cadence so
//! a change committed while you watch shows up on its own.

use crate::client;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::{execute, terminal};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use std::io::stdout;
use std::time::{Duration, Instant};
use wukong_core::events::Resolution;
use wukong_core::ipc::{PkgEntry, Request, Response, StatusInfo, TrackedFile};

const GOLD: Color = Color::Rgb(0xE0, 0xA5, 0x2A);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Inbox,
    Files,
    Activity,
    Packages,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Inbox, Tab::Files, Tab::Activity, Tab::Packages];
    fn title(self) -> &'static str {
        match self {
            Tab::Inbox => "Inbox",
            Tab::Files => "Files",
            Tab::Activity => "Activity",
            Tab::Packages => "Packages",
        }
    }
    fn index(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }
}

#[derive(Default)]
struct Data {
    status: Option<StatusInfo>,
    inbox: Vec<wukong_core::events::InboxItem>,
    files: Vec<TrackedFile>,
    events: Vec<wukong_core::events::Event>,
    packages: Vec<PkgEntry>,
}

struct App {
    tab: Tab,
    selected: usize,
    data: Data,
    flash: Option<String>,
    connected: bool,
}

pub fn run() -> anyhow::Result<()> {
    if !client::connected() {
        println!("wukongd is not running.");
        println!("Start it with `wukong daemon start`, or set up with `wukong init`.");
        return Ok(());
    }

    // A panic mid-draw must not leave the user's terminal in raw mode.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(stdout(), terminal::LeaveAlternateScreen);
        default_hook(info);
    }));

    terminal::enable_raw_mode()?;
    execute!(stdout(), terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout());
    let mut term = Terminal::new(backend)?;

    let mut app = App {
        tab: Tab::Inbox,
        selected: 0,
        data: Data::default(),
        flash: None,
        connected: true,
    };
    app.refresh();

    let mut last_poll = Instant::now();
    let result = loop {
        if let Err(e) = term.draw(|f| app.render(f)) {
            break Err(e.into());
        }
        if event::poll(Duration::from_millis(250)).unwrap_or(false)
            && let Ok(Event::Key(key)) = event::read()
            && key.kind == KeyEventKind::Press
            && app.on_key(key.code)
        {
            break Ok(());
        }
        if last_poll.elapsed() >= Duration::from_secs(2) {
            app.refresh();
            last_poll = Instant::now();
        }
    };

    terminal::disable_raw_mode()?;
    execute!(stdout(), terminal::LeaveAlternateScreen)?;
    result
}

impl App {
    fn refresh(&mut self) {
        self.connected = client::connected();
        if !self.connected {
            return;
        }
        if let Ok(Response::Status(s)) = client::call(Request::Status) {
            self.data.status = Some(s);
        }
        if let Ok(Response::Inbox { items }) = client::call(Request::InboxList) {
            self.data.inbox = items;
        }
        if let Ok(Response::Tracked { files }) = client::call(Request::TrackedList) {
            self.data.files = files;
        }
        if let Ok(Response::Events { events }) = client::call(Request::Events { limit: 200 }) {
            self.data.events = events;
        }
        if let Ok(Response::Packages { entries }) = client::call(Request::PkgList) {
            self.data.packages = entries;
        }
        let len = self.list_len();
        if self.selected >= len {
            self.selected = len.saturating_sub(1);
        }
    }

    fn list_len(&self) -> usize {
        match self.tab {
            Tab::Inbox => self.data.inbox.len(),
            Tab::Files => self.data.files.len(),
            Tab::Activity => self.data.events.len(),
            Tab::Packages => self.data.packages.len(),
        }
    }

    /// Returns true to quit.
    fn on_key(&mut self, code: KeyCode) -> bool {
        self.flash = None;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('1') => self.set_tab(Tab::Inbox),
            KeyCode::Char('2') => self.set_tab(Tab::Files),
            KeyCode::Char('3') => self.set_tab(Tab::Activity),
            KeyCode::Char('4') => self.set_tab(Tab::Packages),
            KeyCode::Char('l') | KeyCode::Tab | KeyCode::Right => self.cycle_tab(1),
            KeyCode::Char('h') | KeyCode::Left => self.cycle_tab(-1),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('a') if self.tab == Tab::Inbox => self.resolve(Resolution::Approve),
            KeyCode::Char('r') if self.tab == Tab::Inbox => self.resolve(Resolution::Redact),
            KeyCode::Char('i') if self.tab == Tab::Inbox => self.resolve(Resolution::Ignore),
            KeyCode::Char('R') => self.refresh(),
            _ => {}
        }
        false
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.selected = 0;
    }

    fn cycle_tab(&mut self, delta: i32) {
        let i = (self.tab.index() as i32 + delta).rem_euclid(Tab::ALL.len() as i32);
        self.set_tab(Tab::ALL[i as usize]);
    }

    fn move_selection(&mut self, delta: i32) {
        let len = self.list_len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as i32 + delta).rem_euclid(len as i32) as usize;
    }

    fn resolve(&mut self, resolution: Resolution) {
        let Some(item) = self.data.inbox.get(self.selected) else {
            return;
        };
        let id = item.id;
        let subject = item.subject.clone();
        if let Ok(Response::Ok { .. }) = client::call(Request::InboxResolve { id, resolution }) {
            self.flash = Some(format!("{} — {}", subject, resolution.as_str()));
            self.refresh();
        }
    }

    fn render(&self, f: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1), // tab bar
                Constraint::Min(0),    // body
                Constraint::Length(1), // status line
            ])
            .split(f.area());

        self.render_tabs(f, chunks[0]);
        match self.tab {
            Tab::Inbox => self.render_inbox(f, chunks[1]),
            Tab::Files => self.render_files(f, chunks[1]),
            Tab::Activity => self.render_activity(f, chunks[1]),
            Tab::Packages => self.render_packages(f, chunks[1]),
        }
        self.render_status(f, chunks[2]);
    }

    fn render_tabs(&self, f: &mut Frame, area: Rect) {
        let titles: Vec<Line> = Tab::ALL
            .iter()
            .map(|t| {
                let count = match t {
                    Tab::Inbox => self.data.inbox.len(),
                    Tab::Files => self.data.files.len(),
                    Tab::Packages => self.data.packages.len(),
                    _ => 0,
                };
                let label = if count > 0 {
                    format!(" {} {} ", t.title(), count)
                } else {
                    format!(" {} ", t.title())
                };
                Line::from(label)
            })
            .collect();
        let tabs = Tabs::new(titles)
            .select(self.tab.index())
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(GOLD)
                    .add_modifier(Modifier::BOLD),
            )
            .divider("");
        f.render_widget(
            Line::from(vec![Span::styled(
                " wukong ",
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
            )]),
            Rect { width: 8, ..area },
        );
        f.render_widget(
            tabs,
            Rect {
                x: area.x + 8,
                width: area.width.saturating_sub(8),
                ..area
            },
        );
    }

    fn render_inbox(&self, f: &mut Frame, area: Rect) {
        if self.data.inbox.is_empty() {
            f.render_widget(
                Paragraph::new("\n  Inbox is clear. Nothing waiting for you.")
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        let split = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);

        let items: Vec<ListItem> = self
            .data
            .inbox
            .iter()
            .map(|item| {
                let tag = match item.kind.as_str() {
                    wukong_core::events::InboxKind::QUARANTINE => {
                        Span::styled("secret ", Style::default().fg(Color::Red))
                    }
                    wukong_core::events::InboxKind::PACKAGE => {
                        Span::styled("adopt? ", Style::default().fg(GOLD))
                    }
                    wukong_core::events::InboxKind::PACKAGE_GONE => {
                        Span::styled("gone   ", Style::default().fg(Color::Red))
                    }
                    _ => Span::styled("track? ", Style::default().fg(GOLD)),
                };
                ListItem::new(Line::from(vec![tag, Span::raw(item.subject.clone())]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(
            List::new(items)
                .block(Block::default().borders(Borders::RIGHT).title(" Review "))
                .highlight_style(
                    Style::default()
                        .bg(Color::Rgb(40, 36, 28))
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▌"),
            split[0],
            &mut state,
        );

        let detail = self.data.inbox.get(self.selected);
        let mut lines: Vec<Line> = Vec::new();
        if let Some(item) = detail {
            lines.push(Line::from(Span::styled(
                item.subject.clone(),
                Style::default().fg(GOLD).add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                item.detail.clone(),
                Style::default().fg(Color::Gray),
            )));
            lines.push(Line::from(""));
            for raw in item.body.lines() {
                lines.push(diff_line(raw));
            }
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().padding(ratatui::widgets::Padding::new(1, 1, 0, 0))),
            split[1],
        );
    }

    fn render_files(&self, f: &mut Frame, area: Rect) {
        if self.data.files.is_empty() {
            f.render_widget(
                Paragraph::new("\n  Nothing tracked yet.\n  From a shell:  wukong track ~/.zshrc")
                    .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        let items: Vec<ListItem> = self
            .data
            .files
            .iter()
            .map(|f| {
                let style = if f.exists {
                    Style::default()
                } else {
                    Style::default().fg(Color::Red)
                };
                ListItem::new(Span::styled(
                    format!(" {} {}", if f.exists { " " } else { "!" }, f.display),
                    style,
                ))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(Color::Rgb(40, 36, 28)))
                .highlight_symbol("▌"),
            area,
            &mut state,
        );
    }

    fn render_activity(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .data
            .events
            .iter()
            .map(|e| {
                ListItem::new(Line::from(vec![
                    Span::styled(short_ts(&e.ts), Style::default().fg(Color::DarkGray)),
                    Span::raw("  "),
                    Span::styled(format!("{:16}", e.kind), Style::default().fg(GOLD)),
                    Span::raw(e.subject.clone()),
                    if e.detail.is_empty() {
                        Span::raw("")
                    } else {
                        Span::styled(
                            format!("  {}", e.detail),
                            Style::default().fg(Color::DarkGray),
                        )
                    },
                ]))
            })
            .collect();
        f.render_widget(
            List::new(items)
                .block(Block::default().padding(ratatui::widgets::Padding::new(1, 0, 0, 0))),
            area,
        );
    }

    fn render_packages(&self, f: &mut Frame, area: Rect) {
        if self.data.packages.is_empty() {
            f.render_widget(
                Paragraph::new(
                    "\n  The manifest is empty.\n  From a shell:  wukong install <pkg>\n  Or take in what's here:  wukong pkg adopt-installed",
                )
                .style(Style::default().fg(Color::DarkGray)),
                area,
            );
            return;
        }
        let items: Vec<ListItem> = self
            .data
            .packages
            .iter()
            .map(|p| {
                let (mark, style) = if p.installed {
                    (" ", Style::default())
                } else {
                    ("!", Style::default().fg(Color::Red))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {mark} {:24}", p.name), style),
                    Span::styled(p.provider.as_str(), Style::default().fg(Color::DarkGray)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.selected));
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(Style::default().bg(Color::Rgb(40, 36, 28)))
                .highlight_symbol("▌"),
            area,
            &mut state,
        );
    }

    fn render_status(&self, f: &mut Frame, area: Rect) {
        let s = self.data.status.as_ref();
        let left = match s {
            Some(s) => format!(
                " {} · {} tracked · {} inbox · {} unpushed",
                s.machine, s.tracked, s.inbox, s.unpushed
            ),
            None => " connecting…".to_string(),
        };
        let hint = match self.tab {
            Tab::Inbox => match self.data.inbox.get(self.selected).map(|i| i.kind.as_str()) {
                Some(wukong_core::events::InboxKind::PACKAGE) => {
                    "a adopt · i never ask again · q quit"
                }
                Some(wukong_core::events::InboxKind::PACKAGE_GONE) => {
                    "a drop from manifest · i keep · q quit"
                }
                _ => "a approve · r redact · i ignore · 1-4 tabs · q quit",
            },
            _ => "j/k move · h/l tabs · R refresh · q quit",
        };
        let right = self.flash.clone().unwrap_or_else(|| hint.to_string());
        let line = Line::from(vec![
            Span::styled(left, Style::default().fg(Color::Gray)),
            Span::raw("  "),
            Span::styled(
                right,
                Style::default().fg(if self.flash.is_some() {
                    GOLD
                } else {
                    Color::DarkGray
                }),
            ),
        ]);
        f.render_widget(
            Paragraph::new(line).style(Style::default().bg(Color::Rgb(24, 22, 18))),
            area,
        );
    }
}

fn diff_line(raw: &str) -> Line<'static> {
    let owned = raw.to_string();
    let style = if raw.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if raw.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if raw.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if raw.starts_with("  line ") || raw.starts_with("Held by") {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Gray)
    };
    Line::from(Span::styled(owned, style))
}

fn short_ts(ts: &str) -> String {
    ts.get(11..19).unwrap_or(ts).to_string()
}
