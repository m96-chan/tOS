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

/// The smallest a pane is allowed to become, in cells.
const MIN_PANE: u32 = 2;

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

    /// Split `pane` along `axis`, placing `new_pane` after it.
    ///
    /// Splitting again along the same axis extends the existing split rather
    /// than nesting, which is what keeps a row of panes evenly sized.
    pub fn split(&mut self, pane: PaneId, axis: Axis, new_pane: PaneId) -> bool {
        let Some(&leaf) = self.leaves.get(&pane) else {
            return false;
        };
        if self.leaves.contains_key(&new_pane) {
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
                for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
                    let is_last = i + 1 == children.len();
                    // The last child absorbs the rounding error so the panes
                    // always tile the area exactly.
                    let size = if is_last {
                        available.saturating_sub(used)
                    } else {
                        ((available as f64 * weight / sum).round() as u32).max(MIN_PANE.min(available))
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
        for (i, (&child, &weight)) in children.iter().zip(weights).enumerate() {
            let is_last = i + 1 == children.len();
            let size = if is_last {
                available.saturating_sub(used)
            } else {
                ((available as f64 * weight / sum).round() as u32).max(MIN_PANE.min(available))
            };
            let child_area = match axis {
                Axis::Columns => Rect::new(area.x + offset, area.y, size, area.height),
                Axis::Rows => Rect::new(area.x, area.y + offset, area.width, size),
            };
            self.collect_dividers(child, child_area, out);
            if !is_last && self.gap > 0 {
                let divider = match axis {
                    Axis::Columns => Rect::new(
                        area.x + offset + size,
                        area.y,
                        self.gap,
                        area.height,
                    ),
                    Axis::Rows => Rect::new(
                        area.x,
                        area.y + offset + size,
                        area.width,
                        self.gap,
                    ),
                };
                out.push((*axis, divider));
            }
            used += size;
            offset += size + self.gap;
        }
    }

    /// Which pane is at a cell, if any.
    pub fn pane_at(&self, area: Rect, x: u32, y: u32) -> Option<PaneId> {
        self.geometry(area)
            .into_iter()
            .find(|(_, rect)| rect.contains(x, y))
            .map(|(pane, _)| pane)
    }

    /// The pane nearest to `from` in `direction`.
    pub fn neighbour(&self, area: Rect, from: PaneId, direction: Direction) -> Option<PaneId> {
        let geometry = self.geometry(area);
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

    /// Move the divider next to `pane` in `direction` by `amount` cells.
    ///
    /// Returns false when there is no divider to move, for instance at the
    /// edge of the screen.
    pub fn resize(&mut self, area: Rect, pane: PaneId, direction: Direction, amount: i32) -> bool {
        let Some(&leaf) = self.leaves.get(&pane) else {
            return false;
        };
        let axis = direction.axis();

        // Walk up to the first split along the right axis that has room to
        // move on the side we want.
        let mut node = leaf;
        while let Some(parent) = self.node(node).parent {
            let (parent_axis, index, count) = match &self.node(parent).kind {
                NodeKind::Split {
                    axis, children, ..
                } => (
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
                    let extent = match axis {
                        Axis::Columns => area.width,
                        Axis::Rows => area.height,
                    };
                    return self.shift_weights(parent, index, neighbour, amount, extent);
                }
            }
            node = parent;
        }
        false
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
        assert!(layout.split(PaneId(1), Axis::Columns, PaneId(2)));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)), Rect::new(0, 0, 40, 24));
        assert_eq!(layout_of(&geometry, PaneId(2)), Rect::new(40, 0, 40, 24));
    }

    #[test]
    fn splitting_into_rows_divides_the_height() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Rows, PaneId(2));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)), Rect::new(0, 0, 80, 12));
        assert_eq!(layout_of(&geometry, PaneId(2)), Rect::new(0, 12, 80, 12));
    }

    #[test]
    fn the_gap_leaves_room_for_a_divider() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 1;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Columns, PaneId(3));
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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Rows, PaneId(3));
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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Columns, PaneId(3));
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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        assert!(layout.close(PaneId(2)));
        assert_eq!(layout.len(), 1);
        assert_eq!(layout.geometry(area()), vec![(PaneId(1), area())]);
    }

    #[test]
    fn closing_collapses_a_split_with_one_child_left() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Rows, PaneId(3));
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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Rows, PaneId(3));

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
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        assert_eq!(layout.neighbour(area(), PaneId(1), Direction::Left), None);
        assert_eq!(layout.neighbour(area(), PaneId(1), Direction::Up), None);
    }

    #[test]
    fn focus_picks_the_best_aligned_pane() {
        // A tall pane on the left, two stacked panes on the right. Moving
        // right from the left pane should land on whichever lines up best.
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.split(PaneId(2), Axis::Rows, PaneId(3));
        // Pane 1's centre is halfway down, as is the boundary between 2 and 3,
        // so either is defensible; what matters is that it is one of them.
        let target = layout.neighbour(area(), PaneId(1), Direction::Right);
        assert!(matches!(target, Some(PaneId(2)) | Some(PaneId(3))));
    }

    #[test]
    fn resizing_moves_the_divider() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        assert!(layout.resize(area(), PaneId(1), Direction::Right, 8));
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 48);
        assert_eq!(layout_of(&geometry, PaneId(2)).width, 32);
    }

    #[test]
    fn resizing_stops_at_the_minimum_size() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        assert!(!layout.resize(area(), PaneId(1), Direction::Right, 100));
        // The layout is untouched when the move is refused.
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 40);
    }

    #[test]
    fn resizing_at_the_edge_does_nothing() {
        let mut layout = Layout::new(PaneId(1));
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        assert!(!layout.resize(area(), PaneId(1), Direction::Left, 4));
    }

    #[test]
    fn balance_restores_equal_shares() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 0;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
        layout.resize(area(), PaneId(1), Direction::Right, 20);
        layout.balance();
        let geometry = layout.geometry(area());
        assert_eq!(layout_of(&geometry, PaneId(1)).width, 40);
    }

    #[test]
    fn pane_at_finds_the_pane_under_a_cell() {
        let mut layout = Layout::new(PaneId(1));
        layout.gap = 1;
        layout.split(PaneId(1), Axis::Columns, PaneId(2));
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
        for i in 1..12u32 {
            let axis = if i % 2 == 0 { Axis::Columns } else { Axis::Rows };
            assert!(layout.split(PaneId(i - 1), axis, PaneId(i)));
        }
        assert_eq!(layout.len(), 12);
        let geometry = layout.geometry(Rect::new(0, 0, 200, 100));
        assert_eq!(geometry.len(), 12);
        // Closing every pane but one must not corrupt the tree.
        for i in (1..12u32).rev() {
            assert!(layout.close(PaneId(i)), "failed to close {i}");
        }
        assert_eq!(layout.len(), 1);
        assert_eq!(
            layout.geometry(Rect::new(0, 0, 200, 100)),
            vec![(PaneId(0), Rect::new(0, 0, 200, 100))]
        );
    }
}
