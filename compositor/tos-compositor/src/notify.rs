//! Notifications: everything the compositor has to say, and everything an
//! application says through it.
//!
//! There used to be one string and a clock. Two things to say inside three
//! seconds meant the first was never read, and anything longer than the space
//! left on the status bar was not drawn at all — which is worst for the
//! messages that matter most, because a failure carries an `io::Error` and
//! those are long.
//!
//! So notifications queue, each gets its turn, and every one of them is kept
//! afterwards: the bar is a glance, the history list is the record. A
//! notification also remembers where it came from, because "pane 3 says the
//! build finished" and "the compositor could not split" are different kinds of
//! news, and only one of them has somewhere to take you.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tos_font::FontStack;
use tos_render::{Rect, Surface};
use tos_session::PaneId;

use crate::chrome::{clip_marked, draw_text, Chrome};
use crate::overlay::OverlayItem;

/// How long one notification stays on screen when nothing is behind it.
const DWELL: Duration = Duration::from_secs(3);
/// How long each stays while others are waiting. Ten at three seconds each
/// would hold the bar for half a minute, and a queue that takes that long to
/// drain is the problem it was meant to solve.
const BUSY_DWELL: Duration = Duration::from_secs(1);
/// The most notifications that wait their turn at once.
///
/// A program in a loop can raise one per line of output. Past this the oldest
/// one still waiting gives up its place — it is in the history either way, so
/// nothing is lost, and what just happened reaches the screen instead of
/// queueing behind a minute of backlog.
const MAX_QUEUED: usize = 16;
/// The most notifications kept to look back at.
const MAX_HISTORY: usize = 64;
/// The widest a title or body is kept, in cells. No status bar is this wide
/// and no history row shows this much, but the bound has to be somewhere.
const MAX_TEXT: usize = 200;
/// The most characters read out of a body before it is cut.
///
/// The text arrives from whatever was on the other end of a pipe, and that can
/// be a megabyte of it. Nothing past this could survive the clip, so nothing
/// past this is looked at.
const MAX_SCAN: usize = 4096;

/// Where a notification came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The compositor itself: a binding acknowledging itself, something that
    /// was refused, something that failed.
    System,
    /// An application in this pane, through OSC 9 or OSC 777.
    Pane(PaneId),
}

/// One thing to tell the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    pub source: Source,
    pub title: String,
    pub body: String,
    /// When it arrived, which is what the history list shows as an age.
    pub at: Instant,
}

impl Notification {
    /// A message from the compositor about itself.
    pub fn system(text: impl AsRef<str>) -> Self {
        Notification {
            source: Source::System,
            title: String::new(),
            body: sanitize(text.as_ref()),
            at: Instant::now(),
        }
    }

    /// A message an application raised in a pane.
    pub fn from_pane(pane: PaneId, title: impl AsRef<str>, body: impl AsRef<str>) -> Self {
        Notification {
            source: Source::Pane(pane),
            title: sanitize(title.as_ref()),
            body: sanitize(body.as_ref()),
            at: Instant::now(),
        }
    }

    /// The notification without its source: title and body, or whichever of
    /// the two an application bothered to send.
    pub fn text(&self) -> String {
        if self.title.is_empty() {
            self.body.clone()
        } else if self.body.is_empty() {
            self.title.clone()
        } else {
            format!("{}: {}", self.title, self.body)
        }
    }

    /// The line to show, with the pane that raised it named.
    ///
    /// Which pane is the part the user cannot work out for themselves: a
    /// notification says a build finished, and on a screen of four panes the
    /// only useful question is which build.
    pub fn status_text(&self) -> String {
        match self.source {
            Source::System => self.text(),
            Source::Pane(pane) => format!("pane {}: {}", pane.0 + 1, self.text()),
        }
    }

    /// How long ago this arrived, in the shortest honest form.
    pub fn age(&self, now: Instant) -> String {
        let seconds = now.saturating_duration_since(self.at).as_secs();
        match seconds {
            0..=4 => "just now".to_string(),
            5..=59 => format!("{seconds}s ago"),
            60..=3599 => format!("{}m ago", seconds / 60),
            _ => format!("{}h ago", seconds / 3600),
        }
    }
}

