//! Interface state. Consumes backend events, produces commands; no drawing.

use std::path::PathBuf;

use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};

use crate::backend::{Command, Event};
use crate::model::{Body, Chat, ChatId, LinkStatus, Message};

const MIN_SIDEBAR: u16 = 20;
const MAX_SIDEBAR: u16 = 100;

/// Messages a page key moves through.
const MESSAGE_PAGE: usize = 10;

pub const QUICK_REACTIONS: [&str; 6] = ["👍", "❤️", "😂", "😮", "😢", "🙏"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Moving through the chat list.
    Chats,
    /// Moving through the open chat's messages.
    Messages,
    /// Typing a message.
    Compose,
    /// Picking a reaction for the selected message.
    React,
    /// Confirming deletion of the selected message.
    Delete,
    /// Confirming logging out.
    Logout,
    Search,
}

pub struct App {
    pub status: LinkStatus,
    pub mode: Mode,
    pub chats: Vec<Chat>,
    pub selected: usize,
    pub open: Option<ChatId>,
    pub messages: Vec<Message>,
    /// Selected message in `messages`; `None` follows the newest.
    pub cursor: Option<usize>,
    pub input: String,
    pub reply_to: Option<String>,
    /// Image file the next send carries, with the input as its caption.
    pub attachment: Option<PathBuf>,
    pub search: String,
    pub results: Vec<Message>,
    pub result: usize,
    /// Message a search result opens the chat at.
    jump_to: Option<String>,
    /// Chats the list showed last frame, for paging.
    pub chat_page: usize,
    /// Chat list width in columns.
    pub sidebar: u16,
    /// Whether the mouse is dragging the sidebar's edge.
    pub resizing: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub outbox: Vec<Command>,
    pub quit: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            status: LinkStatus::Starting,
            mode: Mode::Chats,
            chats: Vec::new(),
            selected: 0,
            open: None,
            messages: Vec::new(),
            cursor: None,
            input: String::new(),
            reply_to: None,
            attachment: None,
            search: String::new(),
            results: Vec::new(),
            result: 0,
            jump_to: None,
            chat_page: 10,
            sidebar: 32,
            resizing: false,
            error: None,
            notice: None,
            outbox: Vec::new(),
            quit: false,
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Status(status) => self.status = status,
            Event::Chats(chats) if chats.is_empty() => {
                // Logged out: nothing of the old account stays on screen.
                self.chats.clear();
                self.messages.clear();
                self.open = None;
                self.cursor = None;
                self.selected = 0;
                self.mode = Mode::Chats;
            }
            Event::Chats(chats) => {
                let current = self.chats.get(self.selected).map(|chat| chat.id.clone());
                self.chats = chats;
                self.selected = current
                    .and_then(|id| self.chats.iter().position(|chat| chat.id == id))
                    .unwrap_or(0);
            }
            Event::History { chat, messages } if self.open.as_ref() == Some(&chat) => {
                self.cursor = self
                    .jump_to
                    .take()
                    .and_then(|id| messages.iter().position(|message| message.id == id));
                if self.cursor.is_some() {
                    self.mode = Mode::Messages;
                }
                self.messages = messages;
            }
            Event::History { .. } => {}
            Event::Message(message) => self.upsert(message),
            Event::Search(results) => {
                self.results = results;
                self.result = 0;
            }
            Event::Error(error) => self.error = Some(error),
            Event::Notice(notice) => self.notice = Some(notice),
            Event::Removed { chat, id } if self.open.as_ref() == Some(&chat) => {
                if let Some(at) = self.messages.iter().position(|m| m.id == id) {
                    self.messages.remove(at);
                    self.cursor = self.cursor.and_then(|c| match c.cmp(&at) {
                        std::cmp::Ordering::Less => Some(c),
                        _ => c.checked_sub(1).filter(|_| !self.messages.is_empty()),
                    });
                }
            }
            Event::Removed { .. } => {}
        }
    }

    fn upsert(&mut self, message: Message) {
        if let Some(chat) = self.chats.iter().position(|chat| chat.id == message.chat) {
            let selected = self.chats.get(self.selected).map(|chat| chat.id.clone());
            let mut entry = self.chats.remove(chat);
            if message.ts >= entry.last {
                entry.last = message.ts;
                entry.preview = message.body.summary();
            }
            let at = self
                .chats
                .partition_point(|chat| (chat.pinned, chat.last) > (entry.pinned, entry.last));
            self.chats.insert(at, entry);
            if let Some(id) = selected {
                self.selected = self
                    .chats
                    .iter()
                    .position(|chat| chat.id == id)
                    .unwrap_or(0);
            }
        }
        if self.open.as_ref() != Some(&message.chat) {
            return;
        }
        match self
            .messages
            .iter_mut()
            .find(|known| known.id == message.id)
        {
            Some(known) => *known = message,
            None => {
                let at = self
                    .messages
                    .partition_point(|known| known.ts <= message.ts);
                self.messages.insert(at, message);
                if let Some(cursor) = self.cursor.as_mut()
                    && at <= *cursor
                {
                    *cursor += 1;
                }
            }
        }
    }

    pub fn selected_message(&self) -> Option<&Message> {
        self.cursor.and_then(|cursor| self.messages.get(cursor))
    }

    fn open_chat(&mut self, id: ChatId) {
        self.selected = self
            .chats
            .iter()
            .position(|chat| chat.id == id)
            .unwrap_or(0);
        self.messages.clear();
        self.cursor = None;
        self.reply_to = None;
        self.open = Some(id.clone());
        self.outbox.push(Command::Open(id));
        self.mode = Mode::Compose;
    }

    pub fn key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.error = None;
        self.notice = None;
        let linking = matches!(self.status, LinkStatus::Qr(_) | LinkStatus::LoggedOut);
        if linking || (self.status != LinkStatus::Connected && self.chats.is_empty()) {
            if key.code == KeyCode::Char('q') || key.code == KeyCode::Esc {
                self.quit = true;
            }
            return;
        }
        match self.mode {
            Mode::Chats => self.chats_key(key),
            Mode::Messages => self.messages_key(key),
            Mode::Compose => self.compose_key(key),
            Mode::React => self.react_key(key),
            Mode::Delete => self.delete_key(key),
            Mode::Logout => {
                if key.code == KeyCode::Char('y') {
                    self.outbox.push(Command::Logout);
                }
                self.mode = Mode::Chats;
            }
            Mode::Search => self.search_key(key),
        }
    }

    /// Pasted or dropped text: an image file attaches, anything else is typed.
    pub fn paste(&mut self, text: &str) {
        match self.mode {
            Mode::Search => {
                self.search.push_str(text.trim_end_matches('\n'));
                self.searched();
            }
            Mode::Chats | Mode::Messages | Mode::Compose if self.open.is_some() => {
                match image_path(text) {
                    Some(path) => self.attachment = Some(path),
                    None => self.input.push_str(text),
                }
                self.mode = Mode::Compose;
            }
            _ => {}
        }
    }

    fn chats_key(&mut self, key: KeyEvent) {
        if let Some(step) = page_step(key, self.chat_page) {
            let last = self.chats.len().saturating_sub(1) as isize;
            self.selected = (self.selected as isize + step).clamp(0, last) as usize;
            return;
        }
        match key.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.chats.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Char('g') | KeyCode::Home => self.selected = 0,
            KeyCode::Char('G') | KeyCode::End => self.selected = self.chats.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                if let Some(chat) = self.chats.get(self.selected) {
                    self.open_chat(chat.id.clone());
                }
            }
            KeyCode::Char('[') => self.resize(self.sidebar.saturating_sub(4)),
            KeyCode::Char(']') => self.resize(self.sidebar + 4),
            KeyCode::Char('L') => self.mode = Mode::Logout,
            KeyCode::Char('p') => {
                if let Some(chat) = self.chats.get(self.selected) {
                    self.outbox.push(Command::Pin {
                        chat: chat.id.clone(),
                        pinned: !chat.pinned,
                    });
                }
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            _ => {}
        }
    }

    /// Sets the chat list's width, within reason.
    pub fn resize(&mut self, width: u16) {
        self.sidebar = width.clamp(MIN_SIDEBAR, MAX_SIDEBAR);
    }

    /// Dragging the chat list's right edge resizes it.
    pub fn mouse(&mut self, mouse: MouseEvent) {
        let edge = self.sidebar.saturating_sub(1);
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) if mouse.column.abs_diff(edge) <= 1 => {
                self.resizing = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                self.resize(mouse.column + 1)
            }
            MouseEventKind::Up(_) => self.resizing = false,
            _ => {}
        }
    }

    fn delete_key(&mut self, key: KeyEvent) {
        let everyone = match key.code {
            KeyCode::Char('e') if self.selected_message().is_some_and(|m| m.from_me) => true,
            KeyCode::Char('m') => false,
            _ => {
                self.mode = Mode::Messages;
                return;
            }
        };
        if let Some(message) = self.selected_message() {
            self.outbox.push(Command::Delete {
                chat: message.chat.clone(),
                id: message.id.clone(),
                everyone,
            });
        }
        self.mode = Mode::Messages;
    }

    fn messages_key(&mut self, key: KeyEvent) {
        let last = self.messages.len().saturating_sub(1);
        let cursor = self.cursor.unwrap_or(last);
        if let Some(step) = page_step(key, MESSAGE_PAGE) {
            let next = (cursor as isize + step).clamp(0, last as isize) as usize;
            self.cursor = (next < last).then_some(next);
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left => {
                self.cursor = None;
                self.mode = Mode::Chats;
            }
            KeyCode::Char('k') | KeyCode::Up => self.cursor = Some(cursor.saturating_sub(1)),
            KeyCode::Char('j') | KeyCode::Down => {
                self.cursor = (cursor < last).then_some(cursor + 1);
            }
            KeyCode::Char('g') | KeyCode::Home => self.cursor = Some(0),
            KeyCode::Char('G') | KeyCode::End => self.cursor = None,
            KeyCode::Enter => match self.messages.get(cursor) {
                Some(message) if !matches!(message.body, Body::Text(_)) => {
                    self.outbox.push(Command::OpenMedia {
                        chat: message.chat.clone(),
                        id: message.id.clone(),
                    });
                }
                _ => self.mode = Mode::Compose,
            },
            KeyCode::Char('i') => self.mode = Mode::Compose,
            // Cmd+S where the terminal reports it, plain s everywhere.
            KeyCode::Char('s') => {
                if let Some(message) = self.messages.get(cursor)
                    && !matches!(message.body, Body::Text(_))
                {
                    self.outbox.push(Command::SaveMedia {
                        chat: message.chat.clone(),
                        id: message.id.clone(),
                    });
                }
            }
            KeyCode::Char('r') => {
                if let Some(message) = self.messages.get(cursor) {
                    self.reply_to = Some(message.id.clone());
                    self.mode = Mode::Compose;
                }
            }
            KeyCode::Char('e') if !self.messages.is_empty() => {
                self.cursor = Some(cursor);
                self.input.clear();
                self.mode = Mode::React;
            }
            KeyCode::Char('d') if !self.messages.is_empty() => {
                self.cursor = Some(cursor);
                self.mode = Mode::Delete;
            }
            KeyCode::Char('/') => self.mode = Mode::Search,
            _ => {}
        }
    }

    fn compose_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if self.attachment.take().is_none() && self.reply_to.take().is_none() {
                    self.mode = Mode::Messages;
                }
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.input.push('\n'),
            KeyCode::Enter => {
                let text = self.input.trim().to_owned();
                // A path typed or pasted without bracketed paste attaches first.
                if self.attachment.is_none()
                    && let Some(path) = image_path(&text)
                {
                    self.attachment = Some(path);
                    self.input.clear();
                    return;
                }
                if let (Some(path), Some(chat)) = (self.attachment.clone(), self.open.clone()) {
                    self.outbox.push(Command::SendImage {
                        chat,
                        path,
                        caption: (!text.is_empty()).then_some(text),
                        reply_to: self.reply_to.take(),
                    });
                    self.attachment = None;
                    self.input.clear();
                    self.cursor = None;
                    return;
                }
                if let (false, Some(chat)) = (text.is_empty(), self.open.clone()) {
                    self.outbox.push(Command::Send {
                        chat,
                        text,
                        reply_to: self.reply_to.take(),
                    });
                    self.input.clear();
                    self.cursor = None;
                }
            }
            _ => {
                edit(&mut self.input, key);
            }
        }
    }

    fn react_key(&mut self, key: KeyEvent) {
        let emoji = match key.code {
            KeyCode::Esc => {
                self.input.clear();
                self.mode = Mode::Messages;
                return;
            }
            KeyCode::Char(c @ '1'..='6') if self.input.is_empty() => {
                QUICK_REACTIONS[c as usize - '1' as usize].to_owned()
            }
            // Backspace on an empty prompt removes our reaction.
            KeyCode::Backspace if self.input.is_empty() => String::new(),
            KeyCode::Backspace => {
                self.input.pop();
                return;
            }
            KeyCode::Enter => std::mem::take(&mut self.input),
            KeyCode::Char(c) => {
                self.input.push(c);
                return;
            }
            _ => return,
        };
        if let Some(message) = self.selected_message() {
            self.outbox.push(Command::React {
                chat: message.chat.clone(),
                id: message.id.clone(),
                emoji,
            });
        }
        self.input.clear();
        self.mode = Mode::Messages;
    }

    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.mode = if self.open.is_some() {
                    Mode::Messages
                } else {
                    Mode::Chats
                };
            }
            KeyCode::Down => {
                self.result = (self.result + 1).min(self.search_len().saturating_sub(1));
            }
            KeyCode::Up => self.result = self.result.saturating_sub(1),
            KeyCode::Enter => {
                let chats = self.matching_chats();
                let target = match chats.get(self.result) {
                    Some(chat) => Some((chat.id.clone(), None)),
                    None => self
                        .results
                        .get(self.result - chats.len())
                        .map(|message| (message.chat.clone(), Some(message.id.clone()))),
                };
                if let Some((chat, id)) = target {
                    self.open_chat(chat);
                    self.jump_to = id;
                }
            }
            _ => {
                if edit(&mut self.search, key) {
                    self.searched();
                }
            }
        }
    }

    fn searched(&mut self) {
        self.result = 0;
        self.results.clear();
        if self.search.chars().count() >= 2 {
            self.outbox.push(Command::Search(self.search.clone()));
        }
    }

    pub fn matching_chats(&self) -> Vec<&Chat> {
        let needle = self.search.to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        self.chats
            .iter()
            .filter(|chat| chat.name.to_lowercase().contains(&needle))
            .take(20)
            .collect()
    }

    fn search_len(&self) -> usize {
        self.matching_chats().len() + self.results.len()
    }
}

