//! Saying out loud what the bindings are.
//!
//! Everything here reads a live [`Keymap`]; nothing restates the table in
//! [`crate::keys`]. That is the whole point. The cheat sheet tOS puts over the
//! panes and the bindings section of `tos --help` are both built from the map
//! that is actually resolving keys, so neither can claim a binding that is not
//! there, and a key rebound at startup renames itself in both for free.
//!
//! The keymap is the wrong shape to read, though: it is a flat table keyed by
//! key combination, so the nine workspace digits are nine entries and the two
//! ways to move focus left are two. A sheet that prints it verbatim is a wall.
//! So rows here are grouped by what the binding does, digit runs are folded
//! into a range, and the key column is cut off before it can crowd out the
//! description it belongs to.

use tos_input::{ImeKey, KeyCode, Keypad, ModifierKey, Modifiers};

use crate::keys::{Action, Binding, Keymap};
use crate::layout::{Axis, Direction};

/// The widest the key column is allowed to get, in characters.
///
/// Every alias a binding has is true, but not all of them are worth the width.
/// The overlay only draws the key column when it fits beside the description,
/// so a row that lists four ways to do one thing ends up showing none of them.
const MAX_KEYS_WIDTH: usize = 30;

/// The fewest bindings in a digit run before it is worth folding into a range.
const MIN_RUN: usize = 3;

/// One row of the cheat sheet: what to press, and what it does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindingHelp {
    /// The key combinations that run it, most useful first, joined with " / ".
    pub keys: String,
    /// What pressing them does, in a few words.
    pub action: String,
}

/// Every binding in `keymap`, grouped and ordered for reading.
pub fn cheat_sheet(keymap: &Keymap) -> Vec<BindingHelp> {
    let mut groups: Vec<Group> = Vec::new();

    for (binding, action) in &keymap.direct {
        // Super is the modifier that needs a compositor owning the keyboard,
        // so it is the alias least likely to work where the user is sitting:
        // it goes last of the three.
        let class = if binding.modifiers.contains(Modifiers::SUPER) {
            Class::Super
        } else {
            Class::Direct
        };
        add(&mut groups, action, class, key_name(binding));
    }
    // Bindings behind a leader that is not bound cannot fire, and a binding
    // that cannot fire is not one to tell anybody about.
    if keymap.leader.is_some() {
        for (binding, action) in &keymap.after_leader {
            add(
                &mut groups,
                action,
                Class::Leader,
                format!("leader {}", key_name(binding)),
            );
        }
    }

    // A HashMap hands its entries over in whatever order it likes, so the
    // order has to be put back here or the same keymap would describe itself
    // differently every run.
    groups.sort_by(|a, b| a.rank.cmp(&b.rank).then_with(|| a.action.cmp(&b.action)));
    groups
        .into_iter()
        .map(|mut group| {
            group.names.sort();
            group.names.dedup();
            let names: Vec<String> = group.names.into_iter().map(|(_, name)| name).collect();
            BindingHelp {
                keys: join(&collapse_digits(&names)),
                action: group.action,
            }
        })
        .collect()
}

/// What to call the leader key, when there is one.
pub fn leader_name(keymap: &Keymap) -> Option<String> {
    keymap.leader.as_ref().map(key_name)
}

/// Whether anything at all fires on super without the leader.
///
/// The shipped map mirrors every leader binding onto super, which is worth
/// saying once above the sheet rather than on each row, where it would cost
/// the width that the descriptions need. A map that mirrors nothing must not
/// have it said.
pub fn super_works_alone(keymap: &Keymap) -> bool {
    keymap
        .direct
        .keys()
        .any(|binding| binding.modifiers.contains(Modifiers::SUPER))
}

