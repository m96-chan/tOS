//! `tos-preview`: put a picture in a pane.

use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::process::ExitCode;

use tos_preview::fit::{self, Metrics};
use tos_preview::transmit::{self, Payload};
use tos_term::png;

const USAGE: &str = "\
tos-preview, the tOS image viewer

usage: tos-preview [options] <file>

options:
  --rgba         send decoded pixels instead of the file itself
  --upscale      enlarge a picture smaller than the pane
  --cell WxH     pixel size of one cell, for a terminal that will not say
  -h, --help     show this message
  -V, --version  show the version

The picture is scaled to fit the pane, keeping its aspect ratio, and drawn
where the cursor is. PNG is the only format; see the crate documentation for
why JPEG is not.
";

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("tos-preview: {message}");
            ExitCode::FAILURE
        }
    }
}

/// The options a command line comes to.
struct Options {
    path: String,
    rgba: bool,
    upscale: bool,
    cell: Option<(u32, u32)>,
}

fn run() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("tos-preview {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }

    let options = match parse(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("tos-preview: {message}");
            eprintln!("try 'tos-preview --help'");
            return Ok(ExitCode::from(2));
        }
    };

    // Checked before the file is read, so that a `tos-preview x.png | less`
    // says what is wrong instead of filling a pipe with escape sequences that
    // mean nothing to whatever is on the far end.
    let stdout = io::stdout();
    let fd = stdout.as_raw_fd();
    if unsafe { libc::isatty(fd) } != 1 {
        return Err("stdout is not a terminal, so there is nowhere to put a picture".to_string());
    }

    let file = std::fs::read(&options.path).map_err(|e| format!("{}: {e}", options.path))?;

    // The same budget the terminal will apply to the payload, so a file that
    // is going to be refused is refused here, with a sentence, rather than
    // after it has been base64-encoded and written out.
    let budget = tos_term::TerminalConfig::default().graphics_budget;
    let image = png::decode(&file, budget).map_err(|e| format!("{}: {e}", options.path))?;

    let mut metrics =
        Metrics::probe(fd).map_err(|e| format!("cannot measure the terminal: {e}"))?;
    if let Some(cell) = options.cell {
        metrics = metrics.with_cell(cell);
    }

    let cells = fit::fit(image.width, image.height, metrics, options.upscale);
    let (payload, data) = if options.rgba {
        (
            Payload::Rgba {
                width: image.width,
                height: image.height,
            },
            image.rgba,
        )
    } else {
        (Payload::Png, file)
    };

    let bytes = transmit::sequence(payload, &data, cells);
    let mut out = stdout.lock();
    out.write_all(&bytes).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())?;
    Ok(ExitCode::SUCCESS)
}

fn parse(args: &[String]) -> Result<Options, String> {
    let mut path: Option<String> = None;
    let mut rgba = false;
    let mut upscale = false;
    let mut cell = None;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--rgba" => rgba = true,
            "--upscale" => upscale = true,
            "--cell" => {
                let value = rest
                    .next()
                    .ok_or("--cell needs a size, as in --cell 8x16")?;
                cell = Some(parse_cell(value)?);
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => {
                if path.replace(other.to_string()).is_some() {
                    return Err("only one file at a time".to_string());
                }
            }
        }
    }

    Ok(Options {
        path: path.ok_or("no file given")?,
        rgba,
        upscale,
        cell,
    })
}

fn parse_cell(value: &str) -> Result<(u32, u32), String> {
    let bad = || format!("not a cell size: {value}");
    let (w, h) = value.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u32 = w.parse().map_err(|_| bad())?;
    let h: u32 = h.parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok((w, h))
}
