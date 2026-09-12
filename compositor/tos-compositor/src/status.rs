//! The status bar: what it says, where each part of it sits, and what
//! clicking one does.
//!
//! What used to be here was a workspace strip with one right-aligned string
//! bolted to it, and the trouble with that shape was not that it looked wrong
//! — it is that there was nowhere to put a clock. The list was a literal in
//! the drawing function, the order was the order of the statements, and
//! "battery, then the time" was a code change rather than a line in a file.
//!
//! So the bar is a list of [`Segment`]s per side, read from the configuration,
//! and everything else here follows from that. A segment is asked for the
//! [`Piece`]s it wants to draw, which is where absence is expressed: a machine
//! with no battery returns nothing rather than a slot saying so, which is the
//! same rule [`crate::system`] follows and for the same reason — an empty slot
//! is honest and `battery: unknown` is not.
//!
//! Layout is worked out once, into [`Bar`], and both the drawing and the click
//! routing read it. That is the whole reason it is a value rather than a pass
//! of the drawing code: a click has to land on the workspace a person can see,
//! and two separate implementations of "where does the third workspace start"
//! would agree right up until somebody renamed one.
//!
//! Exactly one piece on the bar may be elastic, and by default it is the
//! message. Everything else is as wide as its text, the elastic one takes the
//! room left in the middle, and what does not fit into it is clipped with an
//! ellipsis rather than dropped — a compositor message is usually a failure
//! carrying an `io::Error`, and the ones too long for the bar are exactly the
//! ones worth reading.

use tos_font::FontStack;
use tos_render::{Rect, Surface};
use tos_session::PaneId;
use tos_system::audio::Volume;
use tos_system::bluetooth::Adapter;
use tos_system::net::Interface;
use tos_system::power::{ChargeState, PowerState};

use crate::chrome::{self, BarColors};
use crate::clock::Zone;

/// The glyph drawn between two segments.
///
/// The same one the pane dividers use, so that the whole of the compositor's
/// own interface is ruled with one line rather than with a box-drawing
/// character here and a pipe there.
const DIVIDER: &str = "│";

/// One thing the bar can be asked to show.
///
/// Deliberately a closed list rather than a format string with `%b` in it.
/// A format string would need an escape for every fact, a parser, and an
/// answer to what happens when a machine cannot supply one of them halfway
/// through a line; a list of names has none of those problems and reads
/// better in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// Every workspace by name, the active one highlighted. Clickable.
    Workspaces,
    /// Every pane in the active workspace by label, the focused one
    /// highlighted. Clickable.
    ///
    /// This is the answer to a pane that is not focused having no title
    /// anywhere: the compositor has tracked `pane.title` all along and only
    /// ever drawn the focused one, so a person watching a build in the left
    /// pane while typing in the right could not see what the left one was
    /// called. It is not in the default layout because on the one-pane session
    /// almost every session starts as it says exactly what [`Title`] says, at
    /// several times the width, and a default that grows with the number of
    /// panes is a default that eventually pushes the clock off the bar.
    ///
    /// [`Title`]: Segment::Title
    Panes,
    /// The focused pane's label, and how far back its viewport is scrolled.
    Title,
    /// How the active workspace's panes are arranged, when that is not the
    /// split tree every session starts in.
    ///
    /// The answer to layout cycling feeling random: `ctrl+shift+l` moves the
    /// panes and, without this, leaves nothing on screen saying what it moved
    /// them into. Kitty puts the same word in its tab bar.
    Layout,
    /// The leader indicator, or else the notification queue. Elastic, and
    /// clicking it opens the history.
    Message,
    /// The time, in the format and zone `[status]` asked for.
    Clock,
    /// Charge, and whether it is going up. Absent on a machine with no
    /// batteries and no chargers.
    Battery,
    /// The interface worth naming. Absent on a machine with no link.
    Network,
    /// The default card's level. Absent on a machine with no card.
    Volume,
    /// The first adapter's state. Absent on a machine with no adapter.
    Bluetooth,
}

