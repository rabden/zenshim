use std::time::Duration;

use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, Padding, Paragraph, Row, Sparkline, Table, TableState, Tabs,
    Wrap,
};
use ratatui::Frame;

use crate::hub::{hub, Level, RequestRecord, ReqState};
use crate::server::format::ServeFormats;
use crate::stream::ApiFormat;
use crate::tunnel::TunnelStatus;

use super::{mask_key, App, Catalog, Mode, Tab, ToastKind};

// Named ANSI colors only, so the app follows the terminal's own theme.
pub(super) const ACCENT: Color = Color::Cyan;
const GOOD: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

const TOAST_TTL: Duration = Duration::from_secs(4);
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub(super) fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub(super) fn accent() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

pub(super) fn spinner() -> &'static str {
    SPINNER[(hub().uptime().as_millis() / 100) as usize % SPINNER.len()]
}

pub(super) fn panel(title: &str) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .title(Span::styled(format!(" {title} "), bold()))
        .padding(Padding::horizontal(1))
}

/// A rect of at most `width`×`height`, centered in `area`.
pub(super) fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// `key description` pairs for the footer and the help overlay.
pub(super) fn hints<'a>(pairs: &[(&'a str, &'a str)]) -> Line<'a> {
    let mut spans = Vec::new();
    for (key, what) in pairs {
        if !spans.is_empty() {
            spans.push(Span::styled("  ·  ", dim()));
        }
        spans.push(Span::styled(*key, accent()));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*what, dim()));
    }
    Line::from(spans)
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if d.as_millis() < 1000 {
        format!("{}ms", d.as_millis())
    } else if secs < 60 {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn fmt_age(d: Duration) -> String {
    match d.as_secs() {
        s @ 0..60 => format!("{s}s"),
        s @ 60..3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

fn fmt_clock(d: Duration) -> String {
    let secs = d.as_secs();
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

fn fmt_count(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

fn status_span(record: &RequestRecord) -> Span<'static> {
    match record.state {
        ReqState::Active => Span::styled(format!("{} live", spinner()), Style::new().fg(WARN)),
        ReqState::Done(499) => Span::styled("gone", dim()),
        ReqState::Done(status) => {
            let color = match status {
                200..300 => GOOD,
                400..500 => WARN,
                _ => BAD,
            };
            Span::styled(status.to_string(), Style::new().fg(color))
        }
    }
}

pub(super) fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    if area.width < 76 || area.height < 16 {
        let message = Paragraph::new("zenshim needs at least 76×16 — enlarge the terminal")
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: true });
        frame.render_widget(message, centered(area, area.width, 2));
        return;
    }
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);

    draw_header(frame, app, header);
    match app.tab {
        Tab::Dashboard => draw_dashboard(frame, app, body),
        Tab::Requests => draw_requests(frame, app, body),
        Tab::Models => draw_models(frame, app, body),
        Tab::Keys => draw_keys(frame, app, body),
        Tab::Logs => draw_logs(frame, app, body),
    }
    draw_footer(frame, app, footer);

    match &app.mode {
        Mode::Help => draw_help(frame, area),
        Mode::AddKey(value) => draw_add_key(frame, area, value),
        Mode::Confirm(confirm) => draw_confirm(frame, area, &confirm.question()),
        Mode::Normal | Mode::FilterModels => {}
    }
}

