//! Inline images through the kitty graphics protocol (Ghostty, kitty, WezTerm).
//!
//! Each picture is sent once under its own id, then placed over the blank rows
//! the layout left for it after every frame whose placements changed.

use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use base64::Engine;
use ratatui::layout::Rect;

/// Longest side of a picture sent to the terminal, in pixels.
const MAX_SIDE: u32 = 1024;

pub struct Kitty {
    pub enabled: bool,
    /// Message id to terminal image id and pixel size.
    sent: HashMap<String, (u32, u32, u32)>,
    shown: Vec<(String, Rect)>,
}

impl Kitty {
    pub fn detect() -> Self {
        let env = |name: &str| std::env::var(name).unwrap_or_default();
        let enabled = match env("TOOCKED_IMAGES").as_str() {
            "0" => false,
            "1" => true,
            _ => {
                env("TERM_PROGRAM") == "ghostty"
                    || env("TERM").contains("kitty")
                    || env("TERM").contains("ghostty")
                    || !env("KITTY_WINDOW_ID").is_empty()
            }
        };
        Self {
            enabled,
            sent: HashMap::new(),
            shown: Vec::new(),
        }
    }

    /// Forget what is on screen, after the terminal cleared it.
    pub fn invalidate(&mut self) {
        self.shown.clear();
    }

    pub fn draw(
        &mut self,
        out: &mut impl Write,
        placements: &[(String, Rect)],
        images: &HashMap<String, PathBuf>,
    ) -> io::Result<Vec<String>> {
        let mut failed = Vec::new();
        let wanted: Vec<(String, Rect)> = placements
            .iter()
            .filter(|(id, _)| images.contains_key(id))
            .cloned()
            .collect();
        if !self.enabled || wanted == self.shown {
            return Ok(failed);
        }
        // Lowercase `a`: remove placements, keep the pictures for reuse.
        write!(out, "\x1b_Ga=d,d=a,q=2\x1b\\")?;
        let cell = cell_size();
        for (id, rect) in &wanted {
            if !self.sent.contains_key(id) && self.send(out, id, &images[id]).is_err() {
                failed.push(id.clone());
                continue;
            }
            let (image, width, height) = self.sent[id];
            let (cols, rows) = fit(width, height, *rect, cell);
            write!(
                out,
                "\x1b7\x1b[{};{}H\x1b_Ga=p,i={image},c={cols},r={rows},C=1,q=2\x1b\\\x1b8",
                rect.y + 1,
                rect.x + 1,
            )?;
        }
        out.flush()?;
        self.shown = wanted;
        Ok(failed)
    }

    fn send(&mut self, out: &mut impl Write, id: &str, path: &Path) -> io::Result<()> {
        // Downloads have no extension, so the format comes from the bytes.
        let picture = image::ImageReader::open(path)?
            .with_guessed_format()?
            .decode()
            .map_err(io::Error::other)?;
        let picture = if picture.width().max(picture.height()) > MAX_SIDE {
            picture.thumbnail(MAX_SIDE, MAX_SIDE)
        } else {
            picture
        };
        let mut png = Vec::new();
        picture
            .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
            .map_err(io::Error::other)?;
        let data = base64::engine::general_purpose::STANDARD.encode(png);
        let image = self.sent.len() as u32 + 1;
        let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            let more = u8::from(index + 1 < chunks.len());
            let chunk = std::str::from_utf8(chunk).unwrap_or_default();
            if index == 0 {
                write!(out, "\x1b_Ga=t,f=100,i={image},q=2,m={more};{chunk}\x1b\\")?;
            } else {
                write!(out, "\x1b_Gm={more};{chunk}\x1b\\")?;
            }
        }
        self.sent
            .insert(id.to_owned(), (image, picture.width(), picture.height()));
        Ok(())
    }
}

/// A cell's size in pixels, guessing 1:2 when the terminal does not say.
fn cell_size() -> (f32, f32) {
    match ratatui::crossterm::terminal::window_size() {
        Ok(size) if size.width > 0 && size.columns > 0 && size.rows > 0 => (
            size.width as f32 / size.columns as f32,
            size.height as f32 / size.rows as f32,
        ),
        _ => (8.0, 16.0),
    }
}

/// Columns and rows that show the picture whole, keeping its shape.
fn fit(width: u32, height: u32, rect: Rect, (cell_w, cell_h): (f32, f32)) -> (u16, u16) {
    let rows = rect.height as f32;
    let cols = rows * cell_h * width as f32 / (height.max(1) as f32 * cell_w);
    if cols <= rect.width as f32 {
        return ((cols.round() as u16).max(1), rect.height);
    }
    let rows = rect.width as f32 * cell_w * height as f32 / (width.max(1) as f32 * cell_h);
    (rect.width, (rows.round() as u16).max(1))
}
