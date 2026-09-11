//! Kitty graphics protocol.
//!
//! Commands arrive as APC strings: `ESC _ G <key=value,...> ; <base64> ESC \`.
//! This module parses them and owns the image/placement store that cells refer
//! to through [`crate::cell::GraphicsRef`].
//!
//! Raw RGB and RGBA data, PNG (`f=100`) and zlib-compressed payloads (`o=z`)
//! all transmit; PNG and zlib are decoded by [`crate::png`] and
//! [`crate::inflate`]. Anything that cannot be decoded is answered with the
//! protocol's error response rather than being silently dropped, so
//! applications can fall back instead of hanging.
//!
//! Images may also be animations. The store keeps every frame but holds the
//! one that is on screen in [`Image::data`], so the renderer asks for an image
//! and gets the current frame without knowing that animation exists. Time is
//! never read here: [`GraphicsStore::advance_animations`] is told what time it
//! is, which is what lets a test walk an animation frame by frame.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::inflate::{self, InflateError};
use crate::png::{self, PngError};

/// What a command asks the terminal to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `a=t` transmit only.
    Transmit,
    /// `a=T` transmit and display.
    TransmitAndDisplay,
    /// `a=p` display a previously transmitted image.
    Put,
    /// `a=d` delete images or placements.
    Delete,
    /// `a=q` query support without storing anything.
    Query,
    /// `a=f` transmit a frame of an animation for an existing image.
    TransmitFrame,
    /// `a=a` animation control: play state, current frame, loops, gaps.
    AnimationControl,
    /// `a=c` compose one existing frame onto another, not yet implemented.
    ComposeFrames,
}

/// Pixel layout of a transmitted image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Rgb,
    Rgba,
    Png,
}

/// How the payload reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

/// A parsed graphics command.
#[derive(Debug, Clone)]
pub struct GraphicsCommand {
    pub action: Action,
    pub format: Format,
    pub medium: Medium,
    pub image_id: u32,
    pub image_number: u32,
    pub placement_id: u32,
    /// Source image dimensions in pixels.
    pub width: u32,
    pub height: u32,
    /// Source rectangle within the image.
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// Target size in cells; 0 means "derive from pixels".
    pub cols: u32,
    pub rows: u32,
    /// Pixel offset within the first cell.
    pub cell_x: u32,
    pub cell_y: u32,
    pub z_index: i32,
    /// `m=1` means more chunks follow.
    pub more: bool,
    /// `C=1` means do not move the cursor after placing.
    pub cursor_stays: bool,
    /// `q` suppresses responses: 1 = ok only, 2 = all.
    pub quiet: u8,
    /// `d` target for delete commands.
    pub delete: u8,
    /// `o=z` zlib compression.
    pub compressed: bool,
    pub payload: Vec<u8>,
}

impl Default for GraphicsCommand {
    fn default() -> Self {
        GraphicsCommand {
            action: Action::Transmit,
            format: Format::Rgba,
            medium: Medium::Direct,
            image_id: 0,
            image_number: 0,
            placement_id: 0,
            width: 0,
            height: 0,
            src_x: 0,
            src_y: 0,
            src_w: 0,
            src_h: 0,
            cols: 0,
            rows: 0,
            cell_x: 0,
            cell_y: 0,
            z_index: 0,
            more: false,
            cursor_stays: false,
            quiet: 0,
            delete: b'a',
            compressed: false,
            payload: Vec::new(),
        }
    }
}

impl GraphicsCommand {
    /// Parse an APC body. The leading `G` has already been matched by the
    /// caller; `body` is everything after it.
    pub fn parse(body: &[u8]) -> Option<GraphicsCommand> {
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, &body[body.len()..]),
        };

        let mut cmd = GraphicsCommand::default();
        for pair in control.split(|&b| b == b',') {
            if pair.is_empty() {
                continue;
            }
            let eq = pair.iter().position(|&b| b == b'=')?;
            let key = pair[..eq].first().copied()?;
            let value = &pair[eq + 1..];
            let text = std::str::from_utf8(value).ok()?;

            match key {
                b'a' => {
                    cmd.action = match value.first()? {
                        b't' => Action::Transmit,
                        b'T' => Action::TransmitAndDisplay,
                        b'p' => Action::Put,
                        b'd' => Action::Delete,
                        b'q' => Action::Query,
                        b'f' => Action::TransmitFrame,
                        b'a' => Action::AnimationControl,
                        b'c' => Action::ComposeFrames,
                        _ => return None,
                    }
                }
                b'f' => {
                    cmd.format = match text.parse::<u32>().ok()? {
                        24 => Format::Rgb,
                        32 => Format::Rgba,
                        100 => Format::Png,
                        _ => return None,
                    }
                }
                b't' => {
                    cmd.medium = match value.first()? {
                        b'd' => Medium::Direct,
                        b'f' => Medium::File,
                        b't' => Medium::TempFile,
                        b's' => Medium::SharedMemory,
                        _ => return None,
                    }
                }
                b'i' => cmd.image_id = text.parse().ok()?,
                b'I' => cmd.image_number = text.parse().ok()?,
                b'p' => cmd.placement_id = text.parse().ok()?,
                b's' => cmd.width = text.parse().ok()?,
                b'v' => cmd.height = text.parse().ok()?,
                b'x' => cmd.src_x = text.parse().ok()?,
                b'y' => cmd.src_y = text.parse().ok()?,
                b'w' => cmd.src_w = text.parse().ok()?,
                b'h' => cmd.src_h = text.parse().ok()?,
                b'c' => cmd.cols = text.parse().ok()?,
                b'r' => cmd.rows = text.parse().ok()?,
                b'X' => cmd.cell_x = text.parse().ok()?,
                b'Y' => cmd.cell_y = text.parse().ok()?,
                b'z' => cmd.z_index = text.parse().ok()?,
                b'm' => cmd.more = text.parse::<u32>().ok()? != 0,
                b'C' => cmd.cursor_stays = text.parse::<u32>().ok()? != 0,
                b'q' => cmd.quiet = text.parse().ok()?,
                b'd' => cmd.delete = *value.first().unwrap_or(&b'a'),
                b'o' => cmd.compressed = value.first() == Some(&b'z'),
                // Unknown keys are ignored, as the protocol requires.
                _ => {}
            }
        }

        cmd.payload = decode_base64(payload);
        Some(cmd)
    }

    // Animation commands reuse the keys of the placement commands with
    // different meanings, so the parser stores them in the same fields and the
    // accessors below name them for the animation path. Keeping one parser
    // means an unknown or misspelled key still cannot change what a command
    // means.

    /// `a=f`: the 1-based frame this command writes, 0 meaning "append".
    /// `a=a`: the frame whose gap is being changed.
    pub fn frame_number(&self) -> u32 {
        self.rows
    }

    /// `a=f`: the 1-based frame to use as the base for the new frame, 0
    /// meaning the background colour. `a=a`: the frame to make current.
    pub fn base_frame(&self) -> u32 {
        self.cols
    }

    /// The gap in milliseconds. Zero asks for the default, negative values ask
    /// for a gapless frame, which the animation never stops on.
    pub fn frame_gap(&self) -> i32 {
        self.z_index
    }

    /// Destination rectangle of the frame data within the image.
    pub fn frame_rect(&self) -> (u32, u32, u32, u32) {
        (self.src_x, self.src_y, self.width, self.height)
    }

    /// `X=1` overwrites the base pixels instead of alpha blending onto them.
    pub fn frame_overwrites(&self) -> bool {
        self.cell_x == 1
    }

    /// `Y`: background colour for the parts of a frame no data covers, as
    /// 32-bit RGBA.
    pub fn frame_background(&self) -> u32 {
        self.cell_y
    }

    /// `a=a`: 1 stops the animation, 2 runs it while more frames are still
    /// arriving, 3 runs it normally. 0 leaves the state alone.
    pub fn animation_state(&self) -> u32 {
        self.width
    }

    /// `a=a`: 0 leaves the loop count alone, 1 loops forever, `n` loops
    /// `n - 1` times.
    pub fn animation_loops(&self) -> u32 {
        self.height
    }

    /// Identifier used in responses: `i=` if given, otherwise `I=`.
    pub fn response_id(&self) -> String {
        if self.image_id != 0 {
            format!("i={}", self.image_id)
        } else {
            format!("I={}", self.image_number)
        }
    }
}

