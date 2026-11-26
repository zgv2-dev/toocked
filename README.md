# toocked

A minimal terminal WhatsApp client in Rust and [Ratatui](https://ratatui.rs).

Built on [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust), with the
backend design (a tokio worker behind `Command`/`Event` channels) taken from
[zapfast](https://github.com/crmne/zapfast) by Carmine Paolino, MIT licensed.

## Features

- Link by QR code
- Chat list and history (synced from the phone, kept in a local SQLite archive)
- Send text, reply, react
- Search chats and messages
- Inline images through the kitty graphics protocol (Ghostty, kitty, WezTerm)

## Run

```sh
cargo run --release
cargo run -- --mock    # sample data, no account
```

Data lives in `$XDG_DATA_HOME/toocked` (`~/.local/share/toocked`): `session.db`
is the linked device, `archive.db` the messages, `media/` downloaded images.
Delete the folder to unlink and start over.

Images are on when the terminal looks like Ghostty or kitty; force them with
`TOOCKED_IMAGES=1` or turn them off with `TOOCKED_IMAGES=0`.

## Keys

| Where    | Keys |
|----------|------|
| Chats    | `j`/`k` move, `enter` open, `/` search, `q` quit |
| Messages | `j`/`k` select, `r` reply, `e` react, `i` write, `esc` back |
| Compose  | `enter` send, `alt+enter` newline, `esc` messages / cancel reply |
| React    | `1`–`6` quick reactions, or type an emoji and `enter`; `⌫` removes yours |
| Search   | type, `↑`/`↓` pick, `enter` open |

`ctrl+c` quits anywhere.
