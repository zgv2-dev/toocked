//! toocked: a minimal terminal WhatsApp client.

mod app;
mod archive;
mod backend;
mod kitty;
mod mock;
mod model;
mod ui;

use std::path::PathBuf;
use std::time::Duration;

use ratatui::crossterm::event::{self, Event as Input, KeyEventKind};

use crate::app::App;
use crate::backend::Backend;

fn main() -> std::io::Result<()> {
    let mock = std::env::args().any(|arg| arg == "--mock");
    let backend = (!mock).then(|| Backend::spawn(data_dir()));
    let mut app = App::new();
    if mock {
        mock::start().into_iter().for_each(|event| app.apply(event));
    }
    let mut kitty = kitty::Kitty::detect();
    app.inline_images = kitty.enabled;
    let mut terminal = ratatui::init();
    let result = (|| -> std::io::Result<()> {
        while !app.quit {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            kitty.draw(&mut std::io::stdout(), &app.placements, &app.images)?;
            if event::poll(Duration::from_millis(100))? {
                match event::read()? {
                    Input::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
                    Input::Resize(..) => kitty.invalidate(),
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
    ratatui::restore();
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