/// Gap used for a frame that does not ask for one, matching kitty.
const DEFAULT_GAP_MS: u32 = 40;
/// Upper bound on frames per image. Frames arrive from untrusted programs and
/// each one costs a full image of pixels, so the count is capped as well as
/// the bytes.
const MAX_FRAMES: usize = 1024;

/// One frame of an animated image.
#[derive(Debug, Clone)]
pub struct Frame {
    /// RGBA8 pixels for the whole image. Empty for the frame that is currently
    /// on screen, because its pixels live in [`Image::data`] instead of being
    /// copied there; see [`Image::pixels`].
    data: Vec<u8>,
    /// How long the frame stays up. Zero means the frame is never displayed,
    /// which is what a negative `z=` asks for: such frames exist only to be
    /// composed onto.
    gap_ms: u32,
}

/// Whether an image's frames are advancing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationState {
    Stopped,
    /// Running, but the client is still sending frames, so the animation stops
    /// at the last frame it has rather than looping back.
    Loading,
    Running,
}

/// A stored image. Still images have no frames; an animated image keeps every
/// frame, with the visible one's pixels in `data` so the renderer can go on
/// asking the store for an image and get what is on screen now.
#[derive(Debug, Clone)]
pub struct Image {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    /// Always RGBA8, converted at transmission time. For an animated image
    /// this is the current frame.
    pub data: Vec<u8>,
    /// Every frame, root frame first. Empty while the image is a still.
    frames: Vec<Frame>,
    /// Index into `frames` of the frame `data` holds.
    current: usize,
    state: AnimationState,
    /// Changes when a frame's pixels are replaced, not when a different frame
    /// is put on screen. A cache holding prepared copies pairs this with the
    /// frame number: together they name which pixels, while a counter that
    /// also moved on every advance would name a moment instead, and a
    /// looping animation would never reuse anything it had already built.
    generation: u64,
    /// When the current frame went up. `None` means "start timing at the next
    /// advance", which is how the store avoids reading a clock of its own.
    shown_at: Option<Instant>,
    /// Remaining loops, `None` for forever.
    loops_left: Option<u32>,
}

impl Image {
    fn still(id: u32, width: u32, height: u32, data: Vec<u8>) -> Image {
        Image {
            id,
            width,
            height,
            data,
            frames: Vec::new(),
            current: 0,
            state: AnimationState::Stopped,
            generation: 0,
            shown_at: None,
            loops_left: None,
        }
    }

    /// Number of frames, counting the root frame. A still image has one.
    pub fn frame_count(&self) -> usize {
        self.frames.len().max(1)
    }

    /// The 1-based frame currently on screen.
    pub fn current_frame(&self) -> u32 {
        self.current as u32 + 1
    }

    pub fn animation_state(&self) -> AnimationState {
        self.state
    }

    /// A number that changes whenever `data` changes, and never repeats within
    /// one store. It is what a renderer-side cache keyed on an image's pixels
    /// should compare, because an animated image keeps its id while the pixels
    /// under it move on.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Gap of a 1-based frame, in milliseconds.
    pub fn frame_gap(&self, frame: u32) -> Option<u32> {
        self.frames
            .get(frame.checked_sub(1)? as usize)
            .map(|f| f.gap_ms)
    }

    /// Pixels of a frame by index, following the rule that the current frame's
    /// pixels live in `data`.
    fn pixels(&self, index: usize) -> Option<&[u8]> {
        if index == self.current {
            Some(&self.data)
        } else {
            self.frames.get(index).map(|f| f.data.as_slice())
        }
    }

    /// Bytes of one frame. Frames are always stored at full image size so that
    /// a partial update never leaves a short buffer for the renderer to read.
    fn frame_bytes(&self) -> usize {
        (self.width as usize) * (self.height as usize) * 4
    }

    /// Everything this image charges against the store's budget.
    fn stored_bytes(&self) -> usize {
        self.data.len() + self.frames.iter().map(|f| f.data.len()).sum::<usize>()
    }

    /// Whether the image has frames worth stepping through.
    fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    /// Whether stepping the animation could show a different frame. An
    /// animation whose frames are all gapless has nothing to show and must not
    /// keep the compositor awake.
    fn is_playable(&self) -> bool {
        self.is_animated()
            && self.state != AnimationState::Stopped
            && self.frames.iter().any(|f| f.gap_ms > 0)
    }

    /// The frame to show after this one, `None` when the animation has run out
    /// of frames: either it is still loading, or its loops are used up.
    fn next_index(&mut self) -> Option<usize> {
        if self.current + 1 < self.frames.len() {
            return Some(self.current + 1);
        }
        if self.state == AnimationState::Loading {
            return None;
        }
        match self.loops_left {
            None => Some(0),
            Some(0) => None,
            Some(n) => {
                self.loops_left = Some(n - 1);
                Some(0)
            }
        }
    }

    /// Step to whatever frame `now` selects. Returns true when the visible
    /// frame changed.
    fn advance(&mut self, now: Instant) -> bool {
        if !self.is_playable() {
            return false;
        }
        let Some(shown_at) = self.shown_at else {
            // The frame went up without a timestamp, so this call is when it
            // was first seen; it starts the clock instead of advancing.
            self.shown_at = Some(now);
            return false;
        };

        let start = self.current;
        let mut elapsed = now.saturating_duration_since(shown_at).as_millis();
        // A tick that arrives late may have to cross several frames, but never
        // more than one cycle: frames older than that were never on screen and
        // replaying them would only delay catching up.
        let mut caught_up = false;
        for _ in 0..self.frames.len() {
            let gap = self.frames[self.current].gap_ms as u128;
            if gap != 0 {
                if elapsed < gap {
                    caught_up = true;
                    break;
                }
                elapsed -= gap;
            }
            let Some(next) = self.next_index() else {
                // A loading animation waits at its last frame for more; a
                // finished one stays on screen but stops burning ticks.
                if self.state == AnimationState::Running {
                    self.state = AnimationState::Stopped;
                }
                caught_up = true;
                break;
            };
            if !self.show_frame(next) {
                caught_up = true;
                break;
            }
        }

        // Time still owed after a whole cycle went by belongs to frames that
        // were never on screen. Carrying it forward would replay the cycle on
        // the next tick too, and the one after that, for as long as the debt
        // lasted: the animation would race through its loops while looking
        // frozen. Starting the clock again here is what catching up means.
        let leftover = if caught_up {
            Duration::from_millis(elapsed.min(u64::MAX as u128) as u64)
        } else {
            Duration::ZERO
        };
        self.shown_at = Some(now.checked_sub(leftover).unwrap_or(now));
        self.current != start
    }

