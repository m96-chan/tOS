//! Workspaces and focus.
//!
//! A session owns several workspaces, each with its own pane tree and focused
//! pane. Pane identifiers are unique across the whole session so that a pane
//! can move between workspaces without being recreated.

use crate::layout::{self, Arrangement, Axis, Direction, Divider, DividerId, Layout, PaneId, Rect};

/// Identifies a workspace within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(pub u32);

/// One workspace: a pane tree plus which pane has focus.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub layout: Layout,
    focus: PaneId,
    /// A pane temporarily filling the whole workspace.
    zoomed: Option<PaneId>,
    /// How the panes are arranged on screen, which is a different question
    /// from how the tree holds them: every arrangement but
    /// [`Arrangement::Splits`] derives geometry from the pane order and leaves
    /// the tree alone. It lives beside `zoomed` because the two are the same
    /// kind of thing — an override of what the tree would have drawn — and
    /// because [`Workspace::geometry`] is the one place either is answered.
    arrangement: Arrangement,
    /// Set once the user names the workspace, so renumbering leaves it alone.
    renamed: bool,
}

impl Workspace {
    fn new(id: WorkspaceId, position: usize, root: PaneId) -> Self {
        Workspace {
            id,
            name: format!("{}", position + 1),
            layout: Layout::new(root),
            focus: root,
            zoomed: None,
            arrangement: Arrangement::Splits,
            renamed: false,
        }
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    pub fn zoomed(&self) -> Option<PaneId> {
        self.zoomed
    }

    pub fn panes(&self) -> Vec<PaneId> {
        self.layout.panes()
    }

    /// Where each visible pane goes. A zoomed pane is the only one visible,
    /// and otherwise the arrangement in force decides — which for
    /// [`Arrangement::Splits`] means asking the tree.
    ///
    /// The single place geometry is decided, and everything that draws, hit
    /// tests or moves focus has to come through here. Asking `self.layout`
    /// instead yields the tree's opinion, which under any other arrangement is
    /// a picture of a workspace nobody is looking at.
    pub fn geometry(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        match self.zoomed {
            Some(pane) if self.layout.contains(pane) => vec![(pane, area)],
            _ => self
                .arrangement
                .geometry(&self.layout.panes(), area, self.layout.gap)
                .unwrap_or_else(|| self.layout.geometry(area)),
        }
    }

    pub fn arrangement(&self) -> Arrangement {
        self.arrangement
    }

    /// Arrange the panes some other way.
    ///
    /// The zoom goes, for the reason splitting drops it: the point of asking
    /// for an arrangement is to see it, and a zoomed workspace would answer
    /// the key with a screen that does not change.
    pub fn set_arrangement(&mut self, arrangement: Arrangement) {
        self.arrangement = arrangement;
        self.zoomed = None;
    }

    /// Which pane is at a cell, in the arrangement that is on screen.
    pub fn pane_at(&self, area: Rect, x: u32, y: u32) -> Option<PaneId> {
        layout::pane_at(&self.geometry(area), x, y)
    }

    /// The pane next to `from` in `direction`, in the arrangement that is on
    /// screen. Under `Grid` that is a different pane from the one the tree
    /// would name, and the visible one is the one the arrow key meant.
    pub fn neighbour(&self, area: Rect, from: PaneId, direction: Direction) -> Option<PaneId> {
        layout::neighbour(&self.geometry(area), from, direction)
    }

    /// The dividers to draw between the panes.
    ///
    /// A divider is a property of the split tree — it is the thing a drag
    /// moves — so the derived arrangements have none, and drawing the tree's
    /// would rule lines across the middle of panes. The gap between panes is
    /// left showing the background instead, which is what a gap with no
    /// divider in it already looks like.
    pub fn dividers(&self, area: Rect) -> Vec<(Axis, Rect)> {
        if self.arrangement != Arrangement::Splits {
            return Vec::new();
        }
        self.layout.dividers(area)
    }

    /// The divider at a cell, if one is there to be grabbed.
    ///
    /// Gated on the arrangement for the same reason [`Workspace::dividers`]
    /// is, and it has to be said twice because drawing and grabbing are
    /// different questions asked by different code. A derived arrangement
    /// draws no dividers, and a strip that resizes something invisible is
    /// worse than one that does nothing: the weights it would move are the
    /// ones that returning to [`Arrangement::Splits`] promises to hand back
    /// exactly as they were left.
    pub fn divider_at(&self, area: Rect, x: u32, y: u32) -> Option<Divider> {
        if self.arrangement != Arrangement::Splits {
            return None;
        }
        self.layout.divider_at(area, x, y)
    }

    /// Where a divider being dragged is now.
    ///
    /// Answers `None` once the arrangement stops being [`Arrangement::Splits`],
    /// which drops the grab — cycling the layout is a binding, and the
    /// keyboard still works while a button is held down.
    pub fn divider(&self, area: Rect, id: DividerId) -> Option<Divider> {
        if self.arrangement != Arrangement::Splits {
            return None;
        }
        self.layout.divider(area, id)
    }

    pub fn set_focus(&mut self, pane: PaneId) -> bool {
        if !self.layout.contains(pane) {
            return false;
        }
        self.focus = pane;
        // Focusing a pane that the zoom is hiding has to leave the zoom, or
        // keystrokes would go to a pane that is never drawn.
        if self.zoomed.is_some_and(|zoomed| zoomed != pane) {
            self.zoomed = None;
        }
        true
    }

    /// Give the workspace a name of the user's choosing.
    pub fn rename(&mut self, name: impl Into<String>) {
        self.name = name.into();
        self.renamed = true;
    }

    /// Forget the name the user gave, so the workspace answers to its position
    /// again. The number itself comes from the session, which is the only
    /// thing that knows what position this is.
    pub fn clear_name(&mut self) {
        self.renamed = false;
    }
}

/// Everything the compositor knows about panes and workspaces.
#[derive(Debug, Clone)]
pub struct Session {
    workspaces: Vec<Workspace>,
    active: usize,
    /// The pane the session was created with.
    root: PaneId,
    next_pane: u32,
    next_workspace: u32,
}

impl Session {
    /// A session with one workspace holding one pane.
    pub fn new() -> Self {
        let root = PaneId(0);
        let workspace = Workspace::new(WorkspaceId(0), 0, root);
        Session {
            workspaces: vec![workspace],
            active: 0,
            root,
            next_pane: 1,
            next_workspace: 1,
        }
    }

