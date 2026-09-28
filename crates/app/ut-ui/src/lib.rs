//! `ut-ui`: toolkit-agnostic UI layout models.
//!
//! A real GUI toolkit (GTK, a custom wgpu-based renderer, whatever gets
//! chosen) sits above this crate and turns its output into actual pixels
//! and event handlers; nothing here imports a toolkit. Implemented so far:
//! [`Layout`], the pane split tree behind Section 4 "Panes" — horizontal/
//! vertical/nested splits, resizing, closing, and zoom — as pure data with
//! no rendering dependency, so `ut-session::Tab` can eventually hold one of
//! these instead of (or alongside) its flat pane list, and any renderer can
//! turn it into rectangles via [`Layout::rects`].
//!
//! Windows/tabs chrome, the command palette, and everything else under
//! Sections 4/20 that's genuinely toolkit-specific is future work.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Children side by side (a vertical divider line between them).
    Horizontal,
    /// Children stacked (a horizontal divider line between them).
    Vertical,
}

pub type PaneId = u64;

/// A pane split tree: either one pane, or a directional split of two or
/// more children, each with a ratio of the available space (ratios within
/// one `Split` always sum to 1.0).
#[derive(Debug, Clone, PartialEq)]
pub enum Layout {
    Leaf(PaneId),
    Split { direction: Direction, children: Vec<(Layout, f32)> },
}

/// An axis-aligned rectangle in whatever units the caller's `x/y/w/h` were in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Layout {
    pub fn leaf(id: PaneId) -> Self {
        Layout::Leaf(id)
    }

    /// Does this (sub)tree contain `id`?
    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Layout::Leaf(leaf_id) => *leaf_id == id,
            Layout::Split { children, .. } => children.iter().any(|(c, _)| c.contains(id)),
        }
    }

    /// Split the leaf `target` into two panes (`target` and `new_pane`),
    /// arranged along `direction`, each taking half the space `target` had.
    /// Returns `false` if `target` wasn't found.
    pub fn split(&mut self, target: PaneId, new_pane: PaneId, direction: Direction) -> bool {
        match self {
            Layout::Leaf(id) if *id == target => {
                *self = Layout::Split { direction, children: vec![(Layout::Leaf(target), 0.5), (Layout::Leaf(new_pane), 0.5)] };
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { children, .. } => children.iter_mut().any(|(c, _)| c.split(target, new_pane, direction)),
        }
    }

    /// Remove pane `target` from the tree, collapsing single-child splits
    /// and renormalizing sibling ratios. Returns `None` if the whole tree
    /// was just that one pane (nothing left — the caller must close the tab).
    pub fn close(self, target: PaneId) -> Option<Layout> {
        match self {
            Layout::Leaf(id) if id == target => None,
            Layout::Leaf(id) => Some(Layout::Leaf(id)),
            Layout::Split { direction, children } => {
                let mut kept: Vec<(Layout, f32)> = Vec::new();
                for (child, ratio) in children {
                    if let Some(c) = child.close(target) {
                        kept.push((c, ratio));
                    }
                }
                match kept.len() {
                    0 => None,
                    1 => Some(kept.into_iter().next().unwrap().0),
                    _ => {
                        let total: f32 = kept.iter().map(|(_, r)| r).sum();
                        let children = kept.into_iter().map(|(c, r)| (c, r / total)).collect();
                        Some(Layout::Split { direction, children })
                    }
                }
            }
        }
    }

    /// Set `target`'s ratio within its immediate parent split to
    /// `new_ratio` (clamped to a sane minimum), shrinking/growing its
    /// siblings proportionally so the split still sums to 1.0.
    pub fn resize(&mut self, target: PaneId, new_ratio: f32) -> bool {
        let new_ratio = new_ratio.clamp(0.05, 0.95);
        if let Layout::Split { children, .. } = self {
            if let Some(idx) = children.iter().position(|(c, _)| matches!(c, Layout::Leaf(id) if *id == target)) {
                let old_ratio = children[idx].1;
                let remaining_old: f32 = children.iter().enumerate().filter(|(i, _)| *i != idx).map(|(_, (_, r))| r).sum();
                let remaining_new = 1.0 - new_ratio;
                children[idx].1 = new_ratio;
                if remaining_old > 0.0 {
                    for (i, (_, r)) in children.iter_mut().enumerate() {
                        if i != idx {
                            *r = *r / remaining_old * remaining_new;
                        }
                    }
                }
                let _ = old_ratio;
                return true;
            }
            for (child, _) in children.iter_mut() {
                if child.resize(target, new_ratio) {
                    return true;
                }
            }
        }
        false
    }

    /// Compute each leaf's on-screen rectangle within the container `bounds`.
    pub fn rects(&self, bounds: Rect) -> Vec<(PaneId, Rect)> {
        match self {
            Layout::Leaf(id) => vec![(*id, bounds)],
            Layout::Split { direction, children } => {
                let mut out = Vec::new();
                let mut offset = 0.0;
                for (child, ratio) in children {
                    let child_bounds = match direction {
                        Direction::Horizontal => Rect { x: bounds.x + offset * bounds.w, y: bounds.y, w: ratio * bounds.w, h: bounds.h },
                        Direction::Vertical => Rect { x: bounds.x, y: bounds.y + offset * bounds.h, w: bounds.w, h: ratio * bounds.h },
                    };
                    out.extend(child.rects(child_bounds));
                    offset += ratio;
                }
                out
            }
        }
    }

    pub fn leaves(&self) -> Vec<PaneId> {
        match self {
            Layout::Leaf(id) => vec![*id],
            Layout::Split { children, .. } => children.iter().flat_map(|(c, _)| c.leaves()).collect(),
        }
    }
}