    /// How long until this image wants a new frame.
    fn next_delay(&self, now: Instant) -> Option<Duration> {
        if !self.is_playable() {
            return None;
        }
        if self.state == AnimationState::Loading && self.current + 1 >= self.frames.len() {
            return None;
        }
        let Some(shown_at) = self.shown_at else {
            return Some(Duration::ZERO);
        };
        let gap = self.frames[self.current].gap_ms;
        let deadline = shown_at
            .checked_add(Duration::from_millis(gap as u64))
            .unwrap_or(now);
        Some(deadline.saturating_duration_since(now))
    }

    /// Move the visible frame, keeping the "current frame's pixels live in
    /// `data`" rule. Returns false when the index is out of range.
    fn show_frame(&mut self, index: usize) -> bool {
        if index == self.current {
            return true;
        }
        if index >= self.frames.len() {
            return false;
        }
        let next = std::mem::take(&mut self.frames[index].data);
        if next.len() != self.frame_bytes() {
            // Cannot happen for frames this module stores, but a short buffer
            // would be a partly drawn image rather than a panic, so put it back.
            self.frames[index].data = next;
            return false;
        }
        let previous = std::mem::replace(&mut self.data, next);
        let current = self.current;
        self.frames[current].data = previous;
        self.current = index;
        true
    }
}

/// An image placed on the grid.
#[derive(Debug, Clone)]
pub struct Placement {
    pub id: u32,
    pub image_id: u32,
    pub placement_id: u32,
    /// Anchor cell on the screen.
    pub col: u16,
    pub row: u16,
    pub cols: u16,
    pub rows: u16,
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub z_index: i32,
}

/// Image and placement store for one terminal.
#[derive(Debug, Default)]
pub struct GraphicsStore {
    images: HashMap<u32, Image>,
    placements: HashMap<u32, Placement>,
    next_placement: u32,
    next_auto_id: u32,
    /// Partially received image, keyed by image id.
    pending: Option<(u32, GraphicsCommand, Vec<u8>)>,
    /// Total bytes of pixel data held, used to enforce the budget.
    bytes: usize,
    budget: usize,
    /// Handed out to images whose pixels change; see [`Image::generation`].
    generations: u64,
}

impl GraphicsStore {
    pub fn new(budget_bytes: usize) -> Self {
        GraphicsStore {
            budget: budget_bytes,
            next_placement: 1,
            next_auto_id: 1 << 24,
            ..Default::default()
        }
    }

    pub fn image(&self, id: u32) -> Option<&Image> {
        self.images.get(&id)
    }

    pub fn placement(&self, id: u32) -> Option<&Placement> {
        self.placements.get(&id)
    }