fn draw_header(frame: &mut Frame, app: &App, area: Rect) {
    let active = hub()
        .requests()
        .iter()
        .filter(|r| r.state == ReqState::Active)
        .count();
    let mut status = vec![
        Span::styled("● ", Style::new().fg(GOOD)),
        Span::raw(format!("{}:{}", app.ctx.state.host, app.ctx.state.port)),
    ];
    if active > 0 {
        status.push(Span::styled(format!("  {} {active} live", spinner()), Style::new().fg(WARN)));
    }
    // The tabs matter more than the clock when space is short.
    if area.width >= 104 {
        status.push(Span::styled(format!("  up {}", fmt_clock(hub().uptime())), dim()));
    } else {
        status.drain(..2);
    }
    status.push(Span::raw(" "));
    let status = Line::from(status);

    let brand = format!(" zenshim {} ", env!("CARGO_PKG_VERSION"));
    let [left, middle, right] = Layout::horizontal([
        Constraint::Length(brand.chars().count() as u16),
        Constraint::Min(0),
        Constraint::Length(status.width() as u16),
    ])
    .areas(area);

    frame.render_widget(
        Span::styled(brand, Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        left,
    );
    let titles = Tab::ALL
        .iter()
        .enumerate()
        .map(|(i, tab)| Line::from(format!("{} {}", i + 1, tab.title())));
    let tabs = Tabs::new(titles)
        .select(app.tab.index())
        .style(dim())
        .highlight_style(accent().add_modifier(Modifier::UNDERLINED))
        .divider(" ");
    frame.render_widget(tabs, middle);
    frame.render_widget(Paragraph::new(status).alignment(Alignment::Right), right);
}

fn draw_footer(frame: &mut Frame, app: &App, area: Rect) {
    if matches!(app.mode, Mode::FilterModels) {
        let line = Line::from(vec![
            Span::styled(" / ", accent()),
            Span::raw(app.model_query.as_str()),
            Span::styled("▏", accent()),
            Span::styled("   Enter keep  ·  Esc clear", dim()),
        ]);
        frame.render_widget(line, area);
        return;
    }
    if let Some(toast) = app.toast.as_ref().filter(|t| t.at.elapsed() < TOAST_TTL) {
        let (mark, color) = match toast.kind {
            ToastKind::Good => ("✓", GOOD),
            ToastKind::Bad => ("!", BAD),
        };
        let line = Line::from(vec![
            Span::styled(format!(" {mark} "), Style::new().fg(color).add_modifier(Modifier::BOLD)),
            Span::raw(toast.text.as_str()),
        ]);
        frame.render_widget(line, area);
        return;
    }
    let pairs: &[(&str, &str)] = match app.tab {
        Tab::Dashboard => &[
            ("k", "copy key"),
            ("u", "copy url"),
            ("t", "copy tunnel"),
            ("T", "tunnel on/off"),
            ("Tab", "next"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Requests => &[
            ("↑↓", "select"),
            ("f", "filter"),
            ("x", "clear"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Models => &[
            ("Space", "serve on/off"),
            ("Enter", "copy id"),
            ("a", "copy all"),
            ("/", "search"),
            ("f", "free/all"),
            ("r", "refresh"),
            ("?", "help"),
        ],
        Tab::Keys => &[
            ("a", "add"),
            ("d", "remove"),
            ("v", "validate"),
            ("R", "rotate gateway key"),
            ("?", "help"),
            ("q", "quit"),
        ],
        Tab::Logs => &[("↑↓", "scroll"), ("G", "follow"), ("?", "help"), ("q", "quit")],
    };
    let mut line = hints(pairs);
    line.spans.insert(0, Span::raw(" "));
    frame.render_widget(line, area);
}

// ── dashboard ────────────────────────────────────────────────────────

fn draw_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let side_by_side = area.width >= 116;
    let [top, recent] = if side_by_side {
        Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).areas(area)
    } else {
        Layout::vertical([Constraint::Length(14), Constraint::Min(0)]).areas(area)
    };
    let [gateway, traffic] = if side_by_side {
        Layout::horizontal([Constraint::Min(0), Constraint::Length(46)]).areas(top)
    } else {
        Layout::vertical([Constraint::Length(7), Constraint::Length(7)]).areas(top)
    };
    draw_gateway(frame, app, gateway);
    draw_traffic(frame, traffic);

    let records: Vec<RequestRecord> = hub().requests().into_iter().rev().collect();
    let block = panel("Recent requests");
    if records.is_empty() {
        let inner = block.inner(recent);
        frame.render_widget(block, recent);
        draw_waiting(frame, app, inner);
    } else {
        frame.render_widget(requests_table(&records, recent.width).block(block), recent);
    }
}

/// Empty state for a gateway that has not been called yet.
fn draw_waiting(frame: &mut Frame, app: &App, area: Rect) {
    let model = app.ctx.state.models().into_iter().next().unwrap_or_default();
    let lines = vec![
        Line::from(Span::styled("No requests yet — point a client at the gateway:", dim())),
        Line::default(),
        Line::from(format!("curl {}/chat/completions \\", app.openai_url())),
        Line::from("  -H \"Authorization: Bearer $ZENSHIM_KEY\" -H \"Content-Type: application/json\" \\"),
        Line::from(format!(
            "  -d '{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": \"hello\"}}]}}'"
        )),
        Line::default(),
        hints(&[("k", "copies the key"), ("u", "copies the base URL")]),
    ];
    let height = (lines.len() as u16).min(area.height);
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(height),
        Constraint::Fill(1),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(lines), middle);
}

fn draw_gateway(frame: &mut Frame, app: &App, area: Rect) {
    let state = &app.ctx.state;
    let label = |text: &'static str| Span::styled(format!("{text:<11}"), dim());
    let endpoint = |served: bool, url: String| {
        if served {
            Span::raw(url)
        } else {
            Span::styled("not served (--format)", dim())
        }
    };
    let tunnel = match app.ctx.tunnel.status() {
        TunnelStatus::Off => vec![
            Span::styled("○ off", dim()),
            Span::styled(" — localhost only, T starts one", dim()),
        ],
        TunnelStatus::Starting(what) => vec![Span::styled(
            format!("{} {what}…", spinner()),
            Style::new().fg(WARN),
        )],
        TunnelStatus::Up(url) => vec![
            Span::styled("● ", Style::new().fg(GOOD)),
            Span::raw(url),
            Span::styled(" public", Style::new().fg(WARN)),
        ],
        TunnelStatus::Failed(why) => vec![Span::styled(format!("✗ {why}"), Style::new().fg(BAD))],
    };
    let mut tunnel_line = vec![label("Tunnel")];
    tunnel_line.extend(tunnel);

    let gateway_key = state.gateway_key();
    let key_hint = if app.reveal_key { "  K hides" } else { "  K reveals" };
    let keys = state.keys().len();
    let lines = vec![
        Line::from(vec![
            label("OpenAI"),
            endpoint(state.formats.serves(ApiFormat::OpenAi), app.openai_url()),
        ]),
        Line::from(vec![
            label("Anthropic"),
            endpoint(state.formats.serves(ApiFormat::Anthropic), app.anthropic_url()),
        ]),
        Line::from(tunnel_line),
        Line::from(vec![
            label("API key"),
            Span::raw(if gateway_key.is_empty() {
                "none — open gateway".to_string()
            } else {
                mask_key(&gateway_key, app.reveal_key)
            }),
            Span::styled(key_hint, dim()),
        ]),
        Line::from(vec![
            label("Serving"),
            Span::raw(format!(
                "{} models · {keys} upstream key{} · {}",
                state.models().len(),
                if keys == 1 { "" } else { "s" },
                match state.formats {
                    ServeFormats::OpenAi => "openai only",
                    ServeFormats::Anthropic => "anthropic only",
                    ServeFormats::Both => "both dialects",
                }
            )),
        ]),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel("Gateway")), area);
}

fn draw_traffic(frame: &mut Frame, area: Rect) {
    let totals = hub().totals();
    let block = panel("Traffic");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let stat = |name: &'static str, value: String, style: Style| {
        vec![
            Span::styled(format!("{name} "), dim()),
            Span::styled(format!("{value:<8}"), style),
        ]
    };
    let active = totals.requests - totals.completed;
    let error_style = if totals.errors > 0 { Style::new().fg(BAD) } else { bold() };
    let average = if totals.ok > 0 {
        fmt_duration(Duration::from_millis(totals.ok_millis / totals.ok))
    } else {
        "—".to_string()
    };
    let mut first = stat("requests", fmt_count(totals.requests), bold());
    first.extend(stat("live", active.to_string(), bold()));
    first.extend(stat("errors", fmt_count(totals.errors), error_style));
    let mut second = stat("tokens in", fmt_count(totals.tokens_in), bold());
    second.extend(stat("out", fmt_count(totals.tokens_out), bold()));
    second.extend(stat("avg time", average, bold()));

    let [stats, caption, spark] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(vec![Line::from(first), Line::from(second)]), stats);

    let timeline = hub().timeline();
    let shown = &timeline[timeline.len().saturating_sub(spark.width as usize)..];
    frame.render_widget(
        Span::styled(format!("requests per second · last {}s", shown.len()), dim()),
        caption,
    );
    frame.render_widget(Sparkline::default().data(shown).style(Style::new().fg(ACCENT)), spark);
}

