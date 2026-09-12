//! How big the pane is, and how much of it the picture gets.
//!
//! A graphics placement is measured in cells, not pixels: `c=` and `r=` say
//! how many columns and rows the image covers, and the renderer resamples the
//! source into that rectangle. So sizing a picture to a pane is really two
//! questions — how many pixels one cell is, and how many cells the image
//! should be given — and the first one has to be answered before the second
//! can be.

use std::os::unix::io::RawFd;

use tos_platform::tty;

/// The pixel size of one cell when the terminal will not say.
///
/// Wrong in general and right often enough: 8x16 is the VGA text cell, what
/// [`tos_term::TerminalConfig`] defaults to, and close to what most terminals
/// land on at a default font size. Getting it wrong costs the picture its
/// aspect ratio, not its appearance.
pub const DEFAULT_CELL: (u32, u32) = (8, 16);

/// What the kernel knows about the terminal on the other end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    pub cols: u32,
    pub rows: u32,
    pub cell: (u32, u32),
}

impl Metrics {
    /// Ask the kernel, through `TIOCGWINSZ`.
    ///
    /// The pixel fields of a `winsize` are the whole reason this is an ioctl
    /// and not an escape sequence: tOS fills them in for every pane it spawns
    /// (see `tos_compositor::pane::winsize_for`), so inside a tOS session the
    /// cell size is already sitting in the kernel and costs one syscall.
    ///
    /// Terminals that leave the pixel fields at zero are answered with
    /// [`DEFAULT_CELL`] rather than with `CSI 14 t`. Reading a report back
    /// would mean putting the tty into raw mode, writing a query, and waiting
    /// with a timeout for a reply — and a terminal that does not fill in
    /// `winsize` is exactly the terminal least likely to answer, so the
    /// timeout would be the common path rather than the rare one. Trading a
    /// picture that may be stretched for a program that may hang for half a
    /// second and hand back a tty it had switched to raw mode is not a trade
    /// worth making for a preview.
    pub fn probe(fd: RawFd) -> std::io::Result<Metrics> {
        let size = tty::terminal_size(fd)?;
        let cols = size.cols.max(1) as u32;
        let rows = size.rows.max(1) as u32;
        let cell = if size.width_px > 0 && size.height_px > 0 {
            (
                (size.width_px as u32 / cols).max(1),
                (size.height_px as u32 / rows).max(1),
            )
        } else {
            DEFAULT_CELL
        };
        Ok(Metrics { cols, rows, cell })
    }

    /// Replace the measured cell size with one the user supplied.
    pub fn with_cell(self, cell: (u32, u32)) -> Metrics {
        Metrics {
            cell: (cell.0.max(1), cell.1.max(1)),
            ..self
        }
    }

    /// The rows a picture may occupy: one short of the pane.
    ///
    /// The image is placed above where the shell's next prompt will land, and
    /// a picture that filled the pane to the last row would be scrolled off
    /// its own top by that prompt.
    pub fn usable_rows(self) -> u32 {
        self.rows.saturating_sub(1).max(1)
    }

    /// The rectangle a picture may occupy, in pixels.
    pub fn usable_pixels(self) -> (u32, u32) {
        (self.cols * self.cell.0, self.usable_rows() * self.cell.1)
    }
}

/// A placement's size, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cells {
    pub cols: u32,
    pub rows: u32,
}

/// Choose the cell rectangle a `width` x `height` image should be drawn into.
///
/// The picture is scaled to fit `metrics`, keeping its aspect ratio, and is
/// not enlarged past its own pixels unless `upscale` is set: a 64x64 icon
/// blown up to fill a pane is blur, not a preview.
///
/// The answer is rounded to the nearest cell rather than up to the next one.
/// Cells are the only unit a placement has, so some rounding is unavoidable
/// and the picture is stretched by whatever it comes to; rounding to nearest
/// makes that at most half a cell in each direction instead of a whole one.
pub fn fit(width: u32, height: u32, metrics: Metrics, upscale: bool) -> Cells {
    let (avail_w, avail_h) = metrics.usable_pixels();
    let (w, h) = (width.max(1) as u64, height.max(1) as u64);
    let (avail_w, avail_h) = (avail_w.max(1) as u64, avail_h.max(1) as u64);

    let fits = w <= avail_w && h <= avail_h;
    let (target_w, target_h) = if fits && !upscale {
        (w, h)
    } else if w * avail_h >= h * avail_w {
        // Wider than the space it is going into, so width is what runs out
        // first and height follows from it.
        (avail_w, (h * avail_w / w).max(1))
    } else {
        ((w * avail_h / h).max(1), avail_h)
    };

    Cells {
        cols: to_cells(target_w, metrics.cell.0).min(metrics.cols),
        rows: to_cells(target_h, metrics.cell.1).min(metrics.usable_rows()),
    }
}

/// Pixels to cells, rounded to nearest, never zero.
fn to_cells(pixels: u64, cell: u32) -> u32 {
    let cell = cell.max(1) as u64;
    (((pixels + cell / 2) / cell).max(1)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cols: u32, rows: u32) -> Metrics {
        Metrics {
            cols,
            rows,
            cell: (8, 16),
        }
    }

    #[test]
    fn a_small_picture_is_left_at_its_own_size() {
        // 80x32 pixels is exactly ten cells by two.
        let cells = fit(80, 32, pane(80, 24), false);
        assert_eq!(cells, Cells { cols: 10, rows: 2 });
    }

    #[test]
    fn a_wide_picture_runs_out_of_width_first() {
        // The pane is 640x368 usable; a 2:1 picture that wide is 320 tall,
        // which is 20 rows.
        let cells = fit(2000, 1000, pane(80, 24), false);
        assert_eq!(cells, Cells { cols: 80, rows: 20 });
    }

    #[test]
    fn a_tall_picture_runs_out_of_height_first() {
        // 368 usable pixels of height, so a 1:2 picture is 184 wide: 23 cells.
        let cells = fit(1000, 2000, pane(80, 24), false);
        assert_eq!(cells, Cells { cols: 23, rows: 23 });
    }

    #[test]
    fn upscaling_happens_only_when_it_is_asked_for() {
        let small = pane(80, 24);
        assert_eq!(fit(80, 32, small, false), Cells { cols: 10, rows: 2 });
        assert_eq!(fit(80, 32, small, true), Cells { cols: 80, rows: 16 });
    }

    #[test]
    fn a_picture_never_takes_the_row_the_prompt_needs() {
        for rows in 1..40u32 {
            let cells = fit(4000, 4000, pane(80, rows), false);
            assert!(cells.rows < rows.max(2), "{rows} rows: {cells:?}");
            assert!(cells.rows >= 1);
        }
    }

    #[test]
    fn a_picture_thinner_than_a_cell_still_gets_one() {
        let cells = fit(1, 1, pane(80, 24), false);
        assert_eq!(cells, Cells { cols: 1, rows: 1 });
    }

    #[test]
    fn a_pane_that_reports_no_pixels_gets_the_default_cell() {
        // The arithmetic Metrics::probe does, without an fd to do it on.
        let metrics = Metrics {
            cols: 80,
            rows: 24,
            cell: DEFAULT_CELL,
        };
        assert_eq!(metrics.usable_pixels(), (640, 368));
    }

    #[test]
    fn a_supplied_cell_size_overrides_the_measured_one() {
        let metrics = pane(80, 24).with_cell((10, 20));
        assert_eq!(metrics.usable_pixels(), (800, 460));
    }
}