impl Segment {
    /// The word that names this segment in a configuration file.
    pub fn name(self) -> &'static str {
        match self {
            Segment::Workspaces => "workspaces",
            Segment::Panes => "panes",
            Segment::Title => "title",
            Segment::Layout => "layout",
            Segment::Message => "message",
            Segment::Clock => "clock",
            Segment::Battery => "battery",
            Segment::Network => "network",
            Segment::Volume => "volume",
            Segment::Bluetooth => "bluetooth",
        }
    }

    pub fn parse(name: &str) -> Option<Segment> {
        Segment::ALL.iter().copied().find(|s| s.name() == name)
    }

    /// Every segment there is, which is also what an unknown name is reported
    /// against: a file that asks for `batery` gets told what it could have
    /// asked for instead of being left to guess.
    pub const ALL: [Segment; 10] = [
        Segment::Workspaces,
        Segment::Panes,
        Segment::Title,
        Segment::Layout,
        Segment::Message,
        Segment::Clock,
        Segment::Battery,
        Segment::Network,
        Segment::Volume,
        Segment::Bluetooth,
    ];
}

/// Everything `[status]` can say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Segments at the left end, in the order they read on screen.
    pub left: Vec<Segment>,
    /// Segments at the right end, likewise: the last one named is the one
    /// against the edge. Listing them the way they are read is worth more
    /// than the small saving of writing the right side backwards.
    pub right: Vec<Segment>,
    /// The clock's format; see [`crate::clock::format_time`].
    pub clock_format: String,
    pub zone: Zone,
}

impl Default for Settings {
    /// The bar tOS ships with.
    ///
    /// The left is what this session is — which workspace, which pane — and
    /// the right is what the machine is doing, with the time hard against the
    /// edge where every other desktop puts it. Volume, Bluetooth and the pane
    /// strip are left out on purpose rather than forgotten: a volume that
    /// reads `vol 35%` for a week is not news, an adapter that reads `bt off`
    /// on a machine nobody has paired anything to is less than that, and both
    /// are one word in a file away for the person who does want them.
    fn default() -> Settings {
        Settings {
            // The arrangement is on the default bar and costs nothing to
            // have there: it says nothing at all while the workspace is in
            // the splits every session starts in, which is most of the time.
            left: vec![Segment::Workspaces, Segment::Layout, Segment::Title],
            right: vec![
                Segment::Message,
                Segment::Network,
                Segment::Battery,
                Segment::Clock,
            ],
            // Minutes, because that is the granularity a status bar clock is
            // read at and the one a repaint per tick can be justified for; a
            // date, or seconds, is a format string away.
            clock_format: "%H:%M".to_string(),
            zone: Zone::Local,
        }
    }
}

impl Settings {
    /// Whether a clock is on the bar at all.
    ///
    /// Asked before the time is looked up each tick, so that a session with no
    /// clock on it does no work for one and, more to the point, does not ask
    /// for a frame a minute it has no use for.
    pub fn shows_clock(&self) -> bool {
        self.shows(Segment::Clock)
    }

    /// Whether the notification queue has anywhere to appear.
    ///
    /// A layout that leaves `message` out is a layout that would swallow every
    /// failure the compositor has to report, so the compositor falls back to
    /// the banner it already draws for `--no-status-bar`. A person is allowed
    /// to arrange their own bar; nobody is allowed an arrangement in which a
    /// split that could not happen looks like a dead key.
    pub fn shows_message(&self) -> bool {
        self.shows(Segment::Message)
    }

    fn shows(&self, segment: Segment) -> bool {
        self.left.contains(&segment) || self.right.contains(&segment)
    }

    /// Read a `left = ` or `right = ` value: segment names separated by
    /// whitespace, commas, or both, because people write lists both ways.
    pub fn parse_list(value: &str) -> Result<Vec<Segment>, String> {
        let mut segments = Vec::new();
        for word in value.split(|c: char| c.is_whitespace() || c == ',') {
            if word.is_empty() {
                continue;
            }
            match Segment::parse(word) {
                Some(segment) => segments.push(segment),
                None => {
                    let known: Vec<&str> = Segment::ALL.iter().map(|s| s.name()).collect();
                    return Err(format!(
                        "unknown segment: {word} (try one of {})",
                        known.join(", ")
                    ));
                }
            }
        }
        Ok(segments)
    }
}

/// What clicking a piece of the bar does.
///
/// A small closed list on purpose. The obvious next entries are the system
/// menus — clicking the battery to get the power menu, the link to get the
/// network one — and those belong to the issues that own those menus rather
/// than to this one; the seam for them is this enum and the match on it, both
/// of which now exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// Switch to a workspace, numbered from one the way the digit bindings
    /// are, so that the click and the key reach the same call.
    Workspace(usize),
    /// Focus a pane.
    Pane(PaneId),
    /// Open the notification history.
    Notifications,
}

