//! Split pane management — divide a tab into multiple terminal views.
//!
//! Uses a binary tree layout where each node is either a leaf (terminal)
//! or a split (horizontal or vertical) containing two children.

/// Unique identifier for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(pub usize);

/// Split direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDir {
    Horizontal,
    Vertical,
}

/// A node in the pane layout tree.
#[derive(Debug)]
pub enum PaneNode {
    Leaf {
        id: PaneId,
    },
    Split {
        direction: SplitDir,
        /// Fraction of space allocated to the first child (0.0..1.0).
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

/// A resolved pane rectangle in pixel coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PaneRect {
    pub id: PaneId,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Pane manager for a single tab.
pub struct PaneManager {
    root: PaneNode,
    focused: PaneId,
    next_id: usize,
}

impl PaneManager {
    /// Create a new pane manager with one initial pane.
    #[must_use]
    pub fn new() -> Self {
        Self {
            root: PaneNode::Leaf { id: PaneId(0) },
            focused: PaneId(0),
            next_id: 1,
        }
    }

    /// Get the focused pane ID.
    #[must_use]
    pub fn focused(&self) -> PaneId {
        self.focused
    }

    /// Split the focused pane. Returns the new pane's ID.
    pub fn split(&mut self, direction: SplitDir) -> PaneId {
        let new_id = PaneId(self.next_id);
        self.next_id += 1;

        Self::split_node(&mut self.root, self.focused, direction, new_id);
        self.focused = new_id;
        new_id
    }

    fn split_node(node: &mut PaneNode, target: PaneId, direction: SplitDir, new_id: PaneId) -> bool {
        match node {
            PaneNode::Leaf { id } if *id == target => {
                let old_id = *id;
                *node = PaneNode::Split {
                    direction,
                    ratio: 0.5,
                    first: Box::new(PaneNode::Leaf { id: old_id }),
                    second: Box::new(PaneNode::Leaf { id: new_id }),
                };
                true
            }
            PaneNode::Split { first, second, .. } => {
                Self::split_node(first, target, direction, new_id)
                    || Self::split_node(second, target, direction, new_id)
            }
            _ => false,
        }
    }

    /// Close the focused pane. Returns the closed pane's ID, or None if it's the only pane.
    pub fn close_focused(&mut self) -> Option<PaneId> {
        if matches!(self.root, PaneNode::Leaf { .. }) {
            return None;
        }
        let closed = self.focused;
        let ids = self.all_ids();
        if ids.len() <= 1 {
            return None;
        }
        Self::remove_node(&mut self.root, closed);
        // Focus the first remaining pane
        let remaining = self.all_ids();
        if let Some(&first) = remaining.first() {
            self.focused = first;
        }
        Some(closed)
    }

    fn remove_node(node: &mut PaneNode, target: PaneId) -> bool {
        match node {
            PaneNode::Split { first, second, .. } => {
                if let PaneNode::Leaf { id } = first.as_ref() {
                    if *id == target {
                        *node = std::mem::replace(second.as_mut(), PaneNode::Leaf { id: PaneId(usize::MAX) });
                        return true;
                    }
                }
                if let PaneNode::Leaf { id } = second.as_ref() {
                    if *id == target {
                        *node = std::mem::replace(first.as_mut(), PaneNode::Leaf { id: PaneId(usize::MAX) });
                        return true;
                    }
                }
                Self::remove_node(first, target) || Self::remove_node(second, target)
            }
            _ => false,
        }
    }

    /// Focus the next pane in leaf order.
    pub fn focus_next(&mut self) {
        let ids = self.all_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused) {
            self.focused = ids[(pos + 1) % ids.len()];
        }
    }

    /// Focus the previous pane in leaf order.
    pub fn focus_prev(&mut self) {
        let ids = self.all_ids();
        if let Some(pos) = ids.iter().position(|id| *id == self.focused) {
            self.focused = ids[if pos == 0 { ids.len() - 1 } else { pos - 1 }];
        }
    }

    /// Collect all leaf pane IDs in order.
    #[must_use]
    pub fn all_ids(&self) -> Vec<PaneId> {
        let mut ids = Vec::new();
        Self::collect_ids(&self.root, &mut ids);
        ids
    }

    fn collect_ids(node: &PaneNode, ids: &mut Vec<PaneId>) {
        match node {
            PaneNode::Leaf { id } => ids.push(*id),
            PaneNode::Split { first, second, .. } => {
                Self::collect_ids(first, ids);
                Self::collect_ids(second, ids);
            }
        }
    }

    /// Compute pixel rectangles for all panes given a viewport.
    #[must_use]
    pub fn layout(&self, x: f32, y: f32, width: f32, height: f32) -> Vec<PaneRect> {
        let mut rects = Vec::new();
        Self::layout_node(&self.root, x, y, width, height, &mut rects);
        rects
    }

    fn layout_node(
        node: &PaneNode,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        rects: &mut Vec<PaneRect>,
    ) {
        match node {
            PaneNode::Leaf { id } => {
                rects.push(PaneRect {
                    id: *id,
                    x,
                    y,
                    width,
                    height,
                });
            }
            PaneNode::Split {
                direction,
                ratio,
                first,
                second,
            } => match direction {
                SplitDir::Vertical => {
                    let first_w = width * ratio;
                    let second_w = width - first_w;
                    Self::layout_node(first, x, y, first_w, height, rects);
                    Self::layout_node(second, x + first_w, y, second_w, height, rects);
                }
                SplitDir::Horizontal => {
                    let first_h = height * ratio;
                    let second_h = height - first_h;
                    Self::layout_node(first, x, y, width, first_h, rects);
                    Self::layout_node(second, x, y + first_h, width, second_h, rects);
                }
            },
        }
    }

    /// Number of panes.
    #[must_use]
    #[allow(dead_code)]
    pub fn count(&self) -> usize {
        self.all_ids().len()
    }
}

impl Default for PaneManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_has_one_pane() {
        let mgr = PaneManager::new();
        assert_eq!(mgr.count(), 1);
        assert_eq!(mgr.focused(), PaneId(0));
    }

