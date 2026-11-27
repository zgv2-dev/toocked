# toocked

A minimal terminal WhatsApp client in Rust and [Ratatui](https://ratatui.rs).

Built on [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust), with the
backend design (a tokio worker behind `Command`/`Event` channels) taken from
[zapfast](https://github.com/crmne/zapfast) by Carmine Paolino, MIT licensed.

## Features

- Link by QR code
- Chat list and history (synced from the phone, kept in a local SQLite archive)
- Send text and pictures (drop or paste an image file), reply, react
- Delete messages for everyone or for you
- Pinned chats, synced with your phone
- Search chats and messages
- Attachments open in their default app; Cmd+S (or `s`) saves one to Downloads

## Run

```sh
cargo run --release
cargo run -- --mock    # sample data, no account
```

Data lives in `$XDG_DATA_HOME/toocked` (`~/.local/share/toocked`): `session.db`
is the linked device, `archive.db` the messages. Delete the folder to unlink
and start over (or press `L`); the same happens when your phone unlinks the
device. Opened attachments are fetched into the system temp folder.

## Keys

| Where    | Keys |
|----------|------|
| Chats    | `j`/`k` move, `ctrl+d`/`ctrl+u` half page, `ctrl+f`/`ctrl+b` or `PgDn`/`PgUp` page, `g`/`Home` top, `G`/`End` bottom, `enter` open, `p` pin/unpin, `[`/`]` narrower/wider, `L` log out, `/` search, `q` quit |
| Messages | `j`/`k` select, same paging and `g`/`G` keys, `enter` open attachment, `cmd+s`/`s` save it to Downloads, `r` reply, `e` react, `d` delete, `i` write, `esc` back |
| Compose  | `enter` send, `alt+enter` newline, `cmd+delete` clear, `ctrl+delete` delete word, `esc` messages / cancel reply or picture |
| Delete   | `e` for everyone (your messages), `m` for me, `esc` cancel |
| Log out  | `y` unlinks this device and deletes its local data, `esc` cancel |

Drag the chat list's right edge with the mouse to resize it; the width is
remembered. Hold Shift (Option in Ghostty) to select text with the mouse.
| React    | `1`–`6` quick reactions, or type an emoji and `enter`; `⌫` removes yours |
| Search   | type, `↑`/`↓` pick, `enter` open |

`ctrl+c` quits anywhere.
