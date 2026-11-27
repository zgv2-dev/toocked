//! The runtime side: owns the WhatsApp client and the archive.
//!
//! A slimmed-down take on zapfast's worker: link by QR, file history and live
//! messages, send text, pictures, replies and reactions, open attachments.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use whatsapp_rust::download::MediaType;
use whatsapp_rust::media::{ImageOptions, image_message};
use whatsapp_rust::pair::NATIVE_CAMERA_DEEP_LINK_PREFIX;
use whatsapp_rust::prelude::{Bot, BotHandle, Client, Jid, MessageBuilderExt, MessageExt, wa};
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::{Event as Wa, LazyHistorySync};
use whatsapp_rust::types::message::MessageInfo;
use whatsapp_rust::upload::UploadOptions;
use whatsapp_rust::wacore::download::Downloadable;
use whatsapp_rust::wacore::history_sync::{HistorySyncStream, MAX_DECOMPRESSED};
use whatsapp_rust::wacore::proto_helpers::build_quote_context_with_info;
use whatsapp_rust::wacore_binary::jid::JidExt;
use whatsapp_rust::waproto::buffa::Message as _;
use whatsapp_rust::{MessageId, MessageRef, SendRequest};

use super::{Command, Event};
use crate::archive::{Archive, fallback_name};
use crate::model::{Body, ChatId, LinkStatus, Message, Quote};

/// Sender id our own reactions are filed under.
const ME: &str = "me";

/// Results of work spawned off the loop, filed back on it.
enum Done {
    Sent {
        message: Message,
        raw: Vec<u8>,
    },
    Reacted {
        chat: ChatId,
        id: String,
        emoji: String,
    },
    Pinned {
        chat: ChatId,
        pinned: bool,
    },
    Deleted {
        chat: ChatId,
        id: String,
        everyone: bool,
    },
    History(Parsed),
    Failed(String),
}

pub async fn run(dir: PathBuf, mut inbox: UnboundedReceiver<Command>, outbox: mpsc::Sender<Event>) {
    // Attachments are opened from a temp folder; Cmd+S copies one to Downloads.
    let media = std::env::temp_dir().join("toocked");
    if let Err(error) = std::fs::create_dir_all(&media) {
        let _ = outbox.send(Event::Status(LinkStatus::Failed(error.to_string())));
        return;
    }
    let archive = match Archive::open(&dir.join("archive.db")) {
        Ok(archive) => archive,
        Err(error) => {
            let _ = outbox.send(Event::Status(LinkStatus::Failed(error.to_string())));
            return;
        }
    };
    let lids = archive.lids().unwrap_or_default().into_iter().collect();
    let (wa_tx, mut wa_rx) = unbounded_channel();
    let (done_tx, mut done_rx) = unbounded_channel();
    let mut worker = Worker {
        archive,
        outbox,
        session: dir.join("session.db"),
        media,
        wa_tx,
        done: done_tx,
        client: None,
        handle: None,
        lids,
    };
    worker.emit_chats();
    worker.start().await;
    loop {
        tokio::select! {
            Some(command) = inbox.recv() => worker.command(command),
            Some(event) = wa_rx.recv() => worker.wa(event).await,
            Some(done) = done_rx.recv() => worker.done(done),
            else => break,
        }
    }
}

struct Worker {
    archive: Archive,
    outbox: mpsc::Sender<Event>,
    session: PathBuf,
    media: PathBuf,
    wa_tx: UnboundedSender<Arc<Wa>>,
    done: UnboundedSender<Done>,
    client: Option<Arc<Client>>,
    handle: Option<BotHandle>,
    /// Privacy id user to phone number user.
    lids: HashMap<String, String>,
}

impl Worker {
    fn emit(&self, event: Event) {
        let _ = self.outbox.send(event);
    }

    fn emit_chats(&self) {
        match self.archive.chats() {
            Ok(chats) => self.emit(Event::Chats(chats)),
            Err(error) => self.emit(Event::Error(error.to_string())),
        }
    }

