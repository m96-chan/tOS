//! The pane tree.
//!
//! A conventional desktop manages overlapping windows; a multiplexer manages
//! PTYs. tOS owns both, so the layout tree is part of the compositor rather
//! than a tmux layer above it. Panes are laid out in cells, because a cell is
//! the unit of everything else in the system.

use std::collections::HashMap;

/// Identifies a pane for the lifetime of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PaneId(pub u32);

/// A rectangle of cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    pub fn right(&self) -> u32 {
        self.x + self.width
    }

    pub fn bottom(&self) -> u32 {
        self.y + self.height
    }

    pub fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    fn center(&self) -> (f64, f64) {
        (
            self.x as f64 + self.width as f64 / 2.0,
            self.y as f64 + self.height as f64 / 2.0,
        )
    }
}

/// How the children of a split are arranged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children sit side by side.
    Columns,
    /// Children sit one above another.
    Rows,
}

impl Axis {
    pub fn other(self) -> Axis {
        match self {
            Axis::Columns => Axis::Rows,
            Axis::Rows => Axis::Columns,
        }
    }
}

/// A direction to move focus or a divider in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn axis(self) -> Axis {
        match self {
            Direction::Left | Direction::Right => Axis::Columns,
            Direction::Up | Direction::Down => Axis::Rows,
        }
    }

    fn is_positive(self) -> bool {
        matches!(self, Direction::Right | Direction::Down)
    }
}

/// A named way of arranging the panes of a workspace.
///
/// For most of tOS's life the tree below has been the only arrangement there
/// is: a pane sits wherever the split that made it put it, and there was
/// nothing for a "next layout" key to advance through. These are the names it
/// advances through now, chosen to be the ones a Kitty user arrives with.
///
/// Everything but [`Arrangement::Splits`] derives its geometry from the pane
/// order alone — the order [`Layout::panes`] hands back, which is tree order —
/// and ignores the shape of the tree and every weight in it. Nothing here
/// mutates the tree. That is the whole model, and it is what makes leaving an
/// arrangement free: switching to `Tall` and back gives the manual splits and
/// the dragged dividers back exactly as they were, where Kitty discards them
/// the moment the layout is cycled past. The tree stays the source of truth
/// and an arrangement is a way of reading it.
///
/// Kitty's `stack` is deliberately not here. It shows the active window alone
/// and full screen, which is exactly what the zoom already does — see
/// `Workspace::zoomed`, which the same [`Workspace::geometry`] answers for.
/// Two mechanisms for one behaviour would mean two ways to be full screen and
/// a question about what happens when both are on, and no user would ever be
/// able to say which of the two they were in. `horizontal` and `vertical` are
/// missing for a duller reason: each is a `Grid` of one row or one column, and
/// a tree of splits reaches either in a keystroke.
///
/// [`Workspace::geometry`]: crate::session::Workspace::geometry
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Arrangement {
    /// The tree itself: panes where the splits left them.
    #[default]
    Splits,
    /// One full-height pane on the left, the rest stacked in a column beside
    /// it. Kitty's `tall`.
    Tall,
    /// One full-width pane on top, the rest side by side underneath. Kitty's
    /// `fat`.
    Fat,
    /// As square a grid as the pane count allows, filled row by row. Kitty's
    /// `grid`.
    Grid,
}

impl Arrangement {
    /// Every arrangement, in the order the layout keys walk them.
    ///
    /// `Splits` comes first because it is what every session starts in, so
    /// cycling forward is a tour of the derived arrangements that ends back
    /// home rather than a walk away from it.
    pub const ALL: [Arrangement; 4] = [
        Arrangement::Splits,
        Arrangement::Tall,
        Arrangement::Fat,
        Arrangement::Grid,
    ];

    /// What to call this on the status bar.
    pub fn name(self) -> &'static str {
        match self {
            Arrangement::Splits => "splits",
            Arrangement::Tall => "tall",
            Arrangement::Fat => "fat",
            Arrangement::Grid => "grid",
        }
    }

    pub fn next(self) -> Arrangement {
        let index = self.position();
        Arrangement::ALL[(index + 1) % Arrangement::ALL.len()]
    }

    pub fn previous(self) -> Arrangement {
        let index = self.position();
        Arrangement::ALL[(index + Arrangement::ALL.len() - 1) % Arrangement::ALL.len()]
    }

    fn position(self) -> usize {
        Arrangement::ALL
            .iter()
            .position(|&a| a == self)
            .expect("every arrangement is in ALL")
    }

    /// Where `panes` go inside `area`, in the order they are given, or `None`
    /// for [`Arrangement::Splits`].
    ///
    /// `Splits` is not a function of the pane order at all — it is the tree,
    /// and only the tree can answer for it. Saying so with `None` rather than
    /// quietly returning a row of panes is what keeps the one caller that
    /// matters honest: [`Workspace::geometry`] has to fall back to
    /// [`Layout::geometry`], and a wrong answer here would be a screen that
    /// disagreed with the tree it claims never to touch.
    ///
    /// [`Workspace::geometry`]: crate::session::Workspace::geometry
    pub fn geometry(self, panes: &[PaneId], area: Rect, gap: u32) -> Option<Vec<(PaneId, Rect)>> {
        if self == Arrangement::Splits {
            return None;
        }
        let mut out = Vec::with_capacity(panes.len());
        let Some((&master, rest)) = panes.split_first() else {
            return Some(out);
        };
        // One pane fills the workspace whatever the arrangement, and the
        // arrangements below all want a non-empty remainder to divide.
        if rest.is_empty() {
            out.push((master, area));
            return Some(out);
        }

        match self {
            Arrangement::Splits => unreachable!("returned above"),
            Arrangement::Tall => {
                let columns = spans(area.x, area.width, 2, gap);
                out.push((
                    master,
                    Rect::new(columns[0].0, area.y, columns[0].1, area.height),
                ));
                let rows = spans(area.y, area.height, rest.len(), gap);
                for (&pane, (y, height)) in rest.iter().zip(rows) {
                    out.push((pane, Rect::new(columns[1].0, y, columns[1].1, height)));
                }
            }
            Arrangement::Fat => {
                let rows = spans(area.y, area.height, 2, gap);
                out.push((master, Rect::new(area.x, rows[0].0, area.width, rows[0].1)));
                let columns = spans(area.x, area.width, rest.len(), gap);
                for (&pane, (x, width)) in rest.iter().zip(columns) {
                    out.push((pane, Rect::new(x, rows[1].0, width, rows[1].1)));
                }
            }
            Arrangement::Grid => {
                let count = panes.len();
                // The fewest columns that still fit the panes into as many
                // rows: five panes want three columns and two rows, not five
                // columns of nothing.
                let columns = (1usize..).find(|c| c * c >= count).unwrap_or(1);
                let rows = count.div_ceil(columns);
                for (row, (y, height)) in spans(area.y, area.height, rows, gap)
                    .into_iter()
                    .enumerate()
                {
                    let start = row * columns;
                    let end = (start + columns).min(count);
                    // A last row with fewer panes than the rest spreads them
                    // across the full width rather than leaving a hole where
                    // the missing pane would have been: a gap in the grid
                    // reads as a pane that failed to draw.
                    let across = spans(area.x, area.width, end - start, gap);
                    for (&pane, (x, width)) in panes[start..end].iter().zip(across) {
                        out.push((pane, Rect::new(x, y, width, height)));
                    }
                }
            }
        }
        Some(out)
    }
}

