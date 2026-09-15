//! Render the login screen — picture and all — to a PPM image.
//!
//! ```text
//! cargo run --example login_screenshot -- /tmp/tos-login.ppm [WIDTHxHEIGHT]
//! ```
//!
//! The screen this draws is the one a machine with a password comes up at
//! (#112), and the picture over the box is #132. It is the one part of tOS
//! that is drawn in pixels rather than cells, so it is the one part a test
//! asserting about cells cannot show anybody; this is how it gets looked at.

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tos-login.ppm".to_string());
    let size = std::env::args()
        .nth(2)
        .and_then(|arg| {
            let (w, h) = arg.split_once(['x', 'X'])?;
            Some((w.parse().ok()?, h.parse().ok()?))
        })
        .unwrap_or((1280, 720));

    // A credential of this example's own making, so it draws the screen on a
    // machine whose root has no password as well as on one that has.
    let credential = std::env::temp_dir().join("tos-login-screenshot-shadow");
    let hash = tos_crypt::sha512crypt::hash(b"hunter2", b"tOSlogin");
    std::fs::write(&credential, format!("tos:{hash}:::::::\n"))?;

    let config = Config {
        credential,
        credential_user: "tos".into(),
        gated: true,
        ..Config::default()
    };
    let mut compositor = Compositor::new(config, size, None)?;

    let mut framebuffer = OwnedFramebuffer::new(size.0, size.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    std::fs::write(&path, framebuffer.to_ppm())?;
    println!("wrote {path}");
    Ok(())
}