/// What an action does, in the few words a cheat sheet row has room for.
///
/// This doubles as what rows are grouped by, which is why the nine workspace
/// digits collapse: they all do "select a workspace".
pub fn describe(action: &Action) -> String {
    match action {
        Action::Split(Axis::Columns) => "split into columns".into(),
        Action::Split(Axis::Rows) => "split into rows".into(),
        Action::ClosePane => "close the focused pane".into(),
        Action::Focus(direction) => format!("move focus {}", toward(*direction)),
        Action::Resize(direction, _) => format!("move the divider {}", toward(*direction)),
        Action::ToggleZoom => "zoom the focused pane".into(),
        Action::Balance => "even out every split".into(),
        Action::NewWorkspace => "new workspace".into(),
        Action::NextWorkspace => "next workspace".into(),
        Action::PreviousWorkspace => "previous workspace".into(),
        Action::SelectWorkspace(_) => "select a workspace".into(),
        Action::MovePaneToWorkspace(_) => "move the pane to a workspace".into(),
        Action::Scroll(lines) if *lines < 0 => "scroll back".into(),
        Action::Scroll(_) => "scroll forward".into(),
        Action::ScrollPage(pages) if *pages < 0 => "scroll back a page".into(),
        Action::ScrollPage(_) => "scroll forward a page".into(),
        Action::ScrollToBottom => "jump to the live screen".into(),
        Action::BeginSelection => "start a selection".into(),
        Action::Copy => "copy the selection".into(),
        Action::Paste => "paste the clipboard".into(),
        Action::OpenLauncher => "open the launcher".into(),
        Action::RenameWorkspace => "name the workspace".into(),
        Action::ShowNotifications => "notifications, including the ones gone".into(),
        Action::ShowBindings => "show these bindings".into(),
        Action::Refresh => "redraw the screen".into(),
        Action::Lock => "lock the screen".into(),
        Action::ShowNetworks => "network interfaces".into(),
        Action::Quit => "quit tOS".into(),
    }
}

/// Name a key combination the way a person would say it.
pub fn key_name(binding: &Binding) -> String {
    // The lock keys never stopped a binding from firing, so they are no part
    // of its name either.
    let mut modifiers = binding.modifiers.effective();
    let mut key = code_name(binding.code);

    // A shifted punctuation key is named by the character it produces: the
    // binding is known as "?", and "shift+/" is the same thing said the long
    // way round. Letters and digits keep their shift, because "ctrl+shift+t"
    // is how that one is written and nobody calls shift+3 "#".
    if modifiers.contains(Modifiers::SHIFT) {
        if let KeyCode::Char(c) = binding.code {
            if !c.is_alphanumeric() && !c.is_whitespace() {
                if let Some(shifted) = tos_input::keymap::shifted(c) {
                    if shifted != c {
                        key = shifted.to_string();
                        modifiers = modifiers.without(Modifiers::SHIFT);
                    }
                }
            }
        }
    }

    let mut name = String::new();
    for (modifier, text) in [
        (Modifiers::CTRL, "ctrl+"),
        (Modifiers::ALT, "alt+"),
        (Modifiers::SUPER, "super+"),
        (Modifiers::HYPER, "hyper+"),
        (Modifiers::META, "meta+"),
        (Modifiers::SHIFT, "shift+"),
    ] {
        if modifiers.contains(modifier) {
            name.push_str(text);
        }
    }
    name.push_str(&key);
    name
}

/// Which of the three ways a binding can be reached this one is. The order is
/// the order they are offered in, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// Fires on its own, without super: works in a nested session too.
    Direct,
    /// Fires after the leader key.
    Leader,
    /// Fires on its own, but only where super reaches the compositor.
    Super,
}

/// Everything bound to one description, while it is being gathered.
struct Group {
    rank: (u16, u16),
    action: String,
    names: Vec<(Class, String)>,
}

fn add(groups: &mut Vec<Group>, action: &Action, class: Class, name: String) {
    let text = describe(action);
    match groups.iter_mut().find(|group| group.action == text) {
        Some(group) => group.names.push((class, name)),
        None => groups.push(Group {
            rank: rank(action),
            action: text,
            names: vec![(class, name)],
        }),
    }
}

