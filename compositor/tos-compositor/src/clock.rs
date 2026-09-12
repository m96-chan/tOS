//! What time it is, where the machine is standing.
//!
//! A clock on a status bar is two problems wearing one coat. The first is
//! arithmetic: a Unix timestamp counts seconds and a person reads years,
//! months and minutes, and the conversion between them is a closed-form
//! calculation that fits on a page. The second is politics: the offset from
//! UT is a decision taken by a government, published as a database, revised
//! several times a year, and shipped to this machine as a file. Nothing can
//! compute it, so this module reads it.
//!
//! tOS takes no dependency for either half, which is the same rule that made
//! it write its own PNG decoder and its own configuration parser. The
//! arithmetic is Howard Hinnant's civil-from-days, which is a dozen lines of
//! integer division valid for every year anybody will put on a status bar.
//! The politics is the TZif format that `/etc/localtime` is written in,
//! parsed here, plus the POSIX `TZ` string in its footer for the years past
//! the end of its transition table — modern `zic` writes a table that stops a
//! few years out and expects the reader to carry on from the rule, so a parser
//! that stopped at the table would show the wrong hour for half of every year
//! from about 2038 onward.
//!
//! Showing UT and calling it a day was the alternative, and it was rejected:
//! an installed tOS is somebody's whole desktop, the machine already knows
//! what zone it is in because the installer set the symlink, and a clock that
//! is confidently nine hours out is worse than no clock. What is not attempted
//! is leap seconds, which the file also carries: nothing on a status bar is
//! measured to the second in TAI, and the kernel is not handing us TAI anyway.

use std::path::PathBuf;

/// Where `/etc/localtime` points, and where a named zone is looked up.
///
/// Not configurable. Which directory holds the time zone database is a
/// property of the filesystem rather than a preference, and a setting that
/// moved it would only ever be a way to be told the wrong time.
const ZONEINFO: &str = "/usr/share/zoneinfo";

/// The file the system's chosen zone is reached through.
const LOCALTIME: &str = "/etc/localtime";

/// Which zone the clock is in.
///
/// `Local` is the machine's own, which is what almost everyone wants and what
/// the installer has already arranged. `Utc` is here because a machine that is
/// a server as much as a desktop is often run by people who think in UT and
/// would rather the bar did too, and because it is the honest fallback when
/// the zone cannot be read. `Named` is for the case the other two cannot
/// cover: a session being used from somewhere other than where the machine
/// lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Zone {
    Local,
    Utc,
    /// A zone by its database name, such as `Asia/Tokyo`.
    Named(String),
}

impl Zone {
    /// Read a `timezone = ` value.
    ///
    /// Anything that is not one of the two words is taken as a zone name,
    /// because the database has nearly six hundred of them and listing the
    /// ones this parser accepts would be listing the database.
    pub fn parse(value: &str) -> Zone {
        match value {
            "local" | "system" => Zone::Local,
            "utc" | "UTC" => Zone::Utc,
            other => Zone::Named(other.to_string()),
        }
    }
}

/// The clock the status bar draws: a format, and the zone to apply first.
///
/// The zone is loaded once, when the compositor starts, rather than per frame:
/// the file is a few kilobytes and reading it sixty times a second to be told
/// the same thing would be a syscall storm in the one place tOS is careful
/// about them. The cost is that moving `/etc/localtime` under a running
/// session does not move the clock, which is a thing that happens once, at
/// install time, before there is a session.
#[derive(Debug, Clone)]
pub struct Clock {
    format: String,
    zone: TimeZone,
}

impl Clock {
    /// Build a clock, reading whatever file `zone` names.
    ///
    /// A zone that cannot be read falls back to UT rather than failing: the
    /// compositor is `/init` on an installed machine, and there is no version
    /// of "the time zone file is missing" that should keep a shell off the
    /// screen.
    pub fn new(format: impl Into<String>, zone: &Zone) -> Clock {
        Clock {
            format: format.into(),
            zone: TimeZone::load(zone),
        }
    }

    /// What the bar shows at this instant, in seconds since the epoch.
    ///
    /// Takes the time rather than reading it, which is what lets a test watch
    /// a minute turn over without waiting one out.
    pub fn text(&self, unix: i64) -> String {
        let (offset, abbreviation) = self.zone.offset_at(unix);
        let civil = Civil::at(unix + offset as i64);
        format_time(&self.format, &civil, abbreviation, offset)
    }
}

/// A date and a time of day, with no zone attached — the local wall reading
/// that comes out of applying an offset to a timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 is Sunday, which is the numbering the POSIX `TZ` rules use and so
    /// the one it costs nothing to keep.
    pub weekday: u32,
    /// 1 to 366.
    pub year_day: u32,
}

