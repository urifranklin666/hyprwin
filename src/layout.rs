//! Geometry and tiling algorithms.
//!
//! Everything here is pure: given an area and a set of window ids it returns
//! rectangles. That keeps the interesting logic unit-testable without a display.

use crate::config::{Direction, LayoutKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Rect { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }

    /// Shrink on all sides, never below 1x1.
    pub fn inset(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            x: self.x + dx,
            y: self.y + dy,
            w: (self.w - 2 * dx).max(1),
            h: (self.h - 2 * dy).max(1),
        }
    }

    /// Vertical cut: left portion of `ratio`, then the rest.
    pub fn split_h(&self, ratio: f32) -> (Rect, Rect) {
        let left = ((self.w as f32) * ratio).round() as i32;
        let left = left.clamp(1, (self.w - 1).max(1));
        (
            Rect::new(self.x, self.y, left, self.h),
            Rect::new(self.x + left, self.y, (self.w - left).max(1), self.h),
        )
    }

    /// Horizontal cut: top portion of `ratio`, then the rest.
    pub fn split_v(&self, ratio: f32) -> (Rect, Rect) {
        let top = ((self.h as f32) * ratio).round() as i32;
        let top = top.clamp(1, (self.h - 1).max(1));
        (
            Rect::new(self.x, self.y, self.w, top),
            Rect::new(self.x, self.y + top, self.w, (self.h - top).max(1)),
        )
    }

    /// Length of the shared vertical span with `o` (used for left/right neighbours).
    pub fn overlap_v(&self, o: &Rect) -> i32 {
        self.bottom().min(o.bottom()) - self.y.max(o.y)
    }

    /// Length of the shared horizontal span with `o` (used for up/down neighbours).
    pub fn overlap_h(&self, o: &Rect) -> i32 {
        self.right().min(o.right()) - self.x.max(o.x)
    }

    /// Centre point of the rectangle. Unused today; kept alongside the other
    /// geometry helpers rather than re-derived at each call site.
    #[allow(dead_code)]
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// A dwindle (spiral) layout tree.
///
/// Split ratios live in the tree, so resizing one split does not disturb the
/// rest of the layout and windows do not jump when another window closes.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf {
        id: usize,
    },
    Split {
        horizontal: bool,
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct DwindleTree {
    pub root: Option<Node>,
}

const MIN_RATIO: f32 = 0.1;
const MAX_RATIO: f32 = 0.9;

impl DwindleTree {
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn contains(&self, id: usize) -> bool {
        match &self.root {
            Some(n) => Self::contains_in(n, id),
            None => false,
        }
    }

    fn contains_in(node: &Node, id: usize) -> bool {
        match node {
            Node::Leaf { id: i } => *i == id,
            Node::Split { a, b, .. } => Self::contains_in(a, id) || Self::contains_in(b, id),
        }
    }

    pub fn leaf_ids(&self) -> Vec<usize> {
        let mut out = Vec::new();
        if let Some(n) = &self.root {
            Self::collect(n, &mut out);
        }
        out
    }

    fn collect(node: &Node, out: &mut Vec<usize>) {
        match node {
            Node::Leaf { id } => out.push(*id),
            Node::Split { a, b, .. } => {
                Self::collect(a, out);
                Self::collect(b, out);
            }
        }
    }

    /// Insert `id`, splitting the leaf occupied by `focus` if it is present.
    pub fn insert(&mut self, id: usize, focus: Option<usize>) {
        match self.root.take() {
            None => self.root = Some(Node::Leaf { id }),
            Some(mut root) => {
                let target = focus.filter(|f| Self::contains_in(&root, *f));
                let target = target.or_else(|| Self::first_leaf(&root));
                if let Some(t) = target {
                    Self::insert_at(&mut root, id, t, 0);
                }
                self.root = Some(root);
            }
        }
    }

    fn first_leaf(node: &Node) -> Option<usize> {
        match node {
            Node::Leaf { id } => Some(*id),
            Node::Split { a, .. } => Self::first_leaf(a),
        }
    }

    fn insert_at(node: &mut Node, new_id: usize, focus: usize, depth: usize) -> bool {
        match node {
            Node::Leaf { id } => {
                if *id == focus {
                    let horizontal = depth % 2 == 0;
                    *node = Node::Split {
                        horizontal,
                        ratio: 0.5,
                        a: Box::new(Node::Leaf { id: focus }),
                        b: Box::new(Node::Leaf { id: new_id }),
                    };
                    true
                } else {
                    false
                }
            }
            Node::Split { a, b, .. } => {
                if Self::insert_at(a, new_id, focus, depth + 1) {
                    true
                } else {
                    Self::insert_at(b, new_id, focus, depth + 1)
                }
            }
        }
    }

    /// Remove `id`, collapsing its parent split into the surviving sibling.
    pub fn remove(&mut self, id: usize) {
        if let Some(root) = self.root.take() {
            match Self::remove_in(root, id) {
                Some(n) => self.root = Some(n),
                None => self.root = None,
            }
        }
    }

    /// Returns None when the whole subtree disappears, otherwise the (possibly
    /// rewritten) subtree.
    fn remove_in(node: Node, id: usize) -> Option<Node> {
        match node {
            Node::Leaf { id: i } => {
                if i == id {
                    None
                } else {
                    Some(Node::Leaf { id: i })
                }
            }
            Node::Split {
                horizontal,
                ratio,
                a,
                b,
            } => {
                let a = *a;
                let b = *b;
                // Recurse into whichever side actually holds `id`. Testing the
                // side's *membership* rather than the child's shape matters: when
                // both children are splits, choosing by shape sends the search
                // into the wrong subtree, leaves `id` in place and produces a
                // phantom leaf that reserves a slot forever.
                if Self::contains_in(&a, id) {
                    // A side that empties out disappears and its sibling takes
                    // over the whole vacated area, rather than leaving half of
                    // the space empty next to a lone survivor.
                    return match Self::remove_in(a, id) {
                        None => Some(b),
                        Some(a) => Some(Node::Split {
                            horizontal,
                            ratio,
                            a: Box::new(a),
                            b: Box::new(b),
                        }),
                    };
                }
                if Self::contains_in(&b, id) {
                    return match Self::remove_in(b, id) {
                        None => Some(a),
                        Some(b) => Some(Node::Split {
                            horizontal,
                            ratio,
                            a: Box::new(a),
                            b: Box::new(b),
                        }),
                    };
                }
                // Not in this subtree at all; hand it back unchanged.
                Some(Node::Split {
                    horizontal,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                })
            }
        }
    }

    /// Swap the positions of two windows without touching split ratios.
    pub fn swap(&mut self, x: usize, y: usize) {
        if x == y {
            return;
        }
        fn rec(node: &mut Node, x: usize, y: usize) {
            match node {
                Node::Leaf { id } => {
                    if *id == x {
                        *id = y;
                    } else if *id == y {
                        *id = x;
                    }
                }
                Node::Split { a, b, .. } => {
                    rec(a, x, y);
                    rec(b, x, y);
                }
            }
        }
        if let Some(root) = &mut self.root {
            rec(root, x, y);
        }
    }

    /// Adjust the deepest split containing `id`; `delta > 0` grows the first child.
    pub fn resize(&mut self, id: usize, delta: f32) -> bool {
        match &mut self.root {
            Some(root) => Self::resize_in(root, id, delta),
            None => false,
        }
    }

    fn resize_in(node: &mut Node, id: usize, delta: f32) -> bool {
        if let Node::Split { ratio, a, b, .. } = node {
            let in_a = Self::contains_in(a, id);
            let in_b = Self::contains_in(b, id);
            if !in_a && !in_b {
                return false;
            }
            let deeper = if in_a {
                Self::resize_in(a, id, delta)
            } else {
                Self::resize_in(b, id, delta)
            };
            if deeper {
                return true;
            }
            let d = if in_a { delta } else { -delta };
            *ratio = (*ratio + d).clamp(MIN_RATIO, MAX_RATIO);
            return true;
        }
        false
    }

    /// Compute window rectangles for `area`, applying outer and inner gaps.
    pub fn layout(&self, area: Rect, gaps_outer: i32, gaps_inner: i32) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            Self::walk(root, area.inset(gaps_outer, gaps_outer), &mut out);
        }
        if gaps_inner > 0 {
            for (_, r) in out.iter_mut() {
                *r = r.inset(gaps_inner / 2, gaps_inner / 2);
            }
        }
        out
    }

    fn walk(node: &Node, area: Rect, out: &mut Vec<(usize, Rect)>) {
        match node {
            Node::Leaf { id } => out.push((*id, area)),
            Node::Split {
                horizontal,
                ratio,
                a,
                b,
            } => {
                let (ra, rb) = if *horizontal {
                    area.split_h(*ratio)
                } else {
                    area.split_v(*ratio)
                };
                Self::walk(a, ra, out);
                Self::walk(b, rb, out);
            }
        }
    }

    /// Like `layout`, but every split cuts on the same axis, so each window
    /// takes a share of what is left and the panes nest inward instead of
    /// alternating.
    ///
    /// It reuses the tree's stored ratios but ignores its stored `horizontal`
    /// flags: the same tree therefore feeds both `dwindle` and `spiral`, and
    /// switching between them does not reshape the split sizes.
    pub fn layout_spiral(
        &self,
        area: Rect,
        gaps_outer: i32,
        gaps_inner: i32,
    ) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = &self.root {
            Self::walk_spiral(root, area.inset(gaps_outer, gaps_outer), &mut out);
        }
        if gaps_inner > 0 {
            for (_, r) in out.iter_mut() {
                *r = r.inset(gaps_inner / 2, gaps_inner / 2);
            }
        }
        out
    }

    fn walk_spiral(node: &Node, area: Rect, out: &mut Vec<(usize, Rect)>) {
        match node {
            Node::Leaf { id } => out.push((*id, area)),
            Node::Split { ratio, a, b, .. } => {
                // Always the same axis, so the recursion carves nested vertical
                // panes rather than alternating to horizontal ones.
                let (ra, rb) = area.split_h(*ratio);
                Self::walk_spiral(a, ra, out);
                Self::walk_spiral(b, rb, out);
            }
        }
    }
}

