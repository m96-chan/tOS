//! Render a multi-pane tOS session to a PPM image.
//!
//! ```text
//! cargo run --example layout_screenshot -- /tmp/tos.ppm
//! ```
//!
//! This drives the compositor library the same way the binary does, but with
//! a scripted layout instead of live input, which is how the screenshots in
//! the documentation are produced.

use std::time::{Duration, Instant};

use tos_compositor::{Compositor, Config};
use tos_render::OwnedFramebuffer;
use tos_session::{Action, Axis, Direction};

const SIZE: (u32, u32) = (1280, 720);

fn main() -> std::io::Result<()> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "tos-layout.ppm".to_string());

    let config = Config {
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            // Each pane shows something different so the layout is legible.
            "case $TOS_PANE in \
               *) printf '\\033[1;34m%s\\033[0m\\n' \"$(uname -sm)\"; \
                  printf 'pane pid %s\\n\\n' $$; \
                  printf '\\033[31mred\\033[32m green\\033[33m yellow\\033[34m blue\\033[0m\\n'; \
                  printf '\\033[38;2;255;140;0mtruecolour\\033[0m \\033[4munderline\\033[0m \\033[1mbold\\033[0m\\n\\n'; \
                  printf '\\342\\224\\214\\342\\224\\200\\342\\224\\200\\342\\224\\200\\342\\224\\220\\n'; \
                  printf '\\342\\224\\202 ok \\342\\224\\202\\n'; \
                  printf '\\342\\224\\224\\342\\224\\200\\342\\224\\200\\342\\224\\200\\342\\224\\230\\n'; \
                  esac; sleep 30"
                .into(),
        ]),
        ..Config::default()
    };

    let mut compositor = Compositor::new(config, SIZE, None)?;
    compositor.perform(Action::Split(Axis::Columns));
    compositor.perform(Action::Split(Axis::Rows));
    compositor.perform(Action::Focus(Direction::Left));

    // Let every shell draw before the picture is taken.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        compositor.pump_panes();
        std::thread::sleep(Duration::from_millis(50));
    }

    let mut framebuffer = OwnedFramebuffer::new(SIZE.0, SIZE.1);
    {
        let mut surface = framebuffer.surface();
        compositor.render_frame(&mut surface, false);
    }
    std::fs::write(&path, framebuffer.to_ppm())?;
    println!("wrote {path}");
    Ok(())
}