impl Civil {
    /// The wall reading for a count of seconds.
    ///
    /// `seconds` is already local: an offset has been added to a timestamp by
    /// whoever knew what the offset was. Keeping the two steps apart is what
    /// lets the zone lookup be tested on its own.
    pub fn at(seconds: i64) -> Civil {
        // Flooring division, not truncating, because a timestamp before 1970
        // still has to land on the day it belongs to rather than the next one.
        let days = seconds.div_euclid(86_400);
        let rest = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let jan1 = days_from_civil(year, 1, 1);
        Civil {
            year,
            month,
            day,
            hour: (rest / 3600) as u32,
            minute: (rest / 60 % 60) as u32,
            second: (rest % 60) as u32,
            // 1970-01-01 was a Thursday, so shifting by four puts Sunday at
            // zero.
            weekday: (days + 4).rem_euclid(7) as u32,
            year_day: (days - jan1 + 1) as u32,
        }
    }

    /// The seconds this wall reading stands for, with no offset applied.
    pub fn seconds(year: i64, month: u32, day: u32, hour: i64, minute: i64, second: i64) -> i64 {
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second
    }
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Expand a format string.
///
/// The specifiers are a subset of `strftime`'s, chosen by what fits on a bar:
/// the numbers, the names of days and months, the zone, and the three
/// shorthands anyone writes by hand. An unknown specifier is copied out
/// verbatim rather than dropped, because a typo that produces a shorter clock
/// is a typo nobody finds, while one that produces `%q` on the bar is found
/// immediately.
pub fn format_time(format: &str, civil: &Civil, abbreviation: &str, offset: i32) -> String {
    let mut out = String::new();
    let mut chars = format.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let Some(spec) = chars.next() else {
            // A trailing percent is not a specifier; it is a percent sign.
            out.push('%');
            break;
        };
        match spec {
            'Y' => out.push_str(&civil.year.to_string()),
            'y' => out.push_str(&pad2(civil.year.rem_euclid(100) as u32)),
            'm' => out.push_str(&pad2(civil.month)),
            'd' => out.push_str(&pad2(civil.day)),
            // Space padded, which is what a date reads like in prose.
            'e' => out.push_str(&format!("{:2}", civil.day)),
            'H' => out.push_str(&pad2(civil.hour)),
            'I' => out.push_str(&pad2(hour12(civil.hour))),
            'M' => out.push_str(&pad2(civil.minute)),
            'S' => out.push_str(&pad2(civil.second)),
            'j' => out.push_str(&format!("{:03}", civil.year_day)),
            'p' => out.push_str(if civil.hour < 12 { "AM" } else { "PM" }),
            'P' => out.push_str(if civil.hour < 12 { "am" } else { "pm" }),
            'a' => out.push_str(&WEEKDAYS[civil.weekday as usize % 7][..3]),
            'A' => out.push_str(WEEKDAYS[civil.weekday as usize % 7]),
            'b' => out.push_str(&MONTHS[(civil.month as usize).clamp(1, 12) - 1][..3]),
            'B' => out.push_str(MONTHS[(civil.month as usize).clamp(1, 12) - 1]),
            'Z' => out.push_str(abbreviation),
            'z' => out.push_str(&numeric_offset(offset)),
            'F' => out.push_str(&format!(
                "{}-{}-{}",
                civil.year,
                pad2(civil.month),
                pad2(civil.day)
            )),
            'T' => out.push_str(&format!(
                "{}:{}:{}",
                pad2(civil.hour),
                pad2(civil.minute),
                pad2(civil.second)
            )),
            'R' => out.push_str(&format!("{}:{}", pad2(civil.hour), pad2(civil.minute))),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

fn pad2(value: u32) -> String {
    format!("{value:02}")
}

fn hour12(hour: u32) -> u32 {
    match hour % 12 {
        0 => 12,
        other => other,
    }
}

fn numeric_offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.unsigned_abs();
    format!("{sign}{:02}{:02}", seconds / 3600, seconds / 60 % 60)
}

/// Days since 1970-01-01 for a civil date, and back again.
///
/// Hinnant's algorithm, which works by moving the start of the year to March
/// so that the leap day lands at the end of it and every month before it has a
/// length that fits one linear formula. Written out rather than taken from a
/// crate for the reason everything else here is, and it is fifteen lines.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - if month <= 2 { 1 } else { 0 };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let shifted = if month > 2 { month - 3 } else { month + 9 } as i64;
    let day_of_year = (153 * shifted + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    // Flooring division: the algorithm needs the era a negative day falls in,
    // and truncation would put the years before 1600 in the one after theirs.
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_shifted + 2) / 5 + 1) as u32;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    } as u32;
    (year + if month <= 2 { 1 } else { 0 }, month, day)
}