    async fn start(&mut self) {
        let store = match SqliteStore::open(&self.session.to_string_lossy()).await {
            Ok(store) => store,
            Err(error) => {
                return self.emit(Event::Status(LinkStatus::Failed(format!(
                    "device store: {error}"
                ))));
            }
        };
        let sender = self.wa_tx.clone();
        let bot = Bot::builder()
            .with_backend(store)
            .on_event(move |event, _client| {
                let _ = sender.send(event);
                async {}
            })
            .build()
            .await;
        match bot {
            Ok(bot) => {
                let handle = bot.spawn();
                self.client = Some(handle.client());
                self.handle = Some(handle);
                self.emit(Event::Status(LinkStatus::Connecting));
            }
            Err(error) => self.emit(Event::Status(LinkStatus::Failed(error.to_string()))),
        }
    }

    /// A fresh client over the same device store, for a new set of QR codes.
    async fn restart(&mut self) {
        self.client = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.shutdown().await;
        }
        self.start().await;
    }

    /// Drops the linked device and everything filed under it, then offers a
    /// new QR code.
    async fn forget(&mut self) {
        self.emit(Event::Status(LinkStatus::LoggedOut));
        self.client = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.shutdown().await;
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.session.display()));
        }
        if let Err(error) = self.archive.clear() {
            self.emit(Event::Error(format!(
                "Could not clear the archive: {error}"
            )));
        }
        self.lids.clear();
        let _ = std::fs::remove_dir_all(&self.media);
        let _ = std::fs::create_dir_all(&self.media);
        self.emit(Event::Chats(Vec::new()));
        self.start().await;
    }

    async fn wa(&mut self, event: Arc<Wa>) {
        match &*event {
            Wa::PairingQrCode(qr) => self.emit(Event::Status(LinkStatus::Qr(format!(
                "{NATIVE_CAMERA_DEEP_LINK_PREFIX}{}",
                qr.code
            )))),
            Wa::PairingQrCodesExhausted(exhausted) if exhausted.disconnected => {
                self.restart().await
            }
            Wa::PairSuccess(_) => self.emit(Event::Status(LinkStatus::Connecting)),
            Wa::Connected(_) => self.emit(Event::Status(LinkStatus::Connected)),
            Wa::Disconnected(_) => self.emit(Event::Status(LinkStatus::Disconnected(
                "connection lost".into(),
            ))),
            // The phone unlinked us.
            Wa::LoggedOut(_) => self.forget().await,
            Wa::Messages(batch) => {
                for inbound in batch.messages.iter() {
                    self.ingest(&inbound.message, &inbound.info);
                }
            }
            Wa::HistorySync(lazy) => self.history(lazy),
            Wa::PinUpdate(update) => {
                let since = match update.action.pinned {
                    Some(true) => update.timestamp.timestamp().max(1),
                    _ => 0,
                };
                if self
                    .archive
                    .set_pinned(&self.canonical(&update.jid), since)
                    .is_ok()
                {
                    self.emit_chats();
                }
            }
            _ => {}
        }
    }

    fn learn(&mut self, a: &Jid, b: &Jid) {
        let (lid, pn) = match (a.is_lid(), b.is_lid()) {
            (true, false) => (a, b),
            (false, true) => (b, a),
            _ => return,
        };
        let (lid, pn) = (lid.user_base().to_owned(), pn.user_base().to_owned());
        if self.lids.get(&lid) != Some(&pn) {
            let _ = self.archive.learn_lid(&lid, &pn);
            self.lids.insert(lid, pn);
        }
    }

    /// The archive id of a JID: phone-number form where the privacy id is known.
    fn canonical(&self, jid: &Jid) -> String {
        if jid.is_lid()
            && let Some(pn) = self.lids.get(jid.user_base())
        {
            return format!("{pn}@s.whatsapp.net");
        }
        jid.to_non_ad_string()
    }

    fn canonical_str(&self, id: &str) -> String {
        id.parse::<Jid>()
            .map_or_else(|_| id.to_owned(), |jid| self.canonical(&jid))
    }

    fn ingest(&mut self, message: &wa::Message, info: &MessageInfo) {
        let source = &info.source;
        if source.chat.is_status_broadcast() || source.chat.is_broadcast_list() {
            return;
        }
        if let Some(alt) = &source.sender_alt {
            self.learn(&source.sender, alt);
        }
        if let Some(alt) = &source.recipient_alt {
            self.learn(source.recipient.as_ref().unwrap_or(&source.chat), alt);
        }
        let chat = self.canonical(&source.chat);
        let sender = if source.is_from_me {
            String::new()
        } else {
            self.canonical(&source.sender)
        };
        if !source.is_from_me && !info.push_name.is_empty() {
            let _ = self.archive.set_name(&sender, &info.push_name);
        }
        let ts = info.timestamp.timestamp();
        let id = info.id.to_string();
        let raw = message.encode_to_vec();
        let known = self.archive.has_chat(&chat);
        match self.parse(message) {
            Parsed1::Show { body, quote } => {
                let message = Message {
                    sender_name: self
                        .archive
                        .name(&sender)
                        .unwrap_or_else(|| fallback_name(&sender)),
                    chat,
                    id,
                    sender,
                    from_me: source.is_from_me,
                    ts,
                    body,
                    quote,
                    reactions: Vec::new(),
                };
                if self.archive.put(&message, Some(&raw)).is_ok() {
                    self.emit_message(&message.chat, &message.id);
                }
            }
            Parsed1::Replace { target, body } => self.replace(&chat, &target, body),
            Parsed1::React { target, emoji } => {
                let sender = if source.is_from_me {
                    ME.to_owned()
                } else {
                    sender
                };
                if self.archive.react(&chat, &target, &sender, &emoji).is_ok() {
                    self.emit_message(&chat, &target);
                }
            }
            Parsed1::Skip => return,
        }
        if !known {
            self.emit_chats();
        }
    }

    fn replace(&self, chat: &str, target: &str, body: Body) {
        if let Ok(Some(mut message)) = self.archive.message(chat, target) {
            message.body = body;
            if self.archive.put(&message, None).is_ok() {
                self.emit_message(chat, target);
            }
        }
    }

    fn emit_message(&self, chat: &str, id: &str) {
        if let Ok(Some(message)) = self.archive.message(chat, id) {
            self.emit(Event::Message(message));
        }
    }

    fn parse(&self, message: &wa::Message) -> Parsed1 {
        let base = message.get_base_message();
        if let Some(protocol) = base.protocol_message.as_option() {
            use wa::message::protocol_message::Type;
            let target = protocol.key.as_option().and_then(|key| key.id.clone());
            return match (protocol.r#type, target) {
                (Some(Type::REVOKE), Some(target)) => Parsed1::Replace {
                    target,
                    body: Body::Other("🚫 This message was deleted".into()),
                },
                (Some(Type::MESSAGE_EDIT), Some(target)) => match protocol
                    .edited_message
                    .as_option()
                    .and_then(|edited| classify(edited.get_base_message()))
                {
                    Some(body) => Parsed1::Replace { target, body },
                    None => Parsed1::Skip,
                },
                _ => Parsed1::Skip,
            };
        }
        if let Some(reaction) = base.reaction_message.as_option() {
            return match reaction.key.as_option().and_then(|key| key.id.clone()) {
                Some(target) => Parsed1::React {
                    target,
                    emoji: reaction.text.clone().unwrap_or_default(),
                },
                None => Parsed1::Skip,
            };
        }
        match classify(base) {
            Some(body) => Parsed1::Show {
                body,
                quote: self.quote(base),
            },
            None => Parsed1::Skip,
        }
    }

    fn quote(&self, base: &wa::Message) -> Option<Quote> {
        let context = context_of(base)?;
        let id = context.stanza_id.clone().filter(|id| !id.is_empty())?;
        let sender = context
            .participant
            .as_deref()
            .map(|p| self.canonical_str(p))
            .unwrap_or_default();
        let text = context
            .quoted_message
            .as_option()
            .and_then(|quoted| classify(quoted.get_base_message()))
            .map(|body| body.summary())
            .unwrap_or_default();
        let sender = self
            .archive
            .name(&sender)
            .unwrap_or_else(|| fallback_name(&sender));
        Some(Quote { id, sender, text })
    }

    fn history(&self, lazy: &LazyHistorySync) {
        let compressed = lazy.compressed_bytes().clone();
        let done = self.done.clone();
        tokio::task::spawn_blocking(move || {
            let _ = done.send(match parse_history(&compressed) {
                Ok(parsed) => Done::History(parsed),
                Err(error) => Done::Failed(format!("history: {error}")),
            });
        });
    }

    fn file_history(&mut self, parsed: Parsed) {
        for (lid, pn) in &parsed.lids {
            if let (Ok(lid), Ok(pn)) = (lid.parse::<Jid>(), pn.parse::<Jid>()) {
                self.learn(&lid, &pn);
            }
        }
        self.archive.begin();
        for (jid, name) in &parsed.names {
            let _ = self.archive.set_name(&self.canonical_str(jid), name);
        }
        for conversation in parsed.conversations {
            let chat = self.canonical_str(&conversation.id);
            let _ = self
                .archive
                .touch_chat(&chat, conversation.name.as_deref(), conversation.last);
            let _ = self.archive.set_pinned(&chat, conversation.pinned);
            for entry in conversation.messages {
                let Ok(message) = wa::Message::decode_from_slice(&entry.raw) else {
                    continue;
                };
                let sender = if entry.from_me {
                    String::new()
                } else {
                    entry
                        .sender
                        .as_deref()
                        .map(|s| self.canonical_str(s))
                        .unwrap_or_else(|| chat.clone())
                };
                match self.parse(&message) {
                    Parsed1::Show { body, quote } => {
                        let message = Message {
                            chat: chat.clone(),
                            id: entry.id,
                            sender_name: String::new(),
                            sender,
                            from_me: entry.from_me,
                            ts: entry.ts,
                            body,
                            quote,
                            reactions: Vec::new(),
                        };
                        let _ = self.archive.put(&message, Some(&entry.raw));
                    }
                    Parsed1::React { target, emoji } => {
                        let sender = if entry.from_me { ME.to_owned() } else { sender };
                        let _ = self.archive.react(&chat, &target, &sender, &emoji);
                    }
                    Parsed1::Replace { .. } | Parsed1::Skip => {}
                }
            }
        }
        self.archive.commit();
        self.emit_chats();
    }

    fn command(&mut self, command: Command) {
        match command {
            Command::Open(chat) => match self.archive.history(&chat) {
                Ok(messages) => self.emit(Event::History { chat, messages }),
                Err(error) => self.emit(Event::Error(error.to_string())),
            },
            Command::Search(needle) => match self.archive.search(&needle) {
                Ok(results) => self.emit(Event::Search(results)),
                Err(error) => self.emit(Event::Error(error.to_string())),
            },
            Command::Send {
                chat,
                text,
                reply_to,
            } => self.send(chat, text, reply_to),
            Command::SendImage {
                chat,
                path,
                caption,
                reply_to,
            } => self.send_image(chat, path, caption, reply_to),
            Command::React { chat, id, emoji } => self.react(chat, id, emoji),
            Command::Pin { chat, pinned } => self.pin(chat, pinned),
            Command::Delete { chat, id, everyone } => self.delete(chat, id, everyone),
            Command::OpenMedia { chat, id } => self.media(chat, id, false),
            Command::SaveMedia { chat, id } => self.media(chat, id, true),
        }
    }

    /// The reply context for quoting `reply_to`, and how the reply shows it.
    fn quoting(
        &self,
        client: &Client,
        chat: &str,
        jid: &Jid,
        reply_to: Option<String>,
    ) -> (Option<wa::ContextInfo>, Option<Quote>) {
        let Some((quoted, id)) =
            reply_to.and_then(|id| Some((self.archive.message(chat, &id).ok()??, id)))
        else {
            return (None, None);
        };
        let raw = self
            .archive
            .raw(chat, &id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let body = wa::Message::decode_from_slice(&raw).unwrap_or_default();
        let author = match quoted.from_me {
            true => client.pn().map(|pn| pn.to_non_ad()),
            false => quoted.sender.parse::<Jid>().ok(),
        };
        let quote = Quote {
            id: id.clone(),
            sender: if quoted.from_me {
                "You".into()
            } else {
                quoted.sender_name.clone()
            },
            text: quoted.body.summary(),
        };
        let context =
            author.map(|author| build_quote_context_with_info(id, &author, jid, jid, &body));
        (context, Some(quote))
    }

    fn send(&self, chat: ChatId, text: String, reply_to: Option<String>) {
        let (Some(client), Ok(jid)) = (self.client.clone(), chat.parse::<Jid>()) else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let (context, quote) = self.quoting(&client, &chat, &jid, reply_to);
        let message = match context {
            Some(context) => wa::Message::text_with_context(text.clone(), context),
            None => wa::Message::text(text.clone()),
        };
        self.deliver(client, jid, chat, message, Body::Text(text), quote, None);
    }

    fn send_image(
        &self,
        chat: ChatId,
        path: PathBuf,
        caption: Option<String>,
        reply_to: Option<String>,
    ) {
        let (Some(client), Ok(jid)) = (self.client.clone(), chat.parse::<Jid>()) else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let (context, quote) = self.quoting(&client, &chat, &jid, reply_to);
        let done = self.done.clone();
        let this = (self.done.clone(), self.media.clone());
        tokio::spawn(async move {
            let prepared = tokio::task::spawn_blocking(move || prepare_image(&path)).await;
            let (jpeg, thumbnail, width, height) = match prepared {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(error)) => return drop(done.send(Done::Failed(format!("Image: {error}")))),
                Err(error) => return drop(done.send(Done::Failed(error.to_string()))),
            };
            let upload = match client
                .upload(jpeg.clone(), MediaType::Image, UploadOptions::default())
                .await
            {
                Ok(upload) => upload,
                Err(error) => return drop(done.send(Done::Failed(format!("Upload: {error}")))),
            };
            let mut options = ImageOptions::default();
            options.caption = caption.clone();
            options.jpeg_thumbnail = Some(thumbnail);
            options.context_info = context.map(Box::new);
            let mut message = image_message(upload, options);
            if let Some(image) = message.image_message.as_option_mut() {
                image.width = Some(width);
                image.height = Some(height);
            }
            let body = Body::Image { caption };
            deliver(client, jid, chat, message, body, quote, Some(jpeg), this).await;
        });
    }

    /// Sends a built message off the loop and files it once it went out.
    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &self,
        client: Arc<Client>,
        jid: Jid,
        chat: ChatId,
        message: wa::Message,
        body: Body,
        quote: Option<Quote>,
        picture: Option<Vec<u8>>,
    ) {
        let this = (self.done.clone(), self.media.clone());
        tokio::spawn(deliver(
            client, jid, chat, message, body, quote, picture, this,
        ));
    }

    fn react(&self, chat: ChatId, id: String, emoji: String) {
        let Some(client) = self.client.clone() else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let Ok(Some(target)) = self.archive.message(&chat, &id) else {
            return;
        };
        let done = self.done.clone();
        tokio::spawn(async move {
            let result = async {
                let jid: Jid = chat.parse().map_err(|_| "bad chat id".to_owned())?;
                let sender = target.sender.parse::<Jid>().ok();
                let reference = MessageRef::new(
                    &jid,
                    MessageId::new(&id).map_err(|e| e.to_string())?,
                    sender.as_ref().filter(|_| !target.from_me),
                    target.from_me,
                )
                .map_err(|e| e.to_string())?;
                client
                    .send_reaction(&reference, &emoji)
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            let _ = done.send(match result {
                Ok(_) => Done::Reacted { chat, id, emoji },
                Err(error) => Done::Failed(format!("Reaction not sent: {error}")),
            });
        });
    }

    fn pin(&self, chat: ChatId, pinned: bool) {
        let (Some(client), Ok(jid)) = (self.client.clone(), chat.parse::<Jid>()) else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let done = self.done.clone();
        tokio::spawn(async move {
            let actions = client.chat_actions();
            let result = match pinned {
                true => actions.pin_chat(&jid).await,
                false => actions.unpin_chat(&jid).await,
            };
            let _ = done.send(match result {
                Ok(()) => Done::Pinned { chat, pinned },
                Err(error) => Done::Failed(format!("Pin: {error}")),
            });
        });
    }

    /// Revokes our message for everyone, or deletes any message on our devices.
    fn delete(&self, chat: ChatId, id: String, everyone: bool) {
        let Some(client) = self.client.clone() else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let Ok(Some(target)) = self.archive.message(&chat, &id) else {
            return;
        };
        let done = self.done.clone();
        tokio::spawn(async move {
            let result = async {
                let jid: Jid = chat.parse().map_err(|_| "bad chat id".to_owned())?;
                let sender = target.sender.parse::<Jid>().ok();
                let reference = MessageRef::new(
                    &jid,
                    MessageId::new(&id).map_err(|e| e.to_string())?,
                    sender.as_ref().filter(|_| !target.from_me),
                    target.from_me,
                )
                .map_err(|e| e.to_string())?;
                if everyone {
                    client
                        .revoke_message(&reference)
                        .await
                        .map(drop)
                        .map_err(|e| e.to_string())
                } else {
                    client
                        .chat_actions()
                        .delete_message_for_me(&reference, true, Some(target.ts))
                        .await
                        .map_err(|e| e.to_string())
                }
            }
            .await;
            let _ = done.send(match result {
                Ok(()) => Done::Deleted { chat, id, everyone },
                Err(error) => Done::Failed(format!("Not deleted: {error}")),
            });
        });
    }

    /// Fetches a message's attachment into the temp folder if needed, then
    /// opens it in its app or, with `save`, copies it to Downloads.
    fn media(&self, chat: ChatId, id: String, save: bool) {
        let attached = self
            .archive
            .raw(&chat, &id)
            .ok()
            .flatten()
            .and_then(|raw| wa::Message::decode_from_slice(&raw).ok())
            .and_then(|message| attachment(message.get_base_message()));
        let Some((media, extension, name)) = attached else {
            return self.emit(Event::Error("No file in this message".into()));
        };
        let file = format!("{}.{extension}", sanitize(&id));
        let path = self.media.join(&file);
        let (client, outbox) = (self.client.clone(), self.outbox.clone());
        tokio::spawn(async move {
            if !path.exists() {
                let Some(client) = client else {
                    return drop(outbox.send(Event::Error("Not connected".into())));
                };
                let saved = match client.download(media.as_ref(), Default::default()).await {
                    Ok(bytes) => write(&path, &bytes).map_err(|e| e.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                if let Err(error) = saved {
                    return drop(outbox.send(Event::Error(format!("Download: {error}"))));
                }
            }
            let _ = outbox.send(if save {
                match save_to_downloads(&path, &name.unwrap_or(file)) {
                    Ok(saved) => Event::Notice(format!("Saved to {}", saved.display())),
                    Err(error) => Event::Error(format!("Save: {error}")),
                }
            } else {
                match open(&path) {
                    Ok(()) => return,
                    Err(error) => Event::Error(format!("Open: {error}")),
                }
            });
        });
    }

    fn done(&mut self, done: Done) {
        match done {
            Done::Sent { message, raw } => {
                let known = self.archive.has_chat(&message.chat);
                if self.archive.put(&message, Some(&raw)).is_ok() {
                    self.emit_message(&message.chat, &message.id);
                }
                if !known {
                    self.emit_chats();
                }
            }
            Done::Reacted { chat, id, emoji } => {
                if self.archive.react(&chat, &id, ME, &emoji).is_ok() {
                    self.emit_message(&chat, &id);
                }
            }
            Done::Pinned { chat, pinned } => {
                if self
                    .archive
                    .set_pinned(&chat, if pinned { now() } else { 0 })
                    .is_ok()
                {
                    self.emit_chats();
                }
            }
            Done::Deleted {
                chat,
                id,
                everyone: true,
            } => {
                self.replace(
                    &chat,
                    &id,
                    Body::Other("🚫 You deleted this message".into()),
                );
            }
            Done::Deleted {
                chat,
                id,
                everyone: false,
            } => {
                if self.archive.delete(&chat, &id).is_ok() {
                    self.emit(Event::Removed { chat, id });
                    self.emit_chats();
                }
            }
            Done::History(parsed) => self.file_history(parsed),
            Done::Failed(error) => self.emit(Event::Error(error)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn deliver(
    client: Arc<Client>,
    jid: Jid,
    chat: ChatId,
    message: wa::Message,
    body: Body,
    quote: Option<Quote>,
    picture: Option<Vec<u8>>,
    (done, media): (UnboundedSender<Done>, PathBuf),
) {
    let raw = message.encode_to_vec();
    let result = match client.send(SendRequest::new(&jid, message)).await {
        Ok(result) => result,
        Err(error) => return drop(done.send(Done::Failed(format!("Not sent: {error}")))),
    };
    let id = result.message_id.to_string();
    // Kept so opening our own picture needs no download.
    if let Some(picture) = picture {
        let _ = write(&media.join(format!("{}.jpg", sanitize(&id))), &picture);
    }
    let _ = done.send(Done::Sent {
        message: Message {
            chat,
            id,
            sender: String::new(),
            sender_name: String::new(),
            from_me: true,
            ts: now(),
            body,
            quote,
            reactions: Vec::new(),
        },
        raw,
    });
}

/// A picture as WhatsApp sends it: JPEG, a small thumbnail, and its size.
fn prepare_image(path: &Path) -> Result<(Vec<u8>, Vec<u8>, u32, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let picture = image::load_from_memory(&bytes).map_err(|e| e.to_string())?;
    let jpeg = |picture: &image::DynamicImage| -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        picture
            .to_rgb8()
            .write_to(
                &mut std::io::Cursor::new(&mut out),
                image::ImageFormat::Jpeg,
            )
            .map_err(|e| e.to_string())?;
        Ok(out)
    };
    let full = match image::guess_format(&bytes) {
        Ok(image::ImageFormat::Jpeg) => bytes,
        _ => jpeg(&picture)?,
    };
    Ok((
        full,
        jpeg(&picture.thumbnail(100, 100))?,
        picture.width(),
        picture.height(),
    ))
}

type Attachment = (Box<dyn Downloadable + Send + Sync>, String, Option<String>);

/// A message's downloadable attachment, the extension to save it with, and
/// its own file name when it has one.
fn attachment(base: &wa::Message) -> Option<Attachment> {
    let by_mime = |mime: &Option<String>, fallback: &str| {
        let mime = mime.as_deref().unwrap_or_default();
        let essence = mime.split(';').next().unwrap_or_default().trim();
        match essence {
            "image/jpeg" => "jpg",
            "audio/mpeg" => "mp3",
            "audio/mp4" => "m4a",
            "video/quicktime" => "mov",
            _ => essence
                .split('/')
                .nth(1)
                .filter(|s| !s.is_empty())
                .unwrap_or(fallback),
        }
        .to_owned()
    };
    if let Some(m) = base.image_message.as_option() {
        return Some((Box::new(m.clone()), by_mime(&m.mimetype, "jpg"), None));
    }
    if let Some(m) = base
        .video_message
        .as_option()
        .or(base.ptv_message.as_option())
    {
        return Some((Box::new(m.clone()), by_mime(&m.mimetype, "mp4"), None));
    }
    if let Some(m) = base.audio_message.as_option() {
        return Some((Box::new(m.clone()), by_mime(&m.mimetype, "ogg"), None));
    }
    if let Some(m) = base.sticker_message.as_option() {
        return Some((Box::new(m.clone()), by_mime(&m.mimetype, "webp"), None));
    }
    if let Some(m) = base.document_message.as_option() {
        let named = m
            .file_name
            .as_deref()
            .and_then(|name| Path::new(name).extension()?.to_str().map(str::to_owned));
        let extension = named.unwrap_or_else(|| by_mime(&m.mimetype, "bin"));
        let name = m
            .file_name
            .clone()
            .filter(|name| !name.is_empty() && !name.contains('/'));
        return Some((Box::new(m.clone()), extension, name));
    }
    None
}

/// How one protobuf message changes the archive.
enum Parsed1 {
    Show { body: Body, quote: Option<Quote> },
    Replace { target: String, body: Body },
    React { target: String, emoji: String },
    Skip,
}

/// What this client shows of a message, or `None` for protocol traffic.
fn classify(base: &wa::Message) -> Option<Body> {
    if let Some(text) = base.text_content() {
        return Some(Body::Text(text.to_owned()));
    }
    if let Some(image) = base.image_message.as_option() {
        return Some(Body::Image {
            caption: image.caption.clone().filter(|c| !c.is_empty()),
        });
    }
    let label = if base.video_message.is_set() || base.ptv_message.is_set() {
        "🎥 Video".to_owned()
    } else if let Some(audio) = base.audio_message.as_option() {
        if audio.ptt == Some(true) {
            "🎤 Voice message"
        } else {
            "🎵 Audio"
        }
        .to_owned()
    } else if let Some(document) = base.document_message.as_option() {
        format!(
            "📄 {}",
            document
                .file_name
                .clone()
                .unwrap_or_else(|| "Document".into())
        )
    } else if base.sticker_message.is_set() {
        "Sticker".to_owned()
    } else if base.location_message.is_set() || base.live_location_message.is_set() {
        "📍 Location".to_owned()
    } else if base.contact_message.is_set() || base.contacts_array_message.is_set() {
        "👤 Contact".to_owned()
    } else if let Some(poll) = base
        .poll_creation_message
        .as_option()
        .or(base.poll_creation_message_v2.as_option())
        .or(base.poll_creation_message_v3.as_option())
    {
        format!("📊 {}", poll.name.clone().unwrap_or_default())
    } else {
        return None;
    };
    Some(Body::Other(label))
}

/// The reply context of the message's content, if it quotes one.
fn context_of(base: &wa::Message) -> Option<&wa::ContextInfo> {
    [
        base.extended_text_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        base.image_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        base.video_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        base.audio_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        base.document_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
        base.sticker_message
            .as_option()
            .and_then(|m| m.context_info.as_option()),
    ]
    .into_iter()
    .flatten()
    .next()
}

/// A history chunk reduced to what the archive files.
struct Parsed {
    conversations: Vec<Conversation>,
    names: Vec<(String, String)>,
    lids: Vec<(String, String)>,
}

struct Conversation {
    id: String,
    name: Option<String>,
    last: i64,
    /// When the chat was pinned, 0 when it is not.
    pinned: i64,
    messages: Vec<Entry>,
}

struct Entry {
    id: String,
    sender: Option<String>,
    from_me: bool,
    ts: i64,
    raw: Vec<u8>,
}

fn parse_history(compressed: &[u8]) -> Result<Parsed, String> {
    let mut stream = HistorySyncStream::new(compressed, MAX_DECOMPRESSED);
    let mut conversations = Vec::new();
    let mut lids = Vec::new();
    while let Some(conversation) = stream.next_conversation().map_err(|e| e.to_string())? {
        if let (Some(lid), Some(pn)) = (&conversation.lid_jid, &conversation.pn_jid) {
            lids.push((lid.clone(), pn.clone()));
        }
        let messages: Vec<Entry> = conversation
            .messages
            .iter()
            .filter_map(|entry| {
                let info = entry.message.as_option()?;
                let key = info.key.as_option()?;
                let message = info.message.as_option()?;
                Some(Entry {
                    id: key.id.clone().filter(|id| !id.is_empty())?,
                    sender: info
                        .participant
                        .clone()
                        .or_else(|| key.participant.clone())
                        .filter(|s| !s.is_empty()),
                    from_me: key.from_me.unwrap_or(false),
                    ts: info.message_timestamp.unwrap_or(0) as i64,
                    raw: message.encode_to_vec(),
                })
            })
            .collect();
        let last = messages
            .iter()
            .map(|m| m.ts)
            .max()
            .unwrap_or(conversation.conversation_timestamp.unwrap_or(0) as i64);
        conversations.push(Conversation {
            id: conversation.id.clone(),
            name: conversation
                .name
                .clone()
                .or_else(|| conversation.display_name.clone()),
            last,
            pinned: conversation.pinned.unwrap_or(0) as i64,
            messages,
        });
    }
    let remainder = stream.remainder().map_err(|e| e.to_string())?;
    let names = remainder
        .pushnames
        .iter()
        .filter_map(|entry| Some((entry.id.clone()?, entry.pushname.clone()?)))
        .collect();
    lids.extend(
        remainder
            .phone_number_to_lid_mappings
            .iter()
            .filter_map(|entry| Some((entry.lid_jid.clone()?, entry.pn_jid.clone()?))),
    );
    Ok(Parsed {
        conversations,
        names,
        lids,
    })
}

fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension("part");
    std::fs::write(&partial, bytes)?;
    std::fs::rename(partial, path)
}

fn open(path: &Path) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

/// Copies to ~/Downloads under `name`, numbering it rather than overwriting.
fn save_to_downloads(path: &Path, name: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("no HOME"))?;
    let downloads = PathBuf::from(home).join("Downloads");
    std::fs::create_dir_all(&downloads)?;
    let name = Path::new(name);
    let stem = name.file_stem().unwrap_or_default().to_string_lossy();
    let extension = name
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut target = downloads.join(name);
    for n in 1.. {
        if !target.exists() {
            break;
        }
        target = downloads.join(format!("{stem} ({n}){extension}"));
    }
    std::fs::copy(path, &target)?;
    Ok(target)
}

fn sanitize(id: &str) -> String {
    id.chars().filter(char::is_ascii_alphanumeric).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
