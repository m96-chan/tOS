//! DRM/KMS output.
//!
//! This is the backend the architecture is actually about: tOS opens the
//! kernel's display device directly, picks a connector and a mode, allocates
//! dumb buffers, and flips between them. There is no X11, no Wayland and no
//! compositor underneath.
//!
//! The ioctl structures are declared here rather than pulled from a crate so
//! that the dependency surface of the first milestone stays at libc.

use std::ffi::CString;
use std::io;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};

use tos_render::Surface;

use crate::display::Display;

// ---------------------------------------------------------------------------
// ioctl plumbing
// ---------------------------------------------------------------------------

const IOC_NONE: u64 = 0;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const DRM_IOCTL_BASE: u64 = b'd' as u64;

const fn ioc(dir: u64, nr: u64, size: u64) -> u64 {
    (dir << 30) | (size << 16) | (DRM_IOCTL_BASE << 8) | nr
}

const fn iowr<T>(nr: u64) -> u64 {
    ioc(IOC_READ | IOC_WRITE, nr, std::mem::size_of::<T>() as u64)
}

const fn io_only(nr: u64) -> u64 {
    ioc(IOC_NONE, nr, 0)
}

/// `struct drm_mode_modeinfo`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ModeInfo {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub kind: u32,
    pub name: [libc::c_char; 32],
}

impl Default for ModeInfo {
    fn default() -> Self {
        // All zeroes is a valid "no mode", which is what the kernel expects
        // when a CRTC is being disabled.
        unsafe { std::mem::zeroed() }
    }
}

impl ModeInfo {
    pub fn name(&self) -> String {
        // `c_char` is signed on x86_64 and unsigned on arm64, so this cast is
        // needed on one and a no-op on the other; clippy only sees whichever
        // it is compiling for.
        #[allow(clippy::unnecessary_cast)]
        let bytes: Vec<u8> = self
            .name
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    pub fn size(&self) -> (u32, u32) {
        (self.hdisplay as u32, self.vdisplay as u32)
    }

    /// Area in pixels, used to pick the largest mode.
    fn area(&self) -> u64 {
        self.hdisplay as u64 * self.vdisplay as u64
    }

    /// Whether the driver marked this as the panel's native mode.
    fn is_preferred(&self) -> bool {
        const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
        self.kind & DRM_MODE_TYPE_PREFERRED != 0
    }
}

#[repr(C)]
#[derive(Default)]
struct CardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
struct Crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: ModeInfo,
}

