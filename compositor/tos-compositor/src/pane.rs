//! A pane: one terminal, one PTY, one child process.

use std::io;
use std::path::PathBuf;

use tos_pty::{Pty, PtyConfig, Winsize};
use tos_render::TextureCache;
use tos_session::Rect;
use tos_term::{Palette, Terminal, TerminalConfig};

use crate::selection::{Anchor, Selection};

/// A running pane.
pub struct Pane {
    pub terminal: Terminal,
    pub pty: Pty,
    pub title: String,
    /// The program the child was started on, already resolved against `$PATH`.
    /// The pane is the only thing that knows what it is running, and a
    /// launcher pane is only worth anything if that is the chosen program.
    pub program: PathBuf,
    /// Where the pane sits, in cells.
    pub area: Rect,
    /// Set once the child has exited and the terminal has drained.
    pub exited: bool,
    /// The selected text, anchored to the lines it was made over rather than
    /// to the rows those lines happened to be on.
    pub selection: Option<Selection>,
    /// Scaled image textures kept between frames. Rendering is stateless, so
    /// the cache has to live with the thing it belongs to, which is the pane
    /// whose images they are.
    pub textures: TextureCache,
    /// Whether the selection is still being made, rather than finished and
    /// sitting there. A button held down says so, and so does copy mode
    /// driving one from the keyboard; what the two have in common is a person
    /// watching the highlight, which is the only thing [`Pane::pump`] needs to
    /// know. Whether the pointer in particular is dragging is the
    /// compositor's mouse grab, not this.
    pub selection_in_progress: bool,
    /// Input the PTY could not take yet.
    pending_input: Vec<u8>,
    /// Set once queued input had to be dropped.
    input_overflowed: bool,
}

/// Input queued for a child that is not reading. Beyond this, the pane is
/// clearly not consuming anything and holding more would be a memory leak.
const MAX_PENDING_INPUT: usize = 4 * 1024 * 1024;

impl Pane {
    /// Start a child on a new PTY sized to `area`.
    pub fn spawn(
        area: Rect,
        cell_size: (u32, u32),
        scrollback: usize,
        palette: &Palette,
        command: Option<&[String]>,
    ) -> io::Result<Pane> {
        let winsize = winsize_for(area, cell_size);
        let mut config = PtyConfig::shell(winsize);
        if let Some(command) = command {
            let program = tos_pty::which(&command[0]).unwrap_or_else(|| command[0].clone().into());
            config.program = program;
            config.args = command[1..].to_vec();
        }
        let program = config.program.clone();
        let pty = Pty::spawn(&config)?;

        let mut terminal = Terminal::new(
            area.width.max(1) as usize,
            area.height.max(1) as usize,
            TerminalConfig {
                scrollback,
                cell_width: cell_size.0,
                cell_height: cell_size.1,
                ..TerminalConfig::default()
            },
        );
        terminal.set_palette(palette.clone());

        Ok(Pane {
            terminal,
            pty,
            title: String::new(),
            program,
            area,
            exited: false,
            selection: None,
            textures: TextureCache::default(),
            selection_in_progress: false,
            pending_input: Vec::new(),
            input_overflowed: false,
        })
    }

    /// Move or resize the pane, telling both the terminal and the child.
    pub fn set_area(&mut self, area: Rect, cell_size: (u32, u32)) {
        let resized = area.width != self.area.width || area.height != self.area.height;
        self.area = area;
        if !resized {
            return;
        }
        // Resizing moves text between the screen and history and, when the
        // width changes, between columns; the anchors would survive but would
        // no longer be over the text they were drawn around.
        self.clear_selection();
        self.terminal
            .resize(area.width.max(1) as usize, area.height.max(1) as usize);
        // The child only learns about the new size through the PTY.
        let _ = self.pty.resize(winsize_for(area, cell_size));
    }

