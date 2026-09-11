//! The installer as the user meets it.
//!
//! A state machine over screens, fed key events and drawing into a
//! [`Screen`]. It owns no terminal and no disk, so the whole flow — including
//! the confirmation that guards the destructive part — can be driven in a
//! test.

use tos_input::{KeyCode, KeyEvent, Modifiers};

use crate::disk::Disk;
use crate::exec::Backend;
use crate::install::{Installer, Progress, StepOutcome};
use crate::motd;
use crate::plan::{Firmware, Plan, Settings};
use crate::ui::{Color, Rect, Screen, Style};

/// Which screen the installer is showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// The banner, and what is about to happen.
    Welcome,
    /// Choosing a disk.
    PickDisk,
    /// Host name and user name.
    Configure,
    /// The last chance to stop, with the disk name typed out.
    Confirm,
    /// Running.
    Installing,
    /// Finished, one way or the other.
    Finished,
}

/// Which field the configuration screen is editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Hostname,
    Username,
}

/// What the application wants the caller to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Nothing; keep going.
    None,
    /// Run the installation. The caller owns the backend, so it does this.
    Install,
    /// Leave the installer.
    Quit,
    /// Reboot into what was just installed.
    Reboot,
}

/// The installer's state.
pub struct App {
    pub stage: Stage,
    pub disks: Vec<Disk>,
    pub selected: usize,
    pub settings: Settings,
    pub field: Field,
    pub firmware: Firmware,
    /// What the user has typed on the confirmation screen.
    pub confirmation: String,
    pub progress: Progress,
    /// A message shown under the current screen.
    pub notice: Option<String>,
    /// True once the installation has finished successfully.
    pub installed: bool,
    /// Nothing will be written; the plan is only shown.
    pub dry_run: bool,
    quit: bool,
}

impl App {
    pub fn new(disks: Vec<Disk>, firmware: Firmware, dry_run: bool) -> App {
        // Start on the first disk that could actually be installed onto.
        let selected = disks.iter().position(Disk::is_installable).unwrap_or(0);
        App {
            stage: Stage::Welcome,
            disks,
            selected,
            settings: Settings::default(),
            field: Field::Hostname,
            firmware,
            confirmation: String::new(),
            progress: Progress::default(),
            notice: None,
            installed: false,
            dry_run,
            quit: false,
        }
    }

    pub fn should_quit(&self) -> bool {
        self.quit
    }

    /// The disk the cursor is on.
    pub fn disk(&self) -> Option<&Disk> {
        self.disks.get(self.selected)
    }

    /// The plan for the current selection, if a disk is chosen.
    pub fn plan(&self) -> Option<Plan> {
        let disk = self.disk()?;
        if !disk.is_installable() {
            return None;
        }
        Some(Plan::new(
            disk.clone(),
            self.firmware,
            self.settings.clone(),
        ))
    }

    /// Whether any disk on this machine could be installed onto.
    pub fn has_installable_disk(&self) -> bool {
        self.disks.iter().any(Disk::is_installable)
    }

    /// Feed one key event.
    pub fn key(&mut self, event: &KeyEvent) -> Command {
        if !event.is_press() {
            return Command::None;
        }
        // Ctrl+C always means stop, except once the disk is being written: an
        // installation interrupted halfway is worse than one that finishes.
        if event.modifiers.ctrl() && event.code == KeyCode::Char('c') {
            if self.stage == Stage::Installing {
                self.notice = Some("The installation cannot be interrupted.".into());
                return Command::None;
            }
            self.quit = true;
            return Command::Quit;
        }
        self.notice = None;

        match self.stage {
            Stage::Welcome => self.welcome_key(event),
            Stage::PickDisk => self.pick_disk_key(event),
            Stage::Configure => self.configure_key(event),
            Stage::Confirm => self.confirm_key(event),
            Stage::Installing => Command::None,
            Stage::Finished => self.finished_key(event),
        }
    }

    fn welcome_key(&mut self, event: &KeyEvent) -> Command {
        match event.code {
            KeyCode::Enter => {
                if !self.has_installable_disk() {
                    self.notice = Some(
                        "No disk on this machine can be installed onto.".to_string(),
                    );
                    return Command::None;
                }
                self.stage = Stage::PickDisk;
                Command::None
            }
            KeyCode::Escape | KeyCode::Char('q') => {
                self.quit = true;
                Command::Quit
            }
            _ => Command::None,
        }
    }