/// What choosing a row of the history list does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chosen {
    /// The first row: forget everything in the list.
    Clear,
    /// An application's notification: go to the pane that raised it.
    Pane(PaneId),
    /// The compositor's own, or the row an empty list shows. There is nowhere
    /// to go and nothing to undo.
    Nowhere,
}

/// The queue on the status bar and the record behind it.
#[derive(Debug, Default)]
pub struct Notifications {
    /// Waiting their turn, the one at the front being the one on screen.
    queue: VecDeque<Notification>,
    /// Everything that has been raised, oldest first.
    history: VecDeque<Notification>,
    /// When the one at the front of the queue reached the screen. It is set by
    /// the first [`Notifications::advance`] after it gets there rather than
    /// when it was pushed, so one the caller is not showing yet has not
    /// started spending its time.
    shown_at: Option<Instant>,
    /// What each row of the last history list meant, in the order it was
    /// built. The overlay keeps the rows it was handed, so a notification
    /// arriving while the list is up must not change what choosing one does.
    listed: Vec<Chosen>,
}

impl Notifications {
    pub fn new() -> Self {
        Notifications::default()
    }

    /// Raise a message from the compositor about itself.
    pub fn status(&mut self, text: impl AsRef<str>) {
        self.push(Notification::system(text));
    }

    /// Raise one an application asked for.
    pub fn from_pane(&mut self, pane: PaneId, title: impl AsRef<str>, body: impl AsRef<str>) {
        self.push(Notification::from_pane(pane, title, body));
    }

    pub fn push(&mut self, notification: Notification) {
        self.history.push_back(notification.clone());
        while self.history.len() > MAX_HISTORY {
            self.history.pop_front();
        }
        self.queue.push_back(notification);
        // The one at the front may already be on screen, so it is the oldest
        // still waiting that gives up its place.
        while self.queue.len() > MAX_QUEUED {
            self.queue.remove(1);
        }
    }

    /// The notification on screen, if any.
    pub fn current(&self) -> Option<&Notification> {
        self.queue.front()
    }

    /// How many are waiting behind the one on screen.
    pub fn waiting(&self) -> usize {
        self.queue.len().saturating_sub(1)
    }

    /// The line the status bar or the banner should show, if any.
    ///
    /// A queue behind it is part of the line: seeing "(+3)" is what tells you
    /// there is more coming and that the list has all of it.
    pub fn status_line(&self) -> Option<String> {
        let current = self.current()?;
        match self.waiting() {
            0 => Some(current.status_text()),
            waiting => Some(format!("{} (+{waiting})", current.status_text())),
        }
    }

    /// Everything raised so far, newest first.
    pub fn history(&self) -> impl Iterator<Item = &Notification> {
        self.history.iter().rev()
    }

    /// Retire the one on screen when its time is up. Returns true when the
    /// screen needs to change.
    ///
    /// The caller decides when this runs, because only the caller knows
    /// whether the notification is actually visible: while the leader key is
    /// armed the status bar belongs to the leader indicator, and a
    /// notification must not spend its seconds behind one.
    pub fn advance(&mut self, now: Instant) -> bool {
        if self.queue.is_empty() {
            return false;
        }
        let since = now.saturating_duration_since(*self.shown_at.get_or_insert(now));
        // The wait shortens the moment something queues up behind, so a burst
        // drains at a pace that can be read rather than one that is waited on.
        let dwell = if self.waiting() > 0 {
            BUSY_DWELL
        } else {
            DWELL
        };
        if since < dwell {
            return false;
        }
        self.queue.pop_front();
        self.shown_at = None;
        true
    }

    /// Take the queue off the screen, leaving the history alone. Returns true
    /// when something was actually up there.
    pub fn dismiss(&mut self) -> bool {
        self.shown_at = None;
        !std::mem::take(&mut self.queue).is_empty()
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
        self.listed.clear();
    }

