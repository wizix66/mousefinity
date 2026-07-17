//! Interactive configuration UI: pairing, peers, and the screen layout.
//!
//! Saving writes config.toml, bumps the layout revision when the arrangement
//! changed, and pokes a running daemon over IPC so the change syncs to every
//! connected peer immediately.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use anyhow::Result;
use mousefinity_proto::{Edge, Neighbors};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, List, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::config::{self, Config};
use crate::ipc;

#[derive(PartialEq)]
enum Pane {
    Peers,
    Layout,
}

enum Mode {
    Normal,
    AddName,
    AddId,
    ConfirmDelete(String),
    Pick { screen: String, edge: Edge },
}

struct App {
    cfg: Config,
    /// Layout as of the last load/save, to detect when a rev bump is due.
    baseline_layout: BTreeMap<String, Neighbors>,
    my_id: String,
    dirty: bool,
    pane: Pane,
    peers_state: ListState,
    screens_state: ListState,
    pick_state: ListState,
    mode: Mode,
    input_name: String,
    input_id: String,
    status: String,
    quit_armed: bool,
}

pub fn run() -> Result<()> {
    let secret = config::load_or_create_secret()?;
    let my_id = iroh::SecretKey::from_bytes(&secret).public().to_string();
    let cfg = config::load().unwrap_or_else(|_| Config {
        name: crate::host_name(),
        screen: None,
        downloads: None,
        peers: Default::default(),
        layout: Default::default(),
        layout_rev: 0,
    });
    let mut app = App {
        baseline_layout: cfg.layout.clone(),
        cfg,
        my_id,
        dirty: false,
        pane: Pane::Peers,
        peers_state: ListState::default().with_selected(Some(0)),
        screens_state: ListState::default().with_selected(Some(0)),
        pick_state: ListState::default().with_selected(Some(0)),
        mode: Mode::Normal,
        input_name: String::new(),
        input_id: String::new(),
        status: "welcome — press ? keys shown below".into(),
        quit_armed: false,
    };
    ratatui::run(|terminal| -> Result<()> {
        loop {
            terminal.draw(|f| draw(f, &mut app))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            if handle_key(&mut app, key.code, key.modifiers)? {
                return Ok(());
            }
        }
    })
}

// ---- state helpers ----

impl App {
    /// Every screen name known to this config: self, peers, plus any name a
    /// synced layout mentions.
    fn screens(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        set.insert(self.cfg.name.clone());
        set.extend(self.cfg.peers.keys().cloned());
        for (name, n) in &self.cfg.layout {
            set.insert(name.clone());
            for e in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
                if let Some(t) = n.get(e) {
                    set.insert(t.to_string());
                }
            }
        }
        let mut v: Vec<String> = set.into_iter().collect();
        // Self first, rest alphabetical.
        v.sort_by_key(|s| (s != &self.cfg.name, s.clone()));
        v
    }

    fn peer_names(&self) -> Vec<String> {
        self.cfg.peers.keys().cloned().collect()
    }

    fn selected_peer(&self) -> Option<String> {
        self.peer_names()
            .get(self.peers_state.selected().unwrap_or(0))
            .cloned()
    }

    fn selected_screen(&self) -> Option<String> {
        self.screens()
            .get(self.screens_state.selected().unwrap_or(0))
            .cloned()
    }

    fn pick_options(&self, screen: &str) -> Vec<String> {
        let mut v = vec!["(none)".to_string()];
        v.extend(self.screens().into_iter().filter(|s| s != screen));
        v
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.quit_armed = false;
    }

    /// Assign `target` past `edge` of `screen`, maintaining reciprocity the
    /// same way `mousefinity link` does.
    fn set_edge(&mut self, screen: &str, edge: Edge, target: Option<String>) {
        // Un-link the previous neighbour's back-reference if it points here.
        let old = self
            .cfg
            .layout
            .get(screen)
            .and_then(|n| n.get(edge))
            .map(str::to_string);
        if let Some(old) = old {
            if let Some(on) = self.cfg.layout.get_mut(&old) {
                if on.get(edge.opposite()) == Some(screen) {
                    *on.get_mut(edge.opposite()) = None;
                }
                if on.is_empty() {
                    self.cfg.layout.remove(&old);
                }
            }
        }
        *self
            .cfg
            .layout
            .entry(screen.to_string())
            .or_default()
            .get_mut(edge) = target.clone();
        if let Some(t) = &target {
            *self.cfg.layout.entry(t.clone()).or_default().get_mut(edge.opposite()) =
                Some(screen.to_string());
        }
        if self.cfg.layout.get(screen).is_some_and(Neighbors::is_empty) {
            self.cfg.layout.remove(screen);
        }
        self.touch();
    }

    fn remove_peer(&mut self, name: &str) {
        self.cfg.peers.remove(name);
        self.cfg.layout.remove(name);
        for n in self.cfg.layout.values_mut() {
            for e in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
                if n.get(e) == Some(name) {
                    *n.get_mut(e) = None;
                }
            }
        }
        self.cfg.layout.retain(|_, n| !n.is_empty());
        self.touch();
    }

    fn save(&mut self) {
        if self.cfg.layout != self.baseline_layout {
            self.cfg.layout_rev = config::now_ms();
            self.baseline_layout = self.cfg.layout.clone();
        }
        match config::save(&self.cfg) {
            Ok(()) => {
                self.dirty = false;
                self.status = match ipc::client_reload() {
                    Ok(_) => "saved — daemon reloaded, layout syncing to peers".into(),
                    Err(_) => "saved — no running daemon, will sync on next start".into(),
                };
            }
            Err(e) => self.status = format!("save failed: {e:#}"),
        }
    }
}

