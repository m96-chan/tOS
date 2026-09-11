//! Running tOS inside another terminal.
//!
//! This backend exists for development, not as part of the architecture: it
//! takes the same pixel framebuffer the DRM backend would present and encodes
//! it as half block characters, two pixels per host cell. It is the only way
//! to exercise the whole compositor on a machine that is not the target.

use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};

use tos_render::{OwnedFramebuffer, Surface};

use crate::display::Display;
use crate::tty::{terminal_size, RawMode};

/// Upper half block: the foreground paints the top pixel and the background
/// the bottom one, so one host cell carries two framebuffer rows.
const HALF_BLOCK: &str = "\u{2580}";

/// A display that lives inside a host terminal.
pub struct NestedDisplay {
    framebuffer: OwnedFramebuffer,
    /// Previous frame, so unchanged cells are not redrawn.
    previous: Vec<u32>,
    output: RawFd,
    input: RawFd,
    _raw: RawMode,
    cols: u16,
    rows: u16,
    force_redraw: bool,
}

impl NestedDisplay {
    /// Take over the terminal on `stdout`, reading input from `stdin`.
    pub fn acquire() -> io::Result<NestedDisplay> {
        let output = io::stdout().as_raw_fd();
        let input = io::stdin().as_raw_fd();
        let size = terminal_size(output)?;
        if size.cols == 0 || size.rows == 0 {
            return Err(io::Error::other(
                "the host terminal reported no size",
            ));
        }
        // Deliberately not O_NONBLOCK: stdin, stdout and the parent shell's
        // own descriptors usually share one open file description, and the
        // flag lives on that description, so setting it here would leave the
        // user's shell with a non-blocking terminal after tOS exits. Reads
        // only happen once `poll` has said the descriptor is readable, and
        // raw mode sets VMIN to 1, so they return promptly.
        let raw = RawMode::acquire(input)?;

        let mut display = NestedDisplay {
            framebuffer: OwnedFramebuffer::new(size.cols as u32, size.rows as u32 * 2),
            previous: Vec::new(),
            output,
            input,
            _raw: raw,
            cols: size.cols,
            rows: size.rows,
            force_redraw: true,
        };
        display.enter()?;
        Ok(display)
    }

    fn enter(&mut self) -> io::Result<()> {
        // Alternate screen, cursor hidden, mouse and focus reporting on, so
        // the host terminal forwards everything tOS wants.
        self.write_all(b"\x1b[?1049h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h")
    }

    fn leave(&mut self) -> io::Result<()> {
        self.write_all(b"\x1b[?2004l\x1b[?1004l\x1b[?1006l\x1b[?1003l\x1b[?25h\x1b[?1049l\x1b[0m")
    }

    fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            let n = unsafe {
                libc::write(
                    self.output,
                    bytes[written..].as_ptr() as *const libc::c_void,
                    bytes.len() - written,
                )
            };
            if n < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if err.kind() == io::ErrorKind::WouldBlock {
                    // Someone else made this descriptor non-blocking. Wait for
                    // it rather than spinning a core on a full terminal buffer.
                    let mut poll = libc::pollfd {
                        fd: self.output,
                        events: libc::POLLOUT,
                        revents: 0,
                    };
                    unsafe {
                        libc::poll(&mut poll, 1, 50);
                    }
                    continue;
                }
                return Err(err);
            }
            written += n as usize;
        }
        Ok(())
    }

    /// The descriptor input arrives on.
    pub fn input_fd(&self) -> RawFd {
        self.input
    }

    /// Re-read the host terminal size, for example after SIGWINCH. Returns
    /// true when it changed.
    pub fn refresh_size(&mut self) -> io::Result<bool> {
        let size = terminal_size(self.output)?;
        if size.cols == self.cols && size.rows == self.rows {
            return Ok(false);
        }
        self.cols = size.cols.max(1);
        self.rows = size.rows.max(1);
        self.framebuffer
            .resize(self.cols as u32, self.rows as u32 * 2);
        self.previous.clear();
        self.force_redraw = true;
        Ok(true)
    }

    /// Encode the framebuffer as escape sequences.
    fn encode(&mut self) -> Vec<u8> {
        let width = self.framebuffer.width();
        let height = self.framebuffer.height();
        let pixels = self.framebuffer.pixels();
        let diffing = !self.force_redraw && self.previous.len() == pixels.len();

        let mut out = Vec::with_capacity((width * height) as usize / 2 * 8);
        out.extend_from_slice(b"\x1b[H");

        for row in 0..height / 2 {
            let top_row = row * 2;
            let bottom_row = top_row + 1;

            // Skip rows where nothing changed.
            if diffing {
                let start = (top_row * width) as usize;
                let end = ((bottom_row + 1) * width) as usize;
                if pixels[start..end] == self.previous[start..end] {
                    continue;
                }
            }
            out.extend_from_slice(format!("\x1b[{};1H", row + 1).as_bytes());

            let mut last: Option<(u32, u32)> = None;
            for col in 0..width {
                let top = pixels[(top_row * width + col) as usize];
                let bottom = pixels[(bottom_row * width + col) as usize];
                if last != Some((top, bottom)) {
                    out.extend_from_slice(
                        format!(
                            "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                            (top >> 16) & 0xff,
                            (top >> 8) & 0xff,
                            top & 0xff,
                            (bottom >> 16) & 0xff,
                            (bottom >> 8) & 0xff,
                            bottom & 0xff,
                        )
                        .as_bytes(),
                    );
                    last = Some((top, bottom));
                }
                out.extend_from_slice(HALF_BLOCK.as_bytes());
            }
            out.extend_from_slice(b"\x1b[0m");
        }

        self.previous.clear();
        self.previous.extend_from_slice(pixels);
        self.force_redraw = false;
        out
    }
}

