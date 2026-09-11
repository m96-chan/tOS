//! A pane: one terminal, one PTY, one child process.

use std::io;

use tos_pty::{Pty, PtyConfig, Winsize};
use tos_session::Rect;
use tos_term::{Terminal, TerminalConfig};

/// A running pane.
pub struct Pane {
    pub terminal: Terminal,
    pub pty: Pty,
    pub title: String,
    /// Where the pane sits, in cells.
    pub area: Rect,
    /// Set once the child has exited and the terminal has drained.
    pub exited: bool,
    /// A selection being dragged with the mouse, in displayed cells.
    pub selection: Option<tos_render::Selection>,
    /// Whether the mouse button is still down on this pane.
    pub selecting: bool,
}

impl Pane {
    /// Start a child on a new PTY sized to `area`.
    pub fn spawn(
        area: Rect,
        cell_size: (u32, u32),
        scrollback: usize,
        command: Option<&[String]>,
    ) -> io::Result<Pane> {
        let winsize = winsize_for(area, cell_size);
        let mut config = PtyConfig::shell(winsize);
        if let Some(command) = command {
            let program = tos_pty::which(&command[0]).unwrap_or_else(|| command[0].clone().into());
            config.program = program;
            config.args = command[1..].to_vec();
        }
        let pty = Pty::spawn(&config)?;

        let terminal = Terminal::new(
            area.width.max(1) as usize,
            area.height.max(1) as usize,
            TerminalConfig {
                scrollback,
                cell_width: cell_size.0,
                cell_height: cell_size.1,
                ..TerminalConfig::default()
            },
        );

        Ok(Pane {
            terminal,
            pty,
            title: String::new(),
            area,
            exited: false,
            selection: None,
            selecting: false,
        })
    }

    /// Move or resize the pane, telling both the terminal and the child.
    pub fn set_area(&mut self, area: Rect, cell_size: (u32, u32)) {
        let resized = area.width != self.area.width || area.height != self.area.height;
        self.area = area;
        if !resized {
            return;
        }
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

    /// Send bytes to the child, dropping them if the PTY is gone.
    pub fn write(&mut self, bytes: &[u8]) {
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
        let selection = self.selection?;
        let grid = self.terminal.grid();
        let mut out = String::new();
        let rows = grid.rows();
        for y in 0..rows {
            let row = grid.display_row(y);
            let mut line = String::new();
            for x in 0..row.len() {
                if !selection.contains(x, y) {
                    continue;
                }
                let cell = &row.cells()[x];
                if cell.attrs.flags.contains(tos_term::Flags::WIDE_SPACER) {
                    continue;
                }
                line.push(cell.ch);
                if let Some(marks) = &cell.zerowidth {
                    line.extend(marks.iter());
                }
            }
            let trimmed = line.trim_end();
            if !trimmed.is_empty() || !out.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(trimmed);
            }
        }
        (!out.is_empty()).then_some(out)
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