    #[test]
    fn split_vertical() {
        let mut mgr = PaneManager::new();
        let new_id = mgr.split(SplitDir::Vertical);
        assert_eq!(mgr.count(), 2);
        assert_eq!(mgr.focused(), new_id);
    }

    #[test]
    fn split_horizontal() {
        let mut mgr = PaneManager::new();
        let new_id = mgr.split(SplitDir::Horizontal);
        assert_eq!(mgr.count(), 2);
        assert_eq!(mgr.focused(), new_id);
    }

    #[test]
    fn close_pane() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Vertical);
        assert_eq!(mgr.count(), 2);

        let closed = mgr.close_focused();
        assert!(closed.is_some());
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn cannot_close_last_pane() {
        let mut mgr = PaneManager::new();
        assert!(mgr.close_focused().is_none());
    }

    #[test]
    fn focus_navigation() {
        let mut mgr = PaneManager::new();
        let id1 = mgr.split(SplitDir::Vertical);
        // Focus is on the new pane (id1)
        assert_eq!(mgr.focused(), id1);

        mgr.focus_prev();
        assert_eq!(mgr.focused(), PaneId(0));

        mgr.focus_next();
        assert_eq!(mgr.focused(), id1);

        mgr.focus_next();
        assert_eq!(mgr.focused(), PaneId(0)); // wraps
    }

    #[test]
    fn layout_single_pane() {
        let mgr = PaneManager::new();
        let rects = mgr.layout(0.0, 0.0, 800.0, 600.0);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].width, 800.0);
        assert_eq!(rects[0].height, 600.0);
    }

    #[test]
    fn layout_vertical_split() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Vertical);
        let rects = mgr.layout(0.0, 0.0, 800.0, 600.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].width, 400.0);
        assert_eq!(rects[1].width, 400.0);
        assert_eq!(rects[0].x, 0.0);
        assert_eq!(rects[1].x, 400.0);
    }

    #[test]
    fn layout_horizontal_split() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Horizontal);
        let rects = mgr.layout(0.0, 0.0, 800.0, 600.0);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].height, 300.0);
        assert_eq!(rects[1].height, 300.0);
        assert_eq!(rects[0].y, 0.0);
        assert_eq!(rects[1].y, 300.0);
    }

    #[test]
    fn nested_splits() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Vertical);
        // Focus is on the new pane, split it again
        mgr.split(SplitDir::Horizontal);
        assert_eq!(mgr.count(), 3);

        let rects = mgr.layout(0.0, 0.0, 800.0, 600.0);
        assert_eq!(rects.len(), 3);
    }

    #[test]
    fn all_ids_order() {
        let mut mgr = PaneManager::new();
        let id1 = mgr.split(SplitDir::Vertical);
        let ids = mgr.all_ids();
        assert_eq!(ids, vec![PaneId(0), id1]);
    }

    #[test]
    fn deep_nested_splits() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Vertical);
        mgr.split(SplitDir::Horizontal);
        // Focus back to first pane and split again
        mgr.focus_prev();
        mgr.focus_prev();
        mgr.split(SplitDir::Horizontal);
        assert_eq!(mgr.count(), 4);

        let rects = mgr.layout(0.0, 0.0, 800.0, 600.0);
        assert_eq!(rects.len(), 4);
    }

    #[test]
    fn close_non_focused_pane() {
        let mut mgr = PaneManager::new();
        let id1 = mgr.split(SplitDir::Vertical);
        // Focus is on id1, go back to PaneId(0)
        mgr.focus_prev();
        assert_eq!(mgr.focused(), PaneId(0));
        // Focus forward to id1 and close it
        mgr.focus_next();
        assert_eq!(mgr.focused(), id1);
        let closed = mgr.close_focused();
        assert_eq!(closed, Some(id1));
        assert_eq!(mgr.count(), 1);
        assert_eq!(mgr.focused(), PaneId(0));
    }

    #[test]
    fn layout_with_offset() {
        let mgr = PaneManager::new();
        let rects = mgr.layout(50.0, 100.0, 800.0, 600.0);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].x, 50.0);
        assert_eq!(rects[0].y, 100.0);
        assert_eq!(rects[0].width, 800.0);
        assert_eq!(rects[0].height, 600.0);
    }

    #[test]
    fn pane_rect_dimensions() {
        let mut mgr = PaneManager::new();
        mgr.split(SplitDir::Vertical);
        let rects = mgr.layout(0.0, 0.0, 1000.0, 500.0);
        assert_eq!(rects.len(), 2);
        let r0 = &rects[0];
        let r1 = &rects[1];
        assert_eq!(r0.id, PaneId(0));
        assert_eq!(r0.x, 0.0);
        assert_eq!(r0.y, 0.0);
        assert_eq!(r0.width, 500.0);
        assert_eq!(r0.height, 500.0);
        assert_eq!(r1.x, 500.0);
        assert_eq!(r1.y, 0.0);
        assert_eq!(r1.width, 500.0);
        assert_eq!(r1.height, 500.0);
    }

    #[test]
    fn triple_split() {
        let mut mgr = PaneManager::new();
        let id1 = mgr.split(SplitDir::Vertical);
        let id2 = mgr.split(SplitDir::Horizontal);
        assert_eq!(mgr.count(), 3);
        let ids = mgr.all_ids();
        assert!(ids.contains(&PaneId(0)));
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
    }
}