/// Where an action sits in the sheet.
///
/// Reading order, not enum order: what you do to a pane, then to a workspace,
/// then to the scrollback, and the things that end a session last. Sorting by
/// the description instead would put "close the focused pane" above "split",
/// which is not how anyone learns this.
fn rank(action: &Action) -> (u16, u16) {
    let toward = |direction: Direction| match direction {
        Direction::Left => 0,
        Direction::Down => 1,
        Direction::Up => 2,
        Direction::Right => 3,
    };
    match action {
        Action::Split(Axis::Columns) => (0, 0),
        Action::Split(Axis::Rows) => (0, 1),
        Action::ClosePane => (1, 0),
        Action::Focus(direction) => (2, toward(*direction)),
        Action::ToggleZoom => (3, 0),
        Action::Balance => (3, 1),
        Action::Resize(direction, _) => (4, toward(*direction)),
        Action::NewWorkspace => (5, 0),
        Action::NextWorkspace => (5, 1),
        Action::PreviousWorkspace => (5, 2),
        Action::SelectWorkspace(_) => (5, 3),
        Action::MovePaneToWorkspace(_) => (5, 4),
        Action::Scroll(lines) if *lines < 0 => (6, 0),
        Action::Scroll(_) => (6, 1),
        Action::ScrollPage(pages) if *pages < 0 => (6, 2),
        Action::ScrollPage(_) => (6, 3),
        Action::ScrollToBottom => (6, 4),
        Action::BeginSelection => (7, 0),
        Action::Copy => (7, 1),
        Action::Paste => (7, 2),
        Action::OpenLauncher => (8, 0),
        Action::RenameWorkspace => (5, 5),
        Action::ShowNotifications => (8, 1),
        Action::ShowBindings => (8, 2),
        Action::ShowNetworks => (8, 3),
        Action::Refresh => (9, 0),
        Action::Lock => (9, 1),
        Action::Quit => (9, 2),
    }
}

fn toward(direction: Direction) -> &'static str {
    match direction {
        Direction::Left => "left",
        Direction::Right => "right",
        Direction::Up => "up",
        Direction::Down => "down",
    }
}

/// Fold a run of names that differ only in a trailing digit into one range.
///
/// Nine workspace bindings are nine entries in the keymap, which is right for
/// resolving a key and wrong for reading: a sheet that spends nine rows saying
/// almost the same thing buries everything else. The range is built from the
/// digits that are actually bound, so a keymap that only binds four of them
/// says so.
fn collapse_digits(names: &[String]) -> Vec<String> {
    let mut folded: Vec<String> = Vec::new();
    let mut index = 0;
    while index < names.len() {
        let (head, digit) = split_digit(&names[index]);
        if let Some(first) = digit {
            let mut last = first;
            let mut end = index + 1;
            // The names arrive sorted, so a run is contiguous if it is there.
            while end < names.len() {
                let (next_head, next) = split_digit(&names[end]);
                let Some(next) = next else { break };
                if next_head != head || next as u8 != last as u8 + 1 {
                    break;
                }
                last = next;
                end += 1;
            }
            if end - index >= MIN_RUN {
                folded.push(format!("{head}{first}..{last}"));
                index = end;
                continue;
            }
        }
        folded.push(names[index].clone());
        index += 1;
    }
    folded
}

/// A name and the digit it ends in, if it ends in one.
fn split_digit(name: &str) -> (&str, Option<char>) {
    match name.chars().next_back() {
        Some(c) if c.is_ascii_digit() => (&name[..name.len() - 1], Some(c)),
        _ => (name, None),
    }
}

/// Join as many names as fit in the key column, keeping the first whatever
/// happens: a row with no keys on it is not a row.
fn join(names: &[String]) -> String {
    let mut joined = String::new();
    for name in names {
        if joined.is_empty() {
            joined.push_str(name);
            continue;
        }
        if joined.chars().count() + 3 + name.chars().count() > MAX_KEYS_WIDTH {
            break;
        }
        joined.push_str(" / ");
        joined.push_str(name);
    }
    joined
}

