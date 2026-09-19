//! The list of pages, and what the engine's `Target.*` events do to it.
//!
//! A tab here is a CDP *page target*: the engine's own unit of "a page", with
//! its own history, its own renderer and its own WebSocket. That last one is
//! the decision worth naming. CDP can multiplex every target over the browser
//! endpoint with `Target.attachToTarget` and a session id on every message,
//! and a browser with a hundred tabs open would want exactly that; a pane with
//! three does not. One socket per tab costs a thread and a pipe each and keeps
//! [`crate::cdp::Client`] the thing it already is — a connection that answers
//! `call`, queues events and knocks on a pipe — instead of a router that has
//! to sort a session id it would otherwise never see.
//!
//! What follows from one socket per tab is that a tab *is* its connection:
//! closing the tab drops it, which closes the socket, which is all the tidying
//! there is. Nothing here holds a client of its own, so this module is generic
//! over what a tab is connected by and its tests use numbers.
//!
//! # Only the active tab costs anything
//!
//! A screencast is a frame every sixteen milliseconds, and a background tab
//! that sent them would be a pane's worth of PNG encoded for nobody. So the
//! screencast is started on activation and stopped on deactivation, and a
//! background tab is a socket sitting idle. What it still does is *exist*: the
//! page goes on running, and its `Page` events go on arriving on its own
//! socket, which is what keeps the strip's titles true without anything being
//! polled.
//!
//! # Where a title comes from, which is not where it looks like it should
//!
//! `Target.targetInfoChanged` on the browser connection carries a `title`, and
//! the obvious design is to take it: one event, every tab, no page asked
//! anything. It does not work, and the measurement is in this crate's engine
//! tests. Against `chromium-shell` the event fires on *navigation* and carries
//! a title derived from the url — `127.0.0.1:33403/second` — and a page that
//! sets `document.title` afterwards produces no further event at all. A strip
//! built on it shows urls where titles should be, and shows them forever.
//!
//! So the url comes from the browser connection, where it is right, and the
//! title is asked of the page, on the page's own connection, when the page
//! says it has finished loading. That is one `Runtime.evaluate` per load per
//! tab — event-driven rather than polled, and a background tab that never
//! loads anything costs nothing at all.

use crate::cdp::Event;
use crate::json::Json;

/// One page target, and what the row says about it.
pub struct Tab<C> {
    /// The engine's id for the target. Every `Target.*` event names a tab by
    /// this, and it is what closes and activates one.
    pub target: String,
    /// The connection to that target: in the program a [`crate::cdp::Client`],
    /// in the tests whatever is cheap.
    pub connection: C,
    pub title: String,
    pub url: String,
    /// Whether the page is between a navigation and its load event.
    pub loading: bool,
    /// A sentence that stands in for the title until the page says something
    /// else: what it is loading, why a navigation failed, what happened to the
    /// tab that is no longer here.
    pub note: Option<String>,
}

impl<C> Tab<C> {
    pub fn new(target: impl Into<String>, connection: C, url: impl Into<String>) -> Tab<C> {
        Tab {
            target: target.into(),
            connection,
            title: String::new(),
            url: url.into(),
            loading: false,
            note: None,
        }
    }

    /// The whole row, when this is the only tab there is.
    ///
    /// Unchanged from the browser that had no tabs, deliberately: one page in
    /// a pane is still the common case and it should look like it always did.
    pub fn line(&self) -> String {
        if let Some(note) = &self.note {
            return note.clone();
        }
        match (self.title.is_empty(), self.url.is_empty()) {
            (true, true) => "tos-browser".to_string(),
            (true, false) => self.url.clone(),
            (false, true) => self.title.clone(),
            (false, false) => format!("{}  —  {}", self.title, self.url),
        }
    }

    /// The name this tab goes by in the strip, before it is clipped.
    pub fn label(&self) -> &str {
        if let Some(note) = &self.note {
            return note;
        }
        if !self.title.is_empty() {
            return &self.title;
        }
        // A tab that was just opened has the url it was opened with and no
        // title yet, and "about:blank" is not a name for anything.
        if self.url.is_empty() || self.url == "about:blank" {
            return "new tab";
        }
        &self.url
    }
}

/// The tabs, in the order they are shown, and which one is in front.
pub struct Tabs<C> {
    tabs: Vec<Tab<C>>,
    active: usize,
}