// ---- input ----

/// Returns Ok(true) to quit.
fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) -> Result<bool> {
    match &app.mode {
        Mode::Normal => return handle_normal(app, code),
        Mode::AddName => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Enter => {
                let name = app.input_name.trim().to_string();
                if name.is_empty() {
                    app.status = "peer name cannot be empty".into();
                } else if name == app.cfg.name {
                    app.status = "that is this host's own name".into();
                } else {
                    app.mode = Mode::AddId;
                }
            }
            KeyCode::Backspace => {
                app.input_name.pop();
            }
            KeyCode::Char(c) if !c.is_whitespace() => app.input_name.push(c),
            _ => {}
        },
        Mode::AddId => match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Char('v') if mods.contains(KeyModifiers::CONTROL) => {
                if let Some(text) = crate::clipboard::Clip::new().get_text() {
                    app.input_id.push_str(text.trim());
                }
            }
            KeyCode::Enter => {
                let id = app.input_id.trim().to_string();
                match id.parse::<iroh::EndpointId>() {
                    Ok(_) => {
                        let name = app.input_name.trim().to_string();
                        app.cfg.peers.insert(name.clone(), config::Peer { id });
                        app.status = format!("added peer `{name}` — press s to save");
                        app.input_name.clear();
                        app.input_id.clear();
                        app.mode = Mode::Normal;
                        app.touch();
                    }
                    Err(_) => app.status = "that does not parse as a pairing id".into(),
                }
            }
            KeyCode::Backspace => {
                app.input_id.pop();
            }
            KeyCode::Char(c) if c.is_ascii_alphanumeric() => app.input_id.push(c),
            _ => {}
        },
        Mode::ConfirmDelete(name) => {
            let name = name.clone();
            match code {
                KeyCode::Char('d') | KeyCode::Char('y') | KeyCode::Enter => {
                    app.remove_peer(&name);
                    app.status = format!("removed `{name}` — press s to save");
                    app.mode = Mode::Normal;
                }
                _ => {
                    app.mode = Mode::Normal;
                    app.status = "delete cancelled".into();
                }
            }
        }
        Mode::Pick { screen, edge } => {
            let (screen, edge) = (screen.clone(), *edge);
            let options = app.pick_options(&screen);
            match code {
            KeyCode::Esc => app.mode = Mode::Normal,
            KeyCode::Up | KeyCode::Char('k') => {
                app.pick_state.select_previous();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let sel = app.pick_state.selected().unwrap_or(0);
                if sel + 1 < options.len() {
                    app.pick_state.select(Some(sel + 1));
                }
            }
            KeyCode::Enter => {
                let sel = app.pick_state.selected().unwrap_or(0);
                let target = if sel == 0 {
                    None
                } else {
                    options.get(sel).cloned()
                };
                let what = target.clone().unwrap_or_else(|| "(none)".into());
                app.set_edge(&screen, edge, target);
                app.status = format!("{} of `{screen}` -> {what} — press s to save", edge.name());
                app.mode = Mode::Normal;
            }
            _ => {}
            }
        }
    }
    Ok(false)
}