/// How a piece is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// Ordinary text: readable when looked at, quiet when not.
    Normal,
    /// The workspace you are in, the pane you are typing at.
    Active,
    /// The rule between two segments.
    Divider,
}

/// One thing to draw on the bar, before anybody has worked out where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// The words themselves, with no padding: the space either side is added
    /// at layout, so that a piece which has to be clipped keeps it.
    pub text: String,
    pub ink: Ink,
    pub hit: Option<Hit>,
    /// Whether this piece gives up its width to the others.
    ///
    /// At most one piece on the whole bar is: the room left over is a single
    /// gap, and two claims on one gap would need a policy for dividing it that
    /// nobody would be able to predict from looking at the bar. The first
    /// elastic piece found wins and any later one is drawn at its natural
    /// width, which is the rule that needs no explaining.
    pub elastic: bool,
}

impl Piece {
    pub fn new(text: impl Into<String>) -> Piece {
        Piece {
            text: text.into(),
            ink: Ink::Normal,
            hit: None,
            elastic: false,
        }
    }

    pub fn active(mut self, active: bool) -> Piece {
        if active {
            self.ink = Ink::Active;
        }
        self
    }

    pub fn clicking(mut self, hit: Hit) -> Piece {
        self.hit = Some(hit);
        self
    }

    pub fn elastic(mut self) -> Piece {
        self.elastic = true;
        self
    }

    fn divider() -> Piece {
        Piece {
            text: DIVIDER.to_string(),
            ink: Ink::Divider,
            hit: None,
            elastic: false,
        }
    }

    /// How many cells this piece wants, padding included.
    fn natural_width(&self) -> u32 {
        match self.ink {
            // The rule is one cell and is not padded; the pieces either side
            // of it bring their own space.
            Ink::Divider => 1,
            _ => tos_term::str_width(&self.text) as u32 + 2,
        }
    }

    /// The string to draw when this piece has been given `width` cells.
    fn drawn(&self, width: u32) -> String {
        if self.ink == Ink::Divider {
            return self.text.clone();
        }
        if width < 3 {
            // Not enough for the padding and a character, so the padding goes
            // rather than the text: a single cell should say something.
            return chrome::clip_marked(&self.text, width as usize);
        }
        format!(" {} ", chrome::clip_marked(&self.text, width as usize - 2))
    }
}

/// A piece and the cells it ended up in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    /// The text as it will be drawn, padded and clipped.
    pub text: String,
    pub ink: Ink,
    pub hit: Option<Hit>,
    /// Cells from the left edge of the bar.
    pub col: u32,
    pub width: u32,
}

/// The bar, laid out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bar {
    placed: Vec<Placed>,
}

impl Bar {
    /// Work out where everything goes, given the groups each side and the
    /// width of the bar in cells.
    ///
    /// Each group is one segment's pieces; empty groups contribute nothing,
    /// not even a rule, so a machine with no battery does not show the gap
    /// where one would have been.
    ///
    /// When the two sides want more room than there is, the right gives way:
    /// its pieces are placed from the edge inwards and the first one that
    /// would run into the left is clipped, with anything further in dropped.
    /// That keeps the two ends — the workspaces and the clock — which are what
    /// the bar is navigated by, and spends the middle, which is where the
    /// elastic message already was.
    pub fn lay_out(left: &[Vec<Piece>], right: &[Vec<Piece>], cols: u32) -> Bar {
        let left = ruled(left);
        let right = ruled(right);

        let fixed = |pieces: &[Piece]| -> u32 {
            pieces
                .iter()
                .filter(|piece| !piece.elastic)
                .map(Piece::natural_width)
                .sum()
        };
        // The first elastic piece anywhere on the bar, and how much is left
        // for it once everything with a fixed width has had its say.
        let gap = cols.saturating_sub(fixed(&left) + fixed(&right));
        let mut elastic = left
            .iter()
            .chain(right.iter())
            .find(|piece| piece.elastic)
            .map(|piece| piece.natural_width().min(gap));

        let mut placed = Vec::with_capacity(left.len() + right.len());
        let mut col = 0;
        for piece in &left {
            let width = width_of(piece, &mut elastic);
            if col >= cols || width == 0 {
                continue;
            }
            let width = width.min(cols - col);
            placed.push(place(piece, col, width));
            col += width;
        }
        let wall = col;

        // The right side is walked backwards from the edge, which is what
        // makes "the right gives way" a property of the loop rather than a
        // second pass over it.
        let mut edge = cols;
        let mut tail = Vec::new();
        for piece in right.iter().rev() {
            let width = width_of(piece, &mut elastic);
            if width == 0 || edge <= wall {
                continue;
            }
            let width = width.min(edge - wall);
            edge -= width;
            tail.push(place(piece, edge, width));
        }
        tail.reverse();
        placed.extend(tail);
        Bar { placed }
    }