impl Display for NestedDisplay {
    fn size(&self) -> (u32, u32) {
        (self.framebuffer.width(), self.framebuffer.height())
    }

    fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()> {
        {
            let mut surface = self.framebuffer.surface();
            draw(&mut surface);
        }
        let bytes = self.encode();
        self.write_all(&bytes)
    }

    fn retains_contents(&self) -> bool {
        true
    }

    fn release(&mut self) -> io::Result<()> {
        self.leave()
    }

    fn restore(&mut self) -> io::Result<()> {
        self.force_redraw = true;
        self.enter()
    }

    fn name(&self) -> String {
        format!("nested {}x{} host cells", self.cols, self.rows)
    }
}

impl Drop for NestedDisplay {
    fn drop(&mut self) {
        let _ = self.leave();
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoder is the interesting part and does not need a real terminal.
    fn encode_buffer(width: u32, height: u32, fill: impl Fn(u32, u32) -> u32) -> String {
        let mut framebuffer = OwnedFramebuffer::new(width, height);
        {
            let mut surface = framebuffer.surface();
            for y in 0..height {
                for x in 0..width {
                    surface.put(x as i32, y as i32, fill(x, y));
                }
            }
        }
        // Build the same output the display would, without owning a terminal.
        let pixels = framebuffer.pixels().to_vec();
        let mut out = String::from("\x1b[H");
        for row in 0..height / 2 {
            out.push_str(&format!("\x1b[{};1H", row + 1));
            let mut last: Option<(u32, u32)> = None;
            for col in 0..width {
                let top = pixels[((row * 2) * width + col) as usize];
                let bottom = pixels[((row * 2 + 1) * width + col) as usize];
                if last != Some((top, bottom)) {
                    out.push_str(&format!(
                        "\x1b[38;2;{};{};{};48;2;{};{};{}m",
                        (top >> 16) & 0xff,
                        (top >> 8) & 0xff,
                        top & 0xff,
                        (bottom >> 16) & 0xff,
                        (bottom >> 8) & 0xff,
                        bottom & 0xff,
                    ));
                    last = Some((top, bottom));
                }
                out.push_str(HALF_BLOCK);
            }
            out.push_str("\x1b[0m");
        }
        out
    }

    #[test]
    fn two_pixel_rows_become_one_host_row() {
        let encoded = encode_buffer(2, 4, |_, y| if y % 2 == 0 { 0xff0000 } else { 0x0000ff });
        // Two host rows for four pixel rows.
        assert_eq!(encoded.matches("\x1b[1;1H").count(), 1);
        assert_eq!(encoded.matches("\x1b[2;1H").count(), 1);
        assert_eq!(encoded.matches(HALF_BLOCK).count(), 4);
    }

    #[test]
    fn the_top_pixel_is_the_foreground() {
        let encoded = encode_buffer(1, 2, |_, y| if y == 0 { 0x112233 } else { 0x445566 });
        assert!(encoded.contains("\x1b[38;2;17;34;51;48;2;68;85;102m"));
    }

    #[test]
    fn runs_of_one_color_emit_one_escape() {
        let encoded = encode_buffer(8, 2, |_, _| 0x010203);
        // One colour change per row, then eight block characters.
        assert_eq!(encoded.matches("\x1b[38;2;").count(), 1);
        assert_eq!(encoded.matches(HALF_BLOCK).count(), 8);
    }
}
