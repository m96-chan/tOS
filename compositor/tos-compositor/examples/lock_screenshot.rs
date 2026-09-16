//! Render the lock screen — picture and all — to a PPM image.
//!
//! ```text
//! cargo run --example lock_screenshot -- /tmp/tos-lock.ppm [WIDTHxHEIGHT]
//! ```
//!
//! The companion to `login_screenshot`, and for the same reason: the two
//! screens differ in where the picture goes, and where a picture goes is
//! exactly what a test asserting about cells cannot show anybody.
//!
//! Unlike that one this starts a session and then locks it, because a lock is
//! defined by having one behind it. The session runs `sleep` rather than a
//! shell so the frame does not depend on whose `.bashrc` is on the machine.

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tos-lock.ppm".to_string());
    let size = std::env::args()
        .nth(2)
        .and_then(|arg| {
            let (w, h) = arg.split_once(['x', 'X'])?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280, 720));

    // A credential of this example's own making, so it draws the screen on a
    // machine whose root has no password as well as on one that has.
    let credential = std::env::temp_dir().join("tos-lock-screenshot-shadow");
    let hash = tos_crypt::sha512crypt::hash(b"hunter2", b"tOSlock");
    std::fs::write(&credential, format!("tos:{hash}:::::::\n"))?;

    let config = Config {
        credential,
        credential_user: "tos".into(),
        command: Some(vec!["sleep".into(), "60".into()]),
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, size, None)?;
    if !compositor.lock_session() {
        eprintln!("the session would not lock; there is no credential to lock it with");
        return Ok(());
    }

    let mut framebuffer = OwnedFramebuffer::new(size.0, size.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    std::fs::write(&path, framebuffer.to_ppm())?;
    println!("wrote {path}");
    Ok(())
}