    /// Read from the PTY into the terminal. Returns false at end of file.
    pub fn pump(&mut self, buf: &mut [u8]) -> bool {
        loop {
            match self.pty.read(buf) {
                Ok(0) => {
                    self.exited = true;
                    return false;
                }
                Ok(n) => {
                    self.terminal.advance(&buf[..n]);
                    // The program has drawn over the screen, so a selection
                    // left on it is describing text that may no longer be
                    // there. A selection still being made is the exception:
                    // somebody is watching that highlight — with the button
                    // down, or with copy mode's cursor on one end of it — and
                    // wiping it would be wiping work in progress.
                    if !self.selection_in_progress {
                        self.clear_selection();
                    }
                    // A short read means the PTY is drained for now.
                    if n < buf.len() {
                        return true;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(_) => {
                    self.exited = true;
                    return false;
                }
            }
        }
    }

    /// Queue bytes for the child.
    ///
    /// The PTY master is non-blocking and its input buffer is only a few
    /// kilobytes, so a large paste cannot be written in one go. Anything that
    /// does not fit is held and retried by [`Pane::flush_input`]; dropping it
    /// would silently truncate the paste, taking the bracketed paste
    /// terminator with it and leaving the application stuck in paste mode.
    pub fn write(&mut self, bytes: &[u8]) {
        if self.pending_input.is_empty() {
            let written = self.write_now(bytes);
            if written < bytes.len() {
                self.pending_input.extend_from_slice(&bytes[written..]);
            }
            return;
        }
        // Ordering matters: queued bytes go first.
        if self.pending_input.len() + bytes.len() > MAX_PENDING_INPUT {
            // The child is not reading at all. Dropping the oldest bytes is
            // the only option left, and it is worth being loud about it.
            self.input_overflowed = true;
            return;
        }
        self.pending_input.extend_from_slice(bytes);
    }

    /// Retry whatever the PTY could not take earlier.
    ///
    /// Returns true while bytes are still queued, so the caller knows to keep
    /// polling for writability.
    pub fn flush_input(&mut self) -> bool {
        if self.pending_input.is_empty() {
            return false;
        }
        let pending = std::mem::take(&mut self.pending_input);
        let written = self.write_now(&pending);
        if written < pending.len() {
            self.pending_input = pending[written..].to_vec();
        }
        !self.pending_input.is_empty()
    }

    /// Bytes waiting for the child to read them.
    pub fn pending_input(&self) -> usize {
        self.pending_input.len()
    }

    /// Whether input had to be discarded because the child stopped reading.
    pub fn input_overflowed(&self) -> bool {
        self.input_overflowed
    }

    /// Write as much as the PTY will take right now.
    fn write_now(&mut self, bytes: &[u8]) -> usize {
        let mut written = 0;
        while written < bytes.len() {
            match self.pty.write(&bytes[written..]) {
                Ok(0) => break,
                Ok(n) => written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.exited = true;
                    break;
                }
            }
        }
        written
    }

    /// Flush any replies the terminal generated, such as cursor reports.
    pub fn flush_responses(&mut self) {
        let output = self.terminal.take_output();
        if !output.is_empty() {
            self.write(&output);
        }
    }

    /// The text covered by the current selection.
    pub fn selected_text(&self) -> Option<String> {
        self.selection?.text(self.terminal.grid())
    }

    /// Where a displayed cell is in the text, which is what a selection
    /// remembers. The pointer is clamped into the pane, so a drag past an
    /// edge still lands somewhere real.
    pub fn anchor_at(&self, col: usize, row: usize) -> Anchor {
        let grid = self.terminal.grid();
        let row = row.min(grid.rows().saturating_sub(1));
        let col = col.min(grid.cols().saturating_sub(1));
        Anchor::new(grid.display_line(row), col)
    }

    /// Replace the selection, repainting the rows it covers.
    ///
    /// The highlight is the compositor's, not the terminal's, so nothing else
    /// marks those rows as needing another look.
    pub fn set_selection(&mut self, selection: Option<Selection>) {
        if self.selection == selection {
            return;
        }
        self.selection = selection;
        self.terminal.damage_mut().mark_all();
    }

    /// Drop the selection, if there is one.
    pub fn clear_selection(&mut self) {
        self.set_selection(None);
    }

    /// The selection in the rows it is currently drawn on, for the renderer.
    pub fn display_selection(&self) -> Option<tos_render::Selection> {
        self.selection?.display(self.terminal.grid())
    }
}

/// Convert a pane's cell rectangle into the size a child process sees.
pub fn winsize_for(area: Rect, cell_size: (u32, u32)) -> Winsize {
    Winsize::new(
        area.width.max(1) as u16,
        area.height.max(1) as u16,
        (area.width * cell_size.0).min(u16::MAX as u32) as u16,
        (area.height * cell_size.1).min(u16::MAX as u32) as u16,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winsize_reports_cells_and_pixels() {
        let ws = winsize_for(Rect::new(0, 0, 80, 24), (8, 16));
        assert_eq!((ws.cols, ws.rows), (80, 24));
        assert_eq!((ws.width_px, ws.height_px), (640, 384));
    }

    #[test]
    fn a_zero_sized_pane_still_reports_one_cell() {
        let ws = winsize_for(Rect::new(0, 0, 0, 0), (8, 16));
        assert_eq!((ws.cols, ws.rows), (1, 1));
    }
}