    /// The pane every session starts with.
    pub fn root_pane(&self) -> PaneId {
        self.root
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    pub fn workspace_count(&self) -> usize {
        self.workspaces.len()
    }

    pub fn active_index(&self) -> usize {
        self.active
    }

    pub fn active(&self) -> &Workspace {
        &self.workspaces[self.active]
    }

    pub fn active_mut(&mut self) -> &mut Workspace {
        &mut self.workspaces[self.active]
    }

    pub fn focus(&self) -> PaneId {
        self.active().focus
    }

    /// Every pane in the session, including those on inactive workspaces.
    pub fn all_panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        for workspace in &self.workspaces {
            out.extend(workspace.panes());
        }
        out
    }

    /// Which workspace a pane lives on.
    pub fn workspace_of(&self, pane: PaneId) -> Option<WorkspaceId> {
        self.workspaces
            .iter()
            .find(|w| w.layout.contains(pane))
            .map(|w| w.id)
    }

    /// Give every workspace the label matching its position.
    fn renumber(&mut self) {
        for (position, workspace) in self.workspaces.iter_mut().enumerate() {
            // A workspace that was renamed keeps its name.
            if workspace.renamed {
                continue;
            }
            workspace.name = format!("{}", position + 1);
        }
    }

    fn allocate_pane(&mut self) -> PaneId {
        let id = PaneId(self.next_pane);
        self.next_pane += 1;
        id
    }

    /// Split the focused pane inside `area`. Returns the new pane, which the
    /// caller must back with a terminal and a PTY, or `None` when the pane is
    /// too small to divide.
    pub fn split_focused(&mut self, area: Rect, axis: Axis) -> Option<PaneId> {
        let workspace = self.active();
        // The room for a split is judged against the tree even when some
        // other arrangement is on screen, because the tree is where the new
        // pane actually goes; the arrangement then places it wherever the
        // pane order says. That can refuse a split the grid on screen looks
        // roomy enough for, which is the honest answer: the split would be
        // waiting in a tree that cannot hold it, and going back to `splits`
        // is supposed to show what was left there rather than something the
        // area never had space for.
        // Zoom is no exemption from that, whatever it looks like on screen.
        // The test used to be skipped while a pane was zoomed, on the reading
        // that a zoomed pane is laid out as the whole workspace and so has
        // the whole workspace to divide. It does not: the pane goes into the
        // tree, where it is as narrow as it ever was, and `Layout::split`
        // asks `can_split` again and refuses. All the exemption bought was a
        // pane id spent on a split that was going to fail anyway.
        let target = workspace.focus;
        if !workspace.layout.can_split(area, target, axis) {
            return None;
        }
        let new_pane = self.allocate_pane();
        let workspace = self.active_mut();
        let focus = workspace.focus;
        if !workspace.layout.split(area, focus, axis, new_pane) {
            return None;
        }
        // Splitting always leaves a zoomed view, since the point of the split
        // is to see both panes.
        workspace.zoomed = None;
        workspace.focus = new_pane;
        Some(new_pane)
    }