    pub fn placements(&self) -> impl Iterator<Item = &Placement> {
        self.placements.values()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty() && self.placements.is_empty()
    }

    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.pending = None;
        self.bytes = 0;
    }

    /// Store an image, converting the payload to RGBA. Returns an error string
    /// suitable for the protocol response.
    pub fn store(&mut self, cmd: &GraphicsCommand, payload: &[u8]) -> Result<u32, &'static str> {
        if cmd.medium != Medium::Direct {
            return Err("EINVAL:only direct transmission supported");
        }
        let (w, h, data) = decode_payload(cmd, payload, self.budget)?;

        let id = if cmd.image_id != 0 {
            cmd.image_id
        } else {
            self.next_auto_id += 1;
            self.next_auto_id
        };

        if let Some(old) = self.images.remove(&id) {
            // Retransmitting an image replaces it whole, frames included.
            self.bytes -= old.stored_bytes();
        }
        self.bytes += data.len();
        self.generations += 1;
        let mut image = Image::still(id, w, h, data);
        image.generation = self.generations;
        self.images.insert(id, image);
        self.evict_to_budget(id);
        Ok(id)
    }

    /// Drop least-recently-added images until the byte budget is met. The
    /// image just stored is never evicted.
    fn evict_to_budget(&mut self, keep: u32) {
        while self.bytes > self.budget {
            let victim = self
                .images
                .keys()
                .copied()
                .filter(|&id| id != keep)
                .min_by_key(|id| *id);
            match victim {
                Some(id) => {
                    if let Some(img) = self.images.remove(&id) {
                        self.bytes -= img.stored_bytes();
                    }
                    self.placements.retain(|_, p| p.image_id != id);
                }
                None => break,
            }
        }
    }

    /// Accumulate a chunked transmission. Returns the complete payload when
    /// the final chunk (`m=0`) arrives.
    pub fn accumulate(&mut self, cmd: &GraphicsCommand) -> Option<(GraphicsCommand, Vec<u8>)> {
        let key = if cmd.image_id != 0 {
            cmd.image_id
        } else {
            cmd.image_number
        };
        match self.pending.take() {
            // A continuation either names the same image or, as the protocol
            // allows, names nothing at all. A command that names a *different*
            // image starts a new transmission and supersedes this one.
            Some((pending_key, first, mut buf)) if key == 0 || pending_key == key => {
                buf.extend_from_slice(&cmd.payload);
                if cmd.more {
                    self.pending = Some((pending_key, first, buf));
                    None
                } else {
                    Some((first, buf))
                }
            }
            other => {
                // A new transmission supersedes any abandoned one.
                drop(other);
                if cmd.more {
                    self.pending = Some((key, cmd.clone(), cmd.payload.clone()));
                    None
                } else {
                    Some((cmd.clone(), cmd.payload.clone()))
                }
            }
        }
    }

    /// Create a placement. `cell_w`/`cell_h` give the pixel size of one cell so
    /// the default size can be derived.
    pub fn place(
        &mut self,
        cmd: &GraphicsCommand,
        image_id: u32,
        col: u16,
        row: u16,
        cell_w: u32,
        cell_h: u32,
    ) -> Option<u32> {
        let image = self.images.get(&image_id)?;
        let src_w = if cmd.src_w == 0 {
            image.width.saturating_sub(cmd.src_x)
        } else {
            cmd.src_w.min(image.width.saturating_sub(cmd.src_x))
        };
        let src_h = if cmd.src_h == 0 {
            image.height.saturating_sub(cmd.src_y)
        } else {
            cmd.src_h.min(image.height.saturating_sub(cmd.src_y))
        };
        if src_w == 0 || src_h == 0 {
            return None;
        }

        let cols = if cmd.cols != 0 {
            cmd.cols
        } else {
            src_w.div_ceil(cell_w.max(1))
        };
        let rows = if cmd.rows != 0 {
            cmd.rows
        } else {
            src_h.div_ceil(cell_h.max(1))
        };

        // A placement id supplied by the application replaces the previous
        // placement with the same (image, placement) pair.
        if cmd.placement_id != 0 {
            self.placements
                .retain(|_, p| !(p.image_id == image_id && p.placement_id == cmd.placement_id));
        }

        let id = self.next_placement;
        self.next_placement += 1;
        self.placements.insert(
            id,
            Placement {
                id,
                image_id,
                placement_id: cmd.placement_id,
                col,
                row,
                cols: cols.min(u16::MAX as u32) as u16,
                rows: rows.min(u16::MAX as u32) as u16,
                src_x: cmd.src_x,
                src_y: cmd.src_y,
                src_w,
                src_h,
                z_index: cmd.z_index,
            },
        );
        Some(id)
    }

    /// Handle `a=d`. Returns true when the screen needs repainting.
    pub fn delete(&mut self, cmd: &GraphicsCommand) -> bool {
        let free_data = cmd.delete.is_ascii_uppercase();
        let what = cmd.delete.to_ascii_lowercase();
        let before = self.placements.len();

        match what {
            b'a' => self.placements.clear(),
            b'i' => {
                let image_id = cmd.image_id;
                self.placements.retain(|_, p| {
                    p.image_id != image_id
                        || (cmd.placement_id != 0 && p.placement_id != cmd.placement_id)
                });
                if free_data {
                    if let Some(img) = self.images.remove(&image_id) {
                        self.bytes -= img.stored_bytes();
                    }
                }
            }
            b'z' => {
                let z = cmd.z_index;
                self.placements.retain(|_, p| p.z_index != z);
            }
            _ => {}
        }

        if free_data && what == b'a' {
            self.images.clear();
            self.bytes = 0;
        }
        self.placements.len() != before
    }

    /// Drop placements that scrolled off or were overwritten.
    pub fn retain_rows(&mut self, rows: u16) {
        self.placements.retain(|_, p| p.row < rows);
    }

    /// Shift placements up by `n` rows, dropping those that leave the screen.
    pub fn scroll(&mut self, n: u16) {
        self.placements.retain(|_, p| {
            let bottom = p.row as i32 + p.rows as i32;
            bottom - n as i32 > 0
        });
        for p in self.placements.values_mut() {
            p.row = p.row.saturating_sub(n);
        }
    }

    // ---- animation ------------------------------------------------------

    /// Handle `a=f`: add or rewrite a frame of an existing image.
    ///
    /// The frame data covers a rectangle of the image, and the rest of the
    /// frame comes either from an earlier frame (`c=`) or from a flat
    /// background colour (`Y=`), so a client can send only what moved.
    pub fn store_frame(
        &mut self,
        cmd: &GraphicsCommand,
        payload: &[u8],
    ) -> Result<u32, &'static str> {
        if cmd.compressed {
            return Err("EINVAL:compression not supported");
        }
        if cmd.format == Format::Png {
            return Err("EINVAL:PNG not supported");
        }
        if cmd.medium != Medium::Direct {
            return Err("EINVAL:only direct transmission supported");
        }
        let budget = self.budget;
        let id = cmd.image_id;
        let image = self.images.get_mut(&id).ok_or("ENOENT:no such image")?;

        // Every value below comes from an application, so each one is checked
        // against the image rather than trusted.
        let (dest_x, dest_y, mut w, mut h) = cmd.frame_rect();
        if w == 0 {
            w = image.width.saturating_sub(dest_x);
        }
        if h == 0 {
            h = image.height.saturating_sub(dest_y);
        }
        if w == 0 || h == 0 {
            return Err("EINVAL:empty frame");
        }
        if dest_x.saturating_add(w) > image.width || dest_y.saturating_add(h) > image.height {
            return Err("EINVAL:frame outside image");
        }
        let stride = match cmd.format {
            Format::Rgb => 3,
            Format::Rgba => 4,
            Format::Png => unreachable!(),
        };
        let expected = (w as usize)
            .checked_mul(h as usize)
            .and_then(|n| n.checked_mul(stride))
            .ok_or("EINVAL:frame too large")?;
        if payload.len() < expected {
            return Err("EINVAL:truncated payload");
        }

        let frame_bytes = image.frame_bytes();
        let frame = cmd.frame_number();
        let index = match frame {
            0 => image.frames.len().max(1),
            n => (n - 1) as usize,
        };
        // A frame may be rewritten or appended, but not created out past the
        // end, which would leave a hole no data ever fills.
        let existing = image.frames.len().max(1);
        if index > existing {
            return Err("EINVAL:no such frame");
        }
        // A still image already has one frame, the one in `data`, so `r=1`
        // rewrites it rather than adding a second.
        let appending = index >= existing;
        if appending && existing >= MAX_FRAMES {
            return Err("EINVAL:too many frames");
        }
        // Animations are the quickest way to exhaust the store, so a single
        // image is never allowed to outgrow the whole budget.
        if appending && image.stored_bytes().saturating_add(frame_bytes) > budget {
            return Err("EINVAL:animation exceeds graphics budget");
        }

        let mut pixels = match cmd.base_frame() {
            0 => {
                let [r, g, b, a] = cmd.frame_background().to_be_bytes();
                let mut fill = Vec::with_capacity(frame_bytes);
                for _ in 0..frame_bytes / 4 {
                    fill.extend_from_slice(&[r, g, b, a]);
                }
                fill
            }
            base => {
                let base = image
                    .pixels((base - 1) as usize)
                    .ok_or("EINVAL:no such base frame")?;
                if base.len() != frame_bytes {
                    return Err("EINVAL:no such base frame");
                }
                base.to_vec()
            }
        };
        compose(
            &mut pixels,
            image.width,
            (dest_x, dest_y, w, h),
            &payload[..expected],
            stride,
            cmd.frame_overwrites(),
        );

        // The root frame's pixels are already in `data`; give it a slot the
        // first time this image gains a second frame. Kitty leaves that frame
        // with no gap until a client asks for one, but a frame with no gap is
        // never shown, so tOS starts it at the default instead: an animation
        // whose client forgets that command still begins at its first frame.
        if image.frames.is_empty() {
            image.frames.push(Frame {
                data: Vec::new(),
                gap_ms: DEFAULT_GAP_MS,
            });
            image.current = 0;
        }

        let gap = cmd.frame_gap();
        if appending {
            image.frames.push(Frame {
                data: pixels,
                gap_ms: gap_ms(gap, DEFAULT_GAP_MS),
            });
            self.bytes += frame_bytes;
        } else {
            let previous = image.frames[index].gap_ms;
            image.frames[index].gap_ms = gap_ms(gap, previous);
            if index == image.current {
                image.data = pixels;
                self.generations += 1;
                image.generation = self.generations;
            } else {
                image.frames[index].data = pixels;
            }
        }
        self.evict_to_budget(id);
        Ok(id)
    }

    /// Handle `a=a`: play state, current frame, loop count and frame gaps.
    /// Returns true when the visible frame changed.
    pub fn control_animation(&mut self, cmd: &GraphicsCommand) -> Result<bool, &'static str> {
        let image = self
            .images
            .get_mut(&cmd.image_id)
            .ok_or("ENOENT:no such image")?;
        let mut changed = false;

        // `r` with `z` changes one frame's gap. The root frame is given a slot
        // here too, because its gap is usually set before its siblings arrive,
        // and setting it to nothing is how a client says "this frame is only a
        // canvas".
        let frame = cmd.frame_number();
        if frame != 0 && cmd.frame_gap() != 0 {
            let index = (frame - 1) as usize;
            if index == 0 && image.frames.is_empty() {
                image.frames.push(Frame {
                    data: Vec::new(),
                    gap_ms: DEFAULT_GAP_MS,
                });
                image.current = 0;
            }
            let slot = image.frames.get_mut(index).ok_or("EINVAL:no such frame")?;
            slot.gap_ms = gap_ms(cmd.frame_gap(), slot.gap_ms);
        }

        match cmd.animation_loops() {
            0 => {}
            1 => image.loops_left = None,
            n => image.loops_left = Some(n - 1),
        }

        let current = cmd.base_frame();
        if current != 0 {
            let index = (current - 1) as usize;
            let moved = index != image.current;
            if !image.show_frame(index) {
                return Err("EINVAL:no such frame");
            }
            if moved {
                image.shown_at = None;
                changed = true;
            }
        }

        match cmd.animation_state() {
            0 => {}
            1 => image.state = AnimationState::Stopped,
            2 => {
                image.state = AnimationState::Loading;
                image.shown_at = None;
            }
            3 => {
                image.state = AnimationState::Running;
                image.shown_at = None;
            }
            _ => return Err("EINVAL:bad animation state"),
        }
        Ok(changed)
    }

    /// Step every running animation to the frame `now` selects, returning the
    /// images whose visible frame changed so the caller can repaint them.
    ///
    /// The clock belongs to the caller: nothing in this crate reads one, which
    /// is what lets a test walk an animation frame by frame.
    pub fn advance_animations(&mut self, now: Instant) -> Vec<u32> {
        let mut changed = Vec::new();
        for image in self.images.values_mut() {
            if image.advance(now) {
                changed.push(image.id);
            }
        }
        changed
    }

    /// How long until the soonest animation needs its next frame, `None` when
    /// nothing is animating.
    pub fn next_animation_delay(&self, now: Instant) -> Option<Duration> {
        self.images.values().filter_map(|i| i.next_delay(now)).min()
    }
}

