//! Drawing. Reads `App`, never changes chat state.

use chrono::{Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, Mode, QUICK_REACTIONS};
use crate::model::{Body, LinkStatus, Message};

const ACCENT: Color = Color::Green;

pub fn draw(frame: &mut Frame, app: &mut App) {
    if app.status != LinkStatus::Connected && app.chats.is_empty() {
        return link(frame, app);
    }
    let [body, status] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
    let [list, right] =
        Layout::horizontal([Constraint::Length(32), Constraint::Min(1)]).areas(body);
    let [messages, input] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(3)]).areas(right);
    chats(frame, app, list);
    conversation(frame, app, messages);
    composer(frame, app, input);
    status_line(frame, app, status);
    if app.mode == Mode::Search {
        search(frame, app);
    }
}

fn link(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let (text, qr) = match &app.status {
        LinkStatus::Qr(code) => (
            "Scan with WhatsApp › Settings › Linked devices › Link a device".to_owned(),
            Some(code.as_str()),
        ),
        LinkStatus::Starting => ("Starting…".into(), None),
        LinkStatus::Connecting | LinkStatus::Connected => ("Connecting…".into(), None),
        LinkStatus::Disconnected(why) => (format!("Disconnected: {why}. Reconnecting…"), None),
        LinkStatus::LoggedOut => ("Logged out. Restart to link again.".into(), None),
        LinkStatus::Failed(why) => (format!("Failed: {why}"), None),
    };
    let mut lines = qr.map(qr_lines).unwrap_or_default();
    lines.push(Line::default());
    lines.push(Line::from(text).bold());
    lines.push(Line::from("q to quit").dim());
    let height = (lines.len() as u16).min(area.height);
    let top = area.y + (area.height - height) / 2;
    let paragraph = Paragraph::new(lines).centered();
    frame.render_widget(paragraph, Rect::new(area.x, top, area.width, height));
}

/// The QR code in half blocks, two modules per cell, dark on light.
fn qr_lines(data: &str) -> Vec<Line<'static>> {
    let Ok(code) = qrcode::QrCode::new(data) else {
        return vec![Line::from("Could not draw the QR code")];
    };
    let size = code.width() as i32;
    let colors = code.to_colors();
    let dark = |x: i32, y: i32| {
        (0..size).contains(&x)
            && (0..size).contains(&y)
            && colors[(y * size + x) as usize] == qrcode::Color::Dark
    };
    let style = Style::new().fg(Color::Black).bg(Color::White);
    (-2..size + 2)
        .step_by(2)
        .map(|y| {
            let row: String = (-2..size + 2)
                .map(|x| match (dark(x, y), dark(x, y + 1)) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect();
            Line::from(Span::styled(row, style))
        })
        .collect()
}