fn handle_normal(app: &mut App, code: KeyCode) -> Result<bool> {
    match code {
        KeyCode::Char('q') => {
            if !app.dirty || app.quit_armed {
                return Ok(true);
            }
            app.quit_armed = true;
            app.status = "unsaved changes — s to save, q again to discard".into();
        }
        KeyCode::Char('s') => app.save(),
        KeyCode::Tab => {
            app.pane = if app.pane == Pane::Peers {
                Pane::Layout
            } else {
                Pane::Peers
            };
        }
        KeyCode::Char('c') => {
            crate::clipboard::Clip::new().set_text(&app.my_id);
            app.status = "pairing id copied to clipboard".into();
        }
        KeyCode::Char('a') if app.pane == Pane::Peers => {
            app.input_name.clear();
            app.input_id.clear();
            app.mode = Mode::AddName;
        }
        KeyCode::Char('d') if app.pane == Pane::Peers => {
            if let Some(name) = app.selected_peer() {
                app.mode = Mode::ConfirmDelete(name);
            }
        }
        KeyCode::Char('k') | KeyCode::Up if app.pane == Pane::Peers => {
            app.peers_state.select_previous();
        }
        KeyCode::Char('j') | KeyCode::Down if app.pane == Pane::Peers => {
            let n = app.peer_names().len();
            let sel = app.peers_state.selected().unwrap_or(0);
            if sel + 1 < n {
                app.peers_state.select(Some(sel + 1));
            }
        }
        KeyCode::Char('k') if app.pane == Pane::Layout => {
            app.screens_state.select_previous();
        }
        KeyCode::Char('j') if app.pane == Pane::Layout => {
            let n = app.screens().len();
            let sel = app.screens_state.selected().unwrap_or(0);
            if sel + 1 < n {
                app.screens_state.select(Some(sel + 1));
            }
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
            if app.pane == Pane::Layout =>
        {
            if let Some(screen) = app.selected_screen() {
                let edge = match code {
                    KeyCode::Left => Edge::Left,
                    KeyCode::Right => Edge::Right,
                    KeyCode::Up => Edge::Up,
                    _ => Edge::Down,
                };
                app.pick_state.select(Some(0));
                app.mode = Mode::Pick { screen, edge };
            }
        }
        _ => {}
    }
    Ok(false)
}

// ---- drawing ----

fn draw(f: &mut Frame, app: &mut App) {
    let [header, main, footer] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(8), Constraint::Length(1)])
            .areas(f.area());

    let title = Paragraph::new(vec![
        Line::from(format!("mousefinity — {}", app.cfg.name).bold()),
        Line::from(format!("pairing id: {}", app.my_id).dim()),
        Line::from(app.status.clone().italic()),
    ])
    .block(Block::bordered());
    f.render_widget(title, header);

    let [left, right] =
        Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(main);
    draw_peers(f, app, left);
    draw_layout(f, app, right);

    let hints = match &app.mode {
        Mode::Normal => match app.pane {
            Pane::Peers => "Tab panes | j/k select | a add | d remove | c copy my id | s save | q quit",
            Pane::Layout => "Tab panes | j/k select screen | arrow key = set that edge | s save | q quit",
        },
        Mode::AddName => "type peer name | Enter next | Esc cancel",
        Mode::AddId => "type or Ctrl-V paste pairing id | Enter add | Esc cancel",
        Mode::ConfirmDelete(_) => "d/y/Enter confirm removal | any other key cancels",
        Mode::Pick { .. } => "j/k choose | Enter assign | Esc cancel",
    };
    f.render_widget(Line::from(hints).dim(), footer);

    match &app.mode {
        Mode::AddName | Mode::AddId => draw_add_modal(f, app),
        Mode::ConfirmDelete(name) => {
            let name = name.clone();
            draw_confirm_modal(f, &name);
        }
        Mode::Pick { screen, edge } => {
            let (s, e) = (screen.clone(), *edge);
            draw_pick_modal(f, app, &s, e);
        }
        Mode::Normal => {}
    }
}

