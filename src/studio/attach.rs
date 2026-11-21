//! Images in the Agent panel's prompt: pasted (⌘V with an image on the
//! clipboard) or dropped on the panel. Each is kept as a PNG beside the
//! chats (no more than 2048 px on a side), shown as a thumbnail above the
//! prompt until it's sent, then in the chat, and given to the agent with
//! the prompt (and its file named, so `trace_image` can trace it).

use crate::app::Studio;
use eframe::egui::{self, Vec2};
use std::path::{Path, PathBuf};
use zigzag::trace::Image;

/// The longest side an image is given to the agent at.
const MOST: f32 = 2048.0;
/// A thumbnail above the prompt: its longest side (points).
pub const THUMB: f32 = 44.0;
/// The biggest an image is shown (in the chat), points: its texture is
/// made for that, at twice the pixels.
const SHOWN: f32 = 160.0;

/// `img` as PNG bytes.
fn png_of(img: &egui::ColorImage) -> Option<Vec<u8>> {
    let [w, h] = img.size;
    let rgba: Vec<u8> = img
        .pixels
        .iter()
        .flat_map(|c| c.to_srgba_unmultiplied())
        .collect();
    let r = vectorcraft_trace::Raster::new(w as u32, h as u32, rgba).ok()?;
    Some(r.encode_png())
}

/// An image file's bytes (any format tracing reads) as a PNG no bigger
/// than [`MOST`] on a side.
pub fn as_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let img = Image::decode(bytes).ok()?;
    png_of(&crate::trace_ui::thumbnail(&img, Vec2::splat(MOST / 2.0)))
}

/// Kept image `path`'s thumbnail, made once and kept in egui's memory.
pub fn thumb(ctx: &egui::Context, path: &Path) -> Option<egui::TextureHandle> {
    let id = egui::Id::unique(("agent-image", path));
    if let Some(t) = ctx.data(|d| d.get_temp::<Option<egui::TextureHandle>>(id)) {
        return t;
    }
    let t = std::fs::read(path)
        .ok()
        .and_then(|b| Image::decode(&b).ok())
        .map(|img| {
            ctx.load_texture(
                format!("agent-image-{}", path.display()),
                crate::trace_ui::thumbnail(&img, Vec2::splat(SHOWN)),
                egui::TextureOptions::LINEAR,
            )
        });
    ctx.data_mut(|d| d.insert_temp(id, t.clone()));
    t
}

/// Draw kept image `path` as a thumbnail `side` points at most.
pub fn show(ui: &mut egui::Ui, path: &Path, side: f32) -> egui::Response {
    match thumb(ui.ctx(), path) {
        Some(t) => {
            let s = t.size_vec2();
            let k = side / s.x.max(s.y).max(1.0);
            ui.add(
                egui::Image::new(&t)
                    .fit_to_exact_size(s * k)
                    .corner_radius(6),
            )
        }
        None => ui.label("(image)"),
    }
}

impl Studio {
    /// Where pasted and dropped images are kept: beside the chats (or, with
    /// none kept, in a folder of the system's for passing things on).
    pub(crate) fn images_dir(&self) -> PathBuf {
        self.agent
            .store
            .dir()
            // (Chats not kept: nor are their pictures.)
            .filter(|_| self.prefs.agent_history)
            .map(|d| d.join("images"))
            .unwrap_or_else(|| std::env::temp_dir().join("zigzag-agent-images"))
    }

    /// Attach an image to the prompt. It's read, made a PNG of a size to
    /// send and written beside the chats off this thread (a big scan
    /// takes a moment), and is with the prompt once that's done: see
    /// [`Studio::take_attached`].
    fn attach(&mut self, from: From) {
        let dir = self.images_dir();
        let make = move || -> Result<PathBuf, String> {
            let (png, what) = match from {
                From::File(p) => {
                    let what = p.display().to_string();
                    let bytes = std::fs::read(&p).map_err(|e| format!("{what}: {e}"))?;
                    (as_png(&bytes), what)
                }
                From::Pasted(img) => (png_of(&img).and_then(|b| as_png(&b)), "that".to_string()),
            };
            let png = png.ok_or(format!(
                "Couldn't attach {what}: it isn't an image that can be read."
            ))?;
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("{}.png", crate::history::new_id()));
            std::fs::write(&path, png).map_err(|e| format!("Couldn't attach {what}: {e}"))?;
            Ok(path)
        };
        let (tx, rx) = std::sync::mpsc::channel();
        if cfg!(test) {
            // (In step with the test.)
            let _ = tx.send(make());
        } else {
            std::thread::spawn(move || {
                let _ = tx.send(make());
            });
        }
        self.agent.attaching.push((self.session.rec, rx));
        self.agent.focus = true;
        self.take_attached();
    }

    /// The images that have been made ready since last frame join their
    /// prompts (each the prompt of the document it was attached in).
    /// True while some are still being made.
    pub(crate) fn take_attached(&mut self) -> bool {
        let mut waiting = Vec::new();
        for (rec, rx) in std::mem::take(&mut self.agent.attaching) {
            match rx.try_recv() {
                Ok(Ok(path)) => {
                    if rec == self.session.rec {
                        self.agent.attach.push(path);
                    } else if let Some(i) = self.tab_of(rec) {
                        self.tabs.list[i].chat.attach_later(path);
                    } else {
                        // (Its document was closed meanwhile.)
                        let _ = std::fs::remove_file(path);
                    }
                }
                Ok(Err(why)) => self.status = why,
                Err(std::sync::mpsc::TryRecvError::Empty) => waiting.push((rec, rx)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }
        self.agent.attaching = waiting;
        !self.agent.attaching.is_empty()
    }

    /// Attach a pasted image.
    pub(crate) fn attach_pasted(&mut self, img: &egui::ColorImage) -> bool {
        self.attach(From::Pasted(img.clone()));
        true
    }

    /// Attach an image file dropped on the panel.
    pub(crate) fn attach_file(&mut self, path: &Path) -> bool {
        self.attach(From::File(path.to_path_buf()));
        true
    }
}

/// Where an image being attached comes from.
enum From {
    File(PathBuf),
    Pasted(egui::ColorImage),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_image_is_kept_as_a_png_no_bigger_than_its_limit() {
        let big = egui::ColorImage::from_rgba_unmultiplied(
            [3000, 1000],
            &[200, 30, 40, 255].repeat(3000 * 1000),
        );
        let png = png_of(&big).unwrap();
        let small = as_png(&png).unwrap();
        let back = Image::decode(&small).unwrap();
        let (w, h) = back.size();
        assert!(w <= MOST as u32 && h <= MOST as u32, "{w}×{h}");
        assert!((w as f32 / h as f32 - 3.0).abs() < 0.05, "same shape");
        assert!(as_png(b"not an image").is_none());
    }
}