/// Cut `extent` cells from `start` into `count` even pieces with `gap` cells
/// between each pair, as `(offset, size)`.
///
/// Deliberately the same arithmetic the tree divides a split with, down to
/// [`child_size`] and the last piece taking whatever rounding left over, so
/// that an even split and a derived arrangement of the same panes agree to
/// the cell.
///
/// "Agree" means against a split whose children are even, which is what one
/// split of one pane gives. It is not a claim about any tree of the same
/// shape: splitting a pane twice leaves weights of a half and two quarters,
/// because each split halves its sibling rather than redealing the row, so
/// three panes made that way are 39/20/19 of eighty columns where `Grid`
/// gives 26/26/26. The arrangement is an even row by definition; the tree is
/// whatever the user made it.
fn spans(start: u32, extent: u32, count: usize, gap: u32) -> Vec<(u32, u32)> {
    let mut out = Vec::with_capacity(count);
    if count == 0 {
        return out;
    }
    let mut gap = gap;
    let mut total_gap = gap * (count as u32).saturating_sub(1);
    // A gap is decoration, and decoration does not get to take the last row a
    // pane has. When the gaps alone are what push a pane below a single cell,
    // spend them on the panes instead: panes touching is a worse picture than
    // panes separated, and both are better pictures than a pane that is not
    // there.
    if extent.saturating_sub(total_gap) < count as u32 {
        gap = 0;
        total_gap = 0;
    }
    let available = extent.saturating_sub(total_gap);

    // Not enough room to give everyone the floor: share the shortfall out
    // evenly instead of letting [`child_size`] apply it.
    //
    // That function reserves `MIN_PANE` for each child still to come, which
    // is right for a tree — a split that cannot afford its children is one
    // `can_split` refused to make, so the reserve only ever settles rounding.
    // A derived arrangement has no such gate: it places every pane in the
    // workspace, however many there are. Ask `child_size` for nine rows out
    // of sixteen and the reserve for the eight after it exceeds what is left,
    // so the ceiling comes out zero and the *first* panes vanish while the
    // last ones keep their full two rows. Ten panes on an 80x24 console is an
    // ordinary thing to have, and one press of ctrl+shift+l should not make
    // one of them disappear.
    //
    // Below the floor there is no arrangement that is not a compromise, so
    // the compromise is the even one: every pane the same size to within a
    // cell, the remainder going to the earliest. Above it nothing changes,
    // which is what keeps a derived row identical to the splits it mirrors.
    if (available as u64) < MIN_PANE as u64 * count as u64 {
        let base = available / count as u32;
        let extra = available % count as u32;
        let mut offset = 0u32;
        for index in 0..count {
            let size = base + u32::from((index as u32) < extra);
            out.push((start + offset, size));
            offset += size + gap;
        }
        return out;
    }

    let sum = count as f64;
    let mut used = 0u32;
    let mut offset = 0u32;
    for index in 0..count {
        let size = if index + 1 == count {
            available.saturating_sub(used)
        } else {
            child_size(available, used, 1.0, sum, count - index)
        };
        out.push((start + offset, size));
        used += size;
        offset += size + gap;
    }
    out
}

/// Which pane of an already laid out geometry is at a cell, if any.
///
/// Free rather than a method on [`Layout`] because the geometry on screen is
/// not always the tree's: a workspace in a derived [`Arrangement`] puts the
/// same panes in different rectangles, and a hit test that asked the tree
/// would answer for a picture nobody is looking at.
pub fn pane_at(geometry: &[(PaneId, Rect)], x: u32, y: u32) -> Option<PaneId> {
    geometry
        .iter()
        .find(|(_, rect)| rect.contains(x, y))
        .map(|(pane, _)| *pane)
}

/// The pane nearest to `from` in `direction`, within an already laid out
/// geometry.
///
/// Free for the same reason as [`pane_at`], and it matters more here: under
/// `Grid` the tree and the screen disagree about what is to the right of what,
/// and arrow keys that followed the tree would move focus to a pane that is
/// not in the direction the user pressed.
pub fn neighbour(
    geometry: &[(PaneId, Rect)],
    from: PaneId,
    direction: Direction,
) -> Option<PaneId> {
    let current = geometry.iter().find(|(p, _)| *p == from)?.1;
    let (cx, cy) = current.center();

    geometry
        .iter()
        .filter(|(pane, _)| *pane != from)
        .filter(|(_, rect)| match direction {
            Direction::Left => rect.right() <= current.x,
            Direction::Right => rect.x >= current.right(),
            Direction::Up => rect.bottom() <= current.y,
            Direction::Down => rect.y >= current.bottom(),
        })
        // Prefer the closest pane along the axis of travel, breaking ties
        // by how well it lines up across that axis.
        .min_by(|(_, a), (_, b)| {
            let score = |rect: &Rect| {
                let (x, y) = rect.center();
                let (along, across) = match direction {
                    Direction::Left => ((cx - x).abs(), (cy - y).abs()),
                    Direction::Right => ((x - cx).abs(), (cy - y).abs()),
                    Direction::Up => ((cy - y).abs(), (cx - x).abs()),
                    Direction::Down => ((y - cy).abs(), (cx - x).abs()),
                };
                (along, across)
            };
            let (aa, ab) = score(a);
            let (ba, bb) = score(b);
            (aa, ab)
                .partial_cmp(&(ba, bb))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(pane, _)| *pane)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct NodeId(usize);

#[derive(Debug, Clone)]
enum NodeKind {
    Leaf(PaneId),
    Split {
        axis: Axis,
        children: Vec<NodeId>,
        /// One weight per child; they are normalized on use, not on write.
        weights: Vec<f64>,
    },
}

#[derive(Debug, Clone)]
struct Node {
    parent: Option<NodeId>,
    kind: NodeKind,
}

/// Which divider, for as long as it exists.
///
/// The split it belongs to and the child it follows, which is the only thing
/// about a divider that does not move: its rectangle changes with every cell
/// it is dragged, and the panes either side of it are not enough to name it
/// on their own. Opaque on purpose — a caller holds one between a press and
/// the release that ends the drag and has no business taking it apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DividerId {
    split: NodeId,
    index: usize,
}

/// A gap between two panes: where it is, which way it runs, and which one it
/// is.
///
/// [`Axis::Columns`] means the children sit side by side, so the divider
/// between them is a vertical line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divider {
    pub axis: Axis,
    pub rect: Rect,
    pub id: DividerId,
}

/// The smallest a pane is allowed to become, in cells.
const MIN_PANE: u32 = 2;

/// Size of one child of a split.
///
/// The weighted share is floored at [`MIN_PANE`] so a pane never vanishes, but
/// it is also capped so that the children still to come each keep at least
/// that much: without the cap the floors can add up to more than the split
/// has, and the last child ends up zero sized and positioned outside its
/// parent.
fn child_size(available: u32, used: u32, weight: f64, sum: f64, remaining: usize) -> u32 {
    let ideal = (available as f64 * weight / sum).round() as u32;
    // Room the children after this one need at the very least.
    let reserved = MIN_PANE.saturating_mul(remaining.saturating_sub(1) as u32);
    let left = available.saturating_sub(used);
    let ceiling = left.saturating_sub(reserved);
    ideal.max(MIN_PANE).min(ceiling).min(left)
}

/// A tree of panes.
#[derive(Debug, Clone)]
pub struct Layout {
    nodes: Vec<Option<Node>>,
    free: Vec<usize>,
    root: NodeId,
    leaves: HashMap<PaneId, NodeId>,
    /// Cells left between panes, where dividers are drawn.
    pub gap: u32,
}

impl Layout {
    /// A layout holding a single pane.
    pub fn new(root_pane: PaneId) -> Self {
        let mut layout = Layout {
            nodes: Vec::new(),
            free: Vec::new(),
            root: NodeId(0),
            leaves: HashMap::new(),
            gap: 1,
        };
        let root = layout.alloc(Node {
            parent: None,
            kind: NodeKind::Leaf(root_pane),
        });
        layout.root = root;
        layout.leaves.insert(root_pane, root);
        layout
    }