/// One of the offsets a zone uses, and what it is called while it is in force.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Offset {
    /// Seconds to add to UT to get local time. East of Greenwich is positive,
    /// which is the sign convention of the file and the opposite of the one in
    /// a POSIX `TZ` string; see [`parse_posix_offset`].
    seconds: i32,
    abbreviation: String,
}

/// A zone: the changes that have been made, and the rule for the ones that
/// have not happened yet.
#[derive(Debug, Clone)]
pub struct TimeZone {
    /// When each change took effect, in UT, with the offset it changed to.
    /// In order, which the file guarantees and the binary search below needs.
    transitions: Vec<(i64, u8)>,
    types: Vec<Offset>,
    /// What to do after the last transition. `zic` has written slim files
    /// since 2020, meaning a table that stops a few years out and a rule in
    /// the footer that covers everything after it, so this is not an edge
    /// case: on a current machine it is what answers most of the questions
    /// about next summer.
    rule: Option<PosixRule>,
}

impl TimeZone {
    /// UT, which is also what every failure here falls back to.
    pub fn utc() -> TimeZone {
        TimeZone {
            transitions: Vec::new(),
            types: vec![Offset {
                seconds: 0,
                abbreviation: "UTC".to_string(),
            }],
            rule: None,
        }
    }

    /// The zone a [`Zone`] names, or UT when it cannot be had.
    ///
    /// `TZ` is honoured for `Zone::Local` because that is the variable every
    /// other program on the machine obeys, and a session whose clock ignored
    /// it would be the only thing on the screen showing a different hour from
    /// the shell in the pane. Its `:`-prefixed form is the documented way of
    /// saying "this is a path", and a bare name that is not a POSIX rule is
    /// treated as a zone name, which is what every implementation does.
    pub fn load(zone: &Zone) -> TimeZone {
        match zone {
            Zone::Utc => TimeZone::utc(),
            Zone::Named(name) => {
                TimeZone::from_file(&zone_path(name)).unwrap_or_else(TimeZone::utc)
            }
            Zone::Local => TimeZone::local(),
        }
    }

    fn local() -> TimeZone {
        if let Some(value) = std::env::var("TZ").ok().filter(|v| !v.is_empty()) {
            let stripped = value.strip_prefix(':').unwrap_or(&value);
            // A value with a slash in it, or one the rule parser rejects, is a
            // zone name. `TZ=UTC0` is a rule; `TZ=Europe/Berlin` is a file.
            if let Some(zone) = TimeZone::from_file(&zone_path(stripped)) {
                return zone;
            }
            if let Some(rule) = PosixRule::parse(stripped) {
                return TimeZone::from_rule(rule);
            }
        }
        TimeZone::from_file(&PathBuf::from(LOCALTIME)).unwrap_or_else(TimeZone::utc)
    }

    fn from_file(path: &std::path::Path) -> Option<TimeZone> {
        TimeZone::from_tzif(&std::fs::read(path).ok()?)
    }

    fn from_rule(rule: PosixRule) -> TimeZone {
        TimeZone {
            transitions: Vec::new(),
            types: vec![rule.standard.clone()],
            rule: Some(rule),
        }
    }

    /// The offset in force at an instant, and what it is called then.
    pub fn offset_at(&self, unix: i64) -> (i32, &str) {
        // Past the end of the table the rule is the answer, and on a slim file
        // the table ends in the recent past.
        if let Some(rule) = &self.rule {
            let past_table = self
                .transitions
                .last()
                .map(|(at, _)| unix >= *at)
                .unwrap_or(true);
            if past_table {
                let offset = rule.offset_at(unix);
                return (offset.seconds, &offset.abbreviation);
            }
        }
        let index = match self.transitions.binary_search_by_key(&unix, |(at, _)| *at) {
            Ok(found) => Some(found),
            // Before the first transition there is nothing the table can say,
            // so the first offset it lists stands: that is what the zone was
            // doing before anybody wrote any of this down.
            Err(0) => None,
            Err(after) => Some(after - 1),
        };
        let offset = index
            .and_then(|index| self.transitions.get(index))
            .and_then(|(_, kind)| self.types.get(*kind as usize))
            .or_else(|| self.types.first());
        match offset {
            Some(offset) => (offset.seconds, &offset.abbreviation),
            None => (0, "UTC"),
        }
    }