impl<C> Tabs<C> {
    /// The list a browser starts with: the page the engine already had.
    pub fn new(first: Tab<C>) -> Tabs<C> {
        Tabs {
            tabs: vec![first],
            active: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    /// True once the last tab has gone, which is when the program is over.
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> Option<&Tab<C>> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab<C>> {
        self.tabs.get_mut(self.active)
    }

    /// The active tab's target id, which is what a switch is measured against.
    pub fn active_target(&self) -> Option<&str> {
        self.active().map(|tab| tab.target.as_str())
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Tab<C>> {
        self.tabs.iter()
    }

    pub fn index_of(&self, target: &str) -> Option<usize> {
        self.tabs.iter().position(|tab| tab.target == target)
    }

    pub fn get_mut(&mut self, index: usize) -> Option<&mut Tab<C>> {
        self.tabs.get_mut(index)
    }

    /// Add a tab at the end and make it the one in front.
    ///
    /// Which is what a desktop browser does for an open the person asked for,
    /// and a target with an opener is always one of those: a link they clicked
    /// or a `window.open` the click ran.
    pub fn open(&mut self, tab: Tab<C>) -> usize {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.active
    }

    /// Take a tab out of the list and hand it back, so that the caller can
    /// close its connection where a failure can be reported.
    ///
    /// The tab that takes its place in front is the one to its right, or the
    /// new last one — the rule every browser uses, and the one that makes
    /// closing several tabs in a row feel like closing several tabs in a row.
    pub fn close(&mut self, index: usize) -> Option<Tab<C>> {
        if index >= self.tabs.len() {
            return None;
        }
        let tab = self.tabs.remove(index);
        if index < self.active {
            self.active -= 1;
        }
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
        Some(tab)
    }

    /// Forwards, wrapping. `false` when there is nowhere else to be.
    pub fn select_next(&mut self) -> bool {
        if self.tabs.len() < 2 {
            return false;
        }
        self.active = (self.active + 1) % self.tabs.len();
        true
    }

    /// Backwards, wrapping.
    pub fn select_previous(&mut self) -> bool {
        if self.tabs.len() < 2 {
            return false;
        }
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
        true
    }

    /// The nth tab, counted from one the way the strip numbers them.
    ///
    /// A number nobody has a tab for does nothing at all rather than choosing
    /// the nearest: `alt+7` with three tabs open is a typo, and moving to the
    /// third would hide that.
    pub fn select(&mut self, number: usize) -> bool {
        if number == 0 || number > self.tabs.len() {
            return false;
        }
        let wanted = number - 1;
        let moved = wanted != self.active;
        self.active = wanted;
        moved
    }

    /// Make `index` the tab in front.
    pub fn switch_to(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    /// What one event from the browser connection does to the list.
    ///
    /// `open` is asked for a connection only for a target that is becoming a
    /// tab, and may fail: an engine that will not take another socket is a
    /// sentence on the row, not a reason to stop.
    pub fn take(
        &mut self,
        event: &Event,
        open: impl FnOnce(&str) -> Result<C, String>,
    ) -> Outcome<C> {
        let Some(change) = change(event) else {
            return Outcome::Ignored;
        };
        match change {
            Change::Opened { target, url } => {
                if let Some(index) = self.index_of(&target) {
                    // Already ours — the engine says so twice when a target is
                    // created and then attached.
                    return if self.switch_to(index) {
                        Outcome::Opened
                    } else {
                        Outcome::Ignored
                    };
                }
                match open(&target) {
                    Ok(connection) => {
                        self.open(Tab::new(target, connection, url));
                        Outcome::Opened
                    }
                    Err(why) => Outcome::Failed(format!("that link wanted a new tab: {why}")),
                }
            }
            Change::Renamed { target, url } => {
                let Some(index) = self.index_of(&target) else {
                    return Outcome::Ignored;
                };
                let tab = &mut self.tabs[index];
                // An empty url in a target's information means the engine has
                // not decided yet, not that the page has no address.
                if url.is_empty() || tab.url == url {
                    return Outcome::Ignored;
                }
                tab.url = url;
                // A page that has gone somewhere has not got there yet, and
                // the title it had was the last page's. Both are filled in
                // again by that tab's own `Page` events.
                tab.title.clear();
                tab.note = None;
                Outcome::Renamed
            }
            Change::Closed { target } => self.gone(&target, None),
            Change::Crashed { target } => self.gone(
                &target,
                Some("the page in that tab stopped answering, so the tab is gone".to_string()),
            ),
        }
    }

    fn gone(&mut self, target: &str, why: Option<String>) -> Outcome<C> {
        match self.index_of(target) {
            Some(index) => match self.close(index) {
                Some(tab) => Outcome::Gone { tab, why },
                None => Outcome::Ignored,
            },
            None => Outcome::Ignored,
        }
    }
}

/// What [`Tabs::take`] did.
pub enum Outcome<C> {
    /// Nothing the list cares about.
    Ignored,
    /// A tab was added and is now in front.
    Opened,
    /// A tab's title or url moved, so the row is out of date.
    Renamed,
    /// A tab is no longer in the list. Its connection comes back with it, to
    /// be closed by the caller, and `why` is the sentence to put on the row
    /// when the page did not simply close itself.
    Gone { tab: Tab<C>, why: Option<String> },
    /// A target that should have become a tab could not be connected to.
    Failed(String),
}

/// What a `Target.*` event means, with nothing said about tabs.
///
/// Split out from [`Tabs::take`] so that the reading of the engine's JSON can
/// be tested against the JSON the engine sends, and the list's behaviour
/// against a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A page target somebody else opened: a link with `target=_blank`, or the
    /// `window.open` a click ran.
    Opened { target: String, url: String },
    /// A target's url is now this.
    ///
    /// The event carries a title as well and it is deliberately not read: see
    /// the module documentation for what the engine puts in it.
    Renamed { target: String, url: String },
    /// The target is gone, because the page closed itself or because something
    /// else closed it.
    Closed { target: String },
    /// The renderer behind the target died.
    Crashed { target: String },
}

/// Read one event, or decide it says nothing about the tabs.
pub fn change(event: &Event) -> Option<Change> {
    match event.method.as_str() {
        "Target.targetCreated" => {
            let info = event.params.get("targetInfo")?;
            if info.get("type").and_then(Json::as_str) != Some("page") {
                return None;
            }
            // Only a target with an opener. A target this program asked for
            // has none, and neither has the `about:blank` the engine started
            // with — both are already tabs by the time the event arrives, and
            // acting on it again would open the same page twice.
            let opener = info.get("openerId").and_then(Json::as_str)?;
            if opener.is_empty() {
                return None;
            }
            Some(Change::Opened {
                target: info.get("targetId").and_then(Json::as_str)?.to_string(),
                url: info
                    .get("url")
                    .and_then(Json::as_str)
                    .unwrap_or("about:blank")
                    .to_string(),
            })
        }
        "Target.targetInfoChanged" => {
            let info = event.params.get("targetInfo")?;
            if info.get("type").and_then(Json::as_str) != Some("page") {
                return None;
            }
            Some(Change::Renamed {
                target: info.get("targetId").and_then(Json::as_str)?.to_string(),
                url: info
                    .get("url")
                    .and_then(Json::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
        }
        "Target.targetDestroyed" => Some(Change::Closed {
            target: event
                .params
                .get("targetId")
                .and_then(Json::as_str)?
                .to_string(),
        }),
        "Target.targetCrashed" => Some(Change::Crashed {
            target: event
                .params
                .get("targetId")
                .and_then(Json::as_str)?
                .to_string(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tab whose connection is a number, which is as much as the list needs
    /// to know about one.
    fn tab(target: &str, title: &str) -> Tab<u32> {
        let mut tab = Tab::new(target, 0, format!("https://{target}.example"));
        tab.title = title.to_string();
        tab
    }

    fn three() -> Tabs<u32> {
        let mut tabs = Tabs::new(tab("a", "A"));
        tabs.open(tab("b", "B"));
        tabs.open(tab("c", "C"));
        tabs.select(1);
        tabs
    }

    fn event(method: &str, params: &str) -> Event {
        Event {
            method: method.to_string(),
            params: Json::parse(params).expect("the test's own JSON"),
        }
    }

    fn titles(tabs: &Tabs<u32>) -> Vec<&str> {
        tabs.iter().map(|tab| tab.title.as_str()).collect()
    }

    #[test]
    fn opening_a_tab_puts_it_at_the_end_and_in_front() {
        let mut tabs = Tabs::new(tab("a", "A"));
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs.active_index(), 0);
        tabs.open(tab("b", "B"));
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active_index(), 1, "a new tab is switched to");
        assert_eq!(tabs.active_target(), Some("b"));
    }

    #[test]
    fn closing_the_active_tab_shows_the_one_to_its_right() {
        let mut tabs = three();
        assert_eq!(tabs.active_index(), 0);
        tabs.select(2);
        assert_eq!(tabs.close(1).map(|tab| tab.target), Some("b".to_string()));
        assert_eq!(titles(&tabs), ["A", "C"]);
        assert_eq!(tabs.active_target(), Some("c"), "the one to the right");

        // And the last tab in the list falls back to the new last one.
        let mut tabs = three();
        tabs.select(3);
        tabs.close(2);
        assert_eq!(tabs.active_target(), Some("b"));
    }

    #[test]
    fn closing_a_tab_before_the_active_one_keeps_the_active_one() {
        let mut tabs = three();
        tabs.select(3);
        tabs.close(0);
        assert_eq!(titles(&tabs), ["B", "C"]);
        assert_eq!(tabs.active_target(), Some("c"), "still the same page");
    }

    #[test]
    fn closing_the_last_tab_leaves_nothing() {
        let mut tabs = Tabs::new(tab("a", "A"));
        assert!(tabs.close(0).is_some());
        assert!(tabs.is_empty());
        assert!(tabs.active().is_none());
        assert_eq!(tabs.active_target(), None);
        assert!(
            tabs.close(0).is_none(),
            "and there is nothing left to close"
        );
    }

    #[test]
    fn next_and_previous_wrap_in_both_directions() {
        let mut tabs = three();
        assert_eq!(tabs.active_index(), 0);
        assert!(tabs.select_next());
        assert_eq!(tabs.active_index(), 1);
        tabs.select_next();
        assert_eq!(tabs.active_index(), 2);
        assert!(tabs.select_next());
        assert_eq!(tabs.active_index(), 0, "round the end");
        assert!(tabs.select_previous());
        assert_eq!(tabs.active_index(), 2, "and round the beginning");

        // With one tab there is nowhere to go, and saying so is what stops the
        // row being redrawn for nothing.
        let mut one = Tabs::new(tab("a", "A"));
        assert!(!one.select_next());
        assert!(!one.select_previous());
    }

    #[test]
    fn a_number_with_no_tab_behind_it_does_nothing() {
        let mut tabs = three();
        assert!(tabs.select(3));
        assert_eq!(tabs.active_index(), 2);
        assert!(!tabs.select(4), "there is no fourth tab");
        assert_eq!(tabs.active_index(), 2, "and nothing moved");
        assert!(!tabs.select(0), "and no zeroth one");
        assert_eq!(tabs.active_index(), 2);
        assert!(!tabs.select(3), "already there");
    }

    #[test]
    fn a_label_is_the_best_name_the_tab_has() {
        let mut tab: Tab<u32> = Tab::new("a", 0, "about:blank");
        assert_eq!(tab.label(), "new tab");
        tab.url = "https://example.com/a".to_string();
        assert_eq!(tab.label(), "https://example.com/a");
        tab.title = "Example".to_string();
        assert_eq!(tab.label(), "Example");
        tab.note = Some("loading".to_string());
        assert_eq!(tab.label(), "loading");
    }

    #[test]
    fn a_page_that_opens_a_page_becomes_a_tab_and_one_that_does_not_does_not() {
        let mut tabs = Tabs::new(tab("a", "A"));

        // The engine announcing the target this program asked for: no opener,
        // so it is already a tab and the event says nothing.
        let mine = event(
            "Target.targetCreated",
            r#"{"targetInfo":{"targetId":"b","type":"page","url":"about:blank",
                "title":"","attached":false}}"#,
        );
        assert!(matches!(tabs.take(&mine, |_| Ok(1)), Outcome::Ignored));
        assert_eq!(tabs.len(), 1);

        // Something that is not a page: a service worker, an iframe with a
        // process of its own.
        let worker = event(
            "Target.targetCreated",
            r#"{"targetInfo":{"targetId":"w","type":"service_worker","url":"x",
                "openerId":"a","title":""}}"#,
        );
        assert!(matches!(tabs.take(&worker, |_| Ok(2)), Outcome::Ignored));
        assert_eq!(tabs.len(), 1);

        // A link with target=_blank, which is the one that counts.
        let opened = event(
            "Target.targetCreated",
            r#"{"targetInfo":{"targetId":"b","type":"page","openerId":"a",
                "url":"https://example.com/second","title":""}}"#,
        );
        assert!(matches!(tabs.take(&opened, |_| Ok(3)), Outcome::Opened));
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.active_target(), Some("b"), "and it is switched to");
        assert_eq!(
            tabs.active().map(|t| t.url.as_str()),
            Some("https://example.com/second")
        );
        assert_eq!(tabs.active().map(|t| t.connection), Some(3));

        // The same target announced again is not a second tab.
        assert!(matches!(tabs.take(&opened, |_| Ok(4)), Outcome::Ignored));
        assert_eq!(tabs.len(), 2);
    }

    #[test]
    fn a_tab_that_cannot_be_connected_to_is_a_sentence() {
        let mut tabs = Tabs::new(tab("a", "A"));
        let opened = event(
            "Target.targetCreated",
            r#"{"targetInfo":{"targetId":"b","type":"page","openerId":"a","url":"x","title":""}}"#,
        );
        let outcome = tabs.take(&opened, |_| Err("connection refused".to_string()));
        match outcome {
            Outcome::Failed(why) => assert!(why.contains("connection refused"), "{why}"),
            _ => panic!("a refused connection is a failure"),
        }
        assert_eq!(tabs.len(), 1, "and no half a tab was left behind");
    }

    #[test]
    fn a_tab_that_goes_somewhere_says_so_and_loses_the_last_pages_name() {
        let mut tabs = three();
        // The title in this event is the one the engine derives from the url,
        // and taking it would put a url in the strip where a title belongs.
        let renamed = event(
            "Target.targetInfoChanged",
            r#"{"targetInfo":{"targetId":"c","type":"page","title":"example.com/third",
                "url":"https://example.com/third"}}"#,
        );
        assert!(matches!(tabs.take(&renamed, |_| Ok(0)), Outcome::Renamed));
        assert_eq!(titles(&tabs), ["A", "B", ""], "the title is the page's job");
        assert_eq!(
            tabs.iter().nth(2).map(|t| t.url.as_str()),
            Some("https://example.com/third")
        );