/// One dwindle tree per workspace on a monitor.
///
/// Without this, leaving a workspace destroys its tree: the windows are pruned
/// out and re-inserted somewhere arbitrary when you come back. Parking the tree
/// means a window returns to the position it had.
#[derive(Debug, Clone, Default)]
pub struct Forest {
    trees: Vec<Option<Node>>,
}

impl Forest {
    /// Store `tree` for workspace `active` (1-based). The forest grows to fit.
    pub fn activate(&mut self, tree: &DwindleTree, active: u32) {
        let i = active.saturating_sub(1) as usize;
        if self.trees.len() <= i {
            self.trees.resize(i + 1, None);
        }
        self.trees[i] = tree.root.clone();
    }

    /// The tree stored for workspace `active`, or an empty one if never stored.
    pub fn deactivate(&mut self, active: u32) -> DwindleTree {
        let i = active.saturating_sub(1) as usize;
        DwindleTree {
            root: self.trees.get(i).cloned().flatten(),
        }
    }

    /// Drop a window from every parked tree. Used when a window closes.
    pub fn forget(&mut self, id: usize) {
        for slot in self.trees.iter_mut() {
            if let Some(root) = slot.take() {
                let mut tree = DwindleTree { root: Some(root) };
                tree.remove(id);
                *slot = tree.root;
            }
        }
    }