// ── requests ─────────────────────────────────────────────────────────

fn requests_table(records: &[RequestRecord], width: u16) -> Table<'static> {
    let wide = width >= 96;
    let rows = records.iter().map(|r| {
        let tokens = if r.tokens_in + r.tokens_out > 0 {
            format!("{} → {}", fmt_count(r.tokens_in), fmt_count(r.tokens_out))
        } else {
            String::new()
        };
        let mut cells = vec![
            Cell::from(Span::styled(format!("{:>4}", r.id), dim())),
            Cell::from(status_span(r)),
            Cell::from(r.format.label()),
            Cell::from(r.model.clone()),
        ];
        if wide {
            cells.push(Cell::from(Span::styled(r.client.clone(), dim())));
            cells.push(Cell::from(if r.stream { "stream" } else { "" }));
        }
        cells.push(Cell::from(tokens));
        cells.push(Cell::from(Line::from(fmt_duration(r.elapsed())).alignment(Alignment::Right)));
        cells.push(Cell::from(
            Line::from(Span::styled(fmt_age(r.started.elapsed()), dim())).alignment(Alignment::Right),
        ));
        Row::new(cells)
    });
    let mut header = vec!["#", "status", "dialect", "model"];
    let mut widths = vec![
        Constraint::Length(4),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Fill(1),
    ];
    if wide {
        header.extend(["client", ""]);
        widths.extend([Constraint::Length(15), Constraint::Length(6)]);
    }
    header.extend(["tokens", "time", "age"]);
    widths.extend([Constraint::Length(12), Constraint::Length(7), Constraint::Length(4)]);
    let header = Row::new(header.into_iter().enumerate().map(|(i, title)| {
        let right = matches!(title, "time" | "age") || i == 0;
        let line = Line::from(title);
        Cell::from(if right { line.alignment(Alignment::Right) } else { line })
    }))
    .style(dim());
    Table::new(rows, widths)
        .header(header)
        .column_spacing(2)
        .row_highlight_style(bold())
        .highlight_symbol(Span::styled("▌ ", accent()))
}