    /// Build the rows of the history list, newest first, and dismiss whatever
    /// is on the bar: the list is the answer to "what did I miss", so opening
    /// it means those have now been seen.
    pub fn open_history(&mut self, now: Instant) -> Vec<OverlayItem> {
        self.dismiss();
        self.listed.clear();
        if self.history.is_empty() {
            self.listed.push(Chosen::Nowhere);
            return vec![OverlayItem::new("nothing has been raised yet")];
        }
        // A row that empties the list, rather than a binding of its own: the
        // list is where you already are when you decide you are done with it.
        let mut items = vec![OverlayItem::with_detail(
            "clear",
            format!("{} kept", self.history.len()),
        )];
        self.listed.push(Chosen::Clear);
        for notification in self.history.iter().rev() {
            items.push(OverlayItem::with_detail(
                notification.status_text(),
                notification.age(now),
            ));
            self.listed.push(match notification.source {
                Source::Pane(pane) => Chosen::Pane(pane),
                Source::System => Chosen::Nowhere,
            });
        }
        items
    }

    /// What row `index` of the last [`Notifications::open_history`] meant.
    pub fn choose(&self, index: usize) -> Chosen {
        self.listed.get(index).copied().unwrap_or(Chosen::Nowhere)
    }
}

/// Draw a notification over the panes, for a session with no status bar.
///
/// `--no-status-bar` used to mean no notifications, no bells and no errors at
/// all: a split that failed said nothing and looked like a key that had not
/// worked. So without a bar they get a line of their own at the top right, in
/// the accent colours — the one corner nothing else draws in, and a styling no
/// pane can produce by itself.
pub fn draw_banner(
    surface: &mut Surface<'_>,
    fonts: &mut FontStack,
    area: Rect,
    chrome: &Chrome,
    text: &str,
) {
    let metrics = fonts.metrics();
    let cw = metrics.cell_width.max(1);
    let cols = (area.width / cw) as usize;
    if cols < 4 {
        return;
    }
    let label = format!(" {} ", clip_marked(text, cols - 2));
    let width = tos_term::str_width(&label) as u32 * cw;
    draw_text(
        surface,
        fonts,
        area.right() - width as i32,
        area.y,
        &label,
        chrome.accent_text,
        Some(chrome.accent),
        false,
    );
}

