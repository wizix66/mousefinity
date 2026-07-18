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
        network: Default::default(),
        mesh_secret: None,
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
    /// Every screen known to this config: self, peers, plus anything a synced
    /// layout mentions. Layouts arrive translated into local names, so a peer
    /// listed here under our name for it cannot also appear under theirs.
    fn screens(&self) -> Vec<String> {
        let mut set = BTreeSet::new();
        set.insert(self.cfg.name.clone());
        set.extend(self.peer_names());
        for (name, n) in &self.cfg.layout {
            set.insert(self.canonical(name));
            for e in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
                if let Some(t) = n.get(e) {
                    set.insert(self.canonical(t));
                }
            }
        }
        let mut v: Vec<String> = set.into_iter().collect();
        // Self first, rest alphabetical.
        v.sort_by_key(|s| (s != &self.cfg.name, s.clone()));
        v
    }

    /// One row per machine. The id is the identity, so a machine added twice
    /// under different names is one peer as far as the daemon is concerned;
    /// it is listed once, under the first of its names alphabetically.
    fn peer_names(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        self.cfg
            .peers
            .iter()
            .filter(|(_, p)| seen.insert(p.id.clone()))
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Collapse an alias onto the single name the peer list shows for that
    /// machine, so a layout edge written against one alias does not surface as
    /// a second screen. Unknown names (raw ids, this host) pass through.
    fn canonical(&self, name: &str) -> String {
        let Some(id) = self.cfg.peers.get(name).map(|p| &p.id) else {
            return name.to_string();
        };
        self.cfg
            .peers
            .iter()
            .find(|(_, q)| q.id == *id)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| name.to_string())
    }

    /// Every name this config uses for the same machine as `name`.
    fn aliases_of(&self, name: &str) -> Vec<String> {
        let Some(id) = self.cfg.peers.get(name).map(|p| p.id.clone()) else {
            return vec![name.to_string()];
        };
        self.cfg
            .peers
            .iter()
            .filter(|(_, p)| p.id == id)
            .map(|(n, _)| n.clone())
            .collect()
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
        // The list collapses a machine's aliases into one row, so removing
        // that row has to retire all of them — otherwise a hidden duplicate
        // would keep the peer trusted and reappear on the next redraw.
        for alias in self.aliases_of(name) {
            self.cfg.peers.remove(&alias);
            self.cfg.layout.remove(&alias);
            for n in self.cfg.layout.values_mut() {
                for e in [Edge::Left, Edge::Right, Edge::Up, Edge::Down] {
                    if n.get(e) == Some(alias.as_str()) {
                        *n.get_mut(e) = None;
                    }
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
                        app.cfg.peers.insert(
                            name.clone(),
                            config::Peer {
                                id,
                                addrs: vec![],
                            },
                        );
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

/// A screen the layout mentions but this host has no name for stays a raw
/// endpoint id; abbreviate it so it does not swamp the row.
fn screen_label(s: &str) -> String {
    if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
        format!("{}…", &s[..12])
    } else {
        s.to_string()
    }
}

fn draw_layout(f: &mut Frame, app: &mut App, area: Rect) {
    let screens = app.screens();
    let sel = app.screens_state.selected().unwrap_or(0).min(screens.len().saturating_sub(1));
    let items: Vec<Line> = screens
        .iter()
        .map(|s| {
            let n = app.cfg.layout.get(s).cloned().unwrap_or_default();
            let fmt = |o: Option<&str>| o.map(screen_label).unwrap_or_else(|| "·".into());
            let me = if *s == app.cfg.name { " (this host)" } else { "" };
            Line::from(format!(
                "{}{me}   ←{} →{} ↑{} ↓{}",
                screen_label(s),
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
        let fmt = |o: Option<&str>| o.map(screen_label).unwrap_or_else(|| "(none)".into());
        format!(
            "{}: left={} right={} up={} down={}\npress an arrow key to change that edge",
            screen_label(s),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Peer;

    const ID_X: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const ID_Y: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// This host is `desktop`; machine X is trusted under two names, which is
    /// what happens when it was added by hand on one side and imported by mesh
    /// gossip on the other.
    fn app_with_aliases() -> App {
        let mut peers = BTreeMap::new();
        peers.insert("laptop".to_string(), Peer { id: ID_X.into(), addrs: vec![] });
        peers.insert("macbook".to_string(), Peer { id: ID_X.into(), addrs: vec![] });
        peers.insert("tablet".to_string(), Peer { id: ID_Y.into(), addrs: vec![] });
        let mut layout = BTreeMap::new();
        // The layout edge was written against the alias the list hides.
        layout.insert(
            "desktop".to_string(),
            Neighbors { right: Some("macbook".into()), ..Default::default() },
        );
        layout.insert(
            "macbook".to_string(),
            Neighbors { left: Some("desktop".into()), ..Default::default() },
        );
        App {
            baseline_layout: layout.clone(),
            cfg: Config {
                name: "desktop".into(),
                screen: None,
                downloads: None,
                network: Default::default(),
                mesh_secret: None,
                peers,
                layout,
                layout_rev: 0,
            },
            my_id: "0".repeat(64),
            dirty: false,
            pane: Pane::Peers,
            peers_state: ListState::default().with_selected(Some(0)),
            screens_state: ListState::default().with_selected(Some(0)),
            pick_state: ListState::default().with_selected(Some(0)),
            mode: Mode::Normal,
            input_name: String::new(),
            input_id: String::new(),
            status: String::new(),
            quit_armed: false,
        }
    }

    #[test]
    fn one_row_per_machine_not_per_name() {
        let app = app_with_aliases();
        assert_eq!(app.peer_names(), vec!["laptop", "tablet"]);
    }

    #[test]
    fn layout_edge_via_an_alias_does_not_add_a_screen() {
        let app = app_with_aliases();
        // Without canonicalisation `macbook` would show up beside `laptop`.
        assert_eq!(app.screens(), vec!["desktop", "laptop", "tablet"]);
    }

    #[test]
    fn removing_a_machine_removes_all_its_names() {
        let mut app = app_with_aliases();
        app.remove_peer("laptop");
        assert!(
            !app.cfg.peers.contains_key("macbook"),
            "the hidden alias must go too, or the peer stays trusted and reappears"
        );
        assert_eq!(app.peer_names(), vec!["tablet"]);
        // The edge that pointed at the removed machine is cleared, not left
        // dangling at a name nothing resolves.
        assert!(!app.screens().contains(&"macbook".to_string()));
    }

    #[test]
    fn a_raw_id_screen_is_abbreviated_but_a_name_is_left_alone() {
        assert_eq!(screen_label(ID_X), "111111111111…");
        assert_eq!(screen_label("laptop"), "laptop");
    }
}