    /// Close a pane. Returns the panes that should be torn down: the pane
    /// itself, or nothing when it was the last pane of the last workspace.
    pub fn close_pane(&mut self, pane: PaneId) -> Vec<PaneId> {
        let Some(index) = self.workspaces.iter().position(|w| w.layout.contains(pane)) else {
            return Vec::new();
        };

        if self.workspaces[index].layout.len() > 1 {
            let workspace = &mut self.workspaces[index];
            // Focus moves to the pane before this one in layout order, which
            // needs no geometry and so cannot disagree with the real screen.
            let panes = workspace.panes();
            let position = panes.iter().position(|p| *p == pane).unwrap_or(0);
            let neighbour = position
                .checked_sub(1)
                .and_then(|i| panes.get(i).copied())
                .or_else(|| panes.get(position + 1).copied());
            workspace.layout.close(pane);
            if workspace.zoomed == Some(pane) {
                workspace.zoomed = None;
            }
            if workspace.focus == pane {
                if let Some(neighbour) = neighbour {
                    workspace.focus = neighbour;
                }
            }
            return vec![pane];
        }

        // The workspace's last pane: the workspace goes with it, unless it is
        // the only workspace left, in which case the session is over.
        if self.workspaces.len() == 1 {
            return Vec::new();
        }
        self.workspaces.remove(index);
        if self.active >= self.workspaces.len() {
            self.active = self.workspaces.len() - 1;
        } else if index < self.active {
            self.active -= 1;
        }
        // Selection is by position, so the labels have to follow; otherwise
        // the number a user reads addresses a different workspace.
        self.renumber();
        vec![pane]
    }

    /// Move focus within the active workspace.
    pub fn focus_direction(&mut self, area: Rect, direction: Direction) -> bool {
        let workspace = self.active_mut();
        // A zoomed pane has no visible neighbours to move to.
        if workspace.zoomed.is_some() {
            return false;
        }
        let focus = workspace.focus;
        match workspace.neighbour(area, focus, direction) {
            Some(pane) => {
                workspace.focus = pane;
                true
            }
            None => false,
        }
    }

    /// Focus the next pane in layout order, wrapping around.
    pub fn focus_next(&mut self) -> PaneId {
        let workspace = self.active_mut();
        let panes = workspace.layout.panes();
        let current = panes
            .iter()
            .position(|&p| p == workspace.focus)
            .unwrap_or(0);
        let next = panes[(current + 1) % panes.len()];
        workspace.set_focus(next);
        next
    }

    /// Focus the previous pane in layout order, wrapping around.
    ///
    /// The step back is `len - 1` forward rather than a subtraction, because
    /// the position of the focused pane is where the wrap has to happen and
    /// `0 - 1` is the case that would have to be written out anyway.
    pub fn focus_previous(&mut self) -> PaneId {
        let workspace = self.active_mut();
        let panes = workspace.layout.panes();
        let current = panes
            .iter()
            .position(|&p| p == workspace.focus)
            .unwrap_or(0);
        let previous = panes[(current + panes.len() - 1) % panes.len()];
        workspace.set_focus(previous);
        previous
    }

    pub fn set_focus(&mut self, pane: PaneId) -> bool {
        // Focusing a pane on another workspace switches to that workspace.
        let Some(index) = self.workspaces.iter().position(|w| w.layout.contains(pane)) else {
            return false;
        };
        self.active = index;
        self.workspaces[index].set_focus(pane);
        true
    }

    /// Move the divider next to the focused pane.
    ///
    /// Refuses outside [`Arrangement::Splits`], where there is no divider on
    /// screen to move. Moving the tree's divider anyway would be the worse
    /// answer twice over: nothing would happen now, and the splits the user
    /// left behind would come back changed by a key that appeared to do
    /// nothing. The caller is expected to say why it refused.
    pub fn resize_focused(&mut self, area: Rect, direction: Direction, amount: i32) -> bool {
        let workspace = self.active_mut();
        if workspace.arrangement != Arrangement::Splits {
            return false;
        }
        let focus = workspace.focus;
        workspace.layout.resize(area, focus, direction, amount)
    }