    /// Paint the bar into `area`, which is the whole of the status row.
    pub fn draw(
        &self,
        surface: &mut Surface<'_>,
        fonts: &mut FontStack,
        area: Rect,
        colors: &BarColors,
    ) {
        surface.fill(area, colors.background);
        let cw = fonts.metrics().cell_width.max(1);
        for piece in &self.placed {
            let (fg, bg) = match piece.ink {
                Ink::Normal => (colors.foreground, colors.background),
                Ink::Active => (colors.active_text, colors.active),
                Ink::Divider => (colors.divider, colors.background),
            };
            chrome::draw_text(
                surface,
                fonts,
                area.x + (piece.col * cw) as i32,
                area.y,
                &piece.text,
                fg,
                Some(bg),
                piece.ink == Ink::Active,
            );
        }
    }

    /// What was clicked, `col` cells from the left of the bar.
    pub fn hit(&self, col: u32) -> Option<Hit> {
        self.placed
            .iter()
            .find(|piece| col >= piece.col && col < piece.col + piece.width)
            .and_then(|piece| piece.hit)
    }

    /// Everything on the bar, for a test that would rather read the words than
    /// the pixels.
    pub fn pieces(&self) -> &[Placed] {
        &self.placed
    }

    /// Everything on the bar as one line, which is what a test asserting about
    /// content rather than layout wants.
    pub fn text(&self) -> String {
        self.placed
            .iter()
            .map(|piece| piece.text.as_str())
            .collect()
    }
}

/// Flatten groups into one run of pieces with a rule between each pair.
fn ruled(groups: &[Vec<Piece>]) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for group in groups.iter().filter(|group| !group.is_empty()) {
        if !out.is_empty() {
            out.push(Piece::divider());
        }
        out.extend(group.iter().cloned());
    }
    out
}

/// How wide this piece is, spending the elastic allowance if it is the one
/// that claimed it.
fn width_of(piece: &Piece, elastic: &mut Option<u32>) -> u32 {
    if !piece.elastic {
        return piece.natural_width();
    }
    // `take` rather than a peek: the allowance is spent once, so a second
    // elastic piece falls back to its natural width.
    match elastic.take() {
        Some(width) => width,
        None => piece.natural_width(),
    }
}

fn place(piece: &Piece, col: u32, width: u32) -> Placed {
    Placed {
        text: piece.drawn(width),
        ink: piece.ink,
        hit: piece.hit,
        col,
        width,
    }
}

// ---- what the machine's readings say on a bar ---------------------------
//
// Each of these is the short form. `tos-system` already has a `summary()` on
// every one of these types, and those are the long forms, for the menus that
// own them: `62% discharging, 3h 20m left` is the right answer to "tell me
// about the battery" and the wrong one to a strip that also has to fit a
// clock. Short enough to leave room, long enough to be unambiguous, and
// labelled wherever a bare number would not say what it was a number of.

/// Charge, and whether it is going up, or nothing at all to show.
pub fn battery_text(power: &PowerState) -> Option<String> {
    if !power.has_battery() {
        // A desktop lists chargers and no batteries. Saying it is on mains
        // once is worth it — it is the difference between "no battery" and
        // "this reader is broken" — and there is nothing else it could say.
        return match power.on_mains() {
            Some(true) => Some("ac".to_string()),
            _ => None,
        };
    }
    let charge = match power.percent() {
        Some(percent) => format!("{percent}%"),
        // A battery that will not say how full it is still says whether it is
        // charging, which is most of what anybody watches it for.
        None => "bat".to_string(),
    };
    Some(match power.state() {
        // A plus sign rather than the word: it is the one bit of the battery
        // that changes while you are looking at it, and it has to survive the
        // bar being narrow.
        ChargeState::Charging => format!("+{charge}"),
        ChargeState::Full => "full".to_string(),
        _ => charge,
    })
}