fn draw_requests(frame: &mut Frame, app: &App, area: Rect) {
    let records = app.visible_requests();
    let [list, detail] = if area.width >= 110 {
        Layout::horizontal([Constraint::Min(0), Constraint::Length(44)]).areas(area)
    } else {
        Layout::vertical([Constraint::Min(0), Constraint::Length(10)]).areas(area)
    };
    let title = format!("Requests · {} · {}", app.request_filter.label(), records.len());
    let block = panel(&title);
    app.page.set(block.inner(list).height.saturating_sub(1) as usize);

    if records.is_empty() {
        let text = match app.request_filter {
            super::RequestFilter::All => "No requests yet.",
            _ => "Nothing matches this filter — f cycles all / errors / active.",
        };
        frame.render_widget(Paragraph::new(Span::styled(text, dim())).block(block), list);
        frame.render_widget(panel("Detail"), detail);
        return;
    }
    let index = app.request_index(&records);
    let mut state = TableState::default().with_selected(Some(index));
    // Selection needs the marker column, so tell the table how wide it is.
    let table = requests_table(&records, list.width.saturating_sub(2)).block(block);
    frame.render_stateful_widget(table, list, &mut state);

    let record = &records[index];
    let field = |name: &'static str, value: String| {
        Line::from(vec![Span::styled(format!("{name:<10}"), dim()), Span::raw(value)])
    };
    let mut lines = vec![
        Line::from(vec![Span::styled(format!("{:<10}", "status"), dim()), status_span(record)]),
        field("model", record.model.clone()),
        field("dialect", format!(
            "{}{}",
            record.format.label(),
            if record.stream { " · streaming" } else { "" }
        )),
        field("client", record.client.clone()),
        field("input", format!("messages {} · tools offered {}", record.messages, record.tools)),
        field("tokens", format!("{} in · {} out", record.tokens_in, record.tokens_out)),
        field("time", format!(
            "{} · {} ago",
            fmt_duration(record.elapsed()),
            fmt_age(record.started.elapsed())
        )),
        field("key", match record.key {
            Some(i) => format!("upstream key #{}", i + 1),
            None => "—".to_string(),
        }),
    ];
    if record.tool_calls > 0 {
        lines.push(field("tool calls", record.tool_calls.to_string()));
    }
    if !record.note.is_empty() {
        // Right under the status: it is the line that explains it.
        let style = if record.is_error() { Style::new().fg(BAD) } else { Style::new() };
        lines.insert(1, Line::from(vec![
            Span::styled(format!("{:<10}", if record.is_error() { "error" } else { "finish" }), dim()),
            Span::styled(record.note.clone(), style),
        ]));
    }
    let title = format!("Request #{}", record.id);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(panel(&title)),
        detail,
    );
}