    fn pick_disk_key(&mut self, event: &KeyEvent) -> Command {
        match event.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.disks.len() {
                    self.selected += 1;
                }
                Command::None
            }
            KeyCode::Enter => {
                match self.disk().map(|disk| (disk.is_installable(), disk.refusal())) {
                    Some((true, _)) => {
                        self.stage = Stage::Configure;
                        self.field = Field::Hostname;
                    }
                    Some((false, Some(reason))) => {
                        self.notice = Some(format!("That disk cannot be used: {reason}."));
                    }
                    _ => self.notice = Some("There is no disk to install onto.".into()),
                }
                Command::None
            }
            KeyCode::Escape => {
                self.stage = Stage::Welcome;
                Command::None
            }
            _ => Command::None,
        }
    }

    fn configure_key(&mut self, event: &KeyEvent) -> Command {
        match event.code {
            KeyCode::Tab | KeyCode::Down => {
                self.field = match self.field {
                    Field::Hostname => Field::Username,
                    Field::Username => Field::Hostname,
                };
                Command::None
            }
            KeyCode::Up => {
                self.field = match self.field {
                    Field::Hostname => Field::Username,
                    Field::Username => Field::Hostname,
                };
                Command::None
            }
            KeyCode::Backspace => {
                self.field_mut().pop();
                Command::None
            }
            KeyCode::Enter => {
                match self.settings.problem() {
                    Some(problem) => self.notice = Some(problem),
                    None => {
                        self.stage = Stage::Confirm;
                        self.confirmation.clear();
                    }
                }
                Command::None
            }
            KeyCode::Escape => {
                self.stage = Stage::PickDisk;
                Command::None
            }
            KeyCode::Char(_) => {
                if let Some(text) = event.text {
                    // Only the characters a name may contain are accepted, so
                    // an invalid name cannot be typed in the first place.
                    if text.is_ascii_lowercase() || text.is_ascii_digit() || text == '-' {
                        let field = self.field_mut();
                        if field.len() < 32 {
                            field.push(text);
                        }
                    } else if text.is_ascii_uppercase() {
                        let lower = text.to_ascii_lowercase();
                        let field = self.field_mut();
                        if field.len() < 32 {
                            field.push(lower);
                        }
                    }
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    fn field_mut(&mut self) -> &mut String {
        match self.field {
            Field::Hostname => &mut self.settings.hostname,
            Field::Username => &mut self.settings.username,
        }
    }

    fn confirm_key(&mut self, event: &KeyEvent) -> Command {
        match event.code {
            KeyCode::Backspace => {
                self.confirmation.pop();
                Command::None
            }
            KeyCode::Enter => {
                let Some(plan) = self.plan() else {
                    self.notice = Some("The disk is no longer usable.".into());
                    self.stage = Stage::PickDisk;
                    return Command::None;
                };
                if self.confirmation.trim() != plan.confirmation_phrase() {
                    // Getting this wrong is the safety net working, so say so
                    // plainly rather than just refusing.
                    self.notice = Some(format!(
                        "Type {} exactly to erase that disk.",
                        plan.confirmation_phrase()
                    ));
                    return Command::None;
                }
                self.stage = Stage::Installing;
                Command::Install
            }
            KeyCode::Escape => {
                self.confirmation.clear();
                self.stage = Stage::Configure;
                Command::None
            }
            KeyCode::Char(_) => {
                if let Some(text) = event.text {
                    if self.confirmation.len() < 64 {
                        self.confirmation.push(text);
                    }
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    fn finished_key(&mut self, event: &KeyEvent) -> Command {
        match event.code {
            KeyCode::Char('r') if self.installed => Command::Reboot,
            KeyCode::Enter | KeyCode::Escape | KeyCode::Char('q') => {
                self.quit = true;
                Command::Quit
            }
            _ => Command::None,
        }
    }

    /// Run the installation against a backend, recording the outcome.
    pub fn install(&mut self, backend: &mut dyn Backend) {
        let Some(plan) = self.plan() else {
            self.stage = Stage::Finished;
            self.notice = Some("There is nothing to install onto.".into());
            return;
        };

        if self.dry_run {
            // A dry run still walks the whole plan, so what it prints is what
            // a real run would do rather than a guess at it.
            let mut recorder = crate::install::planning_backend();
            let mut installer = Installer::new(plan, &mut recorder);
            installer.run();
            self.progress = installer.progress.clone();
            self.installed = false;
            self.stage = Stage::Finished;
            self.notice = Some("Dry run: nothing was written.".into());
            return;
        }

        let mut installer = Installer::new(plan, backend);
        installer.run();
        self.progress = installer.progress.clone();
        self.installed = self.progress.failure().is_none();
        self.stage = Stage::Finished;
    }

    // ---- drawing --------------------------------------------------------

    /// Draw the current screen.
    pub fn draw(&self, screen: &mut Screen) {
        screen.clear();
        let area = screen.area();
        let body = Rect::new(0, 0, area.width, area.height.saturating_sub(1));

        match self.stage {
            Stage::Welcome => self.draw_welcome(screen, body),
            Stage::PickDisk => self.draw_disks(screen, body),
            Stage::Configure => self.draw_configure(screen, body),
            Stage::Confirm => self.draw_confirm(screen, body),
            Stage::Installing | Stage::Finished => self.draw_progress(screen, body),
        }
        self.draw_footer(screen, area);
    }

    fn draw_banner(&self, screen: &mut Screen, area: Rect, top: u16) -> u16 {
        let lines = banner_lines(&motd::art(), area, top);
        if lines.is_empty() {
            return top;
        }
        let width = banner_width(&lines);
        let left = area.x + area.width.saturating_sub(width) / 2;
        let mut y = top;
        for line in &lines {
            if y >= area.bottom() {
                break;
            }
            let mut x = left;
            for run in line {
                // A banner that brings no colours of its own is drawn in the
                // accent colour, which is what the tOS banner has always been.
                let style = if run.style == Style::default() {
                    Style::fg(ACCENT)
                } else {
                    run.style
                };
                x = screen.text(x, y, &run.text, style);
            }
            y += 1;
        }
        y
    }

    fn draw_welcome(&self, screen: &mut Screen, area: Rect) {
        let mut y = self.draw_banner(screen, area, 1) + 1;
        screen.centre(area, y, "Install tOS on this machine", Style::default().bold());
        y += 2;

        let firmware = format!("This machine booted with {}.", self.firmware.label());
        screen.centre(area, y, &firmware, Style::default().dim());
        y += 1;

        let disks = if self.has_installable_disk() {
            let count = self.disks.iter().filter(|d| d.is_installable()).count();
            format!(
                "{count} disk{} can be installed onto.",
                if count == 1 { "" } else { "s" }
            )
        } else {
            "No disk on this machine can be installed onto.".to_string()
        };
        screen.centre(area, y, &disks, Style::default().dim());
        y += 2;

        if self.dry_run {
            screen.centre(
                area,
                y,
                "Dry run: the plan is shown, nothing is written.",
                Style::fg(WARN),
            );
            y += 2;
        }

        screen.centre(
            area,
            y,
            "Enter to begin    Esc to leave",
            Style::default().dim(),
        );
    }

    fn draw_disks(&self, screen: &mut Screen, area: Rect) {
        let frame = Rect::new(2, 1, area.width.saturating_sub(4), area.height.saturating_sub(3));
        screen.frame(frame, Some("Where should tOS go?"), Style::fg(ACCENT));
        let inner = frame.inset(1);

        for (index, disk) in self.disks.iter().enumerate() {
            let y = inner.y + index as u16;
            if y >= inner.bottom() {
                break;
            }
            let chosen = index == self.selected;
            let usable = disk.is_installable();
            let style = match (chosen, usable) {
                (true, true) => Style::default().bold().reversed(),
                (true, false) => Style::fg(WARN).reversed(),
                (false, true) => Style::default(),
                (false, false) => Style::default().dim(),
            };
            let marker = if chosen { "▸ " } else { "  " };
            let mut text = format!("{marker}{}", disk.summary());
            if let Some(reason) = disk.refusal() {
                text.push_str(&format!("   — {reason}"));
            }
            // The whole row is highlighted, not just the text on it.
            screen.fill(Rect::new(inner.x, y, inner.width, 1), ' ', style);
            screen.text_clipped(inner.x, y, inner.width, &text, style);
        }

        if self.disks.is_empty() {
            screen.text(
                inner.x,
                inner.y,
                "No disks found. tOS needs a writable disk to install onto.",
                Style::fg(WARN),
            );
        }
    }

    fn draw_configure(&self, screen: &mut Screen, area: Rect) {
        let frame = Rect::centred(area, area.width.min(60), 9);
        screen.frame(frame, Some("Name this machine"), Style::fg(ACCENT));
        let inner = frame.inset(2);

        let fields = [
            (Field::Hostname, "Host name", &self.settings.hostname),
            (Field::Username, "User", &self.settings.username),
        ];
        for (index, (field, label, value)) in fields.iter().enumerate() {
            let y = inner.y + index as u16 * 2;
            let focused = *field == self.field;
            screen.text(inner.x, y, &format!("{label:<11}"), Style::default().dim());

            let box_x = inner.x + 11;
            let box_width = inner.width.saturating_sub(11);
            let style = if focused {
                Style::default().bold()
            } else {
                Style::default()
            };
            screen.fill(
                Rect::new(box_x, y, box_width, 1),
                ' ',
                Style::default().on(FIELD),
            );
            screen.text_clipped(box_x, y, box_width, value, style.on(FIELD));
            if focused {
                let cursor = box_x + tos_term::str_width(value) as u16;
                screen.set_cursor(cursor.min(box_x + box_width.saturating_sub(1)), y);
            }
        }

        screen.text(
            inner.x,
            inner.y + 4,
            "Tab switches fields, Enter continues.",
            Style::default().dim(),
        );
    }

    fn draw_confirm(&self, screen: &mut Screen, area: Rect) {
        let Some(plan) = self.plan() else {
            screen.centre(area, area.height / 2, "No disk selected.", Style::fg(WARN));
            return;
        };

        let summary = plan.summary();
        let height = (summary.len() as u16 + 8).min(area.height);
        let frame = Rect::centred(area, area.width.min(70), height);
        screen.frame(frame, Some("This erases the disk"), Style::fg(DANGER));
        let inner = frame.inset(2);

        let mut y = inner.y;
        for line in &summary {
            if y >= inner.bottom().saturating_sub(3) {
                break;
            }
            let style = if line.contains("replacing everything") {
                Style::fg(DANGER).bold()
            } else {
                Style::default()
            };
            screen.text_clipped(inner.x, y, inner.width, line, style);
            y += 1;
        }

        y += 1;
        let phrase = plan.confirmation_phrase();
        screen.text_clipped(
            inner.x,
            y,
            inner.width,
            &format!("Type {phrase} to confirm, then Enter:"),
            Style::default(),
        );
        y += 1;

        let box_width = inner.width.min(24);
        screen.fill(
            Rect::new(inner.x, y, box_width, 1),
            ' ',
            Style::default().on(FIELD),
        );
        screen.text_clipped(
            inner.x,
            y,
            box_width,
            &self.confirmation,
            Style::default().bold().on(FIELD),
        );
        let cursor = inner.x + tos_term::str_width(&self.confirmation) as u16;
        screen.set_cursor(cursor.min(inner.x + box_width - 1), y);
    }

    fn draw_progress(&self, screen: &mut Screen, area: Rect) {
        let steps = self.plan().map(|plan| plan.steps()).unwrap_or_default();
        let list_height = steps.len() as u16 + 2;
        let list = Rect::new(2, 1, area.width.saturating_sub(4), list_height.min(area.height));
        let title = if self.dry_run {
            "What would happen"
        } else {
            "Installing"
        };
        screen.frame(list, Some(title), Style::fg(ACCENT));
        let inner = list.inset(1);

        for (index, step) in steps.iter().enumerate() {
            let y = inner.y + index as u16;
            if y >= inner.bottom() {
                break;
            }
            let outcome = self
                .progress
                .finished
                .iter()
                .find(|(done, _)| done == step)
                .map(|(_, outcome)| outcome.clone());
            let (marker, style) = match (&outcome, self.progress.current == Some(*step)) {
                (Some(StepOutcome::Done), _) => ("✓", Style::fg(GOOD)),
                (Some(StepOutcome::Failed(_)), _) => ("✗", Style::fg(DANGER).bold()),
                (None, true) => ("…", Style::default().bold()),
                (None, false) => ("·", Style::default().dim()),
            };
            screen.text_clipped(
                inner.x,
                y,
                inner.width,
                &format!("{marker} {}", step.label()),
                style,
            );
        }

        // The log fills whatever is left.
        let log_top = list.bottom();
        if log_top + 2 >= area.bottom() {
            return;
        }
        let log = Rect::new(
            2,
            log_top,
            area.width.saturating_sub(4),
            area.bottom() - log_top,
        );
        screen.frame(log, Some("Log"), Style::default().dim());
        let inner = log.inset(1);
        let rows = inner.height as usize;
        let start = self.progress.log.len().saturating_sub(rows);
        for (row, line) in self.progress.log[start..].iter().enumerate() {
            screen.text_clipped(
                inner.x,
                inner.y + row as u16,
                inner.width,
                line,
                Style::default().dim(),
            );
        }
    }

    fn draw_footer(&self, screen: &mut Screen, area: Rect) {
        let y = area.bottom().saturating_sub(1);
        screen.fill(Rect::new(0, y, area.width, 1), ' ', Style::default().reversed());

        let text = match &self.notice {
            Some(notice) => notice.clone(),
            None => match self.stage {
                Stage::Welcome => "Enter  begin      Esc  leave".to_string(),
                Stage::PickDisk => {
                    "↑↓  choose      Enter  continue      Esc  back".to_string()
                }
                Stage::Configure => "Tab  next field   Enter  continue      Esc  back".to_string(),
                Stage::Confirm => "Enter  erase and install      Esc  back".to_string(),
                Stage::Installing => "Installing; this cannot be interrupted.".to_string(),
                Stage::Finished => {
                    if self.installed {
                        "r  reboot into tOS      Enter  leave".to_string()
                    } else {
                        "Enter  leave".to_string()
                    }
                }
            },
        };
        let style = if self.notice.is_some() {
            Style::fg(DANGER).reversed().bold()
        } else {
            Style::default().reversed()
        };
        screen.text_clipped(1, y, area.width.saturating_sub(2), &text, style);
    }

    /// The line shown when everything is done.
    pub fn outcome_message(&self) -> String {
        if self.dry_run {
            return "Dry run finished. Nothing was written.".to_string();
        }
        match self.progress.failure() {
            Some(failure) => format!("Installation failed: {failure}"),
            None if self.installed => {
                "tOS is installed. Remove the boot medium and reboot.".to_string()
            }
            None => "Nothing was installed.".to_string(),
        }
    }
}

/// The banner to draw: the largest one that leaves the welcome screen room for
/// what it has to say.
///
/// The banner on a machine can be anything, including a picture that wants
/// more of the screen than there is. A welcome screen with no room left for
/// its own words would be a worse trade than a smaller banner, so the picture
/// gives way to the built-in one, that to a single line, and that to nothing.
fn banner_lines(art: &str, area: Rect, top: u16) -> Vec<Vec<motd::Run>> {
    for candidate in [art, motd::ART, motd::ART_SMALL] {
        let lines = motd::art_runs(candidate);
        if banner_fits(&lines, area, top) {
            return lines;
        }
    }
    Vec::new()
}

/// How wide a parsed banner is, in cells.
fn banner_width(lines: &[Vec<motd::Run>]) -> u16 {
    lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|run| tos_term::str_width(&run.text))
                .sum::<usize>() as u16
        })
        .max()
        .unwrap_or(0)
}

/// Whether a banner leaves the welcome screen room for what it has to say.
fn banner_fits(lines: &[Vec<motd::Run>], area: Rect, top: u16) -> bool {
    let height = top + lines.len() as u16 + WELCOME_ROWS;
    banner_width(lines) <= area.width && height <= area.height
}

/// The rows `draw_welcome` needs under the banner, dry run note included.
const WELCOME_ROWS: u16 = 10;

const ACCENT: Color = Color::rgb(0x5f, 0x87, 0xd7);
const DANGER: Color = Color::rgb(0xff, 0x7b, 0x7b);
const WARN: Color = Color::rgb(0xc7, 0xa1, 0x4f);
const GOOD: Color = Color::rgb(0x87, 0xdf, 0x87);
const FIELD: Color = Color::rgb(0x2c, 0x2c, 0x34);

/// Convenience for building a key press.
pub fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, Modifiers::NONE)
}

/// Convenience for building a typed character.
pub fn typed(c: char) -> KeyEvent {
    let mut event = KeyEvent::new(KeyCode::Char(c), Modifiers::NONE);
    event.text = Some(c);
    event
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Recorder;

    fn disk(name: &str, gib: u64) -> Disk {
        Disk {
            name: name.into(),
            path: format!("/dev/{name}"),
            bytes: gib * (1 << 30),
            model: "TEST DISK".into(),
            removable: false,
            read_only: false,
            in_use: false,
            is_boot_medium: false,
        }
    }

    fn unusable(name: &str) -> Disk {
        Disk {
            read_only: true,
            ..disk(name, 64)
        }
    }

    fn app() -> App {
        App::new(vec![disk("sda", 64), disk("sdb", 32)], Firmware::Uefi, false)
    }

    /// A backend that looks like the live image with its medium mounted.
    fn live_backend() -> Recorder {
        crate::install::planning_backend()
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.key(&typed(c));
        }
    }

    /// Drive the app from the welcome screen to the point of no return.
    fn reach_confirmation(app: &mut App) {
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::PickDisk);
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::Configure);
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::Confirm);
    }

    /// A banner in the shape `chafa` produces: `rows` lines of `cols` coloured
    /// blocks, with the cursor hiding it wraps its output in.
    fn picture(rows: usize, cols: usize) -> String {
        let mut art = String::from("\x1b[?25l");
        for _ in 0..rows {
            art.push_str("\x1b[38;2;200;100;50m");
            art.extend(std::iter::repeat('\u{2580}').take(cols));
            art.push('\n');
        }
        art.push_str("\x1b[?25h");
        art
    }

    #[test]
    fn a_picture_that_fits_is_the_banner() {
        let area = Rect::new(0, 0, 80, 23);
        let lines = banner_lines(&picture(6, 46), area, 1);
        assert_eq!(lines.len(), 6);
        assert_eq!(banner_width(&lines), 46, "escapes take up no cells");
        assert_eq!(lines[0][0].style.fg, Color::rgb(200, 100, 50));
    }

    #[test]
    fn a_banner_that_crowds_out_the_welcome_screen_gives_way() {
        // 34 rows of picture on a 23 row console would leave nothing to read.
        let area = Rect::new(0, 0, 80, 23);
        let lines = banner_lines(&picture(34, 46), area, 1);
        assert_eq!(lines, motd::art_runs(motd::ART), "should fall back");
    }

    #[test]
    fn a_banner_wider_than_the_screen_gives_way_too() {
        let area = Rect::new(0, 0, 80, 23);
        let lines = banner_lines(&picture(4, 200), area, 1);
        assert_eq!(lines, motd::art_runs(motd::ART));
    }

    #[test]
    fn the_picture_that_ships_fits_the_smallest_screen_tested() {
        // The banner is only worth shipping if it is the one people see.
        let area = Rect::new(0, 0, 80, 23);
        assert_eq!(
            banner_lines(motd::ART, area, 1),
            motd::art_runs(motd::ART),
            "the shipped picture should survive an eighty column console"
        );
    }

    #[test]
    fn a_screen_too_small_for_the_picture_still_gets_a_name() {
        // Twelve rows of picture do not fit here, one line does.
        let area = Rect::new(0, 0, 60, 14);
        let lines = banner_lines(motd::ART, area, 1);
        assert_eq!(lines, motd::art_runs(motd::ART_SMALL));
    }

    #[test]
    fn the_words_win_when_nothing_fits() {
        let area = Rect::new(0, 0, 20, 12);
        assert!(banner_lines(motd::ART, area, 1).is_empty());
    }

    #[test]
    fn a_tiny_screen_still_says_how_to_go_on() {
        let mut screen = Screen::new(40, 14);
        app().draw(&mut screen);
        let text = screen.to_text();
        assert!(text.contains("Install tOS on this machine"), "{text}");
        assert!(text.contains("Enter to begin"), "{text}");
    }

    #[test]
    fn a_tall_screen_keeps_a_tall_picture() {
        // The same picture on hardware, where there are rows to spare.
        let area = Rect::new(0, 0, 80, 60);
        let lines = banner_lines(&picture(34, 46), area, 1);
        assert_eq!(lines.len(), 34);
    }

    #[test]
    fn the_welcome_screen_still_says_its_piece_under_any_banner() {
        // Whatever the banner is, the words below it have to survive.
        let mut screen = Screen::new(80, 24);
        let app = app();
        app.draw(&mut screen);
        let text = screen.to_text();
        assert!(text.contains("Install tOS on this machine"), "{text}");
        assert!(text.contains("Enter to begin"), "{text}");
    }

    #[test]
    fn it_starts_on_the_welcome_screen() {
        assert_eq!(app().stage, Stage::Welcome);
    }

    #[test]
    fn the_cursor_starts_on_a_usable_disk() {
        let app = App::new(
            vec![unusable("sda"), disk("sdb", 64)],
            Firmware::Uefi,
            false,
        );
        assert_eq!(app.disk().unwrap().name, "sdb");
    }

    #[test]
    fn the_welcome_screen_shows_the_banner() {
        let mut screen = Screen::new(80, 24);
        app().draw(&mut screen);
        assert!(
            screen.contains("the terminal is the desktop"),
            "the banner should be on the first screen"
        );
        assert!(screen.contains("Install tOS"));
    }

    #[test]
    fn the_banner_fits_an_eighty_column_screen() {
        // The installer runs in a pane, which is narrower than the display.
        let mut screen = Screen::new(80, 24);
        app().draw(&mut screen);
        for line in screen.to_text().lines() {
            assert!(
                tos_term::str_width(line) <= 80,
                "line overflows: {line:?}"
            );
        }
    }

    #[test]
    fn a_machine_with_no_usable_disk_says_so_and_goes_no_further() {
        let mut app = App::new(vec![unusable("sda")], Firmware::Uefi, false);
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::Welcome, "must not reach the disk picker");
        assert!(app.notice.as_ref().unwrap().contains("No disk"));
    }

    #[test]
    fn the_arrow_keys_move_between_disks() {
        let mut app = app();
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.disk().unwrap().name, "sda");
        app.key(&press(KeyCode::Down));
        assert_eq!(app.disk().unwrap().name, "sdb");
        app.key(&press(KeyCode::Up));
        assert_eq!(app.disk().unwrap().name, "sda");
        // The ends are not wrapped past.
        app.key(&press(KeyCode::Up));
        assert_eq!(app.disk().unwrap().name, "sda");
    }

    #[test]
    fn an_unusable_disk_cannot_be_chosen() {
        let mut app = App::new(
            vec![disk("sda", 64), unusable("sdb")],
            Firmware::Uefi,
            false,
        );
        app.key(&press(KeyCode::Enter));
        app.key(&press(KeyCode::Down));
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::PickDisk, "should not have continued");
        assert!(app.notice.as_ref().unwrap().contains("read only"));
    }

    #[test]
    fn the_disk_list_says_why_a_disk_is_refused() {
        let mut app = App::new(vec![unusable("sda")], Firmware::Uefi, false);
        app.stage = Stage::PickDisk;
        let mut screen = Screen::new(90, 24);
        app.draw(&mut screen);
        assert!(screen.contains("/dev/sda"));
        assert!(screen.contains("read only"));
    }

    #[test]
    fn names_are_typed_and_corrected() {
        let mut app = app();
        app.key(&press(KeyCode::Enter));
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::Configure);

        // Clear the default and type a new one.
        for _ in 0..10 {
            app.key(&press(KeyCode::Backspace));
        }
        assert!(app.settings.hostname.is_empty());
        type_text(&mut app, "workshop");
        assert_eq!(app.settings.hostname, "workshop");

        app.key(&press(KeyCode::Tab));
        assert_eq!(app.field, Field::Username);
        for _ in 0..10 {
            app.key(&press(KeyCode::Backspace));
        }
        type_text(&mut app, "yusuke");
        assert_eq!(app.settings.username, "yusuke");
    }

    #[test]
    fn characters_a_name_cannot_hold_are_not_accepted() {
        let mut app = app();
        app.stage = Stage::Configure;
        for _ in 0..10 {
            app.key(&press(KeyCode::Backspace));
        }
        type_text(&mut app, "a b:c/d");
        // Uppercase is folded rather than refused, which is friendlier.
        type_text(&mut app, "E");
        assert_eq!(app.settings.hostname, "abcde");
    }

    #[test]
    fn an_empty_name_cannot_be_accepted() {
        let mut app = app();
        app.stage = Stage::Configure;
        for _ in 0..10 {
            app.key(&press(KeyCode::Backspace));
        }
        app.key(&press(KeyCode::Enter));
        assert_eq!(app.stage, Stage::Configure);
        assert!(app.notice.as_ref().unwrap().contains("empty"));
    }

    #[test]
    fn the_confirmation_screen_says_what_is_destroyed() {
        let mut app = app();
        reach_confirmation(&mut app);
        let mut screen = Screen::new(90, 30);
        app.draw(&mut screen);
        assert!(screen.contains("/dev/sda"));
        assert!(screen.contains("replacing everything on the disk"));
        assert!(screen.contains("Type sda to confirm"));
    }

    #[test]
    fn nothing_happens_without_the_exact_disk_name() {
        let mut app = app();
        reach_confirmation(&mut app);

        // The word people type without reading.
        type_text(&mut app, "yes");
        assert_eq!(app.key(&press(KeyCode::Enter)), Command::None);
        assert_eq!(app.stage, Stage::Confirm, "must not have started");
        assert!(app.notice.as_ref().unwrap().contains("sda"));
    }

    #[test]
    fn a_near_miss_is_still_refused() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sdb");
        assert_eq!(app.key(&press(KeyCode::Enter)), Command::None);
        assert_eq!(app.stage, Stage::Confirm);
    }

    #[test]
    fn the_exact_disk_name_starts_the_installation() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        assert_eq!(app.key(&press(KeyCode::Enter)), Command::Install);
        assert_eq!(app.stage, Stage::Installing);
    }

    #[test]
    fn escape_goes_back_and_forgets_what_was_typed() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Escape));
        assert_eq!(app.stage, Stage::Configure);
        assert!(app.confirmation.is_empty(), "a stale confirmation is a trap");
    }

    #[test]
    fn ctrl_c_leaves_before_the_disk_is_touched() {
        let mut app = app();
        reach_confirmation(&mut app);
        let event = KeyEvent::new(KeyCode::Char('c'), Modifiers::CTRL);
        assert_eq!(app.key(&event), Command::Quit);
        assert!(app.should_quit());
    }

    #[test]
    fn ctrl_c_does_not_interrupt_a_running_installation() {
        // Stopping halfway leaves a disk that is neither one thing nor another.
        let mut app = app();
        app.stage = Stage::Installing;
        let event = KeyEvent::new(KeyCode::Char('c'), Modifiers::CTRL);
        assert_eq!(app.key(&event), Command::None);
        assert!(!app.should_quit());
        assert!(app.notice.as_ref().unwrap().contains("cannot be interrupted"));
    }

    #[test]
    fn keys_do_nothing_while_installing() {
        let mut app = app();
        app.stage = Stage::Installing;
        for code in [KeyCode::Enter, KeyCode::Escape, KeyCode::Up] {
            assert_eq!(app.key(&press(code)), Command::None);
            assert_eq!(app.stage, Stage::Installing);
        }
    }

    #[test]
    fn a_successful_installation_offers_a_reboot() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Enter));

        let mut backend = live_backend();
        app.install(&mut backend);
        assert_eq!(app.stage, Stage::Finished);
        assert!(app.installed, "{:?}", app.progress.failure());
        assert!(app.outcome_message().contains("installed"));
        assert_eq!(app.key(&typed('r')), Command::Reboot);
    }

    #[test]
    fn a_failed_installation_says_what_went_wrong_and_offers_no_reboot() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Enter));

        let mut backend = live_backend();
        backend.responses.push((
            "mkfs.ext4".to_string(),
            crate::exec::Output {
                status: 1,
                stdout: String::new(),
                stderr: "device is busy".to_string(),
            },
        ));
        app.install(&mut backend);
        assert!(!app.installed);
        let message = app.outcome_message();
        assert!(message.contains("failed"));
        assert!(message.contains("mkfs.ext4"));
        // Rebooting into a half-installed disk is not on offer.
        assert_eq!(app.key(&typed('r')), Command::None);
    }

    #[test]
    fn a_dry_run_writes_nothing_but_still_shows_the_plan() {
        let mut app = App::new(vec![disk("sda", 64)], Firmware::Uefi, true);
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Enter));

        let mut backend = Recorder::new();
        app.install(&mut backend);
        assert!(
            backend.actions.is_empty(),
            "a dry run must not touch the real backend"
        );
        assert!(!app.progress.log.is_empty(), "it still shows the plan");
        assert!(app.outcome_message().contains("Nothing was written"));
        assert!(!app.installed);
    }

    #[test]
    fn the_progress_screen_ticks_steps_off() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Enter));
        let mut backend = live_backend();
        app.install(&mut backend);

        let mut screen = Screen::new(90, 30);
        app.draw(&mut screen);
        let text = screen.to_text();
        assert!(text.contains("✓ Partition the disk"), "{text}");
        assert!(text.contains("✓ Install the bootloader"));
    }

    #[test]
    fn a_failed_step_is_marked_and_the_rest_are_not() {
        let mut app = app();
        reach_confirmation(&mut app);
        type_text(&mut app, "sda");
        app.key(&press(KeyCode::Enter));
        let mut backend = live_backend();
        backend.responses.push((
            "mkfs.vfat".to_string(),
            crate::exec::Output {
                status: 1,
                stdout: String::new(),
                stderr: "no such device".to_string(),
            },
        ));
        app.install(&mut backend);

        let mut screen = Screen::new(90, 30);
        app.draw(&mut screen);
        let text = screen.to_text();
        assert!(text.contains("✗ Create the EFI system partition"), "{text}");
        assert!(text.contains("· Install the bootloader"), "{text}");
    }

    #[test]
    fn the_footer_always_says_what_the_keys_do() {
        let mut app = app();
        for stage in [
            Stage::Welcome,
            Stage::PickDisk,
            Stage::Configure,
            Stage::Confirm,
            Stage::Finished,
        ] {
            app.stage = stage.clone();
            let mut screen = Screen::new(80, 24);
            app.draw(&mut screen);
            let footer = screen.to_text().lines().last().unwrap().to_string();
            assert!(!footer.trim().is_empty(), "no footer on {stage:?}");
        }
    }

    #[test]
    fn a_notice_is_shown_in_the_footer() {
        let mut app = app();
        app.notice = Some("something went wrong".into());
        let mut screen = Screen::new(80, 24);
        app.draw(&mut screen);
        assert!(screen.contains("something went wrong"));
    }

    #[test]
    fn drawing_works_at_awkward_sizes() {
        // A pane can be small; the installer must not panic in one.
        let mut app = app();
        for (cols, rows) in [(20u16, 5u16), (40, 10), (200, 60), (8, 3)] {
            for stage in [
                Stage::Welcome,
                Stage::PickDisk,
                Stage::Configure,
                Stage::Confirm,
                Stage::Installing,
                Stage::Finished,
            ] {
                app.stage = stage;
                let mut screen = Screen::new(cols, rows);
                app.draw(&mut screen);
                screen.render();
            }
        }
    }
}