/// The link, named by what a person would call it: the network they joined if
/// it is wireless and associated, and the kernel's name for it otherwise.
pub fn network_text(link: &Interface) -> String {
    let name = link.ssid().unwrap_or(&link.name);
    if link.is_online() {
        return name.to_string();
    }
    // Being on a link that is not working is worth showing; the `summary()`
    // this leans away from would put an address here, and there isn't one.
    format!("{name} {}", if link.carrier { "no ip" } else { "down" })
}

/// The level, labelled, because a bar can carry three percentages at once and
/// an unlabelled one is a guess.
pub fn volume_text(volume: &Volume) -> String {
    if volume.muted {
        return "vol mute".to_string();
    }
    format!("vol {}%", volume.percent)
}

/// The adapter, in the one word [`Adapter::state`] already reduces it to.
pub fn bluetooth_text(adapter: &Adapter) -> String {
    format!("bt {}", adapter.state())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tos_font::BitmapFont;
    use tos_render::OwnedFramebuffer;
    use tos_system::net::{Kind, LinkState};
    use tos_system::power::{Battery, Supply, SupplyKind};

    fn fonts() -> FontStack {
        FontStack::new(Box::new(BitmapFont::new(1)))
    }

    /// One side of the bar carrying a single segment, which is the shape most
    /// of these tests want: `Bar::lay_out` takes a group per segment.
    fn group(pieces: Vec<Piece>) -> Vec<Vec<Piece>> {
        vec![pieces]
    }

    #[test]
    fn segments_are_named_the_same_way_coming_and_going() {
        for segment in Segment::ALL {
            assert_eq!(Segment::parse(segment.name()), Some(segment));
        }
        assert_eq!(Segment::parse("batery"), None);
    }

    #[test]
    fn a_segment_list_is_read_however_it_was_written() {
        assert_eq!(
            Settings::parse_list("workspaces title"),
            Ok(vec![Segment::Workspaces, Segment::Title])
        );
        assert_eq!(
            Settings::parse_list("workspaces, title,clock"),
            Ok(vec![Segment::Workspaces, Segment::Title, Segment::Clock])
        );
        // An empty list is a side with nothing on it, which is a thing
        // somebody may well want and not a mistake to report.
        assert_eq!(Settings::parse_list("  "), Ok(Vec::new()));
    }

    #[test]
    fn an_unknown_segment_says_what_the_known_ones_are() {
        let problem = Settings::parse_list("workspaces batery").expect_err("should refuse");
        assert!(problem.contains("batery"), "{problem}");
        assert!(problem.contains("battery"), "{problem}");
    }

    #[test]
    fn pieces_are_laid_out_from_both_ends_with_a_rule_between_segments() {
        let bar = Bar::lay_out(
            &[vec![Piece::new("1")], vec![Piece::new("sh")]],
            &[vec![Piece::new("12:00")]],
            40,
        );
        let pieces = bar.pieces();
        // Left: " 1 ", the rule, " sh ". Right: " 12:00 " against the edge.
        assert_eq!(pieces[0].col, 0);
        assert_eq!(pieces[0].text, " 1 ");
        assert_eq!(pieces[1].ink, Ink::Divider);
        assert_eq!(pieces[2].text, " sh ");
        let clock = pieces.last().expect("a clock");
        assert_eq!(clock.text, " 12:00 ");
        assert_eq!(clock.col + clock.width, 40, "the clock is against the edge");
    }

    #[test]
    fn a_segment_with_nothing_to_say_takes_no_room_and_no_rule() {
        // The machine with no battery: the group is empty, and the bar must
        // not show the rule that would have been beside it.
        let bar = Bar::lay_out(
            &[],
            &[
                vec![Piece::new("wlan0")],
                Vec::new(),
                vec![Piece::new("12:00")],
            ],
            40,
        );
        let rules = bar
            .pieces()
            .iter()
            .filter(|piece| piece.ink == Ink::Divider)
            .count();
        assert_eq!(rules, 1, "{}", bar.text());
    }

    /// A bar `cols` wide with a workspace on the left, and a message and a
    /// clock on the right — the shape of the shipped layout. The pieces come
    /// out as `[" 1 ", message, rule, " 12:00 "]`, so the elastic one is
    /// always the second.
    fn bar_around_a_message(cols: u32) -> Bar {
        Bar::lay_out(
            &group(vec![Piece::new("1")]),
            &[
                vec![Piece::new("copied").elastic()],
                vec![Piece::new("12:00")],
            ],
            cols,
        )
    }

    #[test]
    fn the_elastic_piece_takes_what_it_needs_and_gives_back_the_rest() {
        // Elastic means it yields, not that it inflates: a six letter message
        // on a wide bar is eight cells, not half the bar painted in message
        // background.
        let bar = bar_around_a_message(40);
        let message = &bar.pieces()[1];
        assert_eq!(message.text, " copied ");
        // " 12:00 " is 7 and its rule is 1, so the message ends up hard
        // against the clock rather than adrift in the middle of the bar.
        assert_eq!(message.col + message.width, 40 - 8);
    }

    #[test]
    fn the_elastic_piece_is_the_only_thing_that_gives_way_when_room_runs_out() {
        // Seventeen cells: 3 for the workspace and 8 for the clock with its
        // rule, leaving six for a message that wanted eight.
        let bar = bar_around_a_message(17);
        let message = &bar.pieces()[1];
        assert_eq!(message.width, 6);
        assert_eq!(message.text, " cop… ");
        // And the clock, which is not elastic, is untouched by any of it.
        let clock = bar.pieces().last().expect("a clock");
        assert_eq!(clock.text, " 12:00 ");
        assert_eq!(clock.col + clock.width, 17);
    }

    /// Draw a bar of `cols` cells with one highlighted workspace and this
    /// message, and say whether any of the message reached the surface. The
    /// workspace is highlighted so the only ordinary ink can be the message.
    fn bar_with_message(cols: u32, message: &str) -> bool {
        let mut fonts = fonts();
        let colors = crate::chrome::Chrome::default().bar();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * cols, metrics.cell_height);
        let mut fb = OwnedFramebuffer::new(w, h);
        let bar = Bar::lay_out(
            &group(vec![Piece::new("1").active(true)]),
            &group(vec![Piece::new(message).elastic()]),
            cols,
        );
        {
            let mut surface = fb.surface();
            bar.draw(&mut surface, &mut fonts, Rect::new(0, 0, w, h), &colors);
        }
        fb.pixels().iter().any(|&px| px == colors.foreground.pack())
    }

    #[test]
    fn a_message_too_long_for_the_bar_is_clipped_rather_than_dropped() {
        // The message that does not fit is the one worth reading: this is
        // what an `io::Error` from a failed split looks like on a narrow bar.
        let long = "split failed: no such file or directory (os error 2)";
        assert!(bar_with_message(20, long), "the message was not drawn");
        assert!(bar_with_message(80, long));
        // Down to a bar with no room at all for it, which draws nothing and
        // does not panic working out where the text would start.
        assert!(!bar_with_message(3, long));
    }

    #[test]
    fn the_status_bar_fills_its_area() {
        let mut fonts = fonts();
        let colors = crate::chrome::Chrome::default().bar();
        let metrics = fonts.metrics();
        let (w, h) = (metrics.cell_width * 20, metrics.cell_height);
        let mut fb = OwnedFramebuffer::new(w, h);
        let bar = Bar::lay_out(
            &group(vec![
                Piece::new("1").active(true),
                Piece::new("2").active(false),
            ]),
            &group(vec![Piece::new("tOS")]),
            20,
        );
        {
            let mut surface = fb.surface();
            bar.draw(&mut surface, &mut fonts, Rect::new(0, 0, w, h), &colors);
        }
        // The highlighted workspace uses the active colour.
        assert!(fb.pixels().iter().any(|&px| px == colors.active.pack()));
        // And nothing is left transparent.
        assert!(!fb.pixels().iter().all(|&px| px == 0));
    }

    #[test]
    fn a_bar_narrower_than_its_contents_keeps_both_ends() {
        // Sixteen cells for a left side that wants ten and a right side that
        // wants seventeen: the workspace survives, the clock survives, and
        // what is between them is what goes.
        let bar = Bar::lay_out(
            &group(vec![Piece::new("build")]),
            &[vec![Piece::new("wlan0")], vec![Piece::new("12:00")]],
            16,
        );
        let text = bar.text();
        assert!(text.starts_with(" build "), "{text}");
        assert!(text.ends_with(" 12:00 "), "{text}");
        let last = bar.pieces().last().expect("a clock");
        assert_eq!(last.col + last.width, 16);
        // Nothing was placed twice over the same cell.
        for pair in bar.pieces().windows(2) {
            assert!(
                pair[0].col + pair[0].width <= pair[1].col,
                "{:?} overlaps {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_bar_with_no_room_at_all_lays_out_nothing_and_does_not_panic() {
        for cols in [0, 1, 2] {
            let bar = Bar::lay_out(
                &group(vec![Piece::new("workspace one")]),
                &group(vec![Piece::new("12:00")]),
                cols,
            );
            let used: u32 = bar.pieces().iter().map(|piece| piece.width).sum();
            assert!(used <= cols, "{used} cells drawn into {cols}");
        }
    }

    #[test]
    fn a_click_lands_on_the_workspace_it_looks_like_it_landed_on() {
        let bar = Bar::lay_out(
            &group(vec![
                Piece::new("one").clicking(Hit::Workspace(1)).active(true),
                Piece::new("2").clicking(Hit::Workspace(2)),
            ]),
            &group(vec![Piece::new("12:00")]),
            40,
        );
        // " one " is cells 0 to 4, " 2 " is 5 to 7.
        assert_eq!(bar.hit(0), Some(Hit::Workspace(1)));
        assert_eq!(bar.hit(4), Some(Hit::Workspace(1)));
        assert_eq!(bar.hit(5), Some(Hit::Workspace(2)));
        assert_eq!(bar.hit(7), Some(Hit::Workspace(2)));
        // The empty middle and the clock are not workspaces.
        assert_eq!(bar.hit(20), None);
        assert_eq!(bar.hit(39), None);
    }

    fn battery(percent: Option<u8>, state: ChargeState) -> PowerState {
        PowerState {
            batteries: vec![Battery {
                name: "BAT0".to_string(),
                present: true,
                state,
                percent,
                remaining: None,
                full: None,
                unit: None,
                time_remaining: None,
                power_watts: None,
            }],
            mains: Vec::new(),
        }
    }

    #[test]
    fn a_battery_says_how_full_it_is_and_which_way_it_is_going() {
        assert_eq!(
            battery_text(&battery(Some(62), ChargeState::Discharging)).as_deref(),
            Some("62%")
        );
        assert_eq!(
            battery_text(&battery(Some(62), ChargeState::Charging)).as_deref(),
            Some("+62%")
        );
        assert_eq!(
            battery_text(&battery(Some(100), ChargeState::Full)).as_deref(),
            Some("full")
        );
        // A battery that will not say how full it is still says the rest.
        assert_eq!(
            battery_text(&battery(None, ChargeState::Charging)).as_deref(),
            Some("+bat")
        );
    }

    #[test]
    fn a_machine_with_no_battery_shows_a_charger_or_shows_nothing() {
        let desktop = PowerState {
            batteries: Vec::new(),
            mains: vec![Supply {
                name: "ADP1".to_string(),
                kind: SupplyKind::Mains,
                online: Some(true),
            }],
        };
        assert_eq!(battery_text(&desktop).as_deref(), Some("ac"));
        // And a supply that says it is not delivering anything, on a machine
        // with no battery, is not a fact worth a slot on the bar.
        let unplugged = PowerState {
            batteries: Vec::new(),
            mains: vec![Supply {
                name: "ADP1".to_string(),
                kind: SupplyKind::Mains,
                online: Some(false),
            }],
        };
        assert_eq!(battery_text(&unplugged), None);
    }

    fn interface(name: &str, online: bool) -> Interface {
        Interface {
            name: name.to_string(),
            kind: Kind::Wired,
            state: if online {
                LinkState::Up
            } else {
                LinkState::Down
            },
            carrier: online,
            admin_up: online,
            mac: None,
            mtu: None,
            speed_mbps: None,
            addresses: Vec::new(),
            is_default: false,
            gateway: None,
            rx_bytes: 0,
            tx_bytes: 0,
            wireless: None,
        }
    }

    #[test]
    fn a_link_that_is_not_working_says_so_rather_than_going_quiet() {
        assert_eq!(network_text(&interface("eth0", true)), "eth0 no ip");
        assert_eq!(network_text(&interface("eth0", false)), "eth0 down");
    }

    #[test]
    fn the_volume_is_labelled_because_a_bare_percentage_is_a_guess() {
        assert_eq!(
            volume_text(&Volume {
                percent: 35,
                muted: false
            }),
            "vol 35%"
        );
        assert_eq!(
            volume_text(&Volume {
                percent: 35,
                muted: true
            }),
            "vol mute"
        );
    }
}