    /// Toggle whether the focused pane fills the workspace.
    pub fn toggle_zoom(&mut self) -> bool {
        let workspace = self.active_mut();
        // Zooming a workspace with one pane would do nothing visible.
        if workspace.layout.len() == 1 {
            return false;
        }
        workspace.zoomed = match workspace.zoomed {
            Some(_) => None,
            None => Some(workspace.focus),
        };
        true
    }

    /// Even out every split. Returns false when it refused.
    ///
    /// Refused outside [`Arrangement::Splits`] for the same reason as
    /// [`Session::resize_focused`], and it is the restore promise that decides
    /// it: a derived arrangement is already even, so balancing would change
    /// nothing visible while quietly flattening the weights waiting in the
    /// tree. Switching back to `splits` would then not give back what was
    /// left there, which is the one thing this model promises.
    pub fn balance(&mut self) -> bool {
        let workspace = self.active_mut();
        if workspace.arrangement != Arrangement::Splits {
            return false;
        }
        workspace.layout.balance();
        true
    }

    /// Arrange the active workspace the next way, wrapping around.
    pub fn next_layout(&mut self) -> Arrangement {
        let next = self.active().arrangement.next();
        self.active_mut().set_arrangement(next);
        next
    }

    pub fn previous_layout(&mut self) -> Arrangement {
        let previous = self.active().arrangement.previous();
        self.active_mut().set_arrangement(previous);
        previous
    }

    /// Create a workspace with one new pane and switch to it.
    pub fn new_workspace(&mut self) -> PaneId {
        let pane = self.allocate_pane();
        let id = WorkspaceId(self.next_workspace);
        self.next_workspace += 1;
        let position = self.workspaces.len();
        self.workspaces.push(Workspace::new(id, position, pane));
        self.active = position;
        pane
    }

    /// Name the active workspace.
    ///
    /// An empty name is how the user asks for the number back: a blank label
    /// would leave the status bar with nothing to address the workspace by.
    /// Surrounding space cannot be seen there either, so a name made only of
    /// it is the same as no name at all.
    pub fn rename_active(&mut self, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            self.active_mut().clear_name();
        } else {
            self.active_mut().rename(name);
        }
        // A workspace that just lost its name needs its number back, and
        // renumbering leaves every name that is still wanted alone.
        self.renumber();
    }

    pub fn next_workspace(&mut self) -> WorkspaceId {
        self.active = (self.active + 1) % self.workspaces.len();
        self.active().id
    }

    pub fn previous_workspace(&mut self) -> WorkspaceId {
        self.active = (self.active + self.workspaces.len() - 1) % self.workspaces.len();
        self.active().id
    }

    /// Switch to a workspace by its position, 1 based as the bindings are.
    pub fn select_workspace(&mut self, number: usize) -> bool {
        if number == 0 || number > self.workspaces.len() {
            return false;
        }
        self.active = number - 1;
        true
    }

    /// Move the focused pane to another workspace, laid out in `area`.
    pub fn move_focused_to_workspace(&mut self, area: Rect, number: usize) -> bool {
        if number == 0 || number > self.workspaces.len() {
            return false;
        }
        let target = number - 1;
        if target == self.active {
            return false;
        }
        let pane = self.active().focus;
        // A workspace cannot be left with no panes.
        if self.workspaces[self.active].layout.len() == 1 {
            return false;
        }

        let source = self.active;
        // The destination gets the last word, and it may say no, so keep the
        // workspace the pane is leaving. Undoing the move by hand cannot
        // restore it: closing a pane hands its space to its neighbours, and
        // splitting the pane back in afterwards puts it beside whichever pane
        // focus moved to, in a tree weighted differently from the one the user
        // was looking at. Worse, that second split can be refused in its own
        // right when the pane it lands on is too narrow to divide, which left
        // the pane in no tree at all while focus still pointed at it. A copy
        // is the only thing that gives back the workspace as it was found.
        let restore = self.workspaces[source].clone();
        let neighbour = self.workspaces[source]
            .panes()
            .into_iter()
            .find(|p| *p != pane);
        self.workspaces[source].layout.close(pane);
        if self.workspaces[source].zoomed == Some(pane) {
            self.workspaces[source].zoomed = None;
        }
        if let Some(neighbour) = neighbour {
            self.workspaces[source].focus = neighbour;
        }

        let focus = self.workspaces[target].focus;
        if !self.workspaces[target]
            .layout
            .split(area, focus, Axis::Columns, pane)
        {
            self.workspaces[source] = restore;
            return false;
        }
        let destination = &mut self.workspaces[target];
        destination.focus = pane;
        destination.zoomed = None;
        true
    }
}