    /// Parse a TZif file.
    ///
    /// Every length in the header is read before it is used to index anything,
    /// and every slice is taken with a checked range: this file comes from the
    /// filesystem of a machine that boots straight into the compositor, and a
    /// truncated or hostile one has to end in `None` rather than in a panic
    /// with no shell behind it.
    pub fn from_tzif(bytes: &[u8]) -> Option<TimeZone> {
        let header = Header::parse(bytes)?;
        // Version 2 and later repeat the whole thing with 64 bit timestamps,
        // and it is the second copy that is worth having: the first cannot
        // express a transition past 2038, and every zone has some.
        if header.version >= b'2' {
            let rest = bytes.get(header.end_of_data()..)?;
            let wide = Header::parse(rest)?;
            let (zone, used) = TimeZone::read_block(rest, &wide, 8)?;
            let footer = rest.get(used..)?;
            return Some(TimeZone {
                rule: parse_footer(footer),
                ..zone
            });
        }
        Some(TimeZone::read_block(bytes, &header, 4)?.0)
    }

    /// Read one data block, whose transition times are `width` bytes each.
    /// Returns the zone and how far into `bytes` the block reached.
    fn read_block(bytes: &[u8], header: &Header, width: usize) -> Option<(TimeZone, usize)> {
        let mut at = Header::SIZE;
        let mut transitions = Vec::with_capacity(header.transition_count);
        for index in 0..header.transition_count {
            let start = at + index * width;
            let slice = bytes.get(start..start + width)?;
            transitions.push((read_int(slice), 0u8));
        }
        at += header.transition_count * width;

        for (index, transition) in transitions.iter_mut().enumerate() {
            let kind = *bytes.get(at + index)?;
            // An index outside the type table is a corrupt file, and reading
            // it as zero would be inventing an offset.
            if kind as usize >= header.type_count {
                return None;
            }
            transition.1 = kind;
        }
        at += header.transition_count;

        let types_at = at;
        at += header.type_count * 6;
        let names = bytes.get(at..at + header.char_count)?;
        let mut types = Vec::with_capacity(header.type_count);
        for index in 0..header.type_count {
            let record = bytes.get(types_at + index * 6..types_at + index * 6 + 6)?;
            let seconds = read_int(&record[..4]) as i32;
            let name_at = record[5] as usize;
            types.push(Offset {
                seconds,
                abbreviation: read_name(names, name_at)?,
            });
        }
        at += header.char_count;
        // The leap second table and the two flag arrays are skipped rather
        // than read: nothing on a status bar is measured in TAI, and the flags
        // only matter to code resolving a POSIX rule against the file, which
        // is not what the footer rule is for here.
        at += header.leap_count * (width + 4);
        at += header.is_std_count + header.is_ut_count;
        Some((
            TimeZone {
                transitions,
                types,
                rule: None,
            },
            at,
        ))
    }
}

/// Where a zone name is looked for.
fn zone_path(name: &str) -> PathBuf {
    PathBuf::from(ZONEINFO).join(name)
}

/// The fixed part of a TZif header, twice over in a version 2 file.
struct Header {
    version: u8,
    is_ut_count: usize,
    is_std_count: usize,
    leap_count: usize,
    transition_count: usize,
    type_count: usize,
    char_count: usize,
}

impl Header {
    const SIZE: usize = 44;

    fn parse(bytes: &[u8]) -> Option<Header> {
        let head = bytes.get(..Header::SIZE)?;
        if &head[..4] != b"TZif" {
            return None;
        }
        let count = |at: usize| read_int(&head[at..at + 4]) as usize;
        let header = Header {
            version: head[4],
            is_ut_count: count(20),
            is_std_count: count(24),
            leap_count: count(28),
            transition_count: count(32),
            type_count: count(36),
            char_count: count(40),
        };
        // A file with no local time types cannot say what any instant is, and
        // every other read here assumes there is at least one.
        if header.type_count == 0 {
            return None;
        }
        Some(header)
    }

    /// How far the version 1 block this header describes reaches, which is
    /// where the version 2 header begins.
    fn end_of_data(&self) -> usize {
        Header::SIZE
            + self.transition_count * 5
            + self.type_count * 6
            + self.char_count
            + self.leap_count * 8
            + self.is_std_count
            + self.is_ut_count
    }
}

/// A big endian signed integer of four or eight bytes, which is the only
/// number format the file uses.
fn read_int(bytes: &[u8]) -> i64 {
    let mut value: i64 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for byte in bytes {
        value = (value << 8) | *byte as i64;
    }
    value
}

