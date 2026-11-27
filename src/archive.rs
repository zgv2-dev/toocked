//! Local message store: chats, messages, reactions and privacy-id pairs.
//!
//! A much smaller cousin of zapfast's SQLCipher archive. Messages keep their
//! raw protobuf so replies can quote them and images can be downloaded later.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::model::{Body, Chat, Message, Quote, Reaction};

/// Messages a chat opens with.
pub const PAGE: i64 = 200;

pub struct Archive {
    db: Connection,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS chats (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL DEFAULT '',
    last INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS messages (
    chat TEXT NOT NULL,
    id TEXT NOT NULL,
    sender TEXT NOT NULL,
    sender_name TEXT NOT NULL,
    from_me INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    kind TEXT NOT NULL,
    text TEXT NOT NULL,
    quote_id TEXT,
    quote_sender TEXT,
    quote_text TEXT,
    raw BLOB,
    PRIMARY KEY (chat, id)
);
CREATE INDEX IF NOT EXISTS messages_by_time ON messages (chat, ts);
CREATE TABLE IF NOT EXISTS reactions (
    chat TEXT NOT NULL,
    message TEXT NOT NULL,
    sender TEXT NOT NULL,
    emoji TEXT NOT NULL,
    PRIMARY KEY (chat, message, sender)
);
CREATE TABLE IF NOT EXISTS names (
    jid TEXT PRIMARY KEY,
    name TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS lids (
    lid TEXT PRIMARY KEY,
    pn TEXT NOT NULL
);
";

impl Archive {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open(path)?;
        db.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
        db.execute_batch(SCHEMA)?;
        // Archives from before pinning gain the column; newer ones have it.
        let _ = db.execute(
            "ALTER TABLE chats ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(Self { db })
    }

    pub fn begin(&self) {
        let _ = self.db.execute_batch("BEGIN");
    }

    pub fn commit(&self) {
        let _ = self.db.execute_batch("COMMIT");
    }

    /// Creates the chat or moves its activity forward; a name replaces an
    /// empty one only.
    pub fn touch_chat(&self, id: &str, name: Option<&str>, ts: i64) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO chats (id, name, last) VALUES (?1, coalesce(?2, ''), ?3)
             ON CONFLICT (id) DO UPDATE SET
                 last = max(last, excluded.last),
                 name = CASE WHEN ?2 IS NOT NULL AND ?2 != '' THEN ?2 ELSE name END",
            params![id, name, ts],
        )?;
        Ok(())
    }

    pub fn chats(&self) -> rusqlite::Result<Vec<Chat>> {
        let mut statement = self.db.prepare(
            "SELECT c.id, coalesce(nullif(c.name, ''), n.name, ''), c.last,
                    (SELECT kind || char(31) || text FROM messages m
                     WHERE m.chat = c.id ORDER BY ts DESC LIMIT 1),
                    c.pinned != 0
             FROM chats c LEFT JOIN names n ON n.jid = c.id
             ORDER BY c.pinned != 0 DESC, c.last DESC",
        )?;
        statement
            .query_map([], |row| {
                let id: String = row.get(0)?;
                let name: String = row.get(1)?;
                let preview = row
                    .get::<_, Option<String>>(3)?
                    .and_then(|joined| {
                        let (kind, text) = joined.split_once('\u{1f}')?;
                        Some(body(kind, text.to_owned()).summary())
                    })
                    .unwrap_or_default();
                Ok(Chat {
                    name: if name.is_empty() {
                        fallback_name(&id)
                    } else {
                        name
                    },
                    id,
                    last: row.get(2)?,
                    preview,
                    pinned: row.get(4)?,
                })
            })?
            .collect()
    }

    /// Pins at `since` (Unix seconds), or unpins with 0.
    pub fn set_pinned(&self, id: &str, since: i64) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO chats (id, pinned) VALUES (?1, ?2)
             ON CONFLICT (id) DO UPDATE SET pinned = excluded.pinned",
            params![id, since],
        )?;
        Ok(())
    }

    /// Removes a message and its reactions.
    pub fn delete(&self, chat: &str, id: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "DELETE FROM messages WHERE chat = ?1 AND id = ?2",
            params![chat, id],
        )?;
        self.db.execute(
            "DELETE FROM reactions WHERE chat = ?1 AND message = ?2",
            params![chat, id],
        )?;
        Ok(())
    }

    /// Forgets everything, for a device that was logged out.
    pub fn clear(&self) -> rusqlite::Result<()> {
        self.db.execute_batch(
            "BEGIN;
             DELETE FROM messages; DELETE FROM reactions; DELETE FROM chats;
             DELETE FROM names; DELETE FROM lids;
             COMMIT;",
        )
    }

    pub fn has_chat(&self, id: &str) -> bool {
        self.db
            .query_row("SELECT 1 FROM chats WHERE id = ?1", [id], |_| Ok(()))
            .optional()
            .is_ok_and(|row| row.is_some())
    }

    pub fn set_name(&self, jid: &str, name: &str) -> rusqlite::Result<()> {
        if name.is_empty() {
            return Ok(());
        }
        self.db.execute(
            "INSERT INTO names (jid, name) VALUES (?1, ?2)
             ON CONFLICT (jid) DO UPDATE SET name = excluded.name",
            params![jid, name],
        )?;
        Ok(())
    }

    pub fn name(&self, jid: &str) -> Option<String> {
        self.db
            .query_row("SELECT name FROM names WHERE jid = ?1", [jid], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten()
    }

    pub fn learn_lid(&self, lid: &str, pn: &str) -> rusqlite::Result<()> {
        self.db.execute(
            "INSERT INTO lids (lid, pn) VALUES (?1, ?2)
             ON CONFLICT (lid) DO UPDATE SET pn = excluded.pn",
            params![lid, pn],
        )?;
        Ok(())
    }

    pub fn lids(&self) -> rusqlite::Result<Vec<(String, String)>> {
        let mut statement = self.db.prepare("SELECT lid, pn FROM lids")?;
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub fn put(&self, message: &Message, raw: Option<&[u8]>) -> rusqlite::Result<()> {
        let (kind, text) = match &message.body {
            Body::Text(text) => ("text", text.clone()),
            Body::Image { caption } => ("image", caption.clone().unwrap_or_default()),
            Body::Other(label) => ("other", label.clone()),
        };
        let quote = message.quote.as_ref();
        self.db.execute(
            "INSERT INTO messages
                 (chat, id, sender, sender_name, from_me, ts, kind, text,
                  quote_id, quote_sender, quote_text, raw)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT (chat, id) DO UPDATE SET
                 kind = excluded.kind, text = excluded.text,
                 raw = coalesce(excluded.raw, raw)",
            params![
                message.chat,
                message.id,
                message.sender,
                message.sender_name,
                message.from_me,
                message.ts,
                kind,
                text,
                quote.map(|q| &q.id),
                quote.map(|q| &q.sender),
                quote.map(|q| &q.text),
                raw,
            ],
        )?;
        self.touch_chat(&message.chat, None, message.ts)
    }

    /// An empty emoji removes the sender's reaction.
    pub fn react(
        &self,
        chat: &str,
        message: &str,
        sender: &str,
        emoji: &str,
    ) -> rusqlite::Result<()> {
        if emoji.is_empty() {
            self.db.execute(
                "DELETE FROM reactions WHERE chat = ?1 AND message = ?2 AND sender = ?3",
                params![chat, message, sender],
            )?;
        } else {
            self.db.execute(
                "INSERT INTO reactions (chat, message, sender, emoji) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (chat, message, sender) DO UPDATE SET emoji = excluded.emoji",
                params![chat, message, sender, emoji],
            )?;
        }
        Ok(())
    }

    pub fn message(&self, chat: &str, id: &str) -> rusqlite::Result<Option<Message>> {
        let mut message = self
            .db
            .query_row(
                &format!("{SELECT} WHERE chat = ?1 AND id = ?2"),
                params![chat, id],
                row_message,
            )
            .optional()?;
        if let Some(message) = message.as_mut() {
            message.reactions = self.reactions(chat, id)?;
        }
        Ok(message)
    }

    pub fn raw(&self, chat: &str, id: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        Ok(self
            .db
            .query_row(
                "SELECT raw FROM messages WHERE chat = ?1 AND id = ?2",
                params![chat, id],
                |row| row.get::<_, Option<Vec<u8>>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The newest page of a chat, oldest first.
    pub fn history(&self, chat: &str) -> rusqlite::Result<Vec<Message>> {
        let mut statement = self.db.prepare(&format!(
            "SELECT * FROM ({SELECT} WHERE chat = ?1 ORDER BY ts DESC LIMIT ?2) ORDER BY ts"
        ))?;
        let mut messages: Vec<Message> = statement
            .query_map(params![chat, PAGE], row_message)?
            .collect::<rusqlite::Result<_>>()?;
        for message in &mut messages {
            message.reactions = self.reactions(chat, &message.id)?;
        }
        Ok(messages)
    }

    pub fn search(&self, needle: &str) -> rusqlite::Result<Vec<Message>> {
        let pattern = format!(
            "%{}%",
            needle
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut statement = self.db.prepare(&format!(
            "{SELECT} WHERE text LIKE ?1 ESCAPE '\\' ORDER BY ts DESC LIMIT 100"
        ))?;
        statement.query_map([pattern], row_message)?.collect()
    }

    fn reactions(&self, chat: &str, message: &str) -> rusqlite::Result<Vec<Reaction>> {
        let mut statement = self.db.prepare_cached(
            "SELECT sender, emoji FROM reactions WHERE chat = ?1 AND message = ?2",
        )?;
        statement
            .query_map(params![chat, message], |row| {
                Ok(Reaction {
                    mine: row.get::<_, String>(0)? == "me",
                    emoji: row.get(1)?,
                })
            })?
            .collect()
    }
}

const SELECT: &str = "SELECT chat, id, sender,
    coalesce(nullif(sender_name, ''), (SELECT name FROM names WHERE jid = sender), ''),
    from_me, ts, kind, text, quote_id, quote_sender, quote_text FROM messages";

fn row_message(row: &Row) -> rusqlite::Result<Message> {
    let sender: String = row.get(2)?;
    let name: String = row.get(3)?;
    let quote_id: Option<String> = row.get(8)?;
    Ok(Message {
        chat: row.get(0)?,
        id: row.get(1)?,
        sender_name: if name.is_empty() {
            fallback_name(&sender)
        } else {
            name
        },
        sender,
        from_me: row.get(4)?,
        ts: row.get(5)?,
        body: body(&row.get::<_, String>(6)?, row.get(7)?),
        quote: match quote_id {
            Some(id) => Some(Quote {
                id,
                sender: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                text: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
            }),
            None => None,
        },
        reactions: Vec::new(),
    })
}

fn body(kind: &str, text: String) -> Body {
    match kind {
        "text" => Body::Text(text),
        "image" => Body::Image {
            caption: (!text.is_empty()).then_some(text),
        },
        _ => Body::Other(text),
    }
}

/// "+380…" for a phone number, the bare id otherwise.
pub fn fallback_name(jid: &str) -> String {
    match jid.split_once('@') {
        Some((user, "s.whatsapp.net")) => format!("+{user}"),
        Some((user, _)) => user.to_owned(),
        None => jid.to_owned(),
    }
}