/// Turn a `z=` value into a gap: zero keeps what is there already, negative
/// asks for a gapless frame, which the animation passes straight over.
fn gap_ms(requested: i32, current: u32) -> u32 {
    match requested {
        0 => current,
        g if g < 0 => 0,
        g => g as u32,
    }
}

/// Draw transmitted frame data onto the frame's base pixels. `dest` is the
/// rectangle within an image `width` pixels wide; both buffers are RGBA8 apart
/// from the source stride, which is 3 for `f=24`.
fn compose(
    pixels: &mut [u8],
    width: u32,
    dest: (u32, u32, u32, u32),
    payload: &[u8],
    stride: usize,
    overwrite: bool,
) {
    let (dest_x, dest_y, w, h) = dest;
    let row_bytes = w as usize * stride;
    for row in 0..h as usize {
        let src = &payload[row * row_bytes..][..row_bytes];
        let start = ((dest_y as usize + row) * width as usize + dest_x as usize) * 4;
        let Some(dst) = pixels.get_mut(start..start + w as usize * 4) else {
            return;
        };
        for (s, d) in src.chunks_exact(stride).zip(dst.chunks_exact_mut(4)) {
            let alpha = if stride == 4 { s[3] } else { 0xff };
            if overwrite || alpha == 0xff {
                d.copy_from_slice(&[s[0], s[1], s[2], alpha]);
            } else if alpha != 0 {
                // Source-over, kept in integers: the frames a terminal shows
                // are not worth a round trip through floating point.
                let inv = 255 - alpha as u32;
                for c in 0..3 {
                    let blended = s[c] as u32 * alpha as u32 + d[c] as u32 * inv;
                    d[c] = ((blended + 127) / 255) as u8;
                }
                d[3] = (alpha as u32 + d[3] as u32 * inv / 255).min(255) as u8;
            }
        }
    }
}

/// Turn a transmitted payload into RGBA8 and the dimensions that belong to it.
///
/// `budget` is the store's whole byte allowance, used as the ceiling on
/// anything that has to be decompressed. A payload that would expand past it
/// could never be kept, so it is refused while it is still being decoded
/// instead of after it has been materialised — which is the difference
/// between rejecting a decompression bomb and being flattened by one.
fn decode_payload(
    cmd: &GraphicsCommand,
    payload: &[u8],
    budget: usize,
) -> Result<(u32, u32, Vec<u8>), &'static str> {
    match cmd.format {
        Format::Png => {
            // A PNG carries its own dimensions, so `s=`/`v=` are advisory and
            // the file wins if they disagree.
            let decompressed;
            let bytes = if cmd.compressed {
                decompressed = inflate::zlib_decompress(payload, budget).map_err(zlib_error)?;
                &decompressed[..]
            } else {
                payload
            };
            let image = png::decode(bytes, budget).map_err(png_error)?;
            Ok((image.width, image.height, image.rgba))
        }
        Format::Rgb | Format::Rgba => {
            let stride = if cmd.format == Format::Rgb { 3 } else { 4 };
            let (w, h) = (cmd.width, cmd.height);
            if w == 0 || h == 0 {
                return Err("EINVAL:missing dimensions");
            }
            let expected = (w as usize)
                .checked_mul(h as usize)
                .and_then(|pixels| pixels.checked_mul(stride))
                .ok_or("EINVAL:image too large")?;

            let decompressed;
            let bytes = if cmd.compressed {
                // Raw pixels decompress to exactly this many bytes, so the
                // inflater can be told the answer in advance; an image that
                // would not fit the budget is refused at the budget instead.
                decompressed =
                    inflate::zlib_decompress(payload, expected.min(budget)).map_err(zlib_error)?;
                &decompressed[..]
            } else {
                payload
            };
            if bytes.len() < expected {
                return Err("EINVAL:truncated payload");
            }

            let mut data = Vec::with_capacity((w as usize) * (h as usize) * 4);
            for px in bytes[..expected].chunks_exact(stride) {
                data.extend_from_slice(&[px[0], px[1], px[2]]);
                data.push(if stride == 4 { px[3] } else { 0xff });
            }
            Ok((w, h, data))
        }
    }
}

/// Map a decompression failure onto a protocol error response. The one
/// distinction worth keeping is "too big to hold" against "corrupt", because
/// an application can do something useful about the first.
fn zlib_error(err: InflateError) -> &'static str {
    match err {
        InflateError::TooLarge => "EINVAL:compressed payload exceeds the image budget",
        _ => "EINVAL:corrupt zlib payload",
    }
}

fn png_error(err: PngError) -> &'static str {
    match err {
        PngError::TooLarge | PngError::Deflate(InflateError::TooLarge) => {
            "EINVAL:PNG exceeds the image budget"
        }
        PngError::BadSignature => "EINVAL:not a PNG",
        PngError::Unsupported => "EINVAL:unsupported PNG variant",
        _ => "EINVAL:corrupt PNG",
    }
}