    fn alloc(&mut self, node: Node) -> NodeId {
        match self.free.pop() {
            Some(index) => {
                self.nodes[index] = Some(node);
                NodeId(index)
            }
            None => {
                self.nodes.push(Some(node));
                NodeId(self.nodes.len() - 1)
            }
        }
    }

    fn free_node(&mut self, id: NodeId) {
        self.nodes[id.0] = None;
        self.free.push(id.0);
    }

    fn node(&self, id: NodeId) -> &Node {
        self.nodes[id.0].as_ref().expect("live node")
    }

    fn node_mut(&mut self, id: NodeId) -> &mut Node {
        self.nodes[id.0].as_mut().expect("live node")
    }

    /// Number of panes in the tree.
    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    pub fn contains(&self, pane: PaneId) -> bool {
        self.leaves.contains_key(&pane)
    }

    /// Panes in layout order, left to right and top to bottom.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect_panes(self.root, &mut out);
        out
    }

    fn collect_panes(&self, id: NodeId, out: &mut Vec<PaneId>) {
        match &self.node(id).kind {
            NodeKind::Leaf(pane) => out.push(*pane),
            NodeKind::Split { children, .. } => {
                for child in children.clone() {
                    self.collect_panes(child, out);
                }
            }
        }
    }

    /// Whether `pane` is currently large enough to split along `axis`.
    ///
    /// Both halves have to clear [`MIN_PANE`], and the divider needs a cell of
    /// its own. Without this check a split can be accepted that the area
    /// cannot hold, and some pane ends up with no room at all.
    pub fn can_split(&self, area: Rect, pane: PaneId, axis: Axis) -> bool {
        let Some(rect) = self
            .geometry(area)
            .into_iter()
            .find(|(p, _)| *p == pane)
            .map(|(_, rect)| rect)
        else {
            return false;
        };
        let extent = match axis {
            Axis::Columns => rect.width,
            Axis::Rows => rect.height,
        };
        extent >= MIN_PANE * 2 + self.gap
    }

    /// Split `pane` along `axis`, placing `new_pane` after it.
    ///
    /// Splitting again along the same axis extends the existing split rather
    /// than nesting, which is what keeps a row of panes evenly sized.
    ///
    /// Returns false when the pane is too small to divide; `area` is what the
    /// tree is currently laid out in.
    pub fn split(&mut self, area: Rect, pane: PaneId, axis: Axis, new_pane: PaneId) -> bool {
        let Some(&leaf) = self.leaves.get(&pane) else {
            return false;
        };
        if self.leaves.contains_key(&new_pane) {
            return false;
        }
        if !self.can_split(area, pane, axis) {
            return false;
        }

        let new_leaf = self.alloc(Node {
            parent: None,
            kind: NodeKind::Leaf(new_pane),
        });

        let parent = self.node(leaf).parent;
        let extend = match parent {
            Some(parent) => matches!(
                &self.node(parent).kind,
                NodeKind::Split { axis: parent_axis, .. } if *parent_axis == axis
            ),
            None => false,
        };

        if extend {
            let parent = parent.unwrap();
            let (index, share) = match &mut self.node_mut(parent).kind {
                NodeKind::Split {
                    children, weights, ..
                } => {
                    let index = children.iter().position(|&c| c == leaf).expect("child");
                    // The new pane takes half of its sibling's space.
                    let share = weights[index] / 2.0;
                    weights[index] = share;
                    children.insert(index + 1, new_leaf);
                    weights.insert(index + 1, share);
                    (index, share)
                }
                NodeKind::Leaf(_) => unreachable!("checked above"),
            };
            let _ = (index, share);
            self.node_mut(new_leaf).parent = Some(parent);
        } else {
            // Turn the leaf into a split holding the old and the new pane.
            let old_kind = std::mem::replace(
                &mut self.node_mut(leaf).kind,
                NodeKind::Split {
                    axis,
                    children: Vec::new(),
                    weights: Vec::new(),
                },
            );
            let moved = self.alloc(Node {
                parent: Some(leaf),
                kind: old_kind,
            });
            // The node that moved may itself have children pointing at it.
            self.reparent_children(moved);
            self.node_mut(new_leaf).parent = Some(leaf);
            self.node_mut(leaf).kind = NodeKind::Split {
                axis,
                children: vec![moved, new_leaf],
                weights: vec![0.5, 0.5],
            };
            self.leaves.insert(pane, moved);
        }

        self.leaves.insert(new_pane, new_leaf);
        true
    }

    /// After a node's contents move to a new id, its children must be told.
    fn reparent_children(&mut self, id: NodeId) {
        let children = match &self.node(id).kind {
            NodeKind::Split { children, .. } => children.clone(),
            NodeKind::Leaf(_) => return,
        };
        for child in children {
            self.node_mut(child).parent = Some(id);
        }
    }

    /// Remove a pane. Returns false when it is the last one, which the caller
    /// must handle by closing the workspace instead.
    pub fn close(&mut self, pane: PaneId) -> bool {
        let Some(&leaf) = self.leaves.get(&pane) else {
            return false;
        };
        let Some(parent) = self.node(leaf).parent else {
            // Removing the root would leave nothing to draw.
            return false;
        };

        self.leaves.remove(&pane);
        let remaining = match &mut self.node_mut(parent).kind {
            NodeKind::Split {
                children, weights, ..
            } => {
                let index = children.iter().position(|&c| c == leaf).expect("child");
                let freed = weights.remove(index);
                children.remove(index);
                // Give the freed space to the neighbours.
                let share = freed / weights.len().max(1) as f64;
                for weight in weights.iter_mut() {
                    *weight += share;
                }
                children.clone()
            }
            NodeKind::Leaf(_) => unreachable!("a parent is always a split"),
        };
        self.free_node(leaf);

        // A split with one child is just that child.
        if remaining.len() == 1 {
            let survivor = remaining[0];
            let kind = self.node(survivor).kind.clone();
            self.node_mut(parent).kind = kind;
            self.reparent_children(parent);
            if let NodeKind::Leaf(pane) = &self.node(parent).kind {
                self.leaves.insert(*pane, parent);
            }
            self.free_node(survivor);
        }
        true
    }

    /// Compute where every pane goes inside `area`.
    pub fn geometry(&self, area: Rect) -> Vec<(PaneId, Rect)> {
        let mut out = Vec::new();
        self.place(self.root, area, &mut out);
        out
    }

    fn place(&self, id: NodeId, area: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match &self.node(id).kind {
            NodeKind::Leaf(pane) => out.push((*pane, area)),
            NodeKind::Split {
                axis,
                children,
                weights,
            } => {
                let count = children.len() as u32;
                let total_gap = self.gap * count.saturating_sub(1);
                let available = match axis {
                    Axis::Columns => area.width.saturating_sub(total_gap),
                    Axis::Rows => area.height.saturating_sub(total_gap),
                };
                let sum: f64 = weights.iter().sum();
                let sum = if sum <= 0.0 { 1.0 } else { sum };

                // `used` counts pane cells only; `offset` also counts gaps.
                let mut used = 0u32;
                let mut offset = 0u32;
                let count = children.len();
                for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
                    let is_last = i + 1 == count;
                    let size = if is_last {
                        available.saturating_sub(used)
                    } else {
                        child_size(available, used, weight, sum, count - i)
                    };
                    let child_area = match axis {
                        Axis::Columns => Rect::new(area.x + offset, area.y, size, area.height),
                        Axis::Rows => Rect::new(area.x, area.y + offset, area.width, size),
                    };
                    self.place(child, child_area, out);
                    used += size;
                    offset += size + self.gap;
                }
            }
        }
    }

    /// The dividers between panes, for drawing.
    pub fn dividers(&self, area: Rect) -> Vec<(Axis, Rect)> {
        let mut out = Vec::new();
        self.collect_dividers(self.root, area, &mut out);
        out
    }

    fn collect_dividers(&self, id: NodeId, area: Rect, out: &mut Vec<(Axis, Rect)>) {
        let NodeKind::Split {
            axis,
            children,
            weights,
        } = &self.node(id).kind
        else {
            return;
        };
        let count = children.len() as u32;
        let total_gap = self.gap * count.saturating_sub(1);
        let available = match axis {
            Axis::Columns => area.width.saturating_sub(total_gap),
            Axis::Rows => area.height.saturating_sub(total_gap),
        };
        let sum: f64 = weights.iter().sum();
        let sum = if sum <= 0.0 { 1.0 } else { sum };

        let mut used = 0u32;
        let mut offset = 0u32;
        let count = children.len();
        for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
            let is_last = i + 1 == count;
            let size = if is_last {
                available.saturating_sub(used)
            } else {
                child_size(available, used, weight, sum, count - i)
            };
            let child_area = match axis {
                Axis::Columns => Rect::new(area.x + offset, area.y, size, area.height),
                Axis::Rows => Rect::new(area.x, area.y + offset, area.width, size),
            };
            self.collect_dividers(child, child_area, out);
            if !is_last && self.gap > 0 {
                let divider = match axis {
                    Axis::Columns => {
                        Rect::new(area.x + offset + size, area.y, self.gap, area.height)
                    }
                    Axis::Rows => Rect::new(area.x, area.y + offset + size, area.width, self.gap),
                };
                out.push((*axis, divider));
            }
            used += size;
            offset += size + self.gap;
        }
    }

    /// The dividers between panes, with enough of their identity to move one.
    ///
    /// The same gaps [`Layout::dividers`] reports, for the caller that has to
    /// say *which* divider rather than only draw it. A drag is a series of
    /// events about one divider, and the tree it lives in is being reshaped
    /// underneath them, so naming it by where it was when the button went
    /// down would mean chasing a rectangle that has moved.
    pub fn placed_dividers(&self, area: Rect) -> Vec<Divider> {
        let mut out = Vec::new();
        self.collect_placed_dividers(self.root, area, &mut out);
        out
    }

    /// The divider at a cell, if the cell is in one: the gaps are in no
    /// pane's rectangle, so [`Layout::pane_at`] answers `None` for all of
    /// them.
    pub fn divider_at(&self, area: Rect, x: u32, y: u32) -> Option<Divider> {
        self.placed_dividers(area)
            .into_iter()
            .find(|divider| divider.rect.contains(x, y))
    }

    /// Where a divider is now.
    ///
    /// `None` once it has stopped existing, which is what closing a pane on
    /// either side of it does.
    pub fn divider(&self, area: Rect, id: DividerId) -> Option<Divider> {
        self.placed_dividers(area)
            .into_iter()
            .find(|divider| divider.id == id)
    }

    fn collect_placed_dividers(&self, id: NodeId, area: Rect, out: &mut Vec<Divider>) {
        let NodeKind::Split {
            axis,
            children,
            weights,
        } = &self.node(id).kind
        else {
            return;
        };
        let count = children.len();
        let total_gap = self.gap * count.saturating_sub(1) as u32;
        let available = match axis {
            Axis::Columns => area.width.saturating_sub(total_gap),
            Axis::Rows => area.height.saturating_sub(total_gap),
        };
        let sum: f64 = weights.iter().sum();
        let sum = if sum <= 0.0 { 1.0 } else { sum };

        let mut used = 0u32;
        let mut offset = 0u32;
        for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
            let is_last = i + 1 == count;
            let size = if is_last {
                available.saturating_sub(used)
            } else {
                child_size(available, used, weight, sum, count - i)
            };
            let child_area = match axis {
                Axis::Columns => Rect::new(area.x + offset, area.y, size, area.height),
                Axis::Rows => Rect::new(area.x, area.y + offset, area.width, size),
            };
            self.collect_placed_dividers(child, child_area, out);
            if !is_last && self.gap > 0 {
                let rect = match axis {
                    Axis::Columns => {
                        Rect::new(area.x + offset + size, area.y, self.gap, area.height)
                    }
                    Axis::Rows => Rect::new(area.x, area.y + offset + size, area.width, self.gap),
                };
                out.push(Divider {
                    axis: *axis,
                    rect,
                    id: DividerId {
                        split: id,
                        index: i,
                    },
                });
            }
            used += size;
            offset += size + self.gap;
        }
    }

    /// Move one named divider by `amount` cells, positive being right or
    /// down.
    ///
    /// [`Layout::resize`] is keyed on a pane and a direction, which is what a
    /// binding has: it walks up from the pane to the first ancestor split
    /// along that axis with a sibling on that side. A pointer has neither —
    /// it has a rectangle it is holding — and the walk is not an inverse of
    /// that rectangle: the pane beside a divider can have a nearer ancestor
    /// of the same axis, and the drag would silently move a divider somewhere
    /// else in the tree. Naming the split and the child it follows is the
    /// whole of the address, so there is nothing left to guess.
    ///
    /// Returns false when the divider has gone, or when either side would
    /// drop below [`MIN_PANE`] — the same refusal the keyboard gets.
    pub fn resize_at(&mut self, area: Rect, id: DividerId, amount: i32) -> bool {
        let DividerId { split, index } = id;
        // The node may have been freed, or its split collapsed into the leaf
        // that survived it, while the button was held down.
        let Some(Some(node)) = self.nodes.get(split.0) else {
            return false;
        };
        let NodeKind::Split { children, .. } = &node.kind else {
            return false;
        };
        if index + 1 >= children.len() {
            return false;
        }
        let Some(&extent) = self.split_extents(area).get(&split) else {
            return false;
        };
        // Moving the divider along the axis grows the child before it at the
        // expense of the one after; there is no third party to a gap.
        self.shift_weights(split, index, index + 1, amount, extent)
    }

    /// Which pane is at a cell, if any.
    /// Which pane is at a cell, if any, according to the tree.
    ///
    /// A workspace answers this itself, because the tree is only what is on
    /// screen while the arrangement is [`Arrangement::Splits`].
    pub fn pane_at(&self, area: Rect, x: u32, y: u32) -> Option<PaneId> {
        pane_at(&self.geometry(area), x, y)
    }

    /// The pane nearest to `from` in `direction`, according to the tree. Same
    /// caveat as [`Layout::pane_at`].
    pub fn neighbour(&self, area: Rect, from: PaneId, direction: Direction) -> Option<PaneId> {
        neighbour(&self.geometry(area), from, direction)
    }

    /// Move the divider next to `pane` in `direction` by `amount` cells.
    ///
    /// Returns false when there is no divider to move, for instance at the
    /// edge of the screen.
    pub fn resize(&mut self, area: Rect, pane: PaneId, direction: Direction, amount: i32) -> bool {
        let Some(&leaf) = self.leaves.get(&pane) else {
            return false;
        };
        let axis = direction.axis();
        // Weights are relative to the split's own rectangle, so the cell delta
        // has to be measured against that rather than the whole screen.
        let extents = self.split_extents(area);

        // Walk up to the first split along the right axis that has room to
        // move on the side we want.
        let mut node = leaf;
        while let Some(parent) = self.node(node).parent {
            let (parent_axis, index, count) = match &self.node(parent).kind {
                NodeKind::Split { axis, children, .. } => (
                    *axis,
                    children.iter().position(|&c| c == node).expect("child"),
                    children.len(),
                ),
                NodeKind::Leaf(_) => unreachable!(),
            };
            if parent_axis == axis {
                let grow = direction.is_positive();
                // Growing right or down borrows from the next sibling.
                let neighbour = if grow {
                    if index + 1 < count {
                        Some(index + 1)
                    } else {
                        None
                    }
                } else if index > 0 {
                    Some(index - 1)
                } else {
                    None
                };
                if let Some(neighbour) = neighbour {
                    let extent = extents.get(&parent).copied().unwrap_or(match axis {
                        Axis::Columns => area.width,
                        Axis::Rows => area.height,
                    });
                    return self.shift_weights(parent, index, neighbour, amount, extent);
                }
            }
            node = parent;
        }
        false
    }

    /// The usable extent of every split, along its own axis, inside `area`.
    fn split_extents(&self, area: Rect) -> HashMap<NodeId, u32> {
        let mut out = HashMap::new();
        self.collect_extents(self.root, area, &mut out);
        out
    }

    fn collect_extents(&self, id: NodeId, area: Rect, out: &mut HashMap<NodeId, u32>) {
        let NodeKind::Split {
            axis,
            children,
            weights,
        } = &self.node(id).kind
        else {
            return;
        };
        let count = children.len();
        let total_gap = self.gap * count.saturating_sub(1) as u32;
        let available = match axis {
            Axis::Columns => area.width.saturating_sub(total_gap),
            Axis::Rows => area.height.saturating_sub(total_gap),
        };
        out.insert(id, available);

        let sum: f64 = weights.iter().sum();
        let sum = if sum <= 0.0 { 1.0 } else { sum };
        let mut used = 0u32;
        let mut offset = 0u32;
        for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
            let size = if i + 1 == count {
                available.saturating_sub(used)
            } else {
                child_size(available, used, weight, sum, count - i)
            };
            let child_area = match axis {
                Axis::Columns => Rect::new(area.x + offset, area.y, size, area.height),
                Axis::Rows => Rect::new(area.x, area.y + offset, area.width, size),
            };
            self.collect_extents(child, child_area, out);
            used += size;
            offset += size + self.gap;
        }
    }

    fn shift_weights(
        &mut self,
        split: NodeId,
        index: usize,
        neighbour: usize,
        amount: i32,
        extent: u32,
    ) -> bool {
        let NodeKind::Split { weights, .. } = &mut self.node_mut(split).kind else {
            return false;
        };
        let sum: f64 = weights.iter().sum();
        if extent == 0 || sum <= 0.0 {
            return false;
        }
        // Convert the requested cell delta into a share of the split.
        let delta = amount as f64 * sum / extent as f64;
        let floor = sum * MIN_PANE as f64 / extent as f64;
        let from = weights[neighbour];
        let to = weights[index];
        if from - delta < floor || to + delta < floor {
            return false;
        }
        weights[neighbour] = from - delta;
        weights[index] = to + delta;
        true
    }

    /// Give every pane in the tree an equal share of its parent split.
    pub fn balance(&mut self) {
        let ids: Vec<NodeId> = (0..self.nodes.len())
            .filter(|&i| self.nodes[i].is_some())
            .map(NodeId)
            .collect();
        for id in ids {
            if let NodeKind::Split { weights, .. } = &mut self.node_mut(id).kind {
                let share = 1.0 / weights.len() as f64;
                weights.iter_mut().for_each(|w| *w = share);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_arrangement_hides_a_pane_it_was_asked_to_place() {
        use super::*;
        // The tree can never put a pane in no rows at all, because
        // `can_split` refuses the split that would. A derived arrangement has
        // no such gate — it lays out every pane there is — so the floor has
        // to hold here instead, all the way past the point where the floor
        // itself stops fitting.
        // Up to the point where the rows run out: `tall` stacks every pane
        // but the master down one column, so twenty-five is what an
        // eighty-by-twenty-four console can physically show. Past that a
        // pane with no rows is the honest answer, not a bug.
        let area = Rect::new(0, 0, 80, 24);
        for count in 1..=25usize {
            let panes: Vec<PaneId> = (0..count as u32).map(PaneId).collect();
            for arrangement in [Arrangement::Tall, Arrangement::Fat, Arrangement::Grid] {
                let geometry = arrangement
                    .geometry(&panes, area, 1)
                    .expect("a derived arrangement lays out");
                assert_eq!(geometry.len(), count, "{arrangement:?} dropped a pane");
                for (pane, rect) in &geometry {
                    assert!(
                        rect.width > 0 && rect.height > 0,
                        "{arrangement:?} with {count} panes gave {pane:?} {rect:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_shortfall_is_shared_rather_than_spent_on_the_first_panes() {
        use super::*;
        // Sixteen rows between nine panes cannot give each the floor of two.
        // What must not happen is the earliest taking nothing so the last can
        // have their full share: every pane comes out within one cell of
        // every other.
        let sizes: Vec<u32> = spans(0, 24, 9, 1).into_iter().map(|(_, s)| s).collect();
        let smallest = *sizes.iter().min().expect("nine sizes");
        let largest = *sizes.iter().max().expect("nine sizes");
        assert!(smallest > 0, "a pane was given no rows at all: {sizes:?}");
        assert!(largest - smallest <= 1, "shared unevenly: {sizes:?}");
    }

    #[test]
    fn with_room_to_spare_a_derived_row_is_still_the_even_split_it_mirrors() {
        use super::*;
        // The even-shares path must not reach the ordinary case, or a `Grid`
        // of two would stop being rect-for-rect the `Columns` split it is
        // supposed to be indistinguishable from. Two panes, because that is
        // the split that is even: a third made by splitting again is 39/20/19
        // and was never what the arrangement promised to match.
        let area = Rect::new(0, 0, 80, 24);
        let mut layout = Layout::new(PaneId(0));
        layout.split(area, PaneId(0), Axis::Columns, PaneId(1));
        let widths: Vec<(u32, u32)> = layout
            .geometry(area)
            .into_iter()
            .map(|(_, rect)| (rect.x, rect.width))
            .collect();
        assert_eq!(spans(0, 80, 2, 1), widths);
    }
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    fn layout_of(panes: &[(PaneId, Rect)], pane: PaneId) -> Rect {
        panes.iter().find(|(p, _)| *p == pane).expect("pane").1
    }

    #[test]
    fn a_single_pane_fills_the_area() {
        let layout = Layout::new(PaneId(1));
        let geometry = layout.geometry(area());
        assert_eq!(geometry, vec![(PaneId(1), area())]);
    }

    #[test]
    fn splitting_into_columns_divides_the_width() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        assert!(layout.split(area(), PaneId(1), Axis::Columns, PaneId(2)));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)), Rect::new(0, 0, 40, 24));
        assert_eq!(layout_of(&geometry, PaneId(2)), Rect::new(40, 0, 40, 24));
    }

    #[test]
    fn splitting_into_rows_divides_the_height() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Rows, PaneId(2));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)), Rect::new(0, 0, 80, 12));
        assert_eq!(layout_of(&geometry, PaneId(2)), Rect::new(0, 12, 80, 12));
    }

    #[test]
    fn the_gap_leaves_room_for_a_divider() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 1;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        let geometry = layout.geometry(area());
        let left = layout_of(&geometry, PaneId(1));
        let right = layout_of(&geometry, PaneId(2));
        assert_eq!(left.right() + 1, right.x);
        assert_eq!(left.width + right.width + 1, 80);

        let dividers = layout.dividers(area());
        assert_eq!(dividers.len(), 1);
        assert_eq!(dividers[0].1, Rect::new(left.right(), 0, 1, 24));
    }

    #[test]
    fn splitting_the_same_axis_extends_the_row() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Columns, PaneId(3));
        let geometry = layout.geometry(area());
        assert_eq!(geometry.len(), 3);
        // Panes stay in left to right order.
        assert_eq!(layout.panes(), vec![PaneId(1), PaneId(2), PaneId(3)]);
        let widths: Vec<u32> = layout
            .panes()
            .iter()
            .map(|p| layout_of(&geometry, *p).width)
            .collect();
        assert_eq!(widths.iter().sum::<u32>(), 80);
        // The pane that was split gives up half its width.
        assert_eq!(widths[0], 40);
    }

    #[test]
    fn splitting_the_other_axis_nests() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Rows, PaneId(3));
        let geometry = layout.geometry(area());
        let two = layout_of(&geometry, PaneId(2));
        let three = layout_of(&geometry, PaneId(3));
        assert_eq!(two.x, 40);
        assert_eq!(three.x, 40);
        assert_eq!(two.height + three.height, 24);
    }

    #[test]
    fn panes_tile_the_area_exactly() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Columns, PaneId(3));
        // An area that does not divide evenly by three.
        let area = Rect::new(0, 0, 100, 30);
        let geometry = layout.geometry(area);
        let covered: u32 = geometry.iter().map(|(_, r)| r.width).sum();
        assert_eq!(covered, 100, "panes must cover the width with no holes");
        for window in geometry.windows(2) {
            assert_eq!(window[0].1.right(), window[1].1.x, "gap between panes");
        }
    }

    #[test]
    fn closing_a_pane_gives_its_space_to_a_neighbour() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        assert!(layout.close(PaneId(2)));
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.geometry(area()), vec![(PaneId(1), area())]);
    }

    #[test]
    fn closing_collapses_a_split_with_one_child_left() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Rows, PaneId(3));
        layout.close(PaneId(3));
        let geometry = layout.geometry(area());
        // Pane 2 takes the whole right column back.
        assert_eq!(layout_of(&geometry, PaneId(2)), Rect::new(40, 0, 40, 24));
    }

    #[test]
    fn the_last_pane_cannot_be_closed() {
        let mut layout = Layout::new(PaneId(1));
        assert!(!layout.close(PaneId(1)));
        assert_eq!(layout.len(), 1);
    }

    #[test]
    fn closing_an_unknown_pane_is_a_no_op() {
        let mut layout = Layout::new(PaneId(1));
        assert!(!layout.close(PaneId(99)));
    }

    #[test]
    fn focus_moves_to_the_neighbour_in_that_direction() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Rows, PaneId(3));

        assert_eq!(
            layout.neighbour(area(), PaneId(1), Direction::Right),
            Some(PaneId(2))
        );
        assert_eq!(
            layout.neighbour(area(), PaneId(2), Direction::Left),
            Some(PaneId(1))
        );
        assert_eq!(
            layout.neighbour(area(), PaneId(2), Direction::Down),
            Some(PaneId(3))
        );
        assert_eq!(
            layout.neighbour(area(), PaneId(3), Direction::Up),
            Some(PaneId(2))
        );
    }

    #[test]
    fn there_is_no_neighbour_at_the_edge() {
        let mut layout = Layout::new(PaneId(1));
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        assert_eq!(layout.neighbour(area(), PaneId(1), Direction::Left), None);
        assert_eq!(layout.neighbour(area(), PaneId(1), Direction::Up), None);
    }

    #[test]
    fn focus_picks_the_best_aligned_pane() {
        // A tall pane on the left, two stacked panes on the right. Moving
        // right from the left pane should land on whichever lines up best.
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Rows, PaneId(3));
        // Pane 1's centre is halfway down, as is the boundary between 2 and 3,
        // so either is defensible; what matters is that it is one of them.
        let target = layout.neighbour(area(), PaneId(1), Direction::Right);
        assert!(matches!(target, Some(PaneId(2)) | Some(PaneId(3))));
    }

    #[test]
    fn resizing_moves_the_divider() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        assert!(layout.resize(area(), PaneId(1), Direction::Right, 8));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 48);
        assert_eq!(layout_of(&geometry, PaneId(2)).width, 32);
    }

    #[test]
    fn resizing_stops_at_the_minimum_size() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        assert!(!layout.resize(area(), PaneId(1), Direction::Right, 100));
        // The layout is untouched when the move is refused.
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 40);
    }

    #[test]
    fn resizing_at_the_edge_does_nothing() {
        let mut layout = Layout::new(PaneId(1));
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        assert!(!layout.resize(area(), PaneId(1), Direction::Left, 4));
    }

    /// A layout with a divider at two depths and on both axes: two rows, the
    /// lower one split into columns.
    fn nested() -> Layout {
        let mut layout = Layout::new(PaneId(1));
        layout.split(area(), PaneId(1), Axis::Rows, PaneId(2));
        layout.split(area(), PaneId(2), Axis::Columns, PaneId(3));
        layout
    }

    #[test]
    fn the_dividers_that_can_be_grabbed_are_the_ones_that_are_drawn() {
        // Two walks over the same tree, so this is what keeps them the same
        // walk: a divider the pointer can find where none is painted is a
        // strip of screen that resizes the session when nudged.
        let layout = nested();
        let drawn = layout.dividers(area());
        let placed = layout.placed_dividers(area());
        assert_eq!(drawn.len(), placed.len());
        for ((axis, rect), divider) in drawn.iter().zip(&placed) {
            assert_eq!((*axis, *rect), (divider.axis, divider.rect));
        }
    }

    #[test]
    fn the_divider_under_a_cell_is_the_one_in_no_pane() {
        let layout = nested();
        for divider in layout.placed_dividers(area()) {
            let (x, y) = (divider.rect.x, divider.rect.y);
            assert_eq!(layout.pane_at(area(), x, y), None);
            assert_eq!(
                layout.divider_at(area(), x, y).map(|d| d.id),
                Some(divider.id)
            );
        }
        // A cell in a pane is not in a divider.
        assert!(layout.divider_at(area(), 0, 0).is_none());
    }

    #[test]
    fn moving_a_named_divider_moves_the_panes_either_side_of_it() {
        let mut layout = nested();
        let divider = layout
            .placed_dividers(area())
            .into_iter()
            .find(|d| d.axis == Axis::Columns)
            .expect("the lower row is split into columns");
        let before = layout.geometry(area());
        let (left, right) = (layout_of(&before, PaneId(2)), layout_of(&before, PaneId(3)));

        assert!(layout.resize_at(area(), divider.id, 6));
        let after = layout.geometry(area());
        assert_eq!(layout_of(&after, PaneId(2)).width, left.width + 6);
        assert_eq!(layout_of(&after, PaneId(3)).width, right.width - 6);
        // And the divider itself has followed, which is what the next event
        // of a drag is measured from.
        let moved = layout.divider(area(), divider.id).expect("still there");
        assert_eq!(moved.rect.x, divider.rect.x + 6);
    }

    #[test]
    fn moving_a_named_divider_stops_at_the_minimum_size() {
        let mut layout = nested();
        let divider = layout
            .placed_dividers(area())
            .into_iter()
            .find(|d| d.axis == Axis::Columns)
            .expect("a vertical divider");
        let before = layout.geometry(area());
        assert!(!layout.resize_at(area(), divider.id, 100));
        assert_eq!(layout.geometry(area()), before, "nothing should have moved");
    }

    #[test]
    fn two_dividers_a_pane_and_a_direction_confuse_are_moved_one_at_a_time() {
        // Two rows, each split into columns: two vertical dividers that look
        // alike to anything keyed on a direction, since which one
        // `Layout::resize` moves depends on which pane it starts walking up
        // from. The pointer is holding one of them and has to move that one.
        let mut layout = Layout::new(PaneId(1));
        layout.split(area(), PaneId(1), Axis::Rows, PaneId(2));
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(3));
        layout.split(area(), PaneId(2), Axis::Columns, PaneId(4));

        let vertical: Vec<Divider> = layout
            .placed_dividers(area())
            .into_iter()
            .filter(|d| d.axis == Axis::Columns)
            .collect();
        assert_eq!(vertical.len(), 2);
        assert!(layout.resize_at(area(), vertical[0].id, 5));

        let after = layout.placed_dividers(area());
        let moved = after.iter().find(|d| d.id == vertical[0].id).unwrap();
        let other = after.iter().find(|d| d.id == vertical[1].id).unwrap();
        assert_eq!(moved.rect.x, vertical[0].rect.x + 5);
        assert_eq!(
            other.rect.x, vertical[1].rect.x,
            "the other half of the screen"
        );
    }

    #[test]
    fn a_divider_whose_split_has_gone_cannot_be_moved() {
        // A pane can close while the button is still down, and the split it
        // was half of collapses into the pane that survived it.
        let mut layout = nested();
        let divider = layout
            .placed_dividers(area())
            .into_iter()
            .find(|d| d.axis == Axis::Columns)
            .expect("a vertical divider");
        layout.close(PaneId(3));
        assert!(layout.divider(area(), divider.id).is_none());
        assert!(!layout.resize_at(area(), divider.id, 2));
    }

    #[test]
    fn balance_restores_equal_shares() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        layout.resize(area(), PaneId(1), Direction::Right, 20);
        layout.balance();
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 40);
    }

    #[test]
    fn pane_at_finds_the_pane_under_a_cell() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 1;
        layout.split(area(), PaneId(1), Axis::Columns, PaneId(2));
        let geometry = layout.geometry(area());
        let right = layout_of(&geometry, PaneId(2));
        assert_eq!(layout.pane_at(area(), 0, 0), Some(PaneId(1)));
        assert_eq!(layout.pane_at(area(), right.x, 5), Some(PaneId(2)));
        // The divider column belongs to no pane.
        assert_eq!(layout.pane_at(area(), right.x - 1, 0), None);
    }

    #[test]
    fn deep_trees_stay_consistent() {
        let mut layout = Layout::new(PaneId(0));
        layout.gap = 0;
        // Each split halves the newest pane, so the area has to be large
        // enough for eleven halvings along each axis.
        let area = Rect::new(0, 0, 4096, 4096);
        for i in 1..12u32 {
            let axis = if i % 2 == 0 {
                Axis::Columns
            } else {
                Axis::Rows
            };
            assert!(layout.split(area, PaneId(i - 1), axis, PaneId(i)));
        }
        assert_eq!(layout.len(), 12);
        let geometry = layout.geometry(area);
        assert_eq!(geometry.len(), 12);
        // Closing every pane but one must not corrupt the tree.
        for i in (1..12u32).rev() {
            assert!(layout.close(PaneId(i)), "failed to close {i}");
        }
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.geometry(area), vec![(PaneId(0), area)]);
    }

    /// The panes an arrangement is asked to place, as a workspace would hand
    /// them over: in tree order, which is the only thing the derived
    /// arrangements read.
    fn panes(count: u32) -> Vec<PaneId> {
        (1..=count).map(PaneId).collect()
    }

    #[test]
    fn one_pane_fills_the_area_whatever_the_arrangement() {
        for arrangement in Arrangement::ALL {
            if arrangement == Arrangement::Splits {
                continue;
            }
            assert_eq!(
                arrangement.geometry(&panes(1), area(), 1),
                Some(vec![(PaneId(1), area())]),
                "{arrangement:?}"
            );
        }
    }

    #[test]
    fn the_tall_arrangement_gives_the_first_pane_the_left_and_stacks_the_rest() {
        let tall = |count| {
            Arrangement::Tall
                .geometry(&panes(count), area(), 0)
                .unwrap()
        };

        assert_eq!(
            tall(2),
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 24)),
                (PaneId(2), Rect::new(40, 0, 40, 24)),
            ]
        );
        assert_eq!(
            tall(3),
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 24)),
                (PaneId(2), Rect::new(40, 0, 40, 12)),
                (PaneId(3), Rect::new(40, 12, 40, 12)),
            ]
        );
        // The master keeps the full height however many join the column.
        let five = tall(5);
        assert_eq!(five[0], (PaneId(1), Rect::new(0, 0, 40, 24)));
        assert_eq!(
            five[1..]
                .iter()
                .map(|(_, rect)| (rect.y, rect.height))
                .collect::<Vec<_>>(),
            vec![(0, 6), (6, 6), (12, 6), (18, 6)]
        );
    }

    #[test]
    fn the_fat_arrangement_gives_the_first_pane_the_top_and_lines_the_rest_up_underneath() {
        let fat = |count| Arrangement::Fat.geometry(&panes(count), area(), 0).unwrap();

        assert_eq!(
            fat(2),
            vec![
                (PaneId(1), Rect::new(0, 0, 80, 12)),
                (PaneId(2), Rect::new(0, 12, 80, 12)),
            ]
        );
        assert_eq!(
            fat(3),
            vec![
                (PaneId(1), Rect::new(0, 0, 80, 12)),
                (PaneId(2), Rect::new(0, 12, 40, 12)),
                (PaneId(3), Rect::new(40, 12, 40, 12)),
            ]
        );
        let five = fat(5);
        assert_eq!(five[0], (PaneId(1), Rect::new(0, 0, 80, 12)));
        assert!(
            five[1..]
                .iter()
                .all(|(_, rect)| rect.y == 12 && rect.height == 12),
            "{five:?}"
        );
    }

    #[test]
    fn the_grid_arrangement_is_as_square_as_the_pane_count_allows() {
        let grid = |count| {
            Arrangement::Grid
                .geometry(&panes(count), area(), 0)
                .unwrap()
        };

        // Two panes are a row, not a column: a grid one pane deep is still a
        // grid, and splitting the height would waste the width first.
        assert_eq!(
            grid(2),
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 24)),
                (PaneId(2), Rect::new(40, 0, 40, 24)),
            ]
        );
        // Three panes are two columns and two rows, with the odd one spread
        // across the bottom rather than leaving a hole beside it.
        assert_eq!(
            grid(3),
            vec![
                (PaneId(1), Rect::new(0, 0, 40, 12)),
                (PaneId(2), Rect::new(40, 0, 40, 12)),
                (PaneId(3), Rect::new(0, 12, 80, 12)),
            ]
        );
        let five = grid(5);
        assert_eq!(five.len(), 5);
        assert_eq!(
            five[..3]
                .iter()
                .map(|(_, rect)| (rect.x, rect.width, rect.y, rect.height))
                .collect::<Vec<_>>(),
            vec![(0, 27, 0, 12), (27, 27, 0, 12), (54, 26, 0, 12)]
        );
        assert_eq!(
            five[3..]
                .iter()
                .map(|(_, rect)| (rect.x, rect.width, rect.y, rect.height))
                .collect::<Vec<_>>(),
            vec![(0, 40, 12, 12), (40, 40, 12, 12)]
        );
    }

    #[test]
    fn a_derived_arrangement_places_every_pane_inside_the_area_and_over_none_of_the_others() {
        for arrangement in Arrangement::ALL {
            let Some(_) = arrangement.geometry(&panes(1), area(), 1) else {
                continue;
            };
            for count in 1..=8u32 {
                let geometry = arrangement.geometry(&panes(count), area(), 1).unwrap();
                assert_eq!(geometry.len(), count as usize, "{arrangement:?} {count}");
                for (pane, rect) in &geometry {
                    assert!(
                        rect.right() <= area().right() && rect.bottom() <= area().bottom(),
                        "{arrangement:?} {count}: {pane:?} at {rect:?} leaves the area"
                    );
                    assert!(
                        rect.width >= MIN_PANE && rect.height >= MIN_PANE,
                        "{arrangement:?} {count}: {pane:?} at {rect:?} is too small to use"
                    );
                }
                for (i, (_, a)) in geometry.iter().enumerate() {
                    for (_, b) in &geometry[i + 1..] {
                        let overlaps = a.x < b.right()
                            && b.x < a.right()
                            && a.y < b.bottom()
                            && b.y < a.bottom();
                        assert!(!overlaps, "{arrangement:?} {count}: {a:?} over {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_derived_arrangements_use_the_same_arithmetic_as_a_split() {
        // A row of two panes is the same two rectangles however it was
        // arrived at, gap and rounding included. If these ever disagree,
        // leaving `splits` for `grid` and coming back would shuffle every
        // pane by a cell for no reason a user could name.
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 1;
        assert!(layout.split(area(), PaneId(1), Axis::Columns, PaneId(2)));
        assert_eq!(
            Arrangement::Grid.geometry(&layout.panes(), area(), layout.gap),
            Some(layout.geometry(area()))
        );
    }

    #[test]
    fn cycling_the_arrangements_wraps_both_ways() {
        let mut arrangement = Arrangement::Splits;
        for expected in [
            Arrangement::Tall,
            Arrangement::Fat,
            Arrangement::Grid,
            Arrangement::Splits,
        ] {
            arrangement = arrangement.next();
            assert_eq!(arrangement, expected);
        }
        for expected in [
            Arrangement::Grid,
            Arrangement::Fat,
            Arrangement::Tall,
            Arrangement::Splits,
        ] {
            arrangement = arrangement.previous();
            assert_eq!(arrangement, expected);
        }
    }

    #[test]
    fn splits_is_the_one_arrangement_that_cannot_answer_from_the_pane_order() {
        // The `None` is load-bearing: it is what sends `Workspace::geometry`
        // to the tree, and a `Some` here would be a screen drawn from a pane
        // list that knows nothing about the splits in it.
        assert_eq!(Arrangement::Splits.geometry(&panes(3), area(), 1), None);
    }
}

#[cfg(test)]
mod review_regressions {
    use super::*;

    /// Every pane must be inside the area, non-empty, and not overlap.
    fn assert_tiles(layout: &Layout, area: Rect) {
        let geometry = layout.geometry(area);
        for (pane, rect) in &geometry {
            assert!(!rect.is_empty(), "{pane:?} has no area: {rect:?}");
            assert!(
                rect.x >= area.x
                    && rect.y >= area.y
                    && rect.right() <= area.right()
                    && rect.bottom() <= area.bottom(),
                "{pane:?} at {rect:?} escapes {area:?}"
            );
        }
        for (i, (a_pane, a)) in geometry.iter().enumerate() {
            for (b_pane, b) in geometry.iter().skip(i + 1) {
                let overlap =
                    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom();
                assert!(!overlap, "{a_pane:?} {a:?} overlaps {b_pane:?} {b:?}");
            }
        }
    }

    #[test]
    fn many_splits_keep_every_pane_inside_the_area() {
        // Repeatedly splitting the newest pane halves its weight each time, so
        // the shares become very uneven; the minimum size must not then add up
        // to more than the area holds.
        for width in [40u32, 80, 120, 200] {
            let mut layout = Layout::new(PaneId(0));
            let area = Rect::new(0, 0, width, 40);
            for i in 1..16u32 {
                // A refused split is the correct answer once the pane is too
                // small; what must never happen is a pane with no room.
                layout.split(area, PaneId(i - 1), Axis::Columns, PaneId(i));
                assert_tiles(&layout, area);
            }
        }
    }

    #[test]
    fn many_splits_keep_every_pane_inside_a_narrow_area() {
        let mut layout = Layout::new(PaneId(0));
        let area = Rect::new(0, 0, 24, 8);
        for i in 1..12u32 {
            layout.split(area, PaneId(i - 1), Axis::Rows, PaneId(i));
            assert_tiles(&layout, area);
        }
    }

    #[test]
    fn alternating_splits_stay_inside_the_area() {
        let mut layout = Layout::new(PaneId(0));
        let area = Rect::new(0, 0, 100, 30);
        for i in 1..20u32 {
            let axis = if i % 2 == 0 {
                Axis::Columns
            } else {
                Axis::Rows
            };
            layout.split(area, PaneId(i - 1), axis, PaneId(i));
            assert_tiles(&layout, area);
        }
    }

    #[test]
    fn dividers_stay_inside_the_area_too() {
        let mut layout = Layout::new(PaneId(0));
        let area = Rect::new(0, 0, 60, 20);
        for i in 1..10u32 {
            layout.split(area, PaneId(i - 1), Axis::Columns, PaneId(i));
        }
        for (_, divider) in layout.dividers(area) {
            assert!(
                divider.right() <= area.right() && divider.bottom() <= area.bottom(),
                "divider {divider:?} escapes {area:?}"
            );
        }
    }

    #[test]
    fn every_pane_can_be_found_by_its_own_cells() {
        let mut layout = Layout::new(PaneId(0));
        layout.gap = 0;
        let area = Rect::new(0, 0, 80, 24);
        for i in 1..8u32 {
            let axis = if i % 3 == 0 {
                Axis::Rows
            } else {
                Axis::Columns
            };
            layout.split(area, PaneId(i - 1), axis, PaneId(i));
        }
        for (pane, rect) in layout.geometry(area) {
            assert_eq!(
                layout.pane_at(area, rect.x, rect.y),
                Some(pane),
                "{pane:?} at {rect:?} cannot be clicked"
            );
        }
    }

    #[test]
    fn a_pane_too_small_to_divide_is_not_split() {
        let mut layout = Layout::new(PaneId(0));
        layout.gap = 1;
        // Four columns need two cells each plus three dividers.
        let area = Rect::new(0, 0, 4, 4);
        assert!(!layout.can_split(area, PaneId(0), Axis::Columns));
        assert!(!layout.split(area, PaneId(0), Axis::Columns, PaneId(1)));
        assert_eq!(layout.len(), 1, "a refused split must change nothing");

        let roomy = Rect::new(0, 0, 5, 4);
        assert!(layout.can_split(roomy, PaneId(0), Axis::Columns));
        assert!(layout.split(roomy, PaneId(0), Axis::Columns, PaneId(1)));
    }

    #[test]
    fn resizing_a_nested_split_moves_by_the_requested_amount() {
        // The delta must be measured against the split's own rectangle, not
        // the whole screen, or a nested divider moves by the wrong distance.
        let mut layout = Layout::new(PaneId(0));
        layout.gap = 0;
        let area = Rect::new(0, 0, 200, 40);
        layout.split(area, PaneId(0), Axis::Columns, PaneId(1));
        layout.split(area, PaneId(0), Axis::Rows, PaneId(2));
        layout.split(area, PaneId(2), Axis::Columns, PaneId(3));

        let width_of = |layout: &Layout, pane: PaneId| {
            layout
                .geometry(area)
                .into_iter()
                .find(|(p, _)| *p == pane)
                .unwrap()
                .1
                .width
        };
        let before = width_of(&layout, PaneId(2));
        assert!(layout.resize(area, PaneId(2), Direction::Right, 10));
        assert_eq!(width_of(&layout, PaneId(2)), before + 10);
    }

    #[test]
    fn resizing_a_top_level_split_accounts_for_the_gap() {
        let mut layout = Layout::new(PaneId(0));
        layout.gap = 1;
        let area = Rect::new(0, 0, 80, 24);
        layout.split(area, PaneId(0), Axis::Columns, PaneId(1));
        let width_of = |layout: &Layout| layout.geometry(area)[0].1.width;
        let before = width_of(&layout);
        assert!(layout.resize(area, PaneId(0), Direction::Right, 8));
        assert_eq!(width_of(&layout), before + 8);
    }
}