fn code_name(code: KeyCode) -> String {
    match code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Escape => "esc".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Function(n) => format!("f{n}"),
        KeyCode::Keypad(key) => keypad_name(key),
        KeyCode::CapsLock => "capslock".into(),
        KeyCode::NumLock => "numlock".into(),
        KeyCode::ScrollLock => "scrolllock".into(),
        KeyCode::PrintScreen => "printscreen".into(),
        KeyCode::Pause => "pause".into(),
        KeyCode::Menu => "menu".into(),
        KeyCode::ModifierKey(key) => modifier_name(key).into(),
        // The conversion keys a Japanese keyboard has. Nothing binds them
        // today; an input method is what will.
        KeyCode::Ime(key) => match key {
            ImeKey::Convert => "henkan".into(),
            ImeKey::NonConvert => "muhenkan".into(),
            ImeKey::KanaMode => "kana".into(),
        },
        // Nothing can be bound to a key the driver could not name, but the
        // number is still more useful than refusing to say anything.
        KeyCode::Unknown(n) => format!("key{n}"),
    }
}

fn keypad_name(key: Keypad) -> String {
    match key {
        Keypad::Digit(n) => format!("kp{n}"),
        Keypad::Decimal => "kp.".into(),
        Keypad::Divide => "kp/".into(),
        Keypad::Multiply => "kp*".into(),
        Keypad::Subtract => "kp-".into(),
        Keypad::Add => "kp+".into(),
        Keypad::Enter => "kpenter".into(),
        Keypad::Equal => "kp=".into(),
        Keypad::Separator => "kpsep".into(),
        Keypad::Begin => "kpbegin".into(),
    }
}