impl Default for Crtc {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Default)]
struct FbCmd {
    fb_id: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bpp: u32,
    depth: u32,
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct MapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct DestroyDumb {
    handle: u32,
}

#[repr(C)]
#[derive(Default)]
struct PageFlip {
    crtc_id: u32,
    fb_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

fn ioctl_num<T>(nr: u64) -> u64 {
    iowr::<T>(nr)
}

const DRM_IOCTL_SET_MASTER: u64 = io_only(0x1e);
const DRM_IOCTL_DROP_MASTER: u64 = io_only(0x1f);
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
/// How long to wait for a page flip to report back before falling back to a
/// mode set. Two frames at 60Hz, so a slow panel is not given up on early.
const FLIP_TIMEOUT_MS: u64 = 34;

fn ioctl<T>(fd: RawFd, request: u64, arg: &mut T) -> io::Result<()> {
    let result = unsafe { libc::ioctl(fd, request as _, arg as *mut T) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn ioctl_none(fd: RawFd, request: u64) -> io::Result<()> {
    let result = unsafe { libc::ioctl(fd, request as _, 0) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Device and mode discovery
// ---------------------------------------------------------------------------

/// A connector that has a display plugged into it.
#[derive(Debug, Clone)]
pub struct ConnectedOutput {
    pub connector_id: u32,
    pub encoder_id: u32,
    pub crtc_id: u32,
    pub modes: Vec<ModeInfo>,
    pub mm_width: u32,
    pub mm_height: u32,
}

impl ConnectedOutput {
    /// The mode tOS should use: the panel's preferred one, or the largest.
    pub fn best_mode(&self) -> Option<ModeInfo> {
        self.modes
            .iter()
            .find(|m| m.is_preferred())
            .or_else(|| self.modes.iter().max_by_key(|m| m.area()))
            .copied()
    }
}

/// An open DRM device.
pub struct Card {
    fd: RawFd,
    path: PathBuf,
    is_master: bool,
}

impl Card {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Card> {
        let path = path.as_ref().to_path_buf();
        let c_path = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad device path"))?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Card {
            fd,
            path,
            is_master: false,
        })
    }

    /// Open the first card that has a connected display.
    pub fn open_first() -> io::Result<(Card, ConnectedOutput)> {
        let mut last_error = io::Error::new(io::ErrorKind::NotFound, "no DRM device found");
        for index in 0..8 {
            let path = format!("/dev/dri/card{index}");
            if !Path::new(&path).exists() {
                continue;
            }
            match Card::open(&path) {
                Ok(card) => match card.connected_output() {
                    Ok(Some(output)) => return Ok((card, output)),
                    Ok(None) => {
                        last_error =
                            io::Error::new(io::ErrorKind::NotFound, "no display connected");
                    }
                    Err(e) => last_error = e,
                },
                Err(e) => last_error = e,
            }
        }
        Err(last_error)
    }

    pub fn fd(&self) -> RawFd {
        self.fd
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Become DRM master, which is required to set a mode.
    pub fn set_master(&mut self) -> io::Result<()> {
        ioctl_none(self.fd, DRM_IOCTL_SET_MASTER)?;
        self.is_master = true;
        Ok(())
    }

    pub fn drop_master(&mut self) -> io::Result<()> {
        ioctl_none(self.fd, DRM_IOCTL_DROP_MASTER)?;
        self.is_master = false;
        Ok(())
    }

    /// Find the first connector with a display attached and a usable CRTC.
    pub fn connected_output(&self) -> io::Result<Option<ConnectedOutput>> {
        let mut res = CardRes::default();
        ioctl(self.fd, ioctl_num::<CardRes>(0xa0), &mut res)?;

        let mut connector_ids = vec![0u32; res.count_connectors as usize];
        let mut crtc_ids = vec![0u32; res.count_crtcs as usize];
        let mut encoder_ids = vec![0u32; res.count_encoders as usize];
        let mut fb_ids = vec![0u32; res.count_fbs as usize];
        if connector_ids.is_empty() || crtc_ids.is_empty() {
            return Ok(None);
        }

        let allocated = (connector_ids.len(), crtc_ids.len());
        res.connector_id_ptr = connector_ids.as_mut_ptr() as u64;
        res.crtc_id_ptr = crtc_ids.as_mut_ptr() as u64;
        res.encoder_id_ptr = encoder_ids.as_mut_ptr() as u64;
        res.fb_id_ptr = fb_ids.as_mut_ptr() as u64;
        ioctl(self.fd, ioctl_num::<CardRes>(0xa0), &mut res)?;
        // Same race as the connector query: a grown list means the arrays were
        // not filled in, and acting on the zeros would fail confusingly.
        if res.count_connectors as usize > allocated.0 || res.count_crtcs as usize > allocated.1 {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "the display configuration changed while it was being read",
            ));
        }
        connector_ids.truncate(res.count_connectors as usize);
        crtc_ids.truncate(res.count_crtcs as usize);

        for &connector_id in &connector_ids {
            let Some(output) = self.query_connector(connector_id, &crtc_ids)? else {
                continue;
            };
            return Ok(Some(output));
        }
        Ok(None)
    }

    fn query_connector(
        &self,
        connector_id: u32,
        crtc_ids: &[u32],
    ) -> io::Result<Option<ConnectedOutput>> {
        // First call returns the counts, second fills the arrays.
        let mut conn = GetConnector {
            connector_id,
            ..GetConnector::default()
        };
        ioctl(self.fd, ioctl_num::<GetConnector>(0xa7), &mut conn)?;

        const DRM_MODE_CONNECTED: u32 = 1;
        if conn.connection != DRM_MODE_CONNECTED || conn.count_modes == 0 {
            return Ok(None);
        }

        let mut modes = vec![ModeInfo::default(); conn.count_modes as usize];
        let mut encoders = vec![0u32; conn.count_encoders as usize];
        let mut props = vec![0u32; conn.count_props as usize];
        let mut prop_values = vec![0u64; conn.count_props as usize];

        let mut conn = GetConnector {
            connector_id,
            count_modes: modes.len() as u32,
            count_encoders: encoders.len() as u32,
            count_props: props.len() as u32,
            modes_ptr: modes.as_mut_ptr() as u64,
            encoders_ptr: encoders.as_mut_ptr() as u64,
            props_ptr: props.as_mut_ptr() as u64,
            prop_values_ptr: prop_values.as_mut_ptr() as u64,
            ..GetConnector::default()
        };
        ioctl(self.fd, ioctl_num::<GetConnector>(0xa7), &mut conn)?;
        // The kernel copies nothing and reports the larger count when the mode
        // list grew between the two calls, so a count above what was allocated
        // means the arrays were left untouched rather than filled.
        if (conn.count_modes as usize) > modes.len() {
            return Ok(None);
        }
        modes.truncate(conn.count_modes as usize);
        if modes.is_empty() {
            return Ok(None);
        }

        // Prefer the encoder the connector is already using.
        let encoder_id = if conn.encoder_id != 0 {
            conn.encoder_id
        } else {
            match encoders.first() {
                Some(&id) => id,
                None => return Ok(None),
            }
        };

        let mut encoder = GetEncoder {
            encoder_id,
            ..GetEncoder::default()
        };
        ioctl(self.fd, ioctl_num::<GetEncoder>(0xa6), &mut encoder)?;

        let crtc_id = if encoder.crtc_id != 0 {
            encoder.crtc_id
        } else {
            // Any CRTC the encoder says it can drive.
            match crtc_ids
                .iter()
                .enumerate()
                .find(|(i, _)| encoder.possible_crtcs & (1 << i) != 0)
            {
                Some((_, &id)) => id,
                None => return Ok(None),
            }
        };

        Ok(Some(ConnectedOutput {
            connector_id,
            encoder_id,
            crtc_id,
            modes,
            mm_width: conn.mm_width,
            mm_height: conn.mm_height,
        }))
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        if self.is_master {
            let _ = self.drop_master();
        }
        unsafe {
            libc::close(self.fd);
        }
    }
}

// ---------------------------------------------------------------------------
// Dumb buffers
// ---------------------------------------------------------------------------

/// A CPU addressable framebuffer owned by the kernel.
///
/// The device descriptor is kept so the buffer can release itself: leaving
/// that to the caller meant every error path leaked a framebuffer, a GEM
/// handle and a mapping, and a mapping still held keeps the memory alive even
/// after the device is closed.
struct DumbBuffer {
    fd: RawFd,
    handle: u32,
    fb_id: u32,
    width: u32,
    height: u32,
    /// Row length in bytes.
    pitch: u32,
    size: u64,
    map: *mut libc::c_void,
}

impl DumbBuffer {
    fn create(fd: RawFd, width: u32, height: u32) -> io::Result<DumbBuffer> {
        let mut create = CreateDumb {
            width,
            height,
            bpp: 32,
            ..CreateDumb::default()
        };
        ioctl(fd, ioctl_num::<CreateDumb>(0xb2), &mut create)?;

        let mut fb = FbCmd {
            width,
            height,
            pitch: create.pitch,
            bpp: 32,
            depth: 24,
            handle: create.handle,
            fb_id: 0,
        };
        if let Err(e) = ioctl(fd, ioctl_num::<FbCmd>(0xae), &mut fb) {
            let mut destroy = DestroyDumb {
                handle: create.handle,
            };
            let _ = ioctl(fd, ioctl_num::<DestroyDumb>(0xb4), &mut destroy);
            return Err(e);
        }

        // From here on the buffer owns the framebuffer and the handle, so any
        // later failure releases them through `Drop`.
        let mut buffer = DumbBuffer {
            fd,
            handle: create.handle,
            fb_id: fb.fb_id,
            width,
            height,
            pitch: create.pitch,
            size: create.size,
            map: libc::MAP_FAILED,
        };

        let mut map = MapDumb {
            handle: create.handle,
            ..MapDumb::default()
        };
        ioctl(fd, ioctl_num::<MapDumb>(0xb3), &mut map)?;

        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                create.size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                map.offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        buffer.map = ptr;

        // A freshly allocated buffer holds whatever was in memory before.
        unsafe {
            std::ptr::write_bytes(ptr as *mut u8, 0, create.size as usize);
        }
        Ok(buffer)
    }

    /// The buffer as a pixel slice.
    ///
    /// # Safety
    /// The caller must not hold two slices of the same buffer at once.
    fn pixels(&mut self) -> &mut [u32] {
        let count = (self.size / 4) as usize;
        unsafe { std::slice::from_raw_parts_mut(self.map as *mut u32, count) }
    }

    fn stride_pixels(&self) -> u32 {
        self.pitch / 4
    }
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        // Order matters: the mapping holds a reference to the object, so it
        // has to go before the handle is destroyed or the memory stays live.
        if self.map != libc::MAP_FAILED {
            unsafe {
                libc::munmap(self.map, self.size as usize);
            }
        }
        if self.fb_id != 0 {
            let mut fb_id = self.fb_id;
            let _ = ioctl(self.fd, ioctl_num::<u32>(0xaf), &mut fb_id);
        }
        let mut destroy = DestroyDumb {
            handle: self.handle,
        };
        let _ = ioctl(self.fd, ioctl_num::<DestroyDumb>(0xb4), &mut destroy);
    }
}

const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

/// The header every DRM event starts with.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct EventHeader {
    kind: u32,
    length: u32,
}

/// The event the kernel sends when a page flip completes.
#[repr(C)]
#[derive(Default)]
struct EventVblank {
    kind: u32,
    length: u32,
    user_data: u64,
    tv_sec: u32,
    tv_usec: u32,
    sequence: u32,
    crtc_id: u32,
}

// ---------------------------------------------------------------------------
// The display
// ---------------------------------------------------------------------------

/// A DRM/KMS display, double buffered with page flips.
pub struct DrmDisplay {
    card: Card,
    output: ConnectedOutput,
    mode: ModeInfo,
    buffers: [DumbBuffer; 2],
    /// Which buffer the next frame is drawn into.
    back: usize,
    /// Whether the CRTC has been configured yet.
    mode_set: bool,
    flip_pending: bool,
    saved_crtc: Crtc,
    /// Sequence number of the last completed flip, for frame pacing.
    last_sequence: u32,
}

impl DrmDisplay {
    /// Open the first connected display and take over the screen.
    pub fn open() -> io::Result<DrmDisplay> {
        let (card, output) = Card::open_first()?;
        DrmDisplay::with_card(card, output, None)
    }