        // The same url twice does not redraw the row.
        assert!(matches!(tabs.take(&renamed, |_| Ok(0)), Outcome::Ignored));

        // And a target that is not a tab of ours is not an error either.
        let stranger = event(
            "Target.targetInfoChanged",
            r#"{"targetInfo":{"targetId":"zz","type":"page","title":"x","url":"y"}}"#,
        );
        assert!(matches!(tabs.take(&stranger, |_| Ok(0)), Outcome::Ignored));
        assert_eq!(tabs.len(), 3);
    }

    #[test]
    fn a_page_that_closes_itself_takes_its_tab_with_it() {
        let mut tabs = three();
        tabs.select(2);
        let destroyed = event("Target.targetDestroyed", r#"{"targetId":"b"}"#);
        match tabs.take(&destroyed, |_| Ok(0)) {
            Outcome::Gone { tab, why } => {
                assert_eq!(tab.target, "b");
                assert_eq!(why, None, "window.close() needs no explanation");
            }
            _ => panic!("the tab should be gone"),
        }
        assert_eq!(titles(&tabs), ["A", "C"]);

        // A target nobody here has is somebody else's business.
        let stranger = event("Target.targetDestroyed", r#"{"targetId":"zz"}"#);
        assert!(matches!(tabs.take(&stranger, |_| Ok(0)), Outcome::Ignored));
        assert_eq!(tabs.len(), 2);
    }

    #[test]
    fn a_renderer_that_dies_is_a_sentence_and_not_a_panic() {
        let mut tabs = three();
        let crashed = event("Target.targetCrashed", r#"{"targetId":"a","errorCode":5}"#);
        match tabs.take(&crashed, |_| Ok(0)) {
            Outcome::Gone { tab, why } => {
                assert_eq!(tab.target, "a");
                let why = why.expect("a crash says why");
                assert!(why.contains("tab"), "{why}");
            }
            _ => panic!("a crashed tab is gone"),
        }
        assert_eq!(titles(&tabs), ["B", "C"]);
    }

    #[test]
    fn an_event_about_something_else_entirely() {
        let frame = event("Page.screencastFrame", r#"{"data":"x","sessionId":1}"#);
        assert_eq!(change(&frame), None);
        let empty = event("Target.targetCreated", r#"{}"#);
        assert_eq!(change(&empty), None);
    }
}