/// One NUL terminated abbreviation out of the designation block.
fn read_name(names: &[u8], at: usize) -> Option<String> {
    let rest = names.get(at..)?;
    let end = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

/// The rule between the two newlines that close a version 2 file.
fn parse_footer(footer: &[u8]) -> Option<PosixRule> {
    let text = std::str::from_utf8(footer).ok()?;
    let text = text.trim_matches(|c: char| c == '\n' || c == '\0' || c == '\r');
    if text.is_empty() {
        return None;
    }
    PosixRule::parse(text)
}

/// A POSIX `TZ` rule: standard time, and the summer time that interrupts it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PosixRule {
    standard: Offset,
    /// The summer offset and the two dates it runs between, when the zone has
    /// one. A zone that does not — most of Asia, and every zone that has given
    /// up on the practice — parses to a rule with a single offset, which is
    /// the whole answer for every year the table does not cover.
    daylight: Option<(Offset, Change, Change)>,
}

impl PosixRule {
    /// Parse `CET-1CEST,M3.5.0,M10.5.0/3` and the simpler forms of it.
    fn parse(text: &str) -> Option<PosixRule> {
        let mut parts = text.split(',');
        let offsets = parts.next()?;
        let (standard_name, rest) = parse_abbreviation(offsets)?;
        let (standard_offset, rest) = parse_posix_offset(rest)?;
        let standard = Offset {
            seconds: standard_offset,
            abbreviation: standard_name,
        };
        if rest.is_empty() {
            return Some(PosixRule {
                standard,
                daylight: None,
            });
        }
        let (daylight_name, rest) = parse_abbreviation(rest)?;
        // An unstated summer offset is an hour ahead of standard, which the
        // standard says and which every zone that omits it means.
        let daylight_offset = if rest.is_empty() {
            standard_offset + 3600
        } else {
            parse_posix_offset(rest)?.0
        };
        let start = Change::parse(parts.next()?)?;
        let end = Change::parse(parts.next()?)?;
        Some(PosixRule {
            standard,
            daylight: Some((
                Offset {
                    seconds: daylight_offset,
                    abbreviation: daylight_name,
                },
                start,
                end,
            )),
        })
    }

    /// Which of the rule's offsets is in force at an instant.
    ///
    /// The dates in the rule are wall clock times, so each has to be turned
    /// back into an instant before it can be compared with one — and with the
    /// offset that was in force just before the change, not after it, which is
    /// why the two ends use different offsets. A southern hemisphere zone has
    /// its summer straddling the new year, which shows up as a start later in
    /// the year than the end, so the test is inverted rather than special
    /// cased anywhere else.
    fn offset_at(&self, unix: i64) -> &Offset {
        let Some((daylight, start, end)) = &self.daylight else {
            return &self.standard;
        };
        let year = Civil::at(unix + self.standard.seconds as i64).year;
        let starts = start.instant(year) - self.standard.seconds as i64;
        let ends = end.instant(year) - daylight.seconds as i64;
        let summer = if starts <= ends {
            unix >= starts && unix < ends
        } else {
            unix >= starts || unix < ends
        };
        if summer {
            daylight
        } else {
            &self.standard
        }
    }
}

/// When in a year an offset changes, in local wall clock time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    /// `Mm.w.d`: the `w`th `d` of month `m`, where a `w` of 5 means the last
    /// one whether or not there are five.
    MonthWeekDay {
        month: u32,
        week: u32,
        weekday: u32,
        seconds: i64,
    },
    /// `Jn`: the `n`th day of the year, never counting 29 February — so the
    /// same date every year, which is the point of the form.
    JulianNoLeap { day: u32, seconds: i64 },
    /// `n`: the `n`th day of the year counting from zero, leap day included.
    DayOfYear { day: u32, seconds: i64 },
}

impl Change {
    fn parse(text: &str) -> Option<Change> {
        let (date, time) = match text.split_once('/') {
            Some((date, time)) => (date, parse_rule_time(time)?),
            // Two in the morning is the default the standard gives, and the
            // hour most of the world actually changes at.
            None => (text, 2 * 3600),
        };
        if let Some(rest) = date.strip_prefix('M') {
            let mut fields = rest.split('.');
            let month: u32 = fields.next()?.parse().ok()?;
            let week: u32 = fields.next()?.parse().ok()?;
            let weekday: u32 = fields.next()?.parse().ok()?;
            if !(1..=12).contains(&month) || !(1..=5).contains(&week) || weekday > 6 {
                return None;
            }
            return Some(Change::MonthWeekDay {
                month,
                week,
                weekday,
                seconds: time,
            });
        }
        if let Some(rest) = date.strip_prefix('J') {
            let day: u32 = rest.parse().ok()?;
            if !(1..=365).contains(&day) {
                return None;
            }
            return Some(Change::JulianNoLeap { day, seconds: time });
        }
        let day: u32 = date.parse().ok()?;
        if day > 365 {
            return None;
        }
        Some(Change::DayOfYear { day, seconds: time })
    }

