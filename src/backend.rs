//! Channel bridge between the interface and the WhatsApp runtime.
//!
//! Modelled on zapfast's backend split: a dedicated tokio runtime owns the
//! connection and the archive; commands and events cross channels.

use std::path::PathBuf;
use std::sync::mpsc;

use crate::model::{ChatId, Chat, LinkStatus, Message};

mod worker;

#[derive(Debug)]
pub enum Command {
    /// Load the newest page of a chat's history.
    Open(ChatId),
    Send {
        chat: ChatId,
        text: String,
        reply_to: Option<String>,
    },
    /// An empty emoji removes our reaction.
    React {
        chat: ChatId,
        id: String,
        emoji: String,
    },
    Search(String),
    /// Fetch an image message's media into the cache.
    Download { chat: ChatId, id: String },
}

#[derive(Debug)]
pub enum Event {
    Status(LinkStatus),
    Chats(Vec<Chat>),
    History { chat: ChatId, messages: Vec<Message> },
    /// A new or changed message.
    Message(Message),
    Search(Vec<Message>),
    Image { id: String, path: PathBuf },
    Error(String),
}

pub struct Backend {
    commands: tokio::sync::mpsc::UnboundedSender<Command>,
    pub events: mpsc::Receiver<Event>,
}

impl Backend {
    pub fn spawn(dir: PathBuf) -> Self {
        let (commands, inbox) = tokio::sync::mpsc::unbounded_channel();
        let (outbox, events) = mpsc::channel();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            runtime.block_on(worker::run(dir, inbox, outbox));
        });
        Self { commands, events }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}
