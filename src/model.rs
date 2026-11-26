//! Plain data shared by the backend and the interface.

pub type ChatId = String;

#[derive(Clone, Debug, Default)]
pub struct Chat {
    pub id: ChatId,
    pub name: String,
    /// Unix seconds of the newest message.
    pub last: i64,
    pub preview: String,
}

#[derive(Clone, Debug)]
pub enum Body {
    Text(String),
    Image { caption: Option<String> },
    /// Anything this client does not show, labelled ("[video]", "[sticker]").
    Other(String),
}

impl Body {
    pub fn summary(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Image { caption } => match caption {
                Some(caption) => format!("📷 {caption}"),
                None => "📷 Photo".into(),
            },
            Self::Other(label) => label.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Quote {
    pub id: String,
    pub sender: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Reaction {
    pub sender: String,
    pub emoji: String,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub chat: ChatId,
    pub id: String,
    /// Sender JID; empty for our own messages.
    pub sender: String,
    pub sender_name: String,
    pub from_me: bool,
    /// Unix seconds.
    pub ts: i64,
    pub body: Body,
    pub quote: Option<Quote>,
    pub reactions: Vec<Reaction>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LinkStatus {
    Starting,
    /// Waiting for the phone to scan this QR payload.
    Qr(String),
    Connecting,
    Connected,
    Disconnected(String),
    LoggedOut,
    Failed(String),
}
