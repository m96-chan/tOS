//! `tos-browser`: a web page in a pane.

use std::process::ExitCode;

use tos_browser::app::{self, Options};
use tos_browser::engine;

const USAGE: &str = "\
tos-browser, the tOS web browser

usage: tos-browser [options] [url]

options:
  -h, --help     show this message
  -V, --version  show the version

The page is rendered by a headless Chromium, which this program starts and
stops. It is looked for in $TOS_BROWSER_ENGINE first, then on PATH as
chromium-shell, chromium, chromium-browser or google-chrome. tOS does not
ship one; install the one you want.

keys:
  ctrl+l         type a url
  ctrl+r         reload
  alt+left/right back and forward
  ctrl+t         a new tab, with the cursor in the url bar
  ctrl+w         close this tab; closing the last one quits
  ctrl+tab       the next tab, ctrl+shift+tab the one before
  alt+1 .. alt+9 the nth tab
  ctrl+q         quit
Everything else goes to the page. A link that asks for a new window gets a
new tab, and the tab is switched to.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tos-browser {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let mut url = None;
    for arg in &args {
        if arg.starts_with('-') && arg.len() > 1 {
            eprintln!("tos-browser: unknown option: {arg}");
            eprintln!("try 'tos-browser --help'");
            return ExitCode::from(2);
        }
        if url.replace(arg.clone()).is_some() {
            eprintln!("tos-browser: one page at a time");
            return ExitCode::from(2);
        }
    }

    match app::run(Options {
        url: url.unwrap_or_else(|| "about:blank".to_string()),
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // The terminal has already been put back by the time this runs, so
            // the sentence lands in a shell the person can read it in.
            eprintln!("tos-browser: {message}");
            if message.contains(engine::ENGINE_ENV) || message.contains("PATH") {
                eprintln!(
                    "tos-browser: install a chromium, or set {}",
                    engine::ENGINE_ENV
                );
            }
            ExitCode::FAILURE
        }
    }
}