fn draw_peers(f: &mut Frame, app: &mut App, area: Rect) {
    let names = app.peer_names();
    let items: Vec<Line> = if names.is_empty() {
        vec![Line::from("no peers yet — press a to add one".dim())]
    } else {
        names
            .iter()
            .map(|n| {
                let id = &app.cfg.peers[n].id;
                Line::from(format!("{n}  {}…", &id[..12.min(id.len())]))
            })
            .collect()
    };
    let focused = app.pane == Pane::Peers;
    let block = Block::bordered().title(if focused { " Peers* " } else { " Peers " });
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::new().reversed());
    f.render_stateful_widget(list, area, &mut app.peers_state);
}

fn draw_layout(f: &mut Frame, app: &mut App, area: Rect) {
    let screens = app.screens();
    let sel = app.screens_state.selected().unwrap_or(0).min(screens.len().saturating_sub(1));
    let items: Vec<Line> = screens
        .iter()
        .map(|s| {
            let n = app.cfg.layout.get(s).cloned().unwrap_or_default();
            let fmt = |o: Option<&str>| o.unwrap_or("·").to_string();
            let me = if *s == app.cfg.name { " (this host)" } else { "" };
            Line::from(format!(
                "{s}{me}   ←{} →{} ↑{} ↓{}",
                fmt(n.left.as_deref()),
                fmt(n.right.as_deref()),
                fmt(n.up.as_deref()),
                fmt(n.down.as_deref()),
            ))
        })
        .collect();
    let focused = app.pane == Pane::Layout;
    let title = if focused { " Layout* " } else { " Layout " };
    let [list_area, detail_area] =
        Layout::vertical([Constraint::Min(4), Constraint::Length(4)]).areas(area);
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::new().reversed());
    f.render_stateful_widget(list, list_area, &mut app.screens_state);

    let detail = if let Some(s) = screens.get(sel) {
        let n = app.cfg.layout.get(s).cloned().unwrap_or_default();
        let fmt = |o: Option<&str>| o.unwrap_or("(none)").to_string();
        format!(
            "{s}: left={} right={} up={} down={}\npress an arrow key to change that edge",
            fmt(n.left.as_deref()),
            fmt(n.right.as_deref()),
            fmt(n.up.as_deref()),
            fmt(n.down.as_deref()),
        )
    } else {
        "no screens".into()
    };
    f.render_widget(
        Paragraph::new(detail).wrap(Wrap { trim: true }).block(Block::bordered()),
        detail_area,
    );
}

fn modal_area(f: &Frame, w: u16, h: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(w)])
        .flex(Flex::Center)
        .areas(f.area());
    let [area] = Layout::vertical([Constraint::Length(h)])
        .flex(Flex::Center)
        .areas(area);
    area
}

fn draw_add_modal(f: &mut Frame, app: &App) {
    let area = modal_area(f, 74, 6);
    f.render_widget(Clear, area);
    let name_line = if matches!(app.mode, Mode::AddName) {
        Line::from(format!("name: {}_", app.input_name))
    } else {
        Line::from(format!("name: {}", app.input_name).dim())
    };
    let id_line = if matches!(app.mode, Mode::AddId) {
        Line::from(format!("id:   {}_", app.input_id))
    } else {
        Line::from("id:   (press Enter on the name first)".dim())
    };
    let p = Paragraph::new(vec![name_line, id_line])
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" add peer "));
    f.render_widget(p, area);
}

fn draw_confirm_modal(f: &mut Frame, name: &str) {
    let area = modal_area(f, 50, 4);
    f.render_widget(Clear, area);
    let p = Paragraph::new(format!(
        "remove peer `{name}` and its layout links?\nd/y/Enter = yes, anything else = no"
    ))
    .block(Block::bordered().title(" confirm "));
    f.render_widget(p, area);
}

fn draw_pick_modal(f: &mut Frame, app: &mut App, screen: &str, edge: Edge) {
    let options = app.pick_options(screen);
    let area = modal_area(f, 40, (options.len() as u16 + 2).min(14));
    f.render_widget(Clear, area);
    let list = List::new(options.iter().map(|o| Line::from(o.clone())).collect::<Vec<_>>())
        .block(Block::bordered().title(format!(" {} of {screen} ", edge.name())))
        .highlight_style(Style::new().reversed());
    f.render_stateful_widget(list, area, &mut app.pick_state);
}