    /// Open a specific device.
    pub fn open_path(path: impl AsRef<Path>) -> io::Result<DrmDisplay> {
        let card = Card::open(path)?;
        let output = card.connected_output()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no display connected to this card")
        })?;
        DrmDisplay::with_card(card, output, None)
    }

    fn with_card(
        mut card: Card,
        output: ConnectedOutput,
        preferred: Option<ModeInfo>,
    ) -> io::Result<DrmDisplay> {
        let mode = preferred
            .or_else(|| output.best_mode())
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "connector has no modes"))?;
        let (width, height) = mode.size();

        // Becoming master can fail when another compositor holds the device;
        // that is worth reporting clearly rather than failing later.
        card.set_master().map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot become DRM master, is another display server running? ({e})"),
            )
        })?;

        // Remember the current mode so the console can be restored on exit.
        let mut saved_crtc = Crtc {
            crtc_id: output.crtc_id,
            ..Crtc::default()
        };
        let _ = ioctl(card.fd(), ioctl_num::<Crtc>(0xa1), &mut saved_crtc);

        let front = DumbBuffer::create(card.fd(), width, height)?;
        let back = DumbBuffer::create(card.fd(), width, height)?;

        Ok(DrmDisplay {
            card,
            output,
            mode,
            buffers: [front, back],
            back: 1,
            mode_set: false,
            flip_pending: false,
            saved_crtc,
            last_sequence: 0,
        })
    }

    pub fn mode(&self) -> ModeInfo {
        self.mode
    }

    pub fn card_fd(&self) -> RawFd {
        self.card.fd()
    }

    /// Sequence number of the most recently completed page flip.
    pub fn last_sequence(&self) -> u32 {
        self.last_sequence
    }

    /// Configure the CRTC to scan out `fb_id`.
    fn set_crtc(&mut self, fb_id: u32) -> io::Result<()> {
        let mut connectors = [self.output.connector_id];
        let mut crtc = Crtc {
            set_connectors_ptr: connectors.as_mut_ptr() as u64,
            count_connectors: 1,
            crtc_id: self.output.crtc_id,
            fb_id,
            x: 0,
            y: 0,
            gamma_size: 0,
            mode_valid: 1,
            mode: self.mode,
        };
        ioctl(self.card.fd(), ioctl_num::<Crtc>(0xa2), &mut crtc)
    }

    fn page_flip(&mut self, fb_id: u32) -> io::Result<()> {
        let mut flip = PageFlip {
            crtc_id: self.output.crtc_id,
            fb_id,
            flags: DRM_MODE_PAGE_FLIP_EVENT,
            reserved: 0,
            user_data: 0,
        };
        ioctl(self.card.fd(), ioctl_num::<PageFlip>(0xb0), &mut flip)?;
        self.flip_pending = true;
        Ok(())
    }

    /// Wait for an outstanding flip to complete, so the back buffer is safe
    /// to draw into again.
    fn wait_for_flip(&mut self) -> io::Result<()> {
        if !self.flip_pending {
            return Ok(());
        }
        let fd = self.card.fd();
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Waiting must not be open ended, but it also must not give up on the
        // first interruption: drawing into a buffer the display is still
        // scanning out shows a torn frame, and the next flip is then rejected.
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(FLIP_TIMEOUT_MS);
        while self.flip_pending {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let ready = unsafe { libc::poll(&mut poll, 1, remaining.as_millis() as libc::c_int) };
            if ready < 0 {
                let err = io::Error::last_os_error();
                if err.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if ready == 0 {
                break;
            }
            let mut buf = [0u8; 1024];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                break;
            }
            self.consume_events(&buf[..n as usize]);
        }
        // A flip that never reported back leaves the compositor unsure which
        // buffer is live; the next frame falls back to a mode set, which is
        // unambiguous.
        if self.flip_pending {
            self.flip_pending = false;
            self.mode_set = false;
        }
        Ok(())
    }

    /// Walk the kernel's event stream, which packs variable length records.
    fn consume_events(&mut self, mut bytes: &[u8]) {
        const HEADER: usize = 8;
        while bytes.len() >= HEADER {
            let header: EventHeader =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const EventHeader) };
            let length = header.length as usize;
            if length < HEADER || length > bytes.len() {
                break;
            }
            if header.kind == DRM_EVENT_FLIP_COMPLETE
                && length >= std::mem::size_of::<EventVblank>()
            {
                let event: EventVblank =
                    unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const EventVblank) };
                self.last_sequence = event.sequence;
                // The buffer that was being scanned out is now free to draw on.
                self.flip_pending = false;
            }
            bytes = &bytes[length..];
        }
    }

    /// Restore the console's original mode.
    fn restore_crtc(&mut self) {
        if self.saved_crtc.mode_valid != 0 {
            let mut connectors = [self.output.connector_id];
            let mut crtc = Crtc {
                set_connectors_ptr: connectors.as_mut_ptr() as u64,
                count_connectors: 1,
                ..Crtc::default()
            };
            crtc.crtc_id = self.saved_crtc.crtc_id;
            crtc.fb_id = self.saved_crtc.fb_id;
            crtc.x = self.saved_crtc.x;
            crtc.y = self.saved_crtc.y;
            crtc.mode = self.saved_crtc.mode;
            crtc.mode_valid = 1;
            let _ = ioctl(self.card.fd(), ioctl_num::<Crtc>(0xa2), &mut crtc);
        }
    }
}

