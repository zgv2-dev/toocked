//! The runtime side: owns the WhatsApp client and the archive.
//!
//! A slimmed-down take on zapfast's worker: link by QR, file history and live
//! messages, send text, replies and reactions, fetch images.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use whatsapp_rust::pair::NATIVE_CAMERA_DEEP_LINK_PREFIX;
use whatsapp_rust::prelude::{Bot, BotHandle, Client, Jid, MessageBuilderExt, MessageExt, wa};
use whatsapp_rust::store::SqliteStore;
use whatsapp_rust::types::events::{Event as Wa, LazyHistorySync};
use whatsapp_rust::types::message::MessageInfo;
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
    History(Parsed),
    Failed(String),
}

pub async fn run(dir: PathBuf, mut inbox: UnboundedReceiver<Command>, outbox: mpsc::Sender<Event>) {
    let media = dir.join("media");
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
            Wa::LoggedOut(_) => {
                // The phone unlinked us: forget the device and offer a new QR.
                self.emit(Event::Status(LinkStatus::LoggedOut));
                self.client = None;
                if let Some(handle) = self.handle.take() {
                    let _ = handle.shutdown().await;
                }
                for suffix in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{suffix}", self.session.display()));
                }
                self.start().await;
            }
            Wa::Messages(batch) => {
                for inbound in batch.messages.iter() {
                    self.ingest(&inbound.message, &inbound.info);
                }
            }
            Wa::HistorySync(lazy) => self.history(lazy),
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
            Command::React { chat, id, emoji } => self.react(chat, id, emoji),
            Command::Download { chat, id } => self.download(chat, id),
        }
    }

    fn send(&self, chat: ChatId, text: String, reply_to: Option<String>) {
        let (Some(client), Ok(jid)) = (self.client.clone(), chat.parse::<Jid>()) else {
            return self.emit(Event::Error("Not connected".into()));
        };
        let mut quote = None;
        let message =
            match reply_to.and_then(|id| Some((self.archive.message(&chat, &id).ok()??, id))) {
                Some((quoted, id)) => {
                    let raw = self
                        .archive
                        .raw(&chat, &id)
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                    let body = wa::Message::decode_from_slice(&raw).unwrap_or_default();
                    let author = match quoted.from_me {
                        true => client.pn().map(|pn| pn.to_non_ad()),
                        false => quoted.sender.parse::<Jid>().ok(),
                    };
                    quote = Some(Quote {
                        id: id.clone(),
                        sender: if quoted.from_me {
                            "You".into()
                        } else {
                            quoted.sender_name.clone()
                        },
                        text: quoted.body.summary(),
                    });
                    match author {
                        Some(author) => wa::Message::text_with_context(
                            text.clone(),
                            build_quote_context_with_info(id, &author, &jid, &jid, &body),
                        ),
                        None => wa::Message::text(text.clone()),
                    }
                }
                None => wa::Message::text(text.clone()),
            };
        let done = self.done.clone();
        tokio::spawn(async move {
            let raw = message.encode_to_vec();
            let _ = done.send(match client.send(SendRequest::new(&jid, message)).await {
                Ok(result) => Done::Sent {
                    message: Message {
                        chat,
                        id: result.message_id.to_string(),
                        sender: String::new(),
                        sender_name: String::new(),
                        from_me: true,
                        ts: now(),
                        body: Body::Text(text),
                        quote,
                        reactions: Vec::new(),
                    },
                    raw,
                },
                Err(error) => Done::Failed(format!("Not sent: {error}")),
            });
        });
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

    fn download(&self, chat: ChatId, id: String) {
        let path = self.media.join(format!("{}.img", sanitize(&id)));
        if path.exists() {
            return self.emit(Event::Image { id, path });
        }
        let (Some(client), Ok(Some(raw))) = (self.client.clone(), self.archive.raw(&chat, &id))
        else {
            return;
        };
        let outbox = self.outbox.clone();
        tokio::spawn(async move {
            let Ok(message) = wa::Message::decode_from_slice(&raw) else {
                return;
            };
            let Some(image) = message.get_base_message().image_message.as_option() else {
                return;
            };
            match client.download(image, Default::default()).await {
                Ok(bytes) => match write(&path, &bytes) {
                    Ok(()) => {
                        let _ = outbox.send(Event::Image { id, path });
                    }
                    Err(error) => {
                        let _ = outbox.send(Event::Error(error.to_string()));
                    }
                },
                Err(error) => {
                    let _ = outbox.send(Event::Error(format!("Image: {error}")));
                }
            }
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
            Done::History(parsed) => self.file_history(parsed),
            Done::Failed(error) => self.emit(Event::Error(error)),
        }
    }
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

fn sanitize(id: &str) -> String {
    id.chars().filter(char::is_ascii_alphanumeric).collect()
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
