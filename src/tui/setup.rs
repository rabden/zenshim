use std::collections::HashSet;
use std::io::stdout;
use std::sync::mpsc;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};

use crate::config::Config;
use crate::wizard;

use super::mask_key;
use super::ui::{accent, bold, centered, dim, hints, panel, spinner};

enum Check {
    Pending,
    Valid(Vec<String>),
    Invalid(String),
}

enum Step {
    /// Typing upstream keys.
    Keys { input: String, error: Option<String> },
    /// Validating each key against the live catalog.
    Checking { keys: Vec<(String, Check)> },
    /// Choosing which models to serve.
    Models {
        keys: Vec<String>,
        rejected: usize,
        catalog: Vec<String>,
        picked: HashSet<String>,
        cursor: usize,
        /// Show the whole catalog rather than only free-tier models.
        all: bool,
        error: Option<String>,
    },
}

struct Setup {
    runtime: tokio::runtime::Handle,
    http: reqwest::Client,
    port: u16,
    step: Step,
    tx: mpsc::Sender<(usize, Result<Vec<String>, String>)>,
    rx: mpsc::Receiver<(usize, Result<Vec<String>, String>)>,
    /// `Some(config)` once saved; `None` inside means cancelled.
    done: Option<Option<Config>>,
}

/// First-run setup as a full-screen flow: keys → validation → models.
/// Returns the saved config, or `None` if the user backed out.
pub fn run(runtime: tokio::runtime::Handle, http: reqwest::Client, port: u16) -> anyhow::Result<Option<Config>> {
    let (tx, rx) = mpsc::channel();
    let setup = Setup {
        runtime,
        http,
        port,
        step: Step::Keys { input: String::new(), error: None },
        tx,
        rx,
        done: None,
    };
    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableBracketedPaste);
    let result = setup.run(&mut terminal);
    let _ = execute!(stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

fn visible(catalog: &[String], all: bool) -> Vec<&String> {
    catalog.iter().filter(|id| all || id.contains("-free")).collect()
}

impl Setup {
    fn run(mut self, terminal: &mut DefaultTerminal) -> anyhow::Result<Option<Config>> {
        loop {
            while let Ok((index, result)) = self.rx.try_recv() {
                self.on_checked(index, result);
            }
            terminal.draw(|frame| self.draw(frame))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key)?,
                    Event::Paste(text) => {
                        if let Step::Keys { input, .. } = &mut self.step {
                            input.extend(text.chars().filter(|c| !c.is_control()));
                        }
                    }
                    _ => {}
                }
            }
            if let Some(outcome) = self.done.take() {
                return Ok(outcome);
            }
        }
    }

    fn submit_keys(&mut self, input: &str) {
        let mut keys: Vec<String> = Vec::new();
        for key in input.split(',').map(str::trim).filter(|k| !k.is_empty()) {
            if !keys.iter().any(|k| k == key) {
                keys.push(key.to_string());
            }
        }
        if keys.is_empty() {
            keys.push("public".to_string());
        }
        for (index, key) in keys.iter().enumerate() {
            let (http, key, tx) = (self.http.clone(), key.clone(), self.tx.clone());
            self.runtime.spawn(async move {
                let result = wizard::fetch_models(&http, &key).await.map_err(|e| e.to_string());
                let _ = tx.send((index, result));
            });
        }
        self.step = Step::Checking {
            keys: keys.into_iter().map(|k| (k, Check::Pending)).collect(),
        };
    }

    fn on_checked(&mut self, index: usize, result: Result<Vec<String>, String>) {
        let Step::Checking { keys } = &mut self.step else { return };
        if let Some((_, check)) = keys.get_mut(index) {
            *check = match result {
                Ok(ids) => Check::Valid(ids),
                Err(e) => Check::Invalid(e),
            };
        }
        if keys.iter().any(|(_, check)| matches!(check, Check::Pending)) {
            return;
        }
        let rejected = keys.iter().filter(|(_, c)| matches!(c, Check::Invalid(_))).count();
        let first_error = keys.iter().find_map(|(_, c)| match c {
            Check::Invalid(e) => Some(e.clone()),
            _ => None,
        });
        let catalog = keys.iter().find_map(|(_, c)| match c {
            Check::Valid(ids) => Some(ids.clone()),
            _ => None,
        });
        let Some(catalog) = catalog else {
            let input = keys.iter().map(|(k, _)| k.as_str()).filter(|k| *k != "public").collect::<Vec<_>>().join(",");
            self.step = Step::Keys {
                input,
                error: Some(format!("No usable keys — {}", first_error.unwrap_or_default())),
            };
            return;
        };
        let valid: Vec<String> = keys
            .iter()
            .filter(|(_, c)| matches!(c, Check::Valid(_)))
            .map(|(k, _)| k.clone())
            .collect();
        let picked: HashSet<String> = catalog.iter().filter(|id| id.contains("-free")).cloned().collect();
        self.step = Step::Models {
            keys: valid,
            rejected,
            all: picked.is_empty(),
            catalog,
            picked,
            cursor: 0,
            error: None,
        };
    }

    fn on_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.done = Some(None);
            return Ok(());
        }
        match &mut self.step {
            Step::Keys { input, error } => match key.code {
                KeyCode::Esc => self.done = Some(None),
                KeyCode::Enter => {
                    let input = std::mem::take(input);
                    self.submit_keys(&input);
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => {
                    input.push(c);
                    *error = None;
                }
                _ => {}
            },
            Step::Checking { .. } => {
                if key.code == KeyCode::Esc {
                    self.step = Step::Keys { input: String::new(), error: None };
                }
            }
            Step::Models { keys, catalog, picked, cursor, all, error, .. } => {
                let rows = visible(catalog, *all);
                let last = rows.len().saturating_sub(1);
                *cursor = (*cursor).min(last);
                match key.code {
                    KeyCode::Esc => self.step = Step::Keys { input: String::new(), error: None },
                    KeyCode::Up => *cursor = cursor.saturating_sub(1),
                    KeyCode::Down => *cursor = (*cursor + 1).min(last),
                    KeyCode::PageUp => *cursor = cursor.saturating_sub(10),
                    KeyCode::PageDown => *cursor = (*cursor + 10).min(last),
                    KeyCode::Home => *cursor = 0,
                    KeyCode::End => *cursor = last,
                    KeyCode::Char(' ') => {
                        if let Some(id) = rows.get(*cursor) {
                            if !picked.remove(*id) {
                                picked.insert((*id).clone());
                            }
                            *error = None;
                        }
                    }
                    KeyCode::Char('a') => {
                        // Select everything shown, or clear it if it all is already.
                        if rows.iter().all(|id| picked.contains(*id)) {
                            for id in &rows {
                                picked.remove(*id);
                            }
                        } else {
                            picked.extend(rows.iter().map(|id| (*id).clone()));
                        }
                    }
                    KeyCode::Char('f') => {
                        *all = !*all;
                        *cursor = 0;
                    }
                    KeyCode::Enter => {
                        // Catalog order, which is sorted.
                        let models: Vec<String> =
                            catalog.iter().filter(|id| picked.contains(*id)).cloned().collect();
                        if models.is_empty() {
                            *error = Some("Pick at least one model (Space toggles).".to_string());
                        } else {
                            let config = Config {
                                keys: keys.clone(),
                                models,
                                port: self.port,
                                gateway_key: Config::generate_gateway_key(),
                            };
                            config.save()?;
                            self.done = Some(Some(config));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn draw(&self, frame: &mut Frame) {
        let card = centered(frame.area(), 78, 24);
        let block = panel("zenshim setup");
        let inner = block.inner(card);
        frame.render_widget(block, card);
        if inner.height < 8 || inner.width < 40 {
            frame.render_widget(Paragraph::new("Enlarge the terminal to run setup."), inner);
            return;
        }
        let [steps, _, body, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .areas(inner);

        let at = match self.step {
            Step::Keys { .. } => 0,
            Step::Checking { .. } => 1,
            Step::Models { .. } => 2,
        };
        let mut trail = Vec::new();
        for (i, name) in ["1 Keys", "2 Validate", "3 Models"].into_iter().enumerate() {
            if i > 0 {
                trail.push(Span::styled("  ›  ", dim()));
            }
            trail.push(Span::styled(name, if i == at { accent() } else { dim() }));
        }
        frame.render_widget(Line::from(trail), steps);

        match &self.step {
            Step::Keys { input, error } => {
                self.draw_keys(frame, body, input, error.as_deref());
                frame.render_widget(hints(&[("Enter", "continue"), ("Esc", "quit")]), footer);
            }
            Step::Checking { keys } => {
                draw_checking(frame, body, keys);
                frame.render_widget(hints(&[("Esc", "back")]), footer);
            }
            Step::Models { keys, rejected, catalog, picked, cursor, all, error } => {
                draw_models(frame, body, keys.len(), *rejected, catalog, picked, *cursor, *all, error.as_deref());
                frame.render_widget(
                    hints(&[
                        ("Space", "toggle"),
                        ("a", "all"),
                        ("f", if *all { "free only" } else { "whole catalog" }),
                        ("Enter", "save"),
                        ("Esc", "back"),
                    ]),
                    footer,
                );
            }
        }
    }

    fn draw_keys(&self, frame: &mut Frame, area: Rect, input: &str, error: Option<&str>) {
        let [intro, field, note] = Layout::vertical([
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .areas(area);
        let lines = vec![
            Line::from(Span::styled("A local gateway for OpenCode Zen's free tier", bold())),
            Line::from("that any OpenAI- or Anthropic-compatible client can use."),
            Line::default(),
            Line::from("Paste your Zen API keys, comma-separated."),
            Line::from(Span::styled("Leave it blank to use the anonymous free tier.", dim())),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), intro);

        let block = panel("Zen API keys");
        let inner = block.inner(field);
        let room = inner.width.saturating_sub(1) as usize;
        let chars: Vec<char> = input.chars().collect();
        let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
        frame.set_cursor_position(Position::new(inner.x + shown.chars().count() as u16, inner.y));
        frame.render_widget(Paragraph::new(shown).block(block), field);

        if let Some(error) = error {
            let line = Line::from(Span::styled(error.to_string(), Style::new().fg(Color::Red)));
            frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), note);
        } else {
            let line = Line::from(Span::styled(
                format!("Settings are saved to {}", Config::path().display()),
                dim(),
            ));
            frame.render_widget(Paragraph::new(line).wrap(Wrap { trim: true }), note);
        }
    }
}

fn draw_checking(frame: &mut Frame, area: Rect, keys: &[(String, Check)]) {
    let mut lines = vec![Line::from("Checking each key against the live catalog…"), Line::default()];
    for (key, check) in keys {
        let state = match check {
            Check::Pending => Span::styled(format!("{} checking", spinner()), Style::new().fg(Color::Yellow)),
            Check::Valid(_) => Span::styled("✓ valid", Style::new().fg(Color::Green)),
            Check::Invalid(e) => Span::styled(format!("✗ {e}"), Style::new().fg(Color::Red)),
        };
        lines.push(Line::from(vec![Span::raw(format!("{:<32}", mask_key(key, false))), state]));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

#[allow(clippy::too_many_arguments)]
fn draw_models(
    frame: &mut Frame,
    area: Rect,
    keys: usize,
    rejected: usize,
    catalog: &[String],
    picked: &HashSet<String>,
    cursor: usize,
    all: bool,
    error: Option<&str>,
) {
    let [summary, list, note] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let free = catalog.iter().filter(|id| id.contains("-free")).count();
    let mut first = vec![Span::styled(
        format!("✓ {keys} key{} valid", if keys == 1 { "" } else { "s" }),
        Style::new().fg(Color::Green),
    )];
    if rejected > 0 {
        first.push(Span::styled(
            format!(" · {rejected} rejected and dropped"),
            Style::new().fg(Color::Yellow),
        ));
    }
    first.push(Span::styled(
        format!(" · {} models in the catalog, {free} free", catalog.len()),
        dim(),
    ));
    let lines = vec![Line::from(first), Line::from("Choose the models to serve:")];
    frame.render_widget(Paragraph::new(lines), summary);

    let rows = visible(catalog, all);
    let table = Table::new(
        rows.iter().map(|id| {
            let on = picked.contains(*id);
            Row::new(vec![
                Cell::from(if on {
                    Span::styled("[✓]", Style::new().fg(Color::Green))
                } else {
                    Span::styled("[ ]", dim())
                }),
                Cell::from(Span::styled((*id).clone(), if on { Style::new() } else { dim() })),
            ])
        }),
        [Constraint::Length(3), Constraint::Min(10)],
    )
    .row_highlight_style(bold())
    .highlight_symbol(Span::styled("▌ ", accent()));
    let mut state = TableState::default().with_selected(Some(cursor.min(rows.len().saturating_sub(1))));
    frame.render_stateful_widget(table, list, &mut state);

    let line = match error {
        Some(error) => Line::from(Span::styled(error.to_string(), Style::new().fg(Color::Red))),
        None => Line::from(Span::styled(format!("{} selected", picked.len()), dim())),
    };
    frame.render_widget(line, note);
}