impl Display for DrmDisplay {
    fn size(&self) -> (u32, u32) {
        self.mode.size()
    }

    fn physical_size(&self) -> Option<(u32, u32)> {
        if self.output.mm_width == 0 {
            return None;
        }
        Some((self.output.mm_width, self.output.mm_height))
    }

    fn frame(&mut self, draw: &mut dyn FnMut(&mut Surface<'_>)) -> io::Result<()> {
        self.wait_for_flip()?;

        let index = self.back;
        let (width, height, stride) = {
            let buffer = &self.buffers[index];
            (buffer.width, buffer.height, buffer.stride_pixels())
        };
        {
            let buffer = &mut self.buffers[index];
            let pixels = buffer.pixels();
            let mut surface = Surface::new(pixels, width, height, stride);
            draw(&mut surface);
        }

        let fb_id = self.buffers[index].fb_id;
        if !self.mode_set {
            self.set_crtc(fb_id)?;
            self.mode_set = true;
        } else if let Err(e) = self.page_flip(fb_id) {
            // Some drivers reject flips while the CRTC is being reconfigured;
            // falling back to a mode set keeps the screen updating.
            if e.raw_os_error() == Some(libc::EBUSY) || e.raw_os_error() == Some(libc::EINVAL) {
                self.set_crtc(fb_id)?;
                self.flip_pending = false;
            } else {
                return Err(e);
            }
        }
        self.back = 1 - index;
        Ok(())
    }

    fn retains_contents(&self) -> bool {
        // Two buffers alternate, so a frame does not start from the previous
        // one; damage tracking would show stale content from two frames back.
        false
    }

    fn release(&mut self) -> io::Result<()> {
        self.card.drop_master()
    }

    fn restore(&mut self) -> io::Result<()> {
        self.card.set_master()?;
        self.mode_set = false;
        Ok(())
    }

    fn name(&self) -> String {
        let (w, h) = self.mode.size();
        format!(
            "{} connector {} at {w}x{h} ({})",
            self.card.path().display(),
            self.output.connector_id,
            self.mode.name()
        )
    }
}

impl Drop for DrmDisplay {
    fn drop(&mut self) {
        let _ = self.wait_for_flip();
        self.restore_crtc();
        // The buffers release themselves; they must do so before the device
        // descriptor is closed, which the field order guarantees.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_macros() {
        // DRM_IOCTL_MODE_GETRESOURCES is _IOWR('d', 0xA0, drm_mode_card_res).
        assert_eq!(
            ioctl_num::<CardRes>(0xa0),
            0xc000_64a0 | ((std::mem::size_of::<CardRes>() as u64) << 16)
        );
        assert_eq!(DRM_IOCTL_SET_MASTER, 0x0000_641e);
        assert_eq!(DRM_IOCTL_DROP_MASTER, 0x0000_641f);
    }