fn modifier_name(key: ModifierKey) -> &'static str {
    match key {
        ModifierKey::LeftShift => "lshift",
        ModifierKey::RightShift => "rshift",
        ModifierKey::LeftCtrl => "lctrl",
        ModifierKey::RightCtrl => "rctrl",
        ModifierKey::LeftAlt => "lalt",
        ModifierKey::RightAlt => "ralt",
        ModifierKey::LeftSuper => "lsuper",
        ModifierKey::RightSuper => "rsuper",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row<'a>(sheet: &'a [BindingHelp], action: &str) -> &'a BindingHelp {
        sheet
            .iter()
            .find(|row| row.action == action)
            .unwrap_or_else(|| panic!("no row for {action:?} in {sheet:#?}"))
    }

    #[test]
    fn the_sheet_is_the_keymap_and_nothing_else() {
        // The point of the whole module: an empty keymap describes nothing,
        // however many bindings the defaults happen to have.
        let mut keymap = Keymap::empty();
        assert!(cheat_sheet(&keymap).is_empty());

        keymap.bind(
            Binding::new(KeyCode::Function(1), Modifiers::NONE),
            Action::Quit,
        );
        assert_eq!(
            cheat_sheet(&keymap),
            vec![BindingHelp {
                keys: "f1".into(),
                action: "quit tOS".into(),
            }]
        );
    }

    #[test]
    fn a_rebound_key_renames_its_row() {
        // What makes the sheet survive a config file: nothing here knows that
        // the leader is normally ctrl+a or that closing a pane is normally x.
        let mut keymap = Keymap::empty();
        keymap.leader = Some(Binding::new(KeyCode::Char('b'), Modifiers::CTRL));
        keymap.bind_after_leader(
            Binding::new(KeyCode::Char('w'), Modifiers::NONE),
            Action::ClosePane,
        );
        assert_eq!(leader_name(&keymap).unwrap(), "ctrl+b");
        assert_eq!(
            row(&cheat_sheet(&keymap), "close the focused pane").keys,
            "leader w"
        );
    }

    #[test]
    fn bindings_behind_no_leader_are_not_offered() {
        // They cannot fire, so listing them would be a lie.
        let mut keymap = Keymap::empty();
        keymap.bind_after_leader(
            Binding::new(KeyCode::Char('x'), Modifiers::NONE),
            Action::ClosePane,
        );
        assert!(cheat_sheet(&keymap).is_empty());
    }

    #[test]
    fn the_defaults_are_all_described() {
        let sheet = cheat_sheet(&Keymap::default_bindings());
        for action in [
            "split into columns",
            "split into rows",
            "close the focused pane",
            "move focus left",
            "move the divider up",
            "zoom the focused pane",
            "even out every split",
            "new workspace",
            "select a workspace",
            "move the pane to a workspace",
            "scroll back a page",
            "jump to the live screen",
            "copy the selection",
            "open the launcher",
            "show these bindings",
            "quit tOS",
        ] {
            assert!(!row(&sheet, action).keys.is_empty());
        }
    }

    #[test]
    fn one_row_per_thing_a_binding_does() {
        // Two ways to move focus left is one row, not two.
        let sheet = cheat_sheet(&Keymap::default_bindings());
        let mut actions: Vec<&str> = sheet.iter().map(|row| row.action.as_str()).collect();
        let before = actions.len();
        actions.sort_unstable();
        actions.dedup();
        assert_eq!(actions.len(), before, "a description appears twice");

        let left = &row(&sheet, "move focus left").keys;
        assert!(left.contains("leader h"), "{left:?}");
        assert!(left.contains("leader left"), "{left:?}");
    }

    #[test]
    fn the_same_keymap_always_describes_itself_the_same_way() {
        // Two maps with the same bindings hash their keys differently, so this
        // fails the moment the order stops being imposed here.
        assert_eq!(
            cheat_sheet(&Keymap::default_bindings()),
            cheat_sheet(&Keymap::default_bindings())
        );
    }

    #[test]
    fn workspace_digits_fold_into_a_range() {
        let sheet = cheat_sheet(&Keymap::default_bindings());
        let keys = &row(&sheet, "select a workspace").keys;
        assert!(keys.contains("1..9"), "{keys:?}");
        assert!(!keys.contains("leader 5"), "{keys:?}");
    }

    #[test]
    fn a_short_run_of_digits_is_left_alone() {
        // Two of them read better spelled out than as a range.
        let names = vec!["leader 1".to_string(), "leader 2".to_string()];
        assert_eq!(collapse_digits(&names), names);
        // And a gap in the middle is two runs, neither long enough to fold.
        let names = vec![
            "leader 1".to_string(),
            "leader 2".to_string(),
            "leader 4".to_string(),
            "leader 5".to_string(),
        ];
        assert_eq!(collapse_digits(&names), names);
    }

    #[test]
    fn the_key_column_stays_narrow() {
        // It is drawn beside the description, and only when it fits.
        for help in cheat_sheet(&Keymap::default_bindings()) {
            assert!(
                help.keys.chars().count() <= MAX_KEYS_WIDTH,
                "{:?} is too wide",
                help.keys
            );
        }
    }

    #[test]
    fn shifted_punctuation_is_named_by_the_character_it_types() {
        assert_eq!(
            key_name(&Binding::new(KeyCode::Char('/'), Modifiers::SHIFT)),
            "?"
        );
        // Letters and digits keep the modifier, because that is how these two
        // are written everywhere else.
        assert_eq!(
            key_name(&Binding::new(
                KeyCode::Char('t'),
                Modifiers::CTRL.union(Modifiers::SHIFT)
            )),
            "ctrl+shift+t"
        );
        assert_eq!(
            key_name(&Binding::new(
                KeyCode::Char('3'),
                Modifiers::SUPER.union(Modifiers::SHIFT)
            )),
            "super+shift+3"
        );
    }

    #[test]
    fn keys_without_a_character_are_named() {
        assert_eq!(
            key_name(&Binding::new(KeyCode::Char(' '), Modifiers::SUPER)),
            "super+space"
        );
        assert_eq!(
            key_name(&Binding::new(KeyCode::PageUp, Modifiers::SHIFT)),
            "shift+pageup"
        );
        assert_eq!(
            key_name(&Binding::new(KeyCode::Function(4), Modifiers::NONE)),
            "f4"
        );
        // Caps lock never blocked a binding, so it is no part of its name.
        assert_eq!(
            key_name(&Binding::new(
                KeyCode::Enter,
                Modifiers::CTRL.union(Modifiers::CAPS_LOCK)
            )),
            "ctrl+enter"
        );
    }

    #[test]
    fn the_question_mark_is_on_the_sheet_it_opens() {
        // A cheat sheet that does not say how to get the cheat sheet back is
        // one you only find once.
        let sheet = cheat_sheet(&Keymap::default_bindings());
        assert!(row(&sheet, "show these bindings").keys.contains("leader ?"));
    }
}