// ── models ───────────────────────────────────────────────────────────

fn draw_models(frame: &mut Frame, app: &App, area: Rect) {
    let rows = app.model_rows();
    let stats = hub().model_stats();
    let served = app.ctx.state.models().len();
    let scope = if app.models_all { "whole catalog" } else { "free tier" };
    let mut title = format!("Models · {served} served · showing {scope}");
    if !app.model_query.is_empty() {
        title.push_str(&format!(" matching “{}”", app.model_query));
    }
    let block = panel(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [status, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    let status_line = match &app.catalog {
        Catalog::Loading => Line::from(Span::styled(
            format!("{} fetching the live catalog…", spinner()),
            Style::new().fg(WARN),
        )),
        Catalog::Ready(ids) => Line::from(Span::styled(
            format!("{} models in the live catalog", ids.len()),
            dim(),
        )),
        Catalog::Failed(why) => Line::from(Span::styled(
            format!("catalog unavailable ({why}) — r retries; showing served models"),
            Style::new().fg(BAD),
        )),
    };
    frame.render_widget(status_line, status);
    app.page.set(list.height.saturating_sub(1) as usize);

    if rows.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled("No models match — Esc clears the search.", dim())),
            Rect { y: list.y + 1, height: list.height.saturating_sub(1), ..list },
        );
        return;
    }
    let table_rows = rows.iter().map(|row| {
        let stat = stats.get(&row.id).copied().unwrap_or_default();
        let (mark, style) = if row.served {
            ("[✓]", Style::new().fg(GOOD))
        } else {
            ("[ ]", dim())
        };
        let note = if !row.in_catalog && matches!(app.catalog, Catalog::Ready(_)) {
            Span::styled("not in the catalog", Style::new().fg(WARN))
        } else if row.id.contains("-free") {
            Span::styled("free", dim())
        } else {
            Span::raw("")
        };
        let count = |n: u64| if n > 0 { fmt_count(n) } else { String::new() };
        Row::new(vec![
            Cell::from(Span::styled(mark, style)),
            Cell::from(Span::styled(row.id.clone(), if row.served { Style::new() } else { dim() })),
            Cell::from(Line::from(count(stat.requests)).alignment(Alignment::Right)),
            Cell::from(Line::from(count(stat.tokens_out)).alignment(Alignment::Right)),
            Cell::from(note),
        ])
    });
    let header = Row::new(vec![
        Cell::from(""),
        Cell::from("model id"),
        Cell::from(Line::from("requests").alignment(Alignment::Right)),
        Cell::from(Line::from("tokens out").alignment(Alignment::Right)),
        Cell::from(""),
    ])
    .style(dim());
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(3),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(10),
            Constraint::Length(18),
        ],
    )
    .header(header)
    .column_spacing(2)
    .row_highlight_style(bold())
    .highlight_symbol(Span::styled("▌ ", accent()));
    let cursor = app.model_cursor.min(rows.len() - 1);
    let mut state = TableState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(table, list, &mut state);
}