/// Decode base64, skipping whitespace. Invalid characters end the decode.
pub fn decode_base64(input: &[u8]) -> Vec<u8> {
    const INVALID: u8 = 0xff;
    let mut table = [INVALID; 256];
    for (value, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
        .iter()
        .enumerate()
    {
        table[c as usize] = value as u8;
    }

    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for &byte in input {
        if byte == b'=' {
            break;
        }
        if byte.is_ascii_whitespace() {
            continue;
        }
        let v = table[byte as usize];
        if v == INVALID {
            break;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Encode base64, used when tOS answers queries with payloads.
pub fn encode_base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for case in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            let encoded = encode_base64(case.as_bytes());
            assert_eq!(decode_base64(encoded.as_bytes()), case.as_bytes());
        }
    }

    #[test]
    fn base64_known_vector() {
        assert_eq!(encode_base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(decode_base64(b"Zm9vYmFy"), b"foobar");
    }

    #[test]
    fn parses_transmit_and_display() {
        let cmd = GraphicsCommand::parse(b"a=T,f=24,s=2,v=1,i=7;AAAA").unwrap();
        assert_eq!(cmd.action, Action::TransmitAndDisplay);
        assert_eq!(cmd.format, Format::Rgb);
        assert_eq!(cmd.width, 2);
        assert_eq!(cmd.height, 1);
        assert_eq!(cmd.image_id, 7);
    }

    #[test]
    fn stores_rgb_as_rgba() {
        let mut store = GraphicsStore::new(1 << 20);
        let payload = encode_base64(&[1, 2, 3, 4, 5, 6]);
        let cmd =
            GraphicsCommand::parse(format!("a=t,f=24,s=2,v=1,i=1;{payload}").as_bytes()).unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let img = store.image(id).unwrap();
        assert_eq!(img.data, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    /// Build a transmit command carrying `data` as its base64 payload.
    fn transmit(control: &str, data: &[u8]) -> GraphicsCommand {
        let payload = encode_base64(data);
        GraphicsCommand::parse(format!("{control};{payload}").as_bytes()).unwrap()
    }

    #[test]
    fn a_png_payload_is_decoded_to_rgba() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels: Vec<u8> = (0..2 * 2 * 4).map(|i| i as u8).collect();
        let cmd = transmit("a=t,f=100,i=1", &crate::png::tests::rgba_png(2, 2, &pixels));
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let img = store.image(id).unwrap();
        // The file's own dimensions are used, not the ones on the command.
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(img.data, pixels);
    }

    #[test]
    fn a_zlib_compressed_rgba_payload_is_decoded() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let cmd = transmit(
            "a=t,f=32,o=z,s=2,v=1,i=1",
            &crate::inflate::tests::zlib_stored(&pixels),
        );
        let id = store.store(&cmd, &cmd.payload).unwrap();
        assert_eq!(store.image(id).unwrap().data, pixels);
    }

    #[test]
    fn a_zlib_compressed_png_payload_is_decoded() {
        let mut store = GraphicsStore::new(1 << 20);
        let pixels: Vec<u8> = (0..2 * 4).map(|i| (i * 3) as u8).collect();
        let file = crate::png::tests::rgba_png(1, 2, &pixels);
        let cmd = transmit(
            "a=t,f=100,o=z,i=1",
            &crate::inflate::tests::zlib_stored(&file),
        );
        let id = store.store(&cmd, &cmd.payload).unwrap();
        assert_eq!(store.image(id).unwrap().data, pixels);
    }

    #[test]
    fn a_corrupt_png_is_an_error_not_a_panic() {
        let mut store = GraphicsStore::new(1 << 20);
        let full = crate::png::tests::rgba_png(2, 2, &[0x40; 2 * 2 * 4]);
        for cut in 0..full.len() {
            let cmd = transmit("a=t,f=100,i=1", &full[..cut]);
            assert!(store.store(&cmd, &cmd.payload).is_err());
        }
        // And a payload that is not a PNG at all.
        let cmd = transmit("a=t,f=100,i=1", b"not an image");
        assert_eq!(store.store(&cmd, &cmd.payload), Err("EINVAL:not a PNG"));
    }

    #[test]
    fn a_compressed_payload_that_would_burst_the_budget_is_refused() {
        // 4 MiB of zeroes offered to a store that can only hold 1 KiB.
        let mut store = GraphicsStore::new(1024);
        let bomb = crate::inflate::tests::zlib_stored(&vec![0u8; 4 << 20]);
        let cmd = transmit("a=t,f=32,o=z,s=1024,v=1024,i=1", &bomb);
        assert_eq!(
            store.store(&cmd, &cmd.payload),
            Err("EINVAL:compressed payload exceeds the image budget")
        );
    }

    #[test]
    fn a_png_larger_than_the_budget_is_refused_from_its_header() {
        let mut store = GraphicsStore::new(1024);
        let file = crate::png::tests::rgba_png(64, 64, &[0; 64 * 64 * 4]);
        let cmd = transmit("a=t,f=100,i=1", &file);
        assert_eq!(
            store.store(&cmd, &cmd.payload),
            Err("EINVAL:PNG exceeds the image budget")
        );
    }

    #[test]
    fn chunked_transmission_reassembles() {
        let mut store = GraphicsStore::new(1 << 20);
        let first = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,i=3,m=1;AAAA").unwrap();
        assert!(store.accumulate(&first).is_none());
        let last = GraphicsCommand::parse(b"m=0;BBBB").unwrap();
        let (cmd, payload) = store.accumulate(&last).unwrap();
        assert_eq!(cmd.image_id, 3);
        assert_eq!(payload.len(), 6);
    }

    #[test]
    fn a_differently_identified_command_does_not_join_a_pending_transfer() {
        let mut store = GraphicsStore::new(1 << 20);
        let abandoned = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,i=1,m=1;AAAA").unwrap();
        assert!(store.accumulate(&abandoned).is_none());

        // A complete command that identifies itself by image number must be
        // stored as itself, not appended to the stale buffer.
        let fresh = GraphicsCommand::parse(b"a=t,f=32,s=1,v=1,I=7;BBBBBB").unwrap();
        let (cmd, payload) = store.accumulate(&fresh).unwrap();
        assert_eq!(cmd.image_number, 7);
        assert_eq!(cmd.image_id, 0);
        assert_eq!(payload, fresh.payload);
    }

    #[test]
    fn placement_size_derived_from_cell_size() {
        let mut store = GraphicsStore::new(1 << 20);
        let data = vec![0u8; 20 * 32 * 4];
        let payload = encode_base64(&data);
        let cmd =
            GraphicsCommand::parse(format!("a=T,f=32,s=20,v=32,i=1;{payload}").as_bytes()).unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        let placement = store.place(&cmd, id, 0, 0, 8, 16).unwrap();
        let p = store.placement(placement).unwrap();
        assert_eq!(p.cols, 3); // 20px over 8px cells, rounded up
        assert_eq!(p.rows, 2); // 32px over 16px cells
    }

    /// Build a command with its payload already base64 encoded, the way it
    /// would arrive from an application.
    fn command(spec: &str, data: &[u8]) -> GraphicsCommand {
        let payload = encode_base64(data);
        GraphicsCommand::parse(format!("{spec};{payload}").as_bytes()).unwrap()
    }

    /// A one pixel image plus one extra frame, the smallest animation there is.
    fn two_frame_image(store: &mut GraphicsStore) {
        let base = command("a=t,f=32,s=1,v=1,i=1", &[255, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        let frame = command("a=f,f=32,s=1,v=1,i=1,z=40", &[0, 255, 0, 255]);
        store.store_frame(&frame, &frame.payload).unwrap();
        let play = command("a=a,i=1,r=1,z=40,s=3", &[]);
        store.control_animation(&play).unwrap();
    }

    #[test]
    fn animation_keys_reuse_the_placement_keys() {
        let cmd =
            GraphicsCommand::parse(b"a=f,i=1,r=3,c=2,x=4,y=5,s=6,v=7,z=90,X=1,Y=255").unwrap();
        assert_eq!(cmd.action, Action::TransmitFrame);
        assert_eq!(cmd.frame_number(), 3);
        assert_eq!(cmd.base_frame(), 2);
        assert_eq!(cmd.frame_rect(), (4, 5, 6, 7));
        assert_eq!(cmd.frame_gap(), 90);
        assert!(cmd.frame_overwrites());
        assert_eq!(cmd.frame_background(), 255);

        let control = GraphicsCommand::parse(b"a=a,i=1,s=3,v=4,c=2").unwrap();
        assert_eq!(control.action, Action::AnimationControl);
        assert_eq!(control.animation_state(), 3);
        assert_eq!(control.animation_loops(), 4);
        assert_eq!(control.base_frame(), 2);
    }

    #[test]
    fn a_frame_becomes_visible_when_its_gap_has_elapsed() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();

        // The first call only starts the clock, so the root frame stays up.
        assert!(store.advance_animations(start).is_empty());
        assert_eq!(store.image(1).unwrap().data, vec![255, 0, 0, 255]);

        assert!(store
            .advance_animations(start + Duration::from_millis(39))
            .is_empty());
        assert_eq!(store.image(1).unwrap().current_frame(), 1);

        assert_eq!(
            store.advance_animations(start + Duration::from_millis(40)),
            vec![1]
        );
        assert_eq!(store.image(1).unwrap().current_frame(), 2);
        assert_eq!(store.image(1).unwrap().data, vec![0, 255, 0, 255]);
    }

    #[test]
    fn the_generation_moves_when_pixels_are_replaced() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);
        let before = store.image(1).unwrap().generation();

        // Adding a frame does not change what is on screen.
        let extra = command("a=f,f=32,s=1,v=1,i=1,z=40", &[0, 0, 255, 255]);
        store.store_frame(&extra, &extra.payload).unwrap();
        assert_eq!(store.image(1).unwrap().generation(), before);

        // Rewriting the frame that is on screen is new pixels.
        assert_eq!(store.image(1).unwrap().current_frame(), 1);
        let edit = command("a=f,f=32,s=1,v=1,i=1,r=1", &[9, 9, 9, 255]);
        store.store_frame(&edit, &edit.payload).unwrap();
        assert!(store.image(1).unwrap().generation() > before);
    }

    #[test]
    fn a_long_stall_does_not_replay_itself() {
        // A tick that arrives ten seconds late crosses at most one cycle. The
        // seconds it did not use belong to frames nobody saw, so they have to
        // be dropped: kept, the next tick would cross another cycle, and the
        // one after that, racing through the animation while it looked frozen.
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);

        store.advance_animations(start + Duration::from_secs(10));
        let after_stall = store.image(1).unwrap().current_frame();

        // The tick right after the stall is only a millisecond later, so
        // nothing is owed and the frame must hold still.
        store.advance_animations(start + Duration::from_secs(10) + Duration::from_millis(1));
        assert_eq!(
            store.image(1).unwrap().current_frame(),
            after_stall,
            "the stall was replayed instead of being caught up on"
        );

        // And one gap later it moves exactly one frame, as usual.
        store.advance_animations(start + Duration::from_secs(10) + Duration::from_millis(40));
        assert_ne!(store.image(1).unwrap().current_frame(), after_stall);
    }

    #[test]
    fn showing_a_different_frame_is_not_a_new_generation() {
        // The pixels of each frame are untouched by the animation running, so
        // the generation holds still and the frame number is what moves. A
        // cache pairs the two; a generation that ticked here would hand a
        // looping animation a fresh key every time round and it would rebuild
        // work it already had.
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);
        let generation = store.image(1).unwrap().generation();
        let frame = store.image(1).unwrap().current_frame();

        store.advance_animations(start + Duration::from_millis(40));
        let image = store.image(1).unwrap();
        assert_eq!(image.generation(), generation, "the pixels did not change");
        assert_ne!(image.current_frame(), frame, "a different frame is up");
    }

    #[test]
    fn an_animation_loops_back_to_its_first_frame() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);
        store.advance_animations(start + Duration::from_millis(40));
        store.advance_animations(start + Duration::from_millis(80));
        assert_eq!(store.image(1).unwrap().current_frame(), 1);
        assert_eq!(
            store.image(1).unwrap().animation_state(),
            AnimationState::Running
        );
    }

    #[test]
    fn an_animation_stops_when_its_loops_run_out() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        // v=2 means play the animation twice: one loop back to the start.
        let once = command("a=a,i=1,v=2", &[]);
        store.control_animation(&once).unwrap();

        let start = Instant::now();
        store.advance_animations(start);
        for step in 1..=4 {
            store.advance_animations(start + Duration::from_millis(40 * step));
        }
        assert_eq!(
            store.image(1).unwrap().animation_state(),
            AnimationState::Stopped
        );
        assert_eq!(store.image(1).unwrap().current_frame(), 2);
    }

    #[test]
    fn a_stopped_animation_holds_its_frame() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);
        let stop = command("a=a,i=1,s=1", &[]);
        store.control_animation(&stop).unwrap();
        assert!(store
            .advance_animations(start + Duration::from_millis(4000))
            .is_empty());
        assert_eq!(store.image(1).unwrap().current_frame(), 1);
        assert_eq!(store.next_animation_delay(start), None);
    }

    #[test]
    fn a_late_tick_crosses_every_frame_whose_gap_passed() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=1,v=1,i=1", &[0, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        for value in [1u8, 2] {
            let frame = command("a=f,f=32,s=1,v=1,i=1,z=10", &[value, 0, 0, 255]);
            store.store_frame(&frame, &frame.payload).unwrap();
        }
        let play = command("a=a,i=1,r=1,z=10,s=3", &[]);
        store.control_animation(&play).unwrap();

        let start = Instant::now();
        store.advance_animations(start);
        store.advance_animations(start + Duration::from_millis(25));
        assert_eq!(store.image(1).unwrap().current_frame(), 3);
        // The five milliseconds left over carry into the next frame.
        store.advance_animations(start + Duration::from_millis(30));
        assert_eq!(store.image(1).unwrap().current_frame(), 1);
    }

    #[test]
    fn a_gapless_frame_is_passed_straight_over() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        // A third frame with a negative gap exists only to be composed onto.
        let hidden = command("a=f,f=32,s=1,v=1,i=1,z=-1", &[0, 0, 255, 255]);
        store.store_frame(&hidden, &hidden.payload).unwrap();

        let start = Instant::now();
        store.advance_animations(start);
        store.advance_animations(start + Duration::from_millis(40));
        assert_eq!(store.image(1).unwrap().current_frame(), 2);
        // Frame three has no gap, so the step from two lands back on one.
        store.advance_animations(start + Duration::from_millis(80));
        assert_eq!(store.image(1).unwrap().current_frame(), 1);
    }

    #[test]
    fn an_animation_with_no_gaps_at_all_never_runs() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=1,v=1,i=1", &[0, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        let frame = command("a=f,f=32,s=1,v=1,i=1,z=-1", &[1, 0, 0, 255]);
        store.store_frame(&frame, &frame.payload).unwrap();
        let play = command("a=a,i=1,r=1,z=-1,s=3", &[]);
        store.control_animation(&play).unwrap();

        let start = Instant::now();
        store.advance_animations(start);
        assert!(store
            .advance_animations(start + Duration::from_millis(10_000))
            .is_empty());
        assert_eq!(store.next_animation_delay(start), None);
    }

    #[test]
    fn a_frame_composes_onto_the_frame_it_names() {
        let mut store = GraphicsStore::new(1 << 20);
        // A 2x1 image: red, then green.
        let base = command("a=t,f=32,s=2,v=1,i=1", &[255, 0, 0, 255, 0, 255, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        // One blue pixel over the right hand half, on top of frame 1.
        let frame = command("a=f,f=32,s=1,v=1,x=1,y=0,c=1,i=1,z=40", &[0, 0, 255, 255]);
        store.store_frame(&frame, &frame.payload).unwrap();

        let show = command("a=a,i=1,c=2", &[]);
        assert!(store.control_animation(&show).unwrap());
        assert_eq!(
            store.image(1).unwrap().data,
            vec![255, 0, 0, 255, 0, 0, 255, 255]
        );
    }

    #[test]
    fn a_frame_without_a_base_starts_from_the_background_colour() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=2,v=1,i=1", &[255, 0, 0, 255, 255, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        // Y is 32-bit RGBA: opaque black background under one white pixel.
        let frame = command("a=f,f=32,s=1,v=1,x=0,i=1,z=40,Y=255", &[255, 255, 255, 255]);
        store.store_frame(&frame, &frame.payload).unwrap();
        let show = command("a=a,i=1,c=2", &[]);
        store.control_animation(&show).unwrap();
        assert_eq!(
            store.image(1).unwrap().data,
            vec![255, 255, 255, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn a_transparent_frame_pixel_blends_with_the_base() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=1,v=1,i=1", &[0, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        // Half opaque white over black is grey; the same data with X=1 is not.
        let blended = command("a=f,f=32,s=1,v=1,c=1,i=1,z=40", &[255, 255, 255, 128]);
        store.store_frame(&blended, &blended.payload).unwrap();
        let overwrite = command("a=f,f=32,s=1,v=1,c=1,i=1,z=40,X=1", &[255, 255, 255, 128]);
        store.store_frame(&overwrite, &overwrite.payload).unwrap();

        store
            .control_animation(&command("a=a,i=1,c=2", &[]))
            .unwrap();
        assert_eq!(store.image(1).unwrap().data, vec![128, 128, 128, 255]);
        store
            .control_animation(&command("a=a,i=1,c=3", &[]))
            .unwrap();
        assert_eq!(store.image(1).unwrap().data, vec![255, 255, 255, 128]);
    }

    #[test]
    fn a_frame_can_be_rewritten_in_place() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let before = store.image(1).unwrap().frame_count();
        let edit = command("a=f,f=32,s=1,v=1,i=1,r=2,z=90", &[0, 0, 255, 255]);
        store.store_frame(&edit, &edit.payload).unwrap();
        assert_eq!(store.image(1).unwrap().frame_count(), before);
        assert_eq!(store.image(1).unwrap().frame_gap(2), Some(90));

        let start = Instant::now();
        store.advance_animations(start);
        store.advance_animations(start + Duration::from_millis(40));
        assert_eq!(store.image(1).unwrap().data, vec![0, 0, 255, 255]);
    }

    #[test]
    fn a_frame_gap_can_be_changed_without_new_data() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        store
            .control_animation(&command("a=a,i=1,r=2,z=200", &[]))
            .unwrap();
        assert_eq!(store.image(1).unwrap().frame_gap(2), Some(200));
        // Zero means "leave it alone", not "no gap".
        store
            .control_animation(&command("a=a,i=1,r=2,z=0", &[]))
            .unwrap();
        assert_eq!(store.image(1).unwrap().frame_gap(2), Some(200));
    }

    #[test]
    fn malformed_frames_are_rejected_rather_than_stored() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=2,v=2,i=1", &[0u8; 16]);
        store.store(&base, &base.payload).unwrap();

        for spec in [
            "a=f,f=32,s=2,v=2,i=9",     // no such image
            "a=f,f=32,s=2,v=2,x=1,i=1", // rectangle runs past the edge
            "a=f,f=32,s=2,v=2,c=7,i=1", // no such base frame
            "a=f,f=32,s=2,v=2,r=9,i=1", // frame past the end
            "a=f,f=100,s=2,v=2,i=1",    // PNG
            "a=f,f=32,s=2,v=2,i=1,o=z", // compressed
            "a=f,f=32,s=2,v=2,i=1,t=f", // file backed
        ] {
            let cmd = command(spec, &[0u8; 16]);
            assert!(store.store_frame(&cmd, &cmd.payload).is_err(), "{spec}");
        }

        // Short payloads are refused before anything is copied out of them.
        let truncated = command("a=f,f=32,s=2,v=2,i=1", &[0u8; 15]);
        assert!(store.store_frame(&truncated, &truncated.payload).is_err());
        assert_eq!(store.image(1).unwrap().frame_count(), 1);
    }

    #[test]
    fn animation_control_for_an_unknown_image_is_an_error() {
        let mut store = GraphicsStore::new(1 << 20);
        assert!(store
            .control_animation(&command("a=a,i=4,s=3", &[]))
            .is_err());
    }

    #[test]
    fn an_animation_cannot_outgrow_the_graphics_budget() {
        // Room for the base image and exactly one more frame.
        let mut store = GraphicsStore::new(8);
        let base = command("a=t,f=32,s=1,v=1,i=1", &[255, 0, 0, 255]);
        store.store(&base, &base.payload).unwrap();
        let first = command("a=f,f=32,s=1,v=1,i=1,z=40", &[0, 255, 0, 255]);
        assert!(store.store_frame(&first, &first.payload).is_ok());
        let second = command("a=f,f=32,s=1,v=1,i=1,z=40", &[0, 0, 255, 255]);
        assert!(store.store_frame(&second, &second.payload).is_err());
        assert_eq!(store.image(1).unwrap().frame_count(), 2);
    }

    #[test]
    fn retransmitting_an_image_drops_the_frames_it_had() {
        let mut store = GraphicsStore::new(16);
        two_frame_image(&mut store);
        assert_eq!(store.image(1).unwrap().frame_count(), 2);
        let again = command("a=t,f=32,s=1,v=1,i=1", &[1, 2, 3, 4]);
        store.store(&again, &again.payload).unwrap();
        let image = store.image(1).unwrap();
        assert_eq!(image.frame_count(), 1);
        assert_eq!(image.animation_state(), AnimationState::Stopped);
        // The budget must have been given the frames back, or a later frame
        // would be refused for space that nothing is using.
        let frame = command("a=f,f=32,s=1,v=1,i=1,z=40", &[9, 9, 9, 9]);
        assert!(store.store_frame(&frame, &frame.payload).is_ok());
    }

    #[test]
    fn a_still_image_never_animates() {
        let mut store = GraphicsStore::new(1 << 20);
        let base = command("a=t,f=32,s=1,v=1,i=1", &[1, 2, 3, 4]);
        store.store(&base, &base.payload).unwrap();
        store
            .control_animation(&command("a=a,i=1,s=3", &[]))
            .unwrap();
        let start = Instant::now();
        store.advance_animations(start);
        assert!(store
            .advance_animations(start + Duration::from_millis(1000))
            .is_empty());
        assert_eq!(store.next_animation_delay(start), None);
    }

    #[test]
    fn the_delay_to_the_next_frame_counts_down() {
        let mut store = GraphicsStore::new(1 << 20);
        two_frame_image(&mut store);
        let start = Instant::now();
        store.advance_animations(start);
        assert_eq!(
            store.next_animation_delay(start + Duration::from_millis(10)),
            Some(Duration::from_millis(30))
        );
        assert_eq!(
            store.next_animation_delay(start + Duration::from_millis(90)),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn scrolling_drops_offscreen_placements() {
        let mut store = GraphicsStore::new(1 << 20);
        let data = vec![0u8; 8 * 16 * 4];
        let payload = encode_base64(&data);
        let cmd =
            GraphicsCommand::parse(format!("a=T,f=32,s=8,v=16,i=1;{payload}").as_bytes()).unwrap();
        let id = store.store(&cmd, &cmd.payload).unwrap();
        store.place(&cmd, id, 0, 0, 8, 16);
        assert_eq!(store.placements().count(), 1);
        store.scroll(1);
        assert_eq!(store.placements().count(), 0);
    }
}