    /// The wall clock instant this change falls at in a given year.
    fn instant(&self, year: i64) -> i64 {
        match self {
            Change::MonthWeekDay {
                month,
                week,
                weekday,
                seconds,
            } => {
                let first = days_from_civil(year, *month, 1);
                let first_weekday = (first + 4).rem_euclid(7) as u32;
                let offset = (*weekday + 7 - first_weekday) % 7;
                let mut day = 1 + offset + (week - 1) * 7;
                // A fifth Sunday that does not exist means the last one, which
                // is how `M3.5.0` says "the final Sunday in March".
                let length = month_length(year, *month);
                while day > length {
                    day -= 7;
                }
                days_from_civil(year, *month, day) * 86_400 + seconds
            }
            Change::JulianNoLeap { day, seconds } => {
                let mut left = *day;
                let mut month = 1;
                // Counting through the months of a common year is what makes
                // the leap day uncounted: February is 28 days here whatever
                // the year is.
                while month <= 12 && left > month_length(1971, month) {
                    left -= month_length(1971, month);
                    month += 1;
                }
                days_from_civil(year, month.min(12), left.max(1)) * 86_400 + seconds
            }
            Change::DayOfYear { day, seconds } => {
                (days_from_civil(year, 1, 1) + *day as i64) * 86_400 + seconds
            }
        }
    }
}

fn month_length(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 30,
    }
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// The abbreviation at the front of a `TZ` string, in either of its spellings:
/// bare letters, or anything at all inside angle brackets, which is how a zone
/// whose name is `+09` says so without the sign being read as an offset.
fn parse_abbreviation(text: &str) -> Option<(String, &str)> {
    if let Some(rest) = text.strip_prefix('<') {
        let end = rest.find('>')?;
        return Some((rest[..end].to_string(), &rest[end + 1..]));
    }
    let end = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    // Three letters is the shortest a zone abbreviation is allowed to be, and
    // a shorter one here means the string was never a rule in the first place
    // — which is how `TZ=Asia/Tokyo` is told from `TZ=JST-9`.
    if end < 3 {
        return None;
    }
    Some((text[..end].to_string(), &text[end..]))
}

/// An offset in a `TZ` string, whose sign is backwards on purpose.
///
/// `CET-1` means Central European Time, one hour *east* of Greenwich: the
/// number is what you add to local time to get UT, not the other way round.
/// Negating it here is the one place that convention has to be remembered,
/// because everything else in this module, and in the file format, uses the
/// sign a person would expect.
fn parse_posix_offset(text: &str) -> Option<(i32, &str)> {
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != ':')
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let seconds = parse_clock(&rest[..end])?;
    let seconds = if negative { -seconds } else { seconds };
    Some((-seconds as i32, &rest[end..]))
}

/// The time of day in a change rule, which POSIX allows to be negative or
/// past midnight so that a zone can change at, say, 24:00.
fn parse_rule_time(text: &str) -> Option<i64> {
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let seconds = parse_clock(rest)?;
    Some(if negative { -seconds } else { seconds })
}