    /// How many workspaces currently hold a parked layout. Used by tests to
    /// prove that switching away from a workspace does not discard it.
    #[allow(dead_code)]
    pub fn workspaces_tracked(&self) -> usize {
        self.trees.iter().filter(|t| t.is_some()).count()
    }
}

/// Master/stack layout: one master pane on the left, the rest stacked right.
///
/// `masters` windows share the master column, stacked vertically among
/// themselves; the remainder stack in a second column. A `masters` of 1 is the
/// classic single-master look.
pub fn tile_master(
    ids: &[usize],
    area: Rect,
    ratio: f32,
    masters: u32,
    gaps_outer: i32,
    gaps_inner: i32,
) -> Vec<(usize, Rect)> {
    let inner = area.inset(gaps_outer, gaps_outer);
    let mut out = Vec::new();
    if ids.is_empty() {
        return out;
    }
    // At least one window must stay in the stack column, or the master column
    // would reserve room next to nothing at all.
    let masters = (masters.max(1) as usize).min(ids.len().saturating_sub(1).max(1));
    if ids.len() == 1 {
        out.push((ids[0], inner));
    } else {
        let (master_area, stack) = inner.split_h(ratio);
        // Stack the masters down the first column; a lone master fills it.
        let master_h = master_area.h as f32 / masters as f32;
        for (i, id) in ids.iter().take(masters).enumerate() {
            let y = master_area.y + (master_h * i as f32).round() as i32;
            let hh = if i == masters - 1 {
                master_area.bottom() - y
            } else {
                master_h.round() as i32
            };
            out.push((*id, Rect::new(master_area.x, y, master_area.w, hh.max(1))));
        }
        let n = ids.len() - masters;
        let h = stack.h as f32 / n as f32;
        for (i, id) in ids.iter().enumerate().skip(masters) {
            let k = i - masters;
            let y = stack.y + (h * k as f32).round() as i32;
            let hh = if i == ids.len() - 1 {
                stack.bottom() - y
            } else {
                h.round() as i32
            };
            out.push((*id, Rect::new(stack.x, y, stack.w, hh.max(1))));
        }
    }
    if gaps_inner > 0 {
        for (_, r) in out.iter_mut() {
            *r = r.inset(gaps_inner / 2, gaps_inner / 2);
        }
    }
    out
}