impl Default for Session {
    fn default() -> Self {
        Session::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn a_new_session_has_one_pane() {
        let session = Session::new();
        assert_eq!(session.workspace_count(), 1);
        assert_eq!(session.all_panes(), vec![PaneId(0)]);
        assert_eq!(session.focus(), PaneId(0));
    }

    #[test]
    fn splitting_focuses_the_new_pane() {
        let mut session = Session::new();
        let new_pane = session.split_focused(area(), Axis::Columns).unwrap();
        assert_eq!(session.focus(), new_pane);
        assert_eq!(session.all_panes().len(), 2);
    }

    #[test]
    fn pane_identifiers_are_never_reused() {
        let mut session = Session::new();
        let first = session.split_focused(area(), Axis::Columns).unwrap();
        session.close_pane(first);
        let second = session.split_focused(area(), Axis::Columns).unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn closing_a_pane_moves_focus_to_a_survivor() {
        let mut session = Session::new();
        let new_pane = session.split_focused(area(), Axis::Columns).unwrap();
        let closed = session.close_pane(new_pane);
        assert_eq!(closed, vec![new_pane]);
        assert_eq!(session.focus(), PaneId(0));
        assert_eq!(session.all_panes(), vec![PaneId(0)]);
    }

    #[test]
    fn closing_the_last_pane_of_the_last_workspace_ends_the_session() {
        let mut session = Session::new();
        assert!(session.close_pane(PaneId(0)).is_empty());
        assert_eq!(session.workspace_count(), 1);
    }

    #[test]
    fn closing_the_last_pane_of_a_workspace_removes_it() {
        let mut session = Session::new();
        let pane = session.new_workspace();
        assert_eq!(session.workspace_count(), 2);
        assert_eq!(session.close_pane(pane), vec![pane]);
        assert_eq!(session.workspace_count(), 1);
        assert_eq!(session.active_index(), 0);
    }

    #[test]
    fn focus_moves_between_panes() {
        let mut session = Session::new();
        let right = session.split_focused(area(), Axis::Columns).unwrap();
        assert!(session.focus_direction(area(), Direction::Left));
        assert_eq!(session.focus(), PaneId(0));
        assert!(session.focus_direction(area(), Direction::Right));
        assert_eq!(session.focus(), right);
        assert!(!session.focus_direction(area(), Direction::Right));
    }

    #[test]
    fn focus_next_wraps_around() {
        let mut session = Session::new();
        session.split_focused(area(), Axis::Columns);
        assert_eq!(session.focus_next(), PaneId(0));
        assert_eq!(session.focus_next(), PaneId(1));
    }

    #[test]
    fn focus_previous_goes_the_other_way_round_the_workspace() {
        let mut session = Session::new();
        session.split_focused(area(), Axis::Columns);
        session.split_focused(area(), Axis::Rows);
        let panes = session.active().panes();
        assert_eq!(panes.len(), 3);
        // Forward and then back arrives where it started, which a cycle that
        // wrapped in only one direction would not.
        let start = session.focus();
        session.focus_next();
        assert_eq!(session.focus_previous(), start);
        // And the step back off the first pane is the last one, not the first
        // one again.
        assert!(session.set_focus(panes[0]));
        assert_eq!(session.focus_previous(), panes[2]);
    }

    #[test]
    fn cycling_focus_leaves_a_zoom_that_was_hiding_where_it_went() {
        // Both directions go through `set_focus`, which is where that is
        // decided; a cycle that did not would move focus to a pane the zoom
        // keeps off the screen.
        let mut session = Session::new();
        session.split_focused(area(), Axis::Columns);
        assert!(session.toggle_zoom());
        session.focus_next();
        assert!(session.active().zoomed().is_none());
    }

    #[test]
    fn zoom_hides_the_other_panes() {
        let mut session = Session::new();
        let zoomed = session.split_focused(area(), Axis::Columns).unwrap();
        assert!(session.toggle_zoom());
        let geometry = session.active().geometry(area());
        assert_eq!(geometry, vec![(zoomed, area())]);
        assert!(session.toggle_zoom());
        assert_eq!(session.active().geometry(area()).len(), 2);
    }

    #[test]
    fn zoom_does_nothing_with_a_single_pane() {
        let mut session = Session::new();
        assert!(!session.toggle_zoom());
    }

    #[test]
    fn splitting_while_zoomed_shows_both_panes() {
        let mut session = Session::new();
        session.split_focused(area(), Axis::Columns);
        session.toggle_zoom();
        session.split_focused(area(), Axis::Rows);
        assert!(session.active().zoomed().is_none());
        assert_eq!(session.active().geometry(area()).len(), 3);
    }

    #[test]
    fn focus_cannot_leave_a_zoomed_pane() {
        let mut session = Session::new();
        session.split_focused(area(), Axis::Columns);
        session.toggle_zoom();
        assert!(!session.focus_direction(area(), Direction::Left));
    }

    #[test]
    fn workspaces_cycle() {
        let mut session = Session::new();
        session.new_workspace();
        session.new_workspace();
        assert_eq!(session.workspace_count(), 3);
        assert_eq!(session.active_index(), 2);
        session.next_workspace();
        assert_eq!(session.active_index(), 0);
        session.previous_workspace();
        assert_eq!(session.active_index(), 2);
    }

    #[test]
    fn workspaces_are_selected_by_number() {
        let mut session = Session::new();
        session.new_workspace();
        assert!(session.select_workspace(1));
        assert_eq!(session.active_index(), 0);
        assert!(!session.select_workspace(9));
        assert!(!session.select_workspace(0));
    }

    #[test]
    fn a_named_workspace_keeps_its_name_when_the_others_are_renumbered() {
        let mut session = Session::new();
        let middle = session.new_workspace();
        session.new_workspace();
        session.rename_active("build");
        // Closing an earlier workspace moves this one down a place. A number
        // would have to follow the position; a name must not.
        session.close_pane(middle);
        assert_eq!(session.workspaces()[0].name, "1");
        assert_eq!(session.workspaces()[1].name, "build");
    }

    #[test]
    fn an_empty_name_gives_the_workspace_its_number_back() {
        let mut session = Session::new();
        session.new_workspace();
        session.rename_active("build");
        assert_eq!(session.active().name, "build");
        session.rename_active("   ");
        assert_eq!(session.active().name, "2", "space is not a name");
        // And the number follows the position again, which it only can if the
        // rename was forgotten rather than merely overwritten.
        let root = session.root_pane();
        session.close_pane(root);
        assert_eq!(session.workspaces()[0].name, "1");
    }

    #[test]
    fn renaming_touches_only_the_active_workspace() {
        let mut session = Session::new();
        session.new_workspace();
        session.rename_active("build");
        session.select_workspace(1);
        assert_eq!(session.active().name, "1");
    }

    #[test]
    fn a_pane_can_move_to_another_workspace() {
        let mut session = Session::new();
        let moving = session.split_focused(area(), Axis::Columns).unwrap();
        session.new_workspace();
        session.select_workspace(1);
        session.set_focus(moving);

        assert!(session.move_focused_to_workspace(area(), 2));
        assert_eq!(session.workspace_of(moving), Some(WorkspaceId(1)));
        assert_eq!(
            session.active_index(),
            0,
            "the source workspace stays active"
        );
        assert!(!session.workspaces()[0].layout.contains(moving));
    }

    #[test]
    fn a_workspace_is_never_emptied_by_moving_a_pane() {
        let mut session = Session::new();
        session.new_workspace();
        session.select_workspace(1);
        // Workspace 1 has a single pane, so its pane cannot leave.
        assert!(!session.move_focused_to_workspace(area(), 2));
    }

    /// A workspace whose focused pane has no room left to divide, so that
    /// moving a pane into it is refused.
    fn full_workspace(session: &mut Session) {
        session.new_workspace();
        while session.split_focused(area(), Axis::Columns).is_some() {}
    }

    #[test]
    fn a_refused_move_leaves_the_pane_in_the_tree_it_started_in() {
        // The rollback used to put the pane back by splitting it in beside
        // whatever focus had moved to, and that split can be refused in its
        // own right: a pane two cells wide has nowhere to put a second one.
        // The pane then belonged to no workspace at all while `focus` still
        // named it, so every keystroke went to a pane nothing drew and the
        // next close read as the last pane in the session.
        let mut session = Session::new();
        // Splitting the first pane over and over leaves a row that halves
        // towards the left edge, where the outermost panes are too narrow to
        // divide again.
        let root = session.focus();
        for _ in 0..5 {
            session.set_focus(root);
            session.split_focused(area(), Axis::Columns).unwrap();
        }
        let narrow = session.active().panes()[1];
        full_workspace(&mut session);
        session.select_workspace(1);
        session.set_focus(narrow);

        let tree = session.workspaces()[0].panes();
        let geometry = session.workspaces()[0].geometry(area());
        assert!(!session.move_focused_to_workspace(area(), 2));
        assert_eq!(
            session.workspace_of(narrow),
            Some(session.workspaces()[0].id)
        );
        assert_eq!(session.workspaces()[0].panes(), tree);
        assert_eq!(session.workspaces()[0].geometry(area()), geometry);
        assert_eq!(session.focus(), narrow);
        // A pane in the tree closes to itself. An orphan closed to nothing,
        // which is how the compositor is told the session is over.
        assert_eq!(session.close_pane(narrow), vec![narrow]);
    }

    #[test]
    fn a_refused_move_leaves_the_source_widths_alone() {
        // Closing a pane hands its space to its neighbours, so splitting it
        // back in afterwards is not the tree it came from. The pane returned
        // second in a row it had been last in, with every width rewritten,
        // for a key that reported that nothing had happened.
        let mut session = Session::new();
        for _ in 0..4 {
            session.split_focused(area(), Axis::Columns).unwrap();
        }
        let moving = session.focus();
        full_workspace(&mut session);
        session.select_workspace(1);
        assert_eq!(session.focus(), moving);

        let tree = session.workspaces()[0].panes();
        let geometry = session.workspaces()[0].geometry(area());
        assert!(!session.move_focused_to_workspace(area(), 2));
        assert_eq!(session.workspaces()[0].panes(), tree);
        assert_eq!(session.workspaces()[0].geometry(area()), geometry);
    }

    #[test]
    fn focusing_a_pane_elsewhere_switches_workspace() {
        let mut session = Session::new();
        let other = session.new_workspace();
        session.select_workspace(1);
        assert!(session.set_focus(other));
        assert_eq!(session.active_index(), 1);
        assert_eq!(session.focus(), other);
    }

    #[test]
    fn resizing_the_focused_pane_changes_the_geometry() {
        let mut session = Session::new();
        session.active_mut().layout.gap = 0;
        session.split_focused(area(), Axis::Columns);
        let before = session.active().geometry(area())[0].1.width;
        assert!(session.resize_focused(area(), Direction::Left, 6));
        let after = session.active().geometry(area())[0].1.width;
        assert_eq!(after, before - 6);
    }

    /// A session whose tree is one row of `count` panes, which is the shape
    /// that disagrees with every derived arrangement and so is the one worth
    /// testing them against.
    fn row_of(count: usize) -> Session {
        let mut session = Session::new();
        for _ in 1..count {
            session
                .split_focused(area(), Axis::Columns)
                .expect("room for another pane");
        }
        session
    }

    #[test]
    fn leaving_the_split_tree_and_coming_back_restores_it_exactly() {
        // The promise the whole model rests on, and the one Kitty does not
        // make: a derived arrangement reads the tree and never writes to it,
        // so every manual split and every dragged divider is still there.
        let mut session = row_of(3);
        assert!(session.resize_focused(area(), Direction::Left, 7));
        let before = session.active().geometry(area());

        for _ in 0..Arrangement::ALL.len() {
            session.next_layout();
        }
        assert_eq!(session.active().arrangement(), Arrangement::Splits);
        assert_eq!(session.active().geometry(area()), before);
    }

    #[test]
    fn focus_follows_the_arrangement_on_screen_and_not_the_tree() {
        // Four panes side by side in the tree; a grid puts two of them under
        // the other two. Down from the first is nothing at all in the tree and
        // the third pane on screen, and the arrow key meant the screen.
        let mut session = row_of(4);
        let panes = session.active().panes();
        session.set_focus(panes[0]);
        assert!(!session.focus_direction(area(), Direction::Down));

        session.active_mut().set_arrangement(Arrangement::Grid);
        assert!(session.focus_direction(area(), Direction::Down));
        assert_eq!(session.focus(), panes[2]);
        // And the tree, asked directly, still says what it always said.
        assert_eq!(
            session
                .active()
                .layout
                .neighbour(area(), panes[0], Direction::Down),
            None
        );
    }

    #[test]
    fn a_divider_nobody_can_see_is_a_divider_nobody_can_grab() {
        let mut session = row_of(3);
        let area = area();
        // The tree has two gaps in it, and while it is what is on screen they
        // can be both drawn and taken hold of.
        let drawn = session.active().dividers(area);
        assert_eq!(drawn.len(), 2);
        let at = (drawn[0].1.x, drawn[0].1.y);
        assert!(session.active().divider_at(area, at.0, at.1).is_some());
        let held = session
            .active()
            .divider_at(area, at.0, at.1)
            .expect("a divider to hold")
            .id;

        // Under a derived arrangement the same cell draws nothing, and so it
        // must also grab nothing: the weights a drag would move are the ones
        // coming back to `splits` promises to return untouched, and a pointer
        // cannot be allowed to reshape a tree it is not being shown.
        session.active_mut().set_arrangement(Arrangement::Tall);
        assert!(session.active().dividers(area).is_empty());
        assert_eq!(session.active().divider_at(area, at.0, at.1), None);
        // The gate is doing the work, not the geometry: the tree asked
        // directly still offers the divider at that very cell.
        assert!(session
            .active()
            .layout
            .divider_at(area, at.0, at.1)
            .is_some());
        // A drag already in flight when the layout changed asks this instead,
        // and has to be told the same thing or it would keep moving one.
        assert_eq!(session.active().divider(area, held), None);
    }

    #[test]
    fn a_click_lands_on_the_pane_the_arrangement_put_under_it() {
        let mut session = row_of(3);
        let panes = session.active().panes();
        session.active_mut().set_arrangement(Arrangement::Tall);

        // Low and right of the middle: the last pane, because `tall` stacks
        // everything but the master down the right hand side. The tree, whose
        // three panes are columns, hands the same cell to the second pane —
        // one the pointer is nowhere near on screen.
        let cell = (50, 20);
        assert_eq!(
            session.active().pane_at(area(), cell.0, cell.1),
            Some(panes[2])
        );
        assert_eq!(
            session.active().layout.pane_at(area(), cell.0, cell.1),
            Some(panes[1])
        );
    }

    #[test]
    fn a_derived_arrangement_has_no_dividers_to_draw() {
        let mut session = row_of(3);
        assert_eq!(session.active().dividers(area()).len(), 2);
        session.active_mut().set_arrangement(Arrangement::Fat);
        assert!(session.active().dividers(area()).is_empty());
        // The tree still has them; it is the drawing that must not.
        assert_eq!(session.active().layout.dividers(area()).len(), 2);
    }

    #[test]
    fn the_divider_keys_refuse_outside_the_split_tree() {
        let mut session = row_of(3);
        assert!(session.resize_focused(area(), Direction::Left, 4));
        assert!(session.balance());

        for arrangement in [Arrangement::Tall, Arrangement::Fat, Arrangement::Grid] {
            session.active_mut().set_arrangement(arrangement);
            assert!(
                !session.resize_focused(area(), Direction::Left, 4),
                "{arrangement:?} has no divider to move"
            );
            assert!(
                !session.balance(),
                "{arrangement:?} is even already, and evening the tree would
                 lose what going back to splits is supposed to give back"
            );
        }

        // Refusing means refusing to touch the tree, not merely to redraw.
        session.active_mut().set_arrangement(Arrangement::Splits);
        let widths: Vec<u32> = session
            .active()
            .geometry(area())
            .iter()
            .map(|(_, rect)| rect.width)
            .collect();
        assert_eq!(widths[0], widths[1], "balance ran while splits was up");
    }

    #[test]
    fn splitting_under_an_arrangement_adds_a_pane_the_arrangement_places() {
        let mut session = Session::new();
        session.active_mut().set_arrangement(Arrangement::Grid);
        let new_pane = session
            .split_focused(area(), Axis::Columns)
            .expect("room for another pane");

        assert_eq!(session.active().arrangement(), Arrangement::Grid);
        let geometry = session.active().geometry(area());
        assert_eq!(geometry.len(), 2);
        // Two panes in a grid are a row, whatever axis the split named: the
        // tree took the split, and the arrangement decided where it shows.
        assert!(geometry.iter().any(|(pane, _)| *pane == new_pane));
        assert!(geometry
            .iter()
            .all(|(_, rect)| rect.height == area().height));
    }

    #[test]
    fn cycling_the_arrangement_gives_up_the_zoom() {
        // Otherwise the key would answer with a screen that does not change:
        // a zoomed workspace is one pane wherever the others would have gone.
        let mut session = row_of(2);
        assert!(session.toggle_zoom());
        session.next_layout();
        assert!(session.active().zoomed().is_none());
        assert_eq!(session.active().geometry(area()).len(), 2);
    }

    #[test]
    fn the_layout_keys_walk_the_active_workspace_only() {
        let mut session = Session::new();
        session.new_workspace();
        session.next_layout();
        assert_eq!(session.active().arrangement(), Arrangement::Tall);
        session.select_workspace(1);
        assert_eq!(session.active().arrangement(), Arrangement::Splits);
        session.previous_layout();
        assert_eq!(session.active().arrangement(), Arrangement::Grid);
    }
}