/// `hh`, `hh:mm` or `hh:mm:ss` as a count of seconds.
fn parse_clock(text: &str) -> Option<i64> {
    let mut fields = text.split(':');
    let hours: i64 = fields.next()?.parse().ok()?;
    let minutes: i64 = match fields.next() {
        Some(text) => text.parse().ok()?,
        None => 0,
    };
    let seconds: i64 = match fields.next() {
        Some(text) => text.parse().ok()?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    Some(hours * 3600 + minutes * 60 + seconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A timestamp from a date, for tests that want to name an instant rather
    /// than a number.
    fn utc(year: i64, month: u32, day: u32, hour: i64, minute: i64) -> i64 {
        Civil::seconds(year, month, day, hour, minute, 0)
    }

    #[test]
    fn the_epoch_is_a_thursday_in_january() {
        let civil = Civil::at(0);
        assert_eq!((civil.year, civil.month, civil.day), (1970, 1, 1));
        assert_eq!((civil.hour, civil.minute, civil.second), (0, 0, 0));
        assert_eq!(civil.weekday, 4, "1970-01-01 was a Thursday");
        assert_eq!(civil.year_day, 1);
    }

    #[test]
    fn dates_survive_the_round_trip_through_seconds() {
        for (year, month, day) in [
            (1970, 1, 1),
            (1999, 12, 31),
            (2000, 2, 29),
            (2024, 2, 29),
            (2038, 1, 19),
            (2100, 3, 1),
            (1969, 7, 20),
            (1900, 1, 1),
        ] {
            let seconds = Civil::seconds(year, month, day, 13, 45, 6);
            let civil = Civil::at(seconds);
            assert_eq!(
                (civil.year, civil.month, civil.day, civil.hour, civil.minute),
                (year, month, day, 13, 45),
                "{year}-{month}-{day}"
            );
        }
    }

    #[test]
    fn a_time_before_the_epoch_lands_on_the_day_it_belongs_to() {
        // Truncating division would put this on the 1st of January 1970 at
        // some negative hour, which is not a time anybody has ever had.
        let civil = Civil::at(-1);
        assert_eq!((civil.year, civil.month, civil.day), (1969, 12, 31));
        assert_eq!((civil.hour, civil.minute, civil.second), (23, 59, 59));
    }

    #[test]
    fn the_format_expands_what_it_knows_and_shows_what_it_does_not() {
        let civil = Civil::at(utc(2026, 9, 12, 7, 5));
        let text = format_time("%Y-%m-%d %H:%M %a %b %Z %q %%", &civil, "JST", 9 * 3600);
        assert_eq!(text, "2026-09-12 07:05 Sat Sep JST %q %");
    }

    #[test]
    fn twelve_hour_clocks_have_no_zero_oclock() {
        let midnight = Civil::at(utc(2026, 1, 1, 0, 30));
        assert_eq!(format_time("%I:%M %p", &midnight, "UTC", 0), "12:30 AM");
        let noon = Civil::at(utc(2026, 1, 1, 12, 30));
        assert_eq!(format_time("%I:%M %p", &noon, "UTC", 0), "12:30 PM");
    }

    #[test]
    fn the_numeric_offset_keeps_its_sign_and_its_half_hours() {
        let civil = Civil::at(0);
        assert_eq!(format_time("%z", &civil, "IST", 5 * 3600 + 1800), "+0530");
        assert_eq!(format_time("%z", &civil, "MST", -7 * 3600), "-0700");
    }

    /// The rule the tzdata footer for Berlin carries, which is the one this
    /// parser most has to get right: every summer past the end of the
    /// transition table is answered by it.
    const BERLIN: &str = "CET-1CEST,M3.5.0,M10.5.0/3";

    #[test]
    fn a_posix_rule_reads_its_two_offsets_backwards_from_the_sign_it_is_given() {
        let rule = PosixRule::parse(BERLIN).expect("a rule");
        assert_eq!(rule.standard.seconds, 3600, "CET-1 is an hour east");
        assert_eq!(rule.standard.abbreviation, "CET");
        let (daylight, _, _) = rule.daylight.as_ref().expect("summer time");
        assert_eq!(daylight.seconds, 7200);
        assert_eq!(daylight.abbreviation, "CEST");
    }

    #[test]
    fn summer_time_starts_and_ends_on_the_hour_the_rule_names() {
        let rule = PosixRule::parse(BERLIN).expect("a rule");
        // 2026-03-29 01:00 UT is the moment Berlin goes to CEST.
        assert_eq!(rule.offset_at(utc(2026, 3, 29, 0, 59)).seconds, 3600);
        assert_eq!(rule.offset_at(utc(2026, 3, 29, 1, 0)).seconds, 7200);
        // And 2026-10-25 01:00 UT is the moment it comes back.
        assert_eq!(rule.offset_at(utc(2026, 10, 25, 0, 59)).seconds, 7200);
        assert_eq!(rule.offset_at(utc(2026, 10, 25, 1, 0)).seconds, 3600);
    }

    #[test]
    fn a_southern_summer_straddles_the_new_year() {
        // New Zealand: forward in September, back in April, so January is the
        // middle of summer rather than outside it.
        let rule = PosixRule::parse("NZST-12NZDT,M9.5.0,M4.1.0/3").expect("a rule");
        assert_eq!(rule.offset_at(utc(2026, 1, 15, 0, 0)).abbreviation, "NZDT");
        assert_eq!(rule.offset_at(utc(2026, 6, 15, 0, 0)).abbreviation, "NZST");
    }

    #[test]
    fn a_zone_that_never_changes_its_clocks_parses_to_one_offset() {
        let rule = PosixRule::parse("JST-9").expect("a rule");
        assert_eq!(rule.standard.seconds, 9 * 3600);
        assert!(rule.daylight.is_none());
        assert_eq!(rule.offset_at(utc(2026, 7, 1, 0, 0)).seconds, 9 * 3600);
    }

    #[test]
    fn an_abbreviation_in_angle_brackets_is_not_read_as_an_offset() {
        // This is how tzdata writes zones whose name is a number, and reading
        // the `+` as the start of the offset would give the wrong hour.
        let rule = PosixRule::parse("<+09>-9").expect("a rule");
        assert_eq!(rule.standard.abbreviation, "+09");
        assert_eq!(rule.standard.seconds, 9 * 3600);
    }

    #[test]
    fn a_zone_name_is_not_mistaken_for_a_rule() {
        // `TZ` carries either, and a name read as a rule would be an offset
        // invented out of the letters of a continent.
        assert!(PosixRule::parse("Asia/Tokyo").is_none());
        assert!(PosixRule::parse("").is_none());
    }

    #[test]
    fn rubbish_where_a_time_zone_file_should_be_is_refused_rather_than_read() {
        assert!(TimeZone::from_tzif(b"").is_none());
        assert!(TimeZone::from_tzif(b"not a time zone file at all").is_none());
        // A good magic number and nothing behind it: every length in the
        // header would index past the end.
        let mut truncated = vec![0u8; Header::SIZE];
        truncated[..4].copy_from_slice(b"TZif");
        truncated[4] = b'2';
        truncated[39] = 1; // one local time type, so the header itself is sane
        assert!(TimeZone::from_tzif(&truncated).is_none());
    }

    #[test]
    fn utc_is_what_a_clock_falls_back_to() {
        let zone = TimeZone::load(&Zone::Named("Mars/Olympus".to_string()));
        assert_eq!(zone.offset_at(0), (0, "UTC"));
        let clock = Clock::new("%H:%M", &Zone::Utc);
        assert_eq!(clock.text(utc(2026, 9, 12, 7, 5)), "07:05");
    }

    #[test]
    fn zones_are_named_by_what_the_file_says_or_by_the_two_words() {
        assert_eq!(Zone::parse("local"), Zone::Local);
        assert_eq!(Zone::parse("utc"), Zone::Utc);
        assert_eq!(
            Zone::parse("Asia/Tokyo"),
            Zone::Named("Asia/Tokyo".to_string())
        );
    }

    /// The system's own database, when this machine has one. Skipped rather
    /// than failed where it does not: the test suite has to pass in a
    /// container with no tzdata in it.
    fn system_zone(name: &str) -> Option<TimeZone> {
        TimeZone::from_file(&zone_path(name))
    }

    #[test]
    fn a_real_time_zone_file_reads_as_the_hour_that_country_keeps() {
        let Some(tokyo) = system_zone("Asia/Tokyo") else {
            return;
        };
        // Japan has been nine hours east with no summer time since 1951, so
        // every instant a status bar will ever be asked about is the same.
        assert_eq!(tokyo.offset_at(utc(2026, 1, 15, 0, 0)).0, 9 * 3600);
        assert_eq!(tokyo.offset_at(utc(2026, 7, 15, 0, 0)).0, 9 * 3600);
    }

    #[test]
    fn a_real_time_zone_file_changes_its_clocks_when_the_country_does() {
        let Some(berlin) = system_zone("Europe/Berlin") else {
            return;
        };
        // The last Sunday of March 2026, at 01:00 UT. Whether this comes out
        // of the transition table or the footer rule depends on how the file
        // was compiled, which is exactly why both are implemented.
        assert_eq!(berlin.offset_at(utc(2026, 3, 29, 0, 59)).0, 3600);
        assert_eq!(berlin.offset_at(utc(2026, 3, 29, 1, 0)).0, 7200);
        assert_eq!(berlin.offset_at(utc(2026, 3, 29, 1, 0)).1, "CEST");
        // And a date far past the end of any transition table, which only the
        // footer can answer.
        assert_eq!(berlin.offset_at(utc(2075, 7, 1, 0, 0)).0, 7200);
    }

    #[test]
    fn a_clock_puts_the_offset_on_before_it_reads_the_date() {
        // 15:00 UT on the 12th is midnight on the 13th in Tokyo, so an
        // implementation that formatted first and shifted afterwards would
        // have the wrong day as well as the wrong hour.
        let Some(_) = system_zone("Asia/Tokyo") else {
            return;
        };
        let clock = Clock::new("%Y-%m-%d %H:%M", &Zone::Named("Asia/Tokyo".into()));
        assert_eq!(clock.text(utc(2026, 9, 12, 15, 0)), "2026-09-13 00:00");
    }
}
