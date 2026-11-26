//! Interface state. Consumes backend events, produces commands; no drawing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;

use crate::backend::{Command, Event};
use crate::model::{Chat, ChatId, LinkStatus, Message};

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
    pub search: String,
    pub results: Vec<Message>,
    pub result: usize,
    /// Message a search result opens the chat at.
    jump_to: Option<String>,
    /// Whether the terminal shows pictures; otherwise images stay labels.
    pub inline_images: bool,
    pub images: HashMap<String, PathBuf>,
    requested: HashSet<String>,
    /// Image placements the last frame laid out, drawn after it.
    pub placements: Vec<(String, Rect)>,
    pub error: Option<String>,
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
            search: String::new(),
            results: Vec::new(),
            result: 0,
            jump_to: None,
            inline_images: false,
            images: HashMap::new(),
            requested: HashSet::new(),
            placements: Vec::new(),
            error: None,
            outbox: Vec::new(),
            quit: false,
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Status(status) => self.status = status,
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
            Event::Image { id, path } => {
                self.images.insert(id, path);
            }
            Event::Error(error) => self.error = Some(error),
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
            let at = self.chats.partition_point(|chat| chat.last > entry.last);
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

    /// Asks once for an image the interface wants to show.
    pub fn want_image(&mut self, message: &Message) {
        if self.inline_images
            && !self.images.contains_key(&message.id)
            && self.requested.insert(message.id.clone())
        {
            self.outbox.push(Command::Download {
                chat: message.chat.clone(),
                id: message.id.clone(),
            });
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
        if self.status != LinkStatus::Connected && self.chats.is_empty() {
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
            Mode::Search => self.search_key(key),
        }
    }

    fn chats_key(&mut self, key: KeyEvent) {
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
            KeyCode::Char('/') => self.mode = Mode::Search,
            _ => {}
        }
    }

    fn messages_key(&mut self, key: KeyEvent) {
        let last = self.messages.len().saturating_sub(1);
        let cursor = self.cursor.unwrap_or(last);
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
            KeyCode::Char('i') | KeyCode::Enter => self.mode = Mode::Compose,
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
            KeyCode::Char('/') => self.mode = Mode::Search,
            _ => {}
        }
    }

    fn compose_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if self.reply_to.take().is_none() {
                    self.mode = Mode::Messages;
                }
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => self.input.push('\n'),
            KeyCode::Enter => {
                let text = self.input.trim().to_owned();
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
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
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
            KeyCode::Backspace => {
                self.search.pop();
                self.searched();
            }
            KeyCode::Char(c) => {
                self.search.push(c);
                self.searched();
            }
            _ => {}
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