/// How far a paging key moves: PgDn/PgUp and ctrl+f/ctrl+b a page,
/// ctrl+d/ctrl+u half of one.
fn page_step(key: KeyEvent, page: usize) -> Option<isize> {
    let page = page.max(2) as isize;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::PageDown => Some(page),
        KeyCode::PageUp => Some(-page),
        KeyCode::Char('f') if ctrl => Some(page),
        KeyCode::Char('b') if ctrl => Some(-page),
        KeyCode::Char('d') if ctrl => Some(page / 2),
        KeyCode::Char('u') if ctrl => Some(-page / 2),
        _ => None,
    }
}

/// Line editing shared by the text inputs; true when the text changed.
///
/// Terminals send macOS Cmd+Delete as ctrl+u and Ctrl+Delete as ctrl+h or
/// ctrl+backspace, so those are commands, never typed letters.
fn edit(text: &mut String, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let before = text.len();
    match key.code {
        KeyCode::Char('u') if ctrl => text.clear(),
        KeyCode::Backspace if key.modifiers.contains(KeyModifiers::SUPER) => text.clear(),
        KeyCode::Char('w' | 'h') if ctrl => delete_word(text),
        KeyCode::Backspace if ctrl || alt => delete_word(text),
        KeyCode::Backspace => {
            text.pop();
        }
        KeyCode::Char(_) if ctrl || key.modifiers.contains(KeyModifiers::SUPER) => {}
        KeyCode::Char(c) => text.push(c),
        _ => {}
    }
    text.len() != before
}

/// Removes the word before the end, and the spaces after it.
fn delete_word(text: &mut String) {
    let trimmed = text.trim_end().len();
    text.truncate(trimmed);
    let start = text.rfind(char::is_whitespace).map_or(0, |at| at + 1);
    text.truncate(start);
}

/// An existing picture named by pasted text, as terminals hand over a drop:
/// shell-escaped (`a\\ b.png`), quoted, or as a file URL.
fn image_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let text = text.strip_prefix("file://").unwrap_or(text);
    let text = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| text.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
        .unwrap_or(text);
    let mut path = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => path.extend(chars.next()),
            '\n' => return None,
            c => path.push(c),
        }
    }
    let path = PathBuf::from(path);
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    (matches!(extension.as_str(), "jpg" | "jpeg" | "png" | "webp") && path.is_file())
        .then_some(path)
}