/// A layout plus optional "zoomed" state: while zoomed, [`effective_layout`]
/// returns a single-leaf layout for the zoomed pane, but the real
/// multi-pane `layout` is preserved underneath for when it's un-zoomed
/// ("Zoom current pane" / "Maximize current pane").
///
/// [`effective_layout`]: LayoutState::effective_layout
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutState {
    pub layout: Layout,
    pub zoomed: Option<PaneId>,
}

impl LayoutState {
    pub fn new(layout: Layout) -> Self {
        LayoutState { layout, zoomed: None }
    }

    pub fn zoom(&mut self, pane: PaneId) -> bool {
        if self.layout.contains(pane) {
            self.zoomed = Some(pane);
            true
        } else {
            false
        }
    }

    pub fn unzoom(&mut self) {
        self.zoomed = None;
    }

    pub fn effective_layout(&self) -> Layout {
        match self.zoomed {
            Some(id) if self.layout.contains(id) => Layout::Leaf(id),
            _ => self.layout.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_turns_leaf_into_two_panes() {
        let mut layout = Layout::leaf(1);
        assert!(layout.split(1, 2, Direction::Horizontal));
        assert_eq!(layout.leaves(), vec![1, 2]);
    }

    #[test]
    fn split_targets_nested_leaf() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        assert!(layout.split(2, 3, Direction::Vertical));
        assert_eq!(layout.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn split_missing_target_returns_false() {
        let mut layout = Layout::leaf(1);
        assert!(!layout.split(99, 2, Direction::Horizontal));
    }

    #[test]
    fn close_last_pane_returns_none() {
        let layout = Layout::leaf(1);
        assert_eq!(layout.close(1), None);
    }

    #[test]
    fn close_collapses_two_child_split() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        let result = layout.close(2).unwrap();
        assert_eq!(result, Layout::Leaf(1));
    }

    #[test]
    fn close_renormalizes_remaining_ratios() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        layout.split(2, 3, Direction::Horizontal);
        // now: Split[ (1, .5), Split[(2,.5),(3,.5)] * .5 ]
        let result = layout.close(1).unwrap();
        // whatever remains should sum ratios to ~1.0 at top level
        if let Layout::Split { children, .. } = &result {
            let total: f32 = children.iter().map(|(_, r)| r).sum();
            assert!((total - 1.0).abs() < 0.001);
        }
        assert_eq!(result.leaves(), vec![2, 3]);
    }

    #[test]
    fn resize_adjusts_target_and_siblings() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        assert!(layout.resize(1, 0.8));
        if let Layout::Split { children, .. } = &layout {
            assert!((children[0].1 - 0.8).abs() < 0.001);
            assert!((children[1].1 - 0.2).abs() < 0.001);
        } else {
            panic!("expected split");
        }
    }

    #[test]
    fn rects_split_horizontal_divides_width() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        let rects = layout.rects(Rect { x: 0.0, y: 0.0, w: 100.0, h: 50.0 });
        let r1 = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        let r2 = rects.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(r1.w, 50.0);
        assert_eq!(r2.w, 50.0);
        assert_eq!(r1.x, 0.0);
        assert_eq!(r2.x, 50.0);
        assert_eq!(r1.h, 50.0);
    }

    #[test]
    fn rects_split_vertical_divides_height() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Vertical);
        let rects = layout.rects(Rect { x: 0.0, y: 0.0, w: 100.0, h: 40.0 });
        let r1 = rects.iter().find(|(id, _)| *id == 1).unwrap().1;
        let r2 = rects.iter().find(|(id, _)| *id == 2).unwrap().1;
        assert_eq!(r1.h, 20.0);
        assert_eq!(r2.h, 20.0);
        assert_eq!(r2.y, 20.0);
    }

    #[test]
    fn zoom_shows_single_pane_layout() {
        let mut layout = Layout::leaf(1);
        layout.split(1, 2, Direction::Horizontal);
        let mut state = LayoutState::new(layout);
        assert!(state.zoom(2));
        assert_eq!(state.effective_layout(), Layout::Leaf(2));
        state.unzoom();
        assert_eq!(state.effective_layout().leaves(), vec![1, 2]);
    }

    #[test]
    fn zoom_unknown_pane_fails() {
        let layout = Layout::leaf(1);
        let mut state = LayoutState::new(layout);
        assert!(!state.zoom(99));
        assert!(state.zoomed.is_none());
    }
}