/// Make a string safe to draw one character per cell, and bound its length.
///
/// The text is whatever an application chose to send, so it can carry
/// newlines, escape sequences and a megabyte of body. The status bar draws
/// characters into cells: a control character has no cell of its own, and a
/// combining mark has no base here to combine with, so both come out as
/// something the sender did not write. Every run of whitespace becomes one
/// space, which is what turns a multi-line body into a line.
fn sanitize(text: &str) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars().take(MAX_SCAN) {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        // Zero width characters are dropped rather than drawn: a cell grid has
        // no way to stack one onto the character before it.
        if tos_term::char_width(c) == 0 {
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    clip_marked(&out, MAX_TEXT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_font::BitmapFont;
    use tos_render::OwnedFramebuffer;

    fn pane(n: u32) -> PaneId {
        PaneId(n)
    }

    #[test]
    fn a_message_is_shown_until_its_time_is_up() {
        let mut notifications = Notifications::new();
        let start = Instant::now();
        notifications.status("copied");
        assert_eq!(notifications.status_line().as_deref(), Some("copied"));
        assert!(!notifications.advance(start));
        assert!(!notifications.advance(start + DWELL - Duration::from_millis(1)));
        assert!(notifications.advance(start + DWELL));
        assert!(notifications.status_line().is_none());
    }

    #[test]
    fn the_second_message_waits_rather_than_replacing_the_first() {
        let mut notifications = Notifications::new();
        let start = Instant::now();
        notifications.status("first");
        notifications.status("second");
        assert_eq!(notifications.status_line().as_deref(), Some("first (+1)"));
        assert!(!notifications.advance(start));
        // A queue behind it shortens its turn, so a burst drains at a pace
        // that can be read rather than one that has to be waited out.
        assert!(notifications.advance(start + BUSY_DWELL));
        assert_eq!(notifications.status_line().as_deref(), Some("second"));
        // And the second gets a full turn of its own, counted from when it
        // reached the screen rather than from when it was raised.
        let showing = start + BUSY_DWELL;
        assert!(!notifications.advance(showing));
        assert!(!notifications.advance(showing + DWELL - Duration::from_millis(1)));
        assert!(notifications.advance(showing + DWELL));
        assert!(notifications.status_line().is_none());
    }

    #[test]
    fn time_is_only_spent_while_the_caller_is_showing_it() {
        // The leader indicator takes the slot; a notification underneath one
        // has not been seen, so its clock has not started.
        let mut notifications = Notifications::new();
        let start = Instant::now();
        notifications.status("copied");
        let later = start + Duration::from_secs(60);
        assert!(!notifications.advance(later));
        assert!(notifications.advance(later + DWELL));
    }

    #[test]
    fn a_burst_past_the_queue_limit_keeps_the_newest() {
        let mut notifications = Notifications::new();
        for i in 0..MAX_QUEUED * 2 {
            notifications.status(format!("message {i}"));
        }
        assert_eq!(notifications.waiting(), MAX_QUEUED - 1);
        // The one on screen stays on screen, and what was dropped is what had
        // been waiting longest behind it.
        assert_eq!(notifications.current().unwrap().text(), "message 0");
        let last = notifications.queue.back().unwrap();
        assert_eq!(last.text(), format!("message {}", MAX_QUEUED * 2 - 1));
        // Nothing was lost by it: the history still has every one.
        assert_eq!(notifications.history().count(), MAX_QUEUED * 2);
    }

    #[test]
    fn the_history_stops_growing() {
        let mut notifications = Notifications::new();
        for i in 0..MAX_HISTORY * 2 {
            notifications.status(format!("message {i}"));
        }
        assert_eq!(notifications.history().count(), MAX_HISTORY);
        // Newest first, and the oldest are the ones that went.
        let newest = notifications.history().next().unwrap();
        assert_eq!(newest.text(), format!("message {}", MAX_HISTORY * 2 - 1));
    }

    #[test]
    fn an_application_notification_names_its_pane() {
        let notification = Notification::from_pane(pane(2), "build", "finished");
        assert_eq!(notification.text(), "build: finished");
        assert_eq!(notification.status_text(), "pane 3: build: finished");
        // A message of the compositor's own has no pane to name.
        assert_eq!(Notification::system("copied").status_text(), "copied");
    }

    #[test]
    fn a_notification_with_only_one_half_has_no_stray_separator() {
        let notification = Notification::from_pane(pane(0), "", "done");
        assert_eq!(notification.text(), "done");
        let titled = Notification::from_pane(pane(0), "done", "");
        assert_eq!(titled.text(), "done");
    }

    #[test]
    fn control_characters_and_newlines_come_out_as_one_line() {
        let notification = Notification::system("two\nlines\tand\x1b[31m colour");
        assert_eq!(notification.body, "two lines and [31m colour");
    }

    #[test]
    fn a_body_with_nothing_drawable_in_it_is_empty() {
        // Zero width characters have nothing to attach to in a cell grid.
        let notification = Notification::system("\u{200b}\u{0301}\n\t");
        assert!(notification.body.is_empty());
    }

    #[test]
    fn an_enormous_body_is_cut_and_says_so() {
        let notification = Notification::system("x".repeat(100_000));
        assert_eq!(tos_term::str_width(&notification.body), MAX_TEXT);
        assert!(notification.body.ends_with('…'));
    }

    #[test]
    fn leading_and_trailing_whitespace_is_dropped() {
        assert_eq!(Notification::system("  spaced  ").body, "spaced");
    }

    #[test]
    fn the_age_is_the_shortest_honest_form() {
        let notification = Notification::system("old");
        let at = notification.at;
        assert_eq!(notification.age(at), "just now");
        assert_eq!(notification.age(at + Duration::from_secs(30)), "30s ago");
        assert_eq!(notification.age(at + Duration::from_secs(300)), "5m ago");
        assert_eq!(notification.age(at + Duration::from_secs(7200)), "2h ago");
    }

    #[test]
    fn opening_the_history_takes_the_queue_off_the_bar() {
        let mut notifications = Notifications::new();
        notifications.status("first");
        notifications.from_pane(pane(1), "", "second");
        let items = notifications.open_history(Instant::now());
        assert!(notifications.status_line().is_none());
        // The clear row, then both notifications, newest first.
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, ["clear", "pane 2: second", "first"]);
    }

    #[test]
    fn choosing_a_row_goes_where_the_notification_came_from() {
        let mut notifications = Notifications::new();
        notifications.status("mine");
        notifications.from_pane(pane(3), "", "theirs");
        notifications.open_history(Instant::now());
        assert_eq!(notifications.choose(0), Chosen::Clear);
        assert_eq!(notifications.choose(1), Chosen::Pane(pane(3)));
        assert_eq!(notifications.choose(2), Chosen::Nowhere);
        // A row that is not there means nothing rather than panicking.
        assert_eq!(notifications.choose(99), Chosen::Nowhere);
    }

    #[test]
    fn a_notification_arriving_while_the_list_is_up_does_not_move_the_rows() {
        let mut notifications = Notifications::new();
        notifications.from_pane(pane(1), "", "first");
        notifications.open_history(Instant::now());
        notifications.from_pane(pane(7), "", "later");
        // The overlay still holds the rows it was given, so row 1 has to mean
        // what it meant when it was built.
        assert_eq!(notifications.choose(1), Chosen::Pane(pane(1)));
    }

    #[test]
    fn an_empty_history_still_has_a_row_to_show() {
        let mut notifications = Notifications::new();
        let items = notifications.open_history(Instant::now());
        assert_eq!(items.len(), 1);
        assert_eq!(notifications.choose(0), Chosen::Nowhere);
    }

    #[test]
    fn clearing_empties_the_list() {
        let mut notifications = Notifications::new();
        notifications.status("something");
        notifications.open_history(Instant::now());
        notifications.clear_history();
        assert_eq!(notifications.history().count(), 0);
    }

    fn fonts() -> FontStack {
        FontStack::new(Box::new(BitmapFont::new(1)))
    }

    #[test]
    fn the_banner_draws_at_the_top_right() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 40, metrics.cell_height * 4);
        let mut fb = OwnedFramebuffer::new(w, h);
        let chrome = Chrome::default();
        {
            let mut surface = fb.surface();
            draw_banner(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, w, h),
                &chrome,
                "done",
            );
        }
        let accent = chrome.accent.pack();
        let row = metrics.cell_height / 2;
        assert!(
            (0..w).any(|x| fb.pixel(x, row) == accent),
            "no banner drawn"
        );
        // On the right, and only on the top row: the panes keep the rest.
        assert!((0..metrics.cell_width).all(|x| fb.pixel(x, row) != accent));
        assert!((0..w).all(|x| fb.pixel(x, metrics.cell_height * 2) != accent));
    }

    #[test]
    fn a_banner_too_wide_for_the_display_is_clipped_not_dropped() {
        let mut fonts = fonts();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 12, metrics.cell_height * 2);
        let mut fb = OwnedFramebuffer::new(w, h);
        let chrome = Chrome::default();
        {
            let mut surface = fb.surface();
            draw_banner(
                &mut surface,
                &mut fonts,
                Rect::new(0, 0, w, h),
                &chrome,
                &"long message ".repeat(20),
            );
        }
        let accent = chrome.accent.pack();
        let row = metrics.cell_height / 2;
        assert!(
            (0..w).any(|x| fb.pixel(x, row) == accent),
            "no banner drawn"
        );
    }
}