// ── keys ─────────────────────────────────────────────────────────────

fn draw_keys(frame: &mut Frame, app: &App, area: Rect) {
    let keys = app.ctx.state.keys();
    let pending = app.checking.iter().filter(|k| !keys.iter().any(|e| &e.value == *k)).count();
    let [list, gateway] = Layout::vertical([Constraint::Min(0), Constraint::Length(4)]).areas(area);

    let block = panel("Upstream keys");
    app.page.set(block.inner(list).height.saturating_sub(1) as usize);
    let count = |n: u64| if n > 0 { fmt_count(n) } else { "·".to_string() };
    let mut rows: Vec<Row> = keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            let state = if app.checking.contains(&key.value) {
                Span::styled(format!("{} validating…", spinner()), Style::new().fg(WARN))
            } else if let Some(error) = &key.last_error {
                Span::styled(error.clone(), Style::new().fg(BAD))
            } else if key.served > 0 {
                Span::styled("ok", Style::new().fg(GOOD))
            } else {
                Span::styled("not used yet", dim())
            };
            Row::new(vec![
                Cell::from(Span::styled(format!("{:>2}", i + 1), dim())),
                Cell::from(mask_key(&key.value, false)),
                Cell::from(Line::from(count(key.served)).alignment(Alignment::Right)),
                Cell::from(Line::from(count(key.limited)).alignment(Alignment::Right)),
                Cell::from(Line::from(count(key.failed)).alignment(Alignment::Right)),
                Cell::from(state),
            ])
        })
        .collect();
    if pending > 0 {
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(Span::styled(
                format!("{} validating {pending} new key(s)…", spinner()),
                Style::new().fg(WARN),
            )),
        ]));
    }
    let header = Row::new(vec![
        Cell::from(Line::from("#").alignment(Alignment::Right)),
        Cell::from("key"),
        Cell::from(Line::from("served").alignment(Alignment::Right)),
        Cell::from(Line::from("limited").alignment(Alignment::Right)),
        Cell::from(Line::from("failed").alignment(Alignment::Right)),
        Cell::from("state"),
    ])
    .style(dim());
    let table = Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Length(30),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(12),
        ],
    )
    .header(header)
    .column_spacing(2)
    .row_highlight_style(bold())
    .highlight_symbol(Span::styled("▌ ", accent()))
    .block(block);
    let cursor = app.key_cursor.min(keys.len().saturating_sub(1));
    let mut state = TableState::default().with_selected(Some(cursor));
    frame.render_stateful_widget(table, list, &mut state);

    let gateway_key = app.ctx.state.gateway_key();
    let lines = vec![
        Line::from(vec![
            Span::raw(mask_key(&gateway_key, app.reveal_key)),
            Span::styled("   what clients authenticate with", dim()),
        ]),
        hints(&[("k", "copy"), ("K", "reveal/hide"), ("R", "rotate")]),
    ];
    frame.render_widget(Paragraph::new(lines).block(panel("Gateway key")), gateway);
}