    #[test]
    fn structures_match_the_kernel_layout() {
        // These sizes come from drm_mode.h and must not drift.
        assert_eq!(std::mem::size_of::<ModeInfo>(), 68);
        assert_eq!(std::mem::size_of::<CardRes>(), 64);
        assert_eq!(std::mem::size_of::<GetConnector>(), 80);
        assert_eq!(std::mem::size_of::<GetEncoder>(), 20);
        assert_eq!(std::mem::size_of::<Crtc>(), 104);
        assert_eq!(std::mem::size_of::<CreateDumb>(), 32);
        assert_eq!(std::mem::size_of::<MapDumb>(), 16);
        assert_eq!(std::mem::size_of::<PageFlip>(), 24);
        assert_eq!(std::mem::size_of::<FbCmd>(), 28);
        assert_eq!(std::mem::size_of::<EventHeader>(), 8);
        assert_eq!(std::mem::size_of::<EventVblank>(), 32);
    }

    fn mode(width: u16, height: u16, preferred: bool) -> ModeInfo {
        ModeInfo {
            hdisplay: width,
            vdisplay: height,
            kind: if preferred { 1 << 3 } else { 0 },
            ..ModeInfo::default()
        }
    }

    #[test]
    fn the_preferred_mode_wins() {
        let output = ConnectedOutput {
            connector_id: 1,
            encoder_id: 1,
            crtc_id: 1,
            modes: vec![mode(3840, 2160, false), mode(1920, 1080, true)],
            mm_width: 300,
            mm_height: 200,
        };
        assert_eq!(output.best_mode().unwrap().size(), (1920, 1080));
    }

    #[test]
    fn without_a_preference_the_largest_mode_wins() {
        let output = ConnectedOutput {
            connector_id: 1,
            encoder_id: 1,
            crtc_id: 1,
            modes: vec![mode(1024, 768, false), mode(1920, 1080, false)],
            mm_width: 0,
            mm_height: 0,
        };
        assert_eq!(output.best_mode().unwrap().size(), (1920, 1080));
    }

    #[test]
    fn mode_names_are_read_as_text() {
        let mut m = mode(1920, 1080, true);
        for (i, b) in b"1920x1080".iter().enumerate() {
            m.name[i] = *b as libc::c_char;
        }
        assert_eq!(m.name(), "1920x1080");
    }
}