fn chats(frame: &mut Frame, app: &mut App, area: Rect) {
    // Two rows per chat inside the border.
    app.chat_page = (area.height.saturating_sub(2) / 2) as usize;
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .chats
        .iter()
        .map(|chat| {
            let time = if chat.pinned {
                format!("📌 {}", clock(chat.last))
            } else {
                clock(chat.last)
            };
            let name = truncate(&chat.name, width.saturating_sub(time.width() + 1));
            let pad = width.saturating_sub(name.width() + time.width());
            ListItem::new(vec![
                Line::from(vec![
                    Span::raw(name).bold(),
                    Span::raw(" ".repeat(pad)),
                    Span::raw(time).dim(),
                ]),
                Line::from(truncate(&chat.preview.replace('\n', " "), width)).dim(),
            ])
        })
        .collect();
    let focused = app.mode == Mode::Chats;
    let list = List::new(items)
        .block(pane("Chats", focused))
        .highlight_style(if focused {
            Style::new().bg(Color::DarkGray)
        } else {
            Style::new().add_modifier(Modifier::BOLD)
        })
        .highlight_symbol("▍");
    let mut state = ListState::default().with_selected(Some(app.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

fn conversation(frame: &mut Frame, app: &mut App, area: Rect) {
    let title = app
        .open
        .as_ref()
        .and_then(|id| app.chats.iter().find(|chat| &chat.id == id))
        .map(|chat| chat.name.clone())
        .unwrap_or_default();
    let block = pane(&title, app.mode == Mode::Messages);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.messages.is_empty() {
        let hint = if app.open.is_some() {
            "No messages yet"
        } else {
            "Pick a chat"
        };
        frame.render_widget(Paragraph::new(hint).dim().centered(), inner);
        return;
    }
    let width = inner.width.saturating_sub(2) as usize;
    let anchor = app.cursor.unwrap_or(app.messages.len() - 1);
    // Laid out from the anchor upwards until the pane is full.
    let mut blocks = Vec::new();
    let mut used = 0usize;
    for index in (0..=anchor).rev() {
        let lines = message_lines(&app.messages[index], width, app.cursor == Some(index));
        used += lines.len();
        blocks.push(lines);
        if used >= inner.height as usize {
            break;
        }
    }
    let lines: Vec<Line> = blocks.into_iter().rev().flatten().collect();
    let skip = used.saturating_sub(inner.height as usize);
    let lines: Vec<Line> = lines.into_iter().skip(skip).collect();
    // Anchored to the bottom, like any chat.
    let height = lines.len() as u16;
    let area = Rect {
        y: inner.bottom() - height,
        height,
        ..inner
    };
    frame.render_widget(Paragraph::new(lines), area);
}

/// A message as lines: header, quote, body, reactions, gap.
fn message_lines(message: &Message, width: usize, selected: bool) -> Vec<Line<'static>> {
    let mut body: Vec<Line<'static>> = Vec::new();
    let name = if message.from_me {
        "You".to_owned()
    } else {
        message.sender_name.clone()
    };
    body.push(Line::from(vec![
        Span::styled(
            name,
            Style::new()
                .fg(name_color(&message.sender, message.from_me))
                .bold(),
        ),
        Span::raw("  "),
        Span::raw(clock(message.ts)).dim(),
    ]));
    if let Some(quote) = &message.quote {
        let text = format!("{}: {}", quote.sender, quote.text.replace('\n', " "));
        body.push(Line::from(format!("┃ {}", truncate(&text, width.saturating_sub(2)))).dim());
    }
    let text = match &message.body {
        Body::Text(text) => Some(text.clone()),
        Body::Image { caption } => {
            body.push(Line::from("📷 Photo").italic().dim());
            caption.clone()
        }
        Body::Other(label) => {
            body.push(Line::from(label.clone()).italic().dim());
            None
        }
    };
    for line in text.iter().flat_map(|text| wrap(text, width)) {
        body.push(Line::from(line));
    }
    if !message.reactions.is_empty() {
        // Each emoji once with its count; ours highlighted.
        let mut counts: Vec<(String, usize, bool)> = Vec::new();
        for reaction in &message.reactions {
            match counts
                .iter_mut()
                .find(|(emoji, ..)| *emoji == reaction.emoji)
            {
                Some((_, count, mine)) => {
                    *count += 1;
                    *mine |= reaction.mine;
                }
                None => counts.push((reaction.emoji.clone(), 1, reaction.mine)),
            }
        }
        let mut spans = Vec::new();
        for (emoji, count, mine) in counts {
            let text = if count > 1 {
                format!("{emoji} {count}")
            } else {
                emoji
            };
            let style = if mine {
                Style::new().bg(Color::DarkGray)
            } else {
                Style::new()
            };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw("  "));
        }
        body.push(Line::from(spans));
    }
    let marker = if selected {
        Span::styled("▍ ", Style::new().fg(ACCENT))
    } else {
        Span::raw("  ")
    };
    let mut lines: Vec<Line<'static>> = body
        .into_iter()
        .map(|mut line| {
            line.spans.insert(0, marker.clone());
            line
        })
        .collect();
    lines.push(Line::default());
    lines
}

fn composer(frame: &mut Frame, app: &App, area: Rect) {
    if app.mode == Mode::Delete {
        let mine = app.selected_message().is_some_and(|m| m.from_me);
        let key = |k: &'static str| Span::styled(k, Style::new().fg(ACCENT).bold());
        let mut spans = Vec::new();
        if mine {
            spans.extend([key("e"), Span::raw(" for everyone    ")]);
        }
        spans.extend([
            key("m"),
            Span::raw(" for me    "),
            key("esc"),
            Span::raw(" cancel"),
        ]);
        let block = pane("Delete this message?", true);
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
        return;
    }
    let title = match app.mode {
        Mode::React => {
            let quick: Vec<String> = QUICK_REACTIONS
                .iter()
                .enumerate()
                .map(|(i, e)| format!("{}{e}", i + 1))
                .collect();
            format!("React {}  or type one + Enter, ⌫ removes", quick.join(" "))
        }
        _ => match app
            .reply_to
            .as_ref()
            .and_then(|id| app.messages.iter().find(|m| &m.id == id))
        {
            Some(message) => {
                let name = if message.from_me {
                    "You"
                } else {
                    &message.sender_name
                };
                let text = format!(
                    "Reply to {name}: {}",
                    message.body.summary().replace('\n', " ")
                );
                truncate(&text, area.width.saturating_sub(6) as usize)
            }
            None => match &app.attachment {
                Some(path) => {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let text = format!("📷 {name} · caption optional, enter sends, esc removes");
                    truncate(&text, area.width.saturating_sub(6) as usize)
                }
                None => "Message".into(),
            },
        },
    };
    let typing = matches!(app.mode, Mode::Compose | Mode::React);
    let block = pane(&title, typing);
    let inner = block.inner(area);
    // Only the tail of a long draft fits on one row.
    let text = app.input.replace('\n', "⏎");
    let visible = tail(&text, inner.width.saturating_sub(1) as usize);
    frame.render_widget(Paragraph::new(visible.clone()).block(block), area);
    if typing {
        frame.set_cursor_position(Position::new(inner.x + visible.width() as u16, inner.y));
    }
}

fn status_line(frame: &mut Frame, app: &App, area: Rect) {
    let link = match &app.status {
        LinkStatus::Connected => Span::styled("● online", Style::new().fg(ACCENT)),
        LinkStatus::Disconnected(_) | LinkStatus::Connecting | LinkStatus::Starting => {
            Span::styled("● connecting", Style::new().fg(Color::Yellow))
        }
        _ => Span::styled("● offline", Style::new().fg(Color::Red)),
    };
    let hints = match app.mode {
        Mode::Chats => "j/k move  ^d/^u page  g/G top/end  enter open  p pin  / search  q quit",
        Mode::Messages => {
            "j/k select  enter open  s save  r reply  e react  d delete  i write  esc chats"
        }
        Mode::Compose => "enter send  alt+enter newline  esc messages",
        Mode::React => "1-6 quick  enter send  esc cancel",
        Mode::Delete => "e everyone  m me  esc cancel",
        Mode::Search => "type to search  ↑/↓ pick  enter open  esc close",
    };
    let mut spans = vec![link, Span::raw("  "), Span::raw(hints).dim()];
    if let Some(notice) = &app.notice {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(notice.clone(), Style::new().fg(ACCENT)));
    }
    if let Some(error) = &app.error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(error.clone(), Style::new().fg(Color::Red)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn search(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let width = area.width.saturating_sub(8).min(80);
    let height = area.height.saturating_sub(4).min(24);
    let popup = Rect::new(area.x + (area.width - width) / 2, area.y + 2, width, height);
    frame.render_widget(Clear, popup);
    let block = pane("Search", true);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let [query, results] =
        Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    frame.render_widget(Paragraph::new(format!("› {}", app.search)), query);
    frame.set_cursor_position(Position::new(
        query.x + 2 + app.search.width() as u16,
        query.y,
    ));
    let text_width = results.width.saturating_sub(2) as usize;
    let mut items: Vec<ListItem> = app
        .matching_chats()
        .into_iter()
        .map(|chat| {
            ListItem::new(Line::from(vec![
                Span::raw("💬 "),
                Span::raw(chat.name.clone()).bold(),
            ]))
        })
        .collect();
    items.extend(app.results.iter().map(|message| {
        let chat = app
            .chats
            .iter()
            .find(|chat| chat.id == message.chat)
            .map_or(message.chat.as_str(), |chat| chat.name.as_str());
        let who = if message.from_me {
            "You"
        } else {
            &message.sender_name
        };
        ListItem::new(vec![
            Line::from(vec![
                Span::raw(chat.to_owned()).bold(),
                Span::raw(format!("  {}", clock(message.ts))).dim(),
            ]),
            Line::from(truncate(
                &format!("{who}: {}", message.body.summary().replace('\n', " ")),
                text_width,
            ))
            .dim(),
        ])
    }));
    let list = List::new(items)
        .highlight_style(Style::new().bg(Color::DarkGray))
        .highlight_symbol("▍");
    let mut state = ListState::default().with_selected(Some(app.result));
    frame.render_stateful_widget(list, results, &mut state);
}

fn pane(title: &str, focused: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::new().fg(ACCENT)
        } else {
            Style::new().dim()
        })
        .title(format!(" {title} "))
}

fn name_color(sender: &str, me: bool) -> Color {
    if me {
        return ACCENT;
    }
    const COLORS: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Yellow,
        Color::Blue,
        Color::LightRed,
        Color::LightGreen,
    ];
    let hash = sender.bytes().fold(0usize, |hash, byte| {
        hash.wrapping_mul(31).wrapping_add(byte as usize)
    });
    COLORS[hash % COLORS.len()]
}

/// "14:05" today, "12 Nov" before.
fn clock(ts: i64) -> String {
    let Some(time) = Local.timestamp_opt(ts, 0).single() else {
        return String::new();
    };
    if time.date_naive() == Local::now().date_naive() {
        time.format("%H:%M").to_string()
    } else {
        time.format("%-d %b").to_string()
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

fn tail(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut out: Vec<char> = Vec::new();
    for c in text.chars().rev() {
        used += c.width().unwrap_or(0);
        if used > width {
            break;
        }
        out.push(c);
    }
    out.into_iter().rev().collect()
}

/// Wraps at spaces, breaking words longer than a line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let w = word.width();
            if used > 0 && used + 1 + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        lines.push(line);
    }
    lines
}
