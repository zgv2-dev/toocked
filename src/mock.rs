//! Sample data for `--mock`: the interface without an account.

use crate::backend::{Command, Event};
use crate::model::{Body, Chat, LinkStatus, Message, Quote, Reaction};

const NOW: i64 = 1_764_000_000;

pub fn start() -> Vec<Event> {
    let chats = [
        ("olena@s.whatsapp.net", "Олена", "Побачимось у Львові 🙂"),
        ("family@g.us", "Родина", "📷 Photo"),
        ("taras@s.whatsapp.net", "Тарас", "Sent the build"),
        ("work@g.us", "Rust crew", "ratatui 0.30 is out"),
    ];
    let chats = chats
        .iter()
        .enumerate()
        .map(|(i, (id, name, preview))| Chat {
            id: (*id).into(),
            name: (*name).into(),
            last: now() - i as i64 * 5000,
            preview: (*preview).into(),
        })
        .collect();
    vec![Event::Status(LinkStatus::Connected), Event::Chats(chats)]
}

pub fn handle(command: Command) -> Vec<Event> {
    match command {
        Command::Open(chat) => vec![Event::History {
            messages: history(&chat),
            chat,
        }],
        Command::Send {
            chat,
            text,
            reply_to,
        } => vec![Event::Message(Message {
            chat,
            id: format!("me-{}", now()),
            sender: String::new(),
            sender_name: String::new(),
            from_me: true,
            ts: now(),
            body: Body::Text(text),
            quote: reply_to.map(|id| Quote {
                id,
                sender: "…".into(),
                text: "quoted".into(),
            }),
            reactions: Vec::new(),
        })],
        Command::React { .. } | Command::Search(_) => Vec::new(),
        Command::Download { id, .. } => {
            let path = std::env::temp_dir().join("toocked-mock.png");
            let picture = image::RgbImage::from_fn(480, 320, |x, y| {
                image::Rgb([(x / 2) as u8, (y * 3 / 4) as u8, 200])
            });
            match picture.save(&path) {
                Ok(()) => vec![Event::Image { id, path }],
                Err(error) => vec![Event::Error(error.to_string())],
            }
        }
    }
}

fn history(chat: &str) -> Vec<Message> {
    let message = |i: i64, from_me: bool, sender: &str, body: Body| Message {
        chat: chat.into(),
        id: format!("{chat}-{i}"),
        sender: sender.into(),
        sender_name: sender.split('@').next().unwrap_or_default().into(),
        from_me,
        ts: now() - (20 - i) * 600,
        body,
        quote: None,
        reactions: Vec::new(),
    };
    let text = |s: &str| Body::Text(s.into());
    let mut messages = vec![
        message(1, false, "olena@s", text("Привіт! Як справи?")),
        message(
            2,
            true,
            "",
            text("Все добре, пишу термінальний клієнт для WhatsApp на Rust"),
        ),
        message(
            3,
            false,
            "taras@s",
            Body::Image {
                caption: Some("Карпати цього ранку".into()),
            },
        ),
        message(
            4,
            false,
            "olena@s",
            text(
                "Wow. A long message to check that wrapping works across several lines of a narrow pane without breaking words in odd places.",
            ),
        ),
        message(5, true, "", text("Побачимось у Львові 🙂")),
        message(6, false, "taras@s", Body::Other("[voice message]".into())),
    ];
    messages[3].reactions.push(Reaction {
        emoji: "😂".into(),
        mine: true,
    });
    messages[4].quote = Some(Quote {
        id: format!("{chat}-1"),
        sender: "olena".into(),
        text: "Привіт! Як справи?".into(),
    });
    messages
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(NOW, |d| d.as_secs() as i64)
}