// ── logs ─────────────────────────────────────────────────────────────

fn draw_logs(frame: &mut Frame, app: &App, area: Rect) {
    let logs = hub().logs();
    let following = app.log_scroll == 0;
    let title = if following {
        format!("Logs · {} lines · following", logs.len())
    } else {
        format!("Logs · {} lines · scrolled, G follows", logs.len())
    };
    let block = panel(&title);
    let height = block.inner(area).height as usize;
    app.page.set(height);

    let end = logs.len().saturating_sub(app.log_scroll.min(logs.len().saturating_sub(height)));
    let start = end.saturating_sub(height);
    let lines: Vec<Line> = logs[start..end]
        .iter()
        .map(|entry| {
            let style = match entry.level {
                Level::Info => Style::new(),
                Level::Warn => Style::new().fg(WARN),
                Level::Error => Style::new().fg(BAD),
            };
            Line::from(vec![
                Span::styled(format!("{}  ", fmt_clock(entry.at)), dim()),
                Span::styled(entry.text.clone(), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

// ── overlays ─────────────────────────────────────────────────────────

fn overlay<'a>(frame: &mut Frame, area: Rect, title: &'a str, width: u16, height: u16) -> (Rect, Block<'a>) {
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(format!(" {title} "), accent()))
        .padding(Padding::symmetric(2, 1));
    (rect, block)
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let section = |title: &'static str| Line::from(Span::styled(title, bold()));
    let entry = |key: &'static str, what: &'static str| {
        Line::from(vec![Span::styled(format!("  {key:<14}"), accent()), Span::raw(what)])
    };
    let lines = vec![
        section("Everywhere"),
        entry("Tab  ←→  1-5", "switch tabs"),
        entry("k / K", "copy the gateway API key / reveal or hide it"),
        entry("u / U", "copy the OpenAI base URL / the Anthropic base URL"),
        entry("t / T", "copy the tunnel URL / start or stop the tunnel"),
        entry("R", "rotate the gateway key"),
        entry("q  Ctrl-C", "shut down"),
        Line::default(),
        section("Lists"),
        entry("↑↓ PgUp PgDn", "move"),
        entry("g / G", "first / last"),
        Line::default(),
        section("Requests"),
        entry("f", "filter: all → errors → active"),
        entry("x", "clear finished requests"),
        Line::default(),
        section("Models"),
        entry("Space", "serve or stop serving the model (saved)"),
        entry("Enter / a", "copy the model id / all served ids"),
        entry("/  f  r", "search / free tier or whole catalog / refresh"),
        Line::default(),
        section("Keys"),
        entry("a / d / v", "add / remove / validate an upstream key (saved)"),
    ];
    let (rect, block) = overlay(frame, area, "Help", 72, lines.len() as u16 + 4);
    frame.render_widget(Paragraph::new(lines).block(block), rect);
}

fn draw_add_key(frame: &mut Frame, area: Rect, value: &str) {
    let (rect, block) = overlay(frame, area, "Add upstream key", 76, 8);
    let inner = block.inner(rect);
    // Keep the tail visible when the key is longer than the box.
    let room = inner.width.saturating_sub(1) as usize;
    let chars: Vec<char> = value.chars().collect();
    let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
    let cursor = shown.chars().count() as u16;
    let lines = vec![
        Line::from(shown),
        Line::default(),
        Line::from(Span::styled(
            "Paste a Zen API key (several: comma-separated). Enter validates · Esc cancels",
            dim(),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block), rect);
    frame.set_cursor_position(Position::new(inner.x + cursor, inner.y));
}

fn draw_confirm(frame: &mut Frame, area: Rect, question: &str) {
    let (rect, block) = overlay(frame, area, "Confirm", 64, 8);
    let lines = vec![
        Line::from(question.to_string()),
        Line::default(),
        hints(&[("y", "yes"), ("any other key", "cancel")]),
    ];
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }).block(block), rect);
}
