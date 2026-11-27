//! toocked: a minimal terminal WhatsApp client.
// whatsapp-rust's app-state futures nest deeply; proving them Send needs the
// same headroom the library itself sets.
#![recursion_limit = "512"]

mod app;
mod archive;
mod backend;
mod mock;
mod model;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as Input, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};

use crate::app::App;
use crate::backend::Backend;

fn main() -> std::io::Result<()> {
    let mock = std::env::args().any(|arg| arg == "--mock");
    let backend = (!mock).then(|| Backend::spawn(data_dir()));
    let mut app = App::new();
    // The chat list keeps the width it was dragged to.
    let width_file = data_dir().join("sidebar");
    if let Some(width) = std::fs::read_to_string(&width_file)
        .ok()
        .and_then(|w| w.trim().parse().ok())
    {
        app.resize(width);
    }
    let width = app.sidebar;
    if mock {
        mock::start().into_iter().for_each(|event| app.apply(event));
    }
    let mut terminal = ratatui::init();
    // Drops and pastes arrive whole, so a dropped picture can be attached.
    ratatui::crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)?;
    // Lets Ghostty and kitty report Cmd as a modifier, so Cmd+S reaches us.
    let enhanced = ratatui::crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
        && ratatui::crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
    let result = (|| -> std::io::Result<()> {
        while !app.quit {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Input::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
                    Input::Paste(text) => app.paste(&text),
                    Input::Mouse(mouse) => app.mouse(mouse),
                    _ => {}
                }
            }
            for command in std::mem::take(&mut app.outbox) {
                match &backend {
                    Some(backend) => backend.send(command),
                    None => mock::handle(command)
                        .into_iter()
                        .for_each(|event| app.apply(event)),
                }
            }
            if let Some(backend) = &backend {
                while let Ok(event) = backend.events.try_recv() {
                    app.apply(event);
                }
            }
        }
        Ok(())
    })();
    if enhanced {
        let _ = ratatui::crossterm::execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
    }
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture
    );
    ratatui::restore();
    if app.sidebar != width {
        let _ = std::fs::create_dir_all(data_dir());
        let _ = std::fs::write(width_file, app.sidebar.to_string());
    }
    result
}

/// `$XDG_DATA_HOME/toocked`, else `~/.local/share/toocked`.
fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("toocked")
}