/// A single column: every window full width, stacked down the work area.
pub fn tile_tall(
    ids: &[usize],
    area: Rect,
    gaps_outer: i32,
    gaps_inner: i32,
) -> Vec<(usize, Rect)> {
    let inner = area.inset(gaps_outer, gaps_outer);
    let mut out = Vec::new();
    if ids.is_empty() {
        return out;
    }
    let n = ids.len();
    let h = inner.h as f32 / n as f32;
    for (i, id) in ids.iter().enumerate() {
        let y = inner.y + (h * i as f32).round() as i32;
        let hh = if i == n - 1 {
            inner.bottom() - y
        } else {
            h.round() as i32
        };
        out.push((*id, Rect::new(inner.x, y, inner.w, hh.max(1))));
    }
    if gaps_inner > 0 {
        for (_, r) in out.iter_mut() {
            *r = r.inset(gaps_inner / 2, gaps_inner / 2);
        }
    }
    out
}

/// An even grid, `ceil(sqrt(n))` columns wide, filled row by row.
pub fn tile_grid(
    ids: &[usize],
    area: Rect,
    gaps_outer: i32,
    gaps_inner: i32,
) -> Vec<(usize, Rect)> {
    let inner = area.inset(gaps_outer, gaps_outer);
    let mut out = Vec::new();
    if ids.is_empty() {
        return out;
    }
    let n = ids.len();
    let cols = (n as f64).sqrt().ceil().max(1.0) as usize;
    let rows = n.div_ceil(cols);
    let ch = inner.h as f32 / rows as f32;
    for (i, id) in ids.iter().enumerate() {
        let r = i / cols;
        let c = i % cols;
        // The final row can be short. Let its windows share the full width
        // rather than leave a hole where the missing columns would have been,
        // which also means the grid always reaches the far edge.
        let in_row = (n - r * cols).min(cols);
        let cw = inner.w as f32 / in_row as f32;
        let x = inner.x + (cw * c as f32).round() as i32;
        let w = if c == in_row - 1 {
            inner.right() - x
        } else {
            cw.round() as i32
        };
        let y = inner.y + (ch * r as f32).round() as i32;
        let h = if r == rows - 1 {
            inner.bottom() - y
        } else {
            ch.round() as i32
        };
        out.push((*id, Rect::new(x, y, w.max(1), h.max(1))));
    }
    if gaps_inner > 0 {
        for (_, r) in out.iter_mut() {
            *r = r.inset(gaps_inner / 2, gaps_inner / 2);
        }
    }
    out
}

/// Every window fills the work area, in `ids` order.
///
/// The last id in `ids` is the one to raise: `retile_monitor` moves windows in
/// order, so the final `SetWindowPos` is the topmost.
pub fn tile_stacking(ids: &[usize], area: Rect, gaps_outer: i32) -> Vec<(usize, Rect)> {
    let inner = area.inset(gaps_outer, gaps_outer);
    ids.iter().map(|id| (*id, inner)).collect()
}

/// Dispatch to the configured algorithm.
///
/// `order` is the master/stack order for `master`; the dwindle tree supplies
/// every other arrangement. `masters` is `master_count` and is ignored by the
/// other kinds.
// The parameters are the whole tiling context; bundling them into a struct for
// one dispatcher would only move the same fields somewhere else.
#[allow(clippy::too_many_arguments)]
pub fn compute(
    kind: LayoutKind,
    tree: &DwindleTree,
    order: &[usize],
    area: Rect,
    ratio: f32,
    masters: u32,
    gaps_outer: i32,
    gaps_inner: i32,
) -> Vec<(usize, Rect)> {
    match kind {
        LayoutKind::Dwindle => tree.layout(area, gaps_outer, gaps_inner),
        LayoutKind::Master => tile_master(order, area, ratio, masters, gaps_outer, gaps_inner),
        LayoutKind::Spiral => tree.layout_spiral(area, gaps_outer, gaps_inner),
        LayoutKind::Tall => tile_tall(order, area, gaps_outer, gaps_inner),
        LayoutKind::Grid => tile_grid(order, area, gaps_outer, gaps_inner),
        LayoutKind::Stacking => tile_stacking(order, area, gaps_outer),
    }
}

/// Geometric neighbour search, the same approach Hyprland uses for focus moves.
pub fn neighbor(rects: &[(usize, Rect)], focus: usize, dir: Direction) -> Option<usize> {
    let f = rects.iter().find(|(id, _)| *id == focus)?.1;
    let mut best: Option<(i32, i32, usize)> = None;
    for (id, r) in rects {
        if *id == focus {
            continue;
        }
        let (dist, ov) = match dir {
            Direction::Left => {
                if r.right() > f.x + 2 {
                    continue;
                }
                (f.x - r.right(), f.overlap_v(r))
            }
            Direction::Right => {
                if r.x < f.right() - 2 {
                    continue;
                }
                (r.x - f.right(), f.overlap_v(r))
            }
            Direction::Up => {
                if r.bottom() > f.y + 2 {
                    continue;
                }
                (f.y - r.bottom(), f.overlap_h(r))
            }
            Direction::Down => {
                if r.y < f.bottom() - 2 {
                    continue;
                }
                (r.y - f.bottom(), f.overlap_h(r))
            }
        };
        let better = match best {
            None => true,
            Some((bd, bo, _)) => ov > bo || (ov == bo && dist < bd),
        };
        if better && ov > 0 {
            best = Some((dist, ov, *id));
        }
    }
    best.map(|(_, _, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: Rect = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
    };

    fn no_gaps() -> (i32, i32) {
        (0, 0)
    }

    #[test]
    fn single_window_fills_area() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, FULL);
    }

    #[test]
    fn two_windows_split_in_half() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1.w, 960);
        assert_eq!(r[1].1.w, 960);
        assert_eq!(r[1].1.x, 960);
        assert_eq!(r[0].1.h, 1080);
    }

    #[test]
    fn three_windows_spiral_alternates_axis() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r.len(), 3);
        // Depth 0 is a vertical cut, depth 1 splits the right half horizontally.
        let by_id = |id: usize| r.iter().find(|(i, _)| *i == id).unwrap().1;
        assert_eq!(by_id(1), Rect::new(0, 0, 960, 1080));
        assert_eq!(by_id(2), Rect::new(960, 0, 960, 540));
        assert_eq!(by_id(3), Rect::new(960, 540, 960, 540));
    }

    #[test]
    fn tiles_never_overlap() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        for i in 2..=6 {
            t.insert(i, Some(i - 1));
        }
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r.len(), 6);
        for i in 0..r.len() {
            for j in (i + 1)..r.len() {
                let a = r[i].1;
                let b = r[j].1;
                let ox = a.overlap_h(&b);
                let oy = a.overlap_v(&b);
                assert!(ox <= 0 || oy <= 0, "windows {i} and {j} overlap");
            }
        }
    }

    #[test]
    fn tiles_stay_inside_the_work_area() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        for i in 2..=5 {
            t.insert(i, Some(i - 1));
        }
        for (_, r) in t.layout(FULL, 8, 6) {
            assert!(r.x >= 8 && r.y >= 8);
            assert!(r.right() <= FULL.right() - 8);
            assert!(r.bottom() <= FULL.bottom() - 8);
            assert!(!r.is_empty());
        }
    }

    #[test]
    fn removing_a_window_collapses_without_moving_others() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        let before = t.layout(FULL, 0, 0);
        let one_before = before.iter().find(|(i, _)| *i == 1).unwrap().1;
        t.remove(3);
        let after = t.layout(FULL, 0, 0);
        assert_eq!(after.len(), 2);
        let one_after = after.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert_eq!(one_before, one_after, "unrelated window must not move");
        assert!(t.contains(2));
    }

    #[test]
    fn removing_the_last_window_empties_the_tree() {
        let mut t = DwindleTree::default();
        t.insert(7, None);
        t.remove(7);
        assert!(t.is_empty());
        assert!(t.layout(FULL, 0, 0).is_empty());
    }

    #[test]
    fn the_survivor_takes_over_the_whole_vacated_area() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.remove(1);
        // The root split has one child left, so window 2 must expand into the
        // whole area instead of keeping its half and leaving the rest empty.
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].0, 2);
        assert_eq!(r[0].1, FULL);
    }

    #[test]
    fn remove_reaches_a_leaf_behind_a_sibling_split() {
        // Insert four windows so removing the last one has to recurse past a
        // sibling that is itself a split. Choosing the child by shape rather
        // than membership used to leave window 3 in the tree as a phantom leaf
        // that reserved a slot and left a hole in the layout.
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        t.insert(4, Some(3));
        assert_eq!(t.leaf_ids().len(), 4);
        t.remove(3);
        assert!(!t.contains(3), "the removed window must leave the tree");
        assert_eq!(t.leaf_ids().len(), 3, "no phantom leaf may remain");
        assert_eq!(t.layout(FULL, 0, 0).len(), 3);
    }

    #[test]
    fn remove_is_a_noop_for_an_absent_id() {
        // Every removal from a populated tree still visits a case where `id` is
        // not present, so the recursive walk must keep such subtrees intact.
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        t.insert(4, Some(1));
        let before = t.clone();
        t.remove(99);
        assert_eq!(t.root, before.root, "removing an absent id changes nothing");
    }

    #[test]
    fn removing_every_window_in_turn_leaves_nothing_behind() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        for i in 2..=6 {
            t.insert(i, Some(i - 1));
        }
        for i in 1..=6 {
            t.remove(i);
        }
        assert!(t.is_empty(), "an emptied tree must have no survivors");
        assert!(t.layout(FULL, 0, 0).is_empty());
    }

    #[test]
    fn pruning_a_minimized_leaf_reflows_and_restoring_it_returns_the_slot() {
        // The retiler keeps minimized windows in the tree and lays out a pruned
        // copy. Pruning must expand the survivors; restoring must put the
        // window back exactly where it was, not splice it in as a new split.
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        let before = t.layout(FULL, 0, 0);
        let chatgpt = before.iter().find(|(i, _)| *i == 2).unwrap().1;
        assert_eq!(chatgpt.h, 540, "the inner split starts even");

        let mut pruned = t.clone();
        pruned.remove(2);
        let during = pruned.layout(FULL, 0, 0);
        assert!(
            during.iter().all(|(i, _)| *i != 2),
            "the minimized window must not be placed at all"
        );
        let one_during = during.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert_eq!(
            one_during,
            Rect::new(0, 0, 960, 1080),
            "window 1 keeps its column while 3 reflows within the other"
        );

        let after = t.layout(FULL, 0, 0);
        assert_eq!(
            after, before,
            "restoring must reproduce the original layout"
        );
    }

    #[test]
    fn insert_with_unknown_focus_still_lands_somewhere() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(999));
        assert_eq!(t.leaf_ids().len(), 2);
    }

    #[test]
    fn resize_changes_only_the_deepest_split() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        // Window 3 lives in the second child of the inner split, so a positive
        // delta must grow window 3 and shrink its sibling window 2.
        assert!(t.resize(3, 0.2));
        let r = t.layout(FULL, 0, 0);
        let by_id = |id: usize| r.iter().find(|(i, _)| *i == id).unwrap().1;
        // Window 1's outer split is untouched...
        assert_eq!(by_id(1).w, 960);
        // ...while the inner split grew window 3's share at window 2's expense.
        assert!(by_id(3).h > 540, "the resized window grows");
        assert!(by_id(2).h < 540, "its sibling gives up the space");
        assert_eq!(
            by_id(2).h + by_id(3).h,
            1080,
            "the split still fills the area"
        );
    }

    #[test]
    fn resize_grows_the_first_child_of_a_split() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        // Window 1 is the first child of the root split.
        assert!(t.resize(1, 0.1));
        let r = t.layout(FULL, 0, 0);
        let by_id = |id: usize| r.iter().find(|(i, _)| *i == id).unwrap().1;
        assert!(by_id(1).w > 960);
        assert!(by_id(2).w < 960);
    }

    #[test]
    fn resize_is_clamped() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        for _ in 0..100 {
            t.resize(1, 0.5);
        }
        let r = t.layout(FULL, 0, 0);
        let one = r.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert!(one.w <= 1728, "ratio must clamp at 0.9 of 1920");
    }

    #[test]
    fn resize_with_unknown_id_is_a_noop() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        assert!(!t.resize(42, 0.3));
    }

    #[test]
    fn swap_exchanges_positions() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.swap(1, 2);
        let r = t.layout(FULL, 0, 0);
        assert_eq!(r[0].0, 2);
        assert_eq!(r[1].0, 1);
    }

    #[test]
    fn master_layout_puts_one_window_full_width() {
        let r = tile_master(&[1], FULL, 0.6, 1, 0, 0);
        assert_eq!(r[0].1, FULL);
    }

    #[test]
    fn master_layout_stacks_the_remainder() {
        let r = tile_master(&[1, 2, 3], FULL, 0.6, 1, 0, 0);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].1.w, 1152);
        assert_eq!(r[1].1.x, 1152);
        assert_eq!(r[1].1.h, 540);
        assert_eq!(r[2].1.y, 540);
        assert_eq!(r[2].1.bottom(), 1080);
    }

    #[test]
    fn two_masters_split_the_first_column_between_them() {
        let r = tile_master(&[1, 2, 3], FULL, 0.5, 2, 0, 0);
        // Window 1 and 2 share the master column (0..960), window 3 takes the
        // whole stack column.
        assert_eq!(r[0].1, Rect::new(0, 0, 960, 540));
        assert_eq!(r[1].1, Rect::new(0, 540, 960, 540));
        assert_eq!(r[2].1, Rect::new(960, 0, 960, 1080));
    }

    #[test]
    fn more_masters_than_windows_never_empties_the_stack() {
        // Asking for 4 masters with 2 windows must clamp to 2, never reserving a
        // master column with nothing in it.
        let r = tile_master(&[1, 2], FULL, 0.5, 4, 0, 0);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].1.w, 960);
        assert_eq!(r[1].1.w, 960);
        assert_eq!(r[1].1.x, 960);
    }

    #[test]
    fn tall_layout_is_one_full_width_column() {
        let r = tile_tall(&[1, 2, 3], FULL, 0, 0);
        assert_eq!(r.len(), 3);
        for (_, rect) in &r {
            assert_eq!(rect.x, 0);
            assert_eq!(rect.w, 1920, "every window spans the full width");
        }
        assert_eq!(r[0].1.h, 360);
        assert_eq!(r[2].1.bottom(), 1080, "the last window reaches the bottom");
    }

    #[test]
    fn grid_uses_a_square_column_count_and_covers_the_area() {
        // Four windows make a 2x2 grid.
        let r = tile_grid(&[1, 2, 3, 4], FULL, 0, 0);
        assert_eq!(r.len(), 4);
        assert_eq!(r[0].1, Rect::new(0, 0, 960, 540));
        assert_eq!(r[3].1, Rect::new(960, 540, 960, 540));
        // Five windows make a 3-wide grid, so the second row is partly empty and
        // the last placed cell still reaches the far edge.
        let r = tile_grid(&[1, 2, 3, 4, 5], FULL, 0, 0);
        assert_eq!(r.len(), 5);
        assert_eq!(r[4].1.right(), 1920, "the grid reaches the right edge");
    }

    #[test]
    fn stacking_puts_every_window_at_the_same_rect() {
        let r = tile_stacking(&[1, 2, 3], FULL, 0);
        assert_eq!(r.len(), 3);
        assert!(r.iter().all(|(_, rect)| *rect == FULL));
    }

    #[test]
    fn spiral_cuts_every_split_on_the_same_axis() {
        // The dwindle tree alternates: window 3 sits to the right of window 2 and
        // below it. Spiral keeps cutting horizontally, so window 3 stays to the
        // right instead — a visibly different arrangement from the same tree.
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        let by_id = |r: Vec<(usize, Rect)>, id: usize| r.iter().find(|(i, _)| *i == id).unwrap().1;
        let dwindle = t.layout(FULL, 0, 0);
        let spiral = t.layout_spiral(FULL, 0, 0);
        assert_eq!(by_id(dwindle.clone(), 2), Rect::new(960, 0, 960, 540));
        // Spiral: window 1 half, window 2 half of the remainder, window 3 the rest.
        assert_eq!(by_id(spiral.clone(), 2).h, 1080, "no vertical cut is made");
        assert_eq!(by_id(spiral, 3).x, 1440, "window 3 sits fully to the right");
        // Both place every window and stay in bounds.
        for kind in [dwindle, t.layout_spiral(FULL, 8, 6)] {
            assert_eq!(kind.len(), 3);
            for (_, r) in kind {
                assert!(!r.is_empty());
            }
        }
    }

    #[test]
    fn master_layout_covers_the_whole_area() {
        let r = tile_master(&[1, 2, 3, 4], FULL, 0.5, 1, 8, 6);
        for (_, rect) in &r {
            assert!(!rect.is_empty());
            assert!(rect.x >= 8 && rect.y >= 8);
            assert!(rect.right() <= FULL.right() - 8);
            assert!(rect.bottom() <= FULL.bottom() - 8);
        }
    }

    #[test]
    fn neighbor_finds_the_window_to_the_right() {
        let rects = vec![
            (1, Rect::new(0, 0, 960, 1080)),
            (2, Rect::new(960, 0, 960, 1080)),
        ];
        assert_eq!(neighbor(&rects, 1, Direction::Right), Some(2));
        assert_eq!(neighbor(&rects, 2, Direction::Left), Some(1));
    }

    #[test]
    fn neighbor_finds_the_window_below_in_master_layout() {
        let rects = vec![
            (1, Rect::new(0, 0, 1152, 1080)),
            (2, Rect::new(1152, 0, 768, 540)),
            (3, Rect::new(1152, 540, 768, 540)),
        ];
        assert_eq!(neighbor(&rects, 2, Direction::Down), Some(3));
        assert_eq!(neighbor(&rects, 3, Direction::Up), Some(2));
        assert_eq!(neighbor(&rects, 3, Direction::Left), Some(1));
    }

    #[test]
    fn neighbor_returns_none_at_the_edge() {
        let rects = vec![
            (1, Rect::new(0, 0, 960, 1080)),
            (2, Rect::new(960, 0, 960, 1080)),
        ];
        assert_eq!(neighbor(&rects, 1, Direction::Left), None);
        assert_eq!(neighbor(&rects, 2, Direction::Right), None);
    }

    #[test]
    fn gaps_shrink_the_usable_area_symmetrically() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        let r = t.layout(FULL, 10, 0);
        assert_eq!(r[0].1, Rect::new(10, 10, 1900, 1060));
    }

    #[test]
    fn compute_dispatches_on_layout_kind() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        let (go, gi) = no_gaps();
        let dwindle = compute(LayoutKind::Dwindle, &t, &[1, 2], FULL, 0.6, 1, go, gi);
        let master = compute(LayoutKind::Master, &t, &[1, 2], FULL, 0.6, 1, go, gi);
        assert_eq!(dwindle.len(), 2);
        assert_eq!(master[0].1.w, 1152);
        // Every kind must place the same ids exactly once and stay in bounds, so
        // a caller can switch between them without special-casing one.
        for kind in LayoutKind::CYCLE {
            let r = compute(kind, &t, &[1, 2], FULL, 0.6, 1, go, gi);
            assert_eq!(r.len(), 2, "{kind:?} must place both windows");
            for (_, rect) in &r {
                assert!(!rect.is_empty(), "{kind:?} produced an empty rect");
                assert!(rect.x >= 0 && rect.y >= 0);
                assert!(rect.right() <= FULL.right() && rect.bottom() <= FULL.bottom());
            }
        }
    }

    #[test]
    fn forest_remembers_a_workspace_layout() {
        let mut t = DwindleTree::default();
        t.insert(1, None);
        t.insert(2, Some(1));
        t.insert(3, Some(2));
        // Grow the inner split so the restored tree is distinguishable.
        t.resize(3, 0.2);

        let mut forest = Forest::default();
        forest.activate(&t, 1);
        let restored = forest.deactivate(1);
        assert_eq!(
            restored.root, t.root,
            "ws 1 must come back exactly as it was"
        );

        let rects = restored.layout(FULL, 0, 0);
        let one = rects.iter().find(|(i, _)| *i == 1).unwrap().1;
        assert_eq!(
            one.w, 960,
            "the preserved split ratio survives the round trip"
        );
    }

    #[test]
    fn forest_keeps_workspaces_separate() {
        let mut a = DwindleTree::default();
        a.insert(10, None);
        let mut b = DwindleTree::default();
        b.insert(20, None);
        b.insert(21, Some(20));

        let mut forest = Forest::default();
        forest.activate(&a, 1);
        forest.activate(&b, 3);

        assert_eq!(forest.deactivate(1).leaf_ids(), vec![10]);
        assert_eq!(forest.deactivate(3).leaf_ids(), vec![20, 21]);
        assert_eq!(forest.workspaces_tracked(), 2);
    }

    #[test]
    fn forest_returns_empty_for_an_untouched_workspace() {
        let mut forest = Forest::default();
        let mut t = DwindleTree::default();
        t.insert(1, None);
        forest.activate(&t, 2);
        assert!(forest.deactivate(5).is_empty());
        assert!(
            forest.deactivate(0).is_empty(),
            "workspace 0 must not panic"
        );
    }

    #[test]
    fn forest_forget_drops_a_closed_window_everywhere() {
        let mut a = DwindleTree::default();
        a.insert(1, None);
        a.insert(2, Some(1));
        let mut b = DwindleTree::default();
        b.insert(2, None);
        b.insert(3, Some(2));

        let mut forest = Forest::default();
        forest.activate(&a, 1);
        forest.activate(&b, 2);
        forest.forget(2);

        assert_eq!(forest.deactivate(1).leaf_ids(), vec![1]);
        assert_eq!(forest.deactivate(2).leaf_ids(), vec![3]);
    }

    #[test]
    fn forest_overwriting_a_workspace_replaces_the_layout() {
        let mut forest = Forest::default();
        let mut first = DwindleTree::default();
        first.insert(1, None);
        first.insert(2, Some(1));
        forest.activate(&first, 1);

        let mut second = DwindleTree::default();
        second.insert(9, None);
        forest.activate(&second, 1);

        assert_eq!(forest.deactivate(1).leaf_ids(), vec![9]);
    }
}
