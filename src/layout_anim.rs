//! Smooth splits and closes. The Electron app's grid jumps on both; here a
//! split grows the new pane out of its sibling and a close shrinks the pane
//! away, on the focus-mode curve (`cubic-bezier(0.22, 1, 0.36, 1)`).
//!
//! It all happens in the render layer: the persisted layout tree is never
//! touched. Each frame, [`LayoutAnim::observe`] compares the active
//! workspace's tree with the one drawn last frame. A change that is exactly
//! one leaf split (whoever made it: ⌘D, the header buttons, an agent's
//! `open --split`) or exactly one leaf removed starts a tween of the split
//! ratio involved; anything else — a workspace switch, the first frame after
//! startup, a swap, several panes closing at once — just shows the new
//! layout. A close keeps drawing the tree as it was (the "ghost") with the
//! closed pane as a blank tile whose share eases to 0; the pane itself is gone
//! at once (its shell is shut down as before).

use std::time::{Duration, Instant};

use crate::model::layout::{LayoutNode, NodePath};

/// A split: the new pane grows from nothing to its share.
pub const SPLIT_ANIM: Duration = Duration::from_millis(200);
/// A close: the pane's share eases to nothing.
pub const CLOSE_ANIM: Duration = Duration::from_millis(180);

/// The focus-mode curve (grid.rs FOCUS_ANIM).
fn curve(t: f32) -> f32 {
    crate::anim::cubic_bezier(0.22, 1., 0.36, 1., t)
}

/// The ratio range splits are drawn with (grid.rs clamps the same way).
pub fn shown_ratio(ratio: f32) -> f32 {
    ratio.clamp(0.1, 0.9)
}

/// One split's ratio easing between two values.
#[derive(Clone, Debug, PartialEq)]
pub struct RatioTween {
    pub path: NodePath,
    pub from: f32,
    pub to: f32,
    pub start: Instant,
    pub duration: Duration,
}

impl RatioTween {
    fn progress(&self, now: Instant) -> f32 {
        let t = now.saturating_duration_since(self.start).as_secs_f32() / self.duration.as_secs_f32();
        t.clamp(0., 1.)
    }

    pub fn value(&self, now: Instant) -> f32 {
        // Exact at both ends (the bisected curve is only close to 0 and 1).
        let e = match self.progress(now) {
            p if p <= 0. => 0.,
            p if p >= 1. => 1.,
            p => curve(p),
        };
        self.from + (self.to - self.from) * e
    }

    pub fn done(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.start) >= self.duration
    }
}

/// What changed between two frames' trees, when it's worth animating.
#[derive(Clone, Debug, PartialEq)]
pub enum Change {
    /// A leaf became the split at `path` (in the new tree); the new pane is
    /// its second child when `new_second`.
    Split { path: NodePath, new_second: bool },
    /// The leaf `id` was removed; in the old tree its parent split is at
    /// `path` and it was the second child when `second`.
    Close { id: String, path: NodePath, second: bool },
}

/// Where a leaf is in the tree.
pub fn leaf_path(node: &LayoutNode, id: &str) -> Option<NodePath> {
    match node {
        LayoutNode::Leaf(l) => (l.container_id == id).then(Vec::new),
        LayoutNode::Split(s) => [(false, &s.first), (true, &s.second)].into_iter().find_map(|(side, child)| {
            let mut p = leaf_path(child, id)?;
            p.insert(0, side);
            Some(p)
        }),
    }
}

/// The one id in `more` that `fewer` lacks, when `more` is `fewer` plus exactly one.
fn one_added(fewer: &[String], more: &[String]) -> Option<String> {
    if more.len() != fewer.len() + 1 || !fewer.iter().all(|id| more.contains(id)) {
        return None;
    }
    more.iter().find(|id| !fewer.contains(id)).cloned()
}

/// Recognise a single split or a single close: `new` must be `old` with one
/// leaf split (or `old` with one leaf removed) and nothing else changed.
pub fn detect(old: &LayoutNode, new: &LayoutNode) -> Option<Change> {
    let (old_ids, new_ids) = (old.leaf_ids(), new.leaf_ids());
    if let Some(added) = one_added(&old_ids, &new_ids) {
        if new.clone().remove_leaf(&added).as_ref() != Some(old) {
            return None;
        }
        let mut path = leaf_path(new, &added)?;
        let new_second = path.pop()?;
        return Some(Change::Split { path, new_second });
    }
    if let Some(removed) = one_added(&new_ids, &old_ids) {
        if old.clone().remove_leaf(&removed).as_ref() != Some(new) {
            return None;
        }
        let mut path = leaf_path(old, &removed)?;
        let second = path.pop()?;
        return Some(Change::Close { id: removed, path, second });
    }
    None
}

/// A close in progress: the tree as it was, drawn until the tween ends.
#[derive(Clone, Debug)]
struct Ghost {
    tree: LayoutNode,
    closing: String,
}

#[derive(Default)]
pub struct LayoutAnim {
    /// The workspace and tree drawn last frame.
    seen: Option<(String, Option<LayoutNode>)>,
    tweens: Vec<RatioTween>,
    ghost: Option<Ghost>,
    /// The terminals are holding their size (see `TerminalView::set_resize_hold`).
    held: bool,
}

impl crate::app::AppView {
    /// Once per frame, before the grid is drawn: pick up a split or close in
    /// the active workspace, keep frames coming while it plays, and hold the
    /// terminals' grid size meanwhile so a tween costs one PTY resize (when it
    /// settles), not one per frame.
    pub(crate) fn settle_layout_anim(&mut self, window: &mut gpui_kit::Window, cx: &mut gpui_kit::Context<Self>) {
        let ws = self.state.active();
        let (id, tree) = (ws.map(|w| w.id.clone()), ws.and_then(|w| w.layout_tree.clone()));
        let moving = self.layout_anim.observe(id.as_deref(), tree.as_ref(), Instant::now());
        if moving {
            window.request_animation_frame();
        }
        // Panes made this frame (the split's new one) are held too.
        if moving || self.layout_anim.held {
            for pane in self.panes.values() {
                pane.view.update(cx, |v, _| v.set_resize_hold(moving));
            }
        }
        self.layout_anim.held = moving;
    }
}

impl LayoutAnim {
    /// Once per frame, before the grid is drawn: start a tween when the active
    /// workspace's tree was split or lost a leaf since last frame, and drop
    /// finished ones. True while anything is still moving.
    pub fn observe(&mut self, ws_id: Option<&str>, tree: Option<&LayoutNode>, now: Instant) -> bool {
        let same_ws = matches!((&self.seen, ws_id), (Some((seen, _)), Some(id)) if seen == id);
        if !same_ws {
            self.tweens.clear();
            self.ghost = None;
        } else if self.seen.as_ref().map(|(_, t)| t.as_ref()) != Some(tree) {
            let old = self.seen.as_ref().and_then(|(_, t)| t.as_ref());
            match old.zip(tree).and_then(|(old, new)| detect(old, new).map(|c| (old, new, c))) {
                Some((_, new, Change::Split { path, new_second })) => {
                    // A close's ghost no longer matches the tree; earlier splits'
                    // paths are untouched by splitting a leaf elsewhere.
                    if self.ghost.take().is_some() {
                        self.tweens.clear();
                    }
                    if let Some(LayoutNode::Split(s)) = new.at(&path) {
                        self.tweens.retain(|t| t.path != path);
                        self.tweens.push(RatioTween {
                            path,
                            from: if new_second { 1. } else { 0. },
                            to: shown_ratio(s.ratio),
                            start: now,
                            duration: SPLIT_ANIM,
                        });
                    }
                }
                Some((old, _, Change::Close { id, path, second })) => {
                    // Shrink from wherever that split is drawn right now.
                    let from = match (&self.ghost, self.ratio(&path, now)) {
                        (None, Some(r)) => r,
                        _ => match old.at(&path) {
                            Some(LayoutNode::Split(s)) => shown_ratio(s.ratio),
                            _ => 0.5,
                        },
                    };
                    self.tweens = vec![RatioTween { path, from, to: if second { 1. } else { 0. }, start: now, duration: CLOSE_ANIM }];
                    self.ghost = Some(Ghost { tree: old.clone(), closing: id });
                }
                None => {
                    self.tweens.clear();
                    self.ghost = None;
                }
            }
        }
        self.seen = ws_id.map(|id| (id.to_owned(), tree.cloned()));

        self.tweens.retain(|t| !t.done(now));
        if self.tweens.is_empty() {
            self.ghost = None;
        }
        !self.tweens.is_empty()
    }

    /// The tree to draw instead of the workspace's own while a close plays.
    pub fn display_tree(&self) -> Option<&LayoutNode> {
        self.ghost.as_ref().map(|g| &g.tree)
    }

    /// True while the drawn tree is a close's ghost (its paths aren't the real tree's).
    pub fn is_ghost(&self) -> bool {
        self.ghost.is_some()
    }

    /// The pane being closed, drawn as a blank tile.
    pub fn is_closing(&self, id: &str) -> bool {
        self.ghost.as_ref().is_some_and(|g| g.closing == id)
    }

    /// The ratio to draw the split at `path` with, while it's tweening.
    pub fn ratio(&self, path: &[bool], now: Instant) -> Option<f32> {
        self.tweens.iter().find(|t| t.path == path).map(|t| t.value(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::layout::Direction;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn two() -> LayoutNode {
        let mut t = LayoutNode::leaf("a");
        t.split_leaf("a", "b", Direction::Horizontal);
        t
    }

    #[test]
    fn tween_eases_from_start_to_end() {
        let start = Instant::now();
        let t = RatioTween { path: vec![], from: 1., to: 0.5, start, duration: SPLIT_ANIM };
        assert_eq!(t.value(start), 1.);
        assert!((t.value(start + SPLIT_ANIM) - 0.5).abs() < 1e-4);
        assert!((t.value(start + ms(10_000)) - 0.5).abs() < 1e-4, "clamped past the end");
        // Monotonic, and front-loaded like the focus-mode curve.
        let mut prev = 1.;
        for i in 1..=20 {
            let v = t.value(start + SPLIT_ANIM * i / 20);
            assert!(v <= prev + 1e-6);
            prev = v;
        }
        assert!(t.value(start + ms(60)) < 0.65, "{}", t.value(start + ms(60)));
        assert!(!t.done(start + ms(199)) && t.done(start + SPLIT_ANIM));
    }

    #[test]
    fn detects_a_split() {
        let old = LayoutNode::leaf("a");
        assert_eq!(detect(&old, &two()), Some(Change::Split { path: vec![], new_second: true }));
        // Nested: splitting b inside a horizontal split.
        let mut nested = two();
        nested.split_leaf("b", "c", Direction::Vertical);
        assert_eq!(detect(&two(), &nested), Some(Change::Split { path: vec![true], new_second: true }));
    }

    #[test]
    fn detects_a_close() {
        let t = two();
        assert_eq!(detect(&t, &LayoutNode::leaf("a")), Some(Change::Close { id: "b".into(), path: vec![], second: true }));
        assert_eq!(detect(&t, &LayoutNode::leaf("b")), Some(Change::Close { id: "a".into(), path: vec![], second: false }));
        let mut nested = two();
        nested.split_leaf("a", "c", Direction::Vertical);
        let after = nested.clone().remove_leaf("c").unwrap();
        assert_eq!(detect(&nested, &after), Some(Change::Close { id: "c".into(), path: vec![false], second: true }));
    }

    #[test]
    fn ignores_other_changes() {
        let t = two();
        let mut swapped = t.clone();
        swapped.swap_leaves("a", "b");
        assert_eq!(detect(&t, &swapped), None, "a swap");
        let mut resized = t.clone();
        if let LayoutNode::Split(s) = &mut resized {
            s.ratio = 0.3;
        }
        assert_eq!(detect(&t, &resized), None, "a divider drag");
        // A split plus a ratio change elsewhere isn't a plain split.
        let mut both = resized.clone();
        both.split_leaf("a", "c", Direction::Vertical);
        assert_eq!(detect(&t, &both), None);
        // Two panes closed at once.
        let mut three = two();
        three.split_leaf("b", "c", Direction::Vertical);
        assert_eq!(detect(&three, &LayoutNode::leaf("a")), None);
    }

    #[test]
    fn split_grows_the_new_pane_then_settles() {
        let mut a = LayoutAnim::default();
        let t0 = Instant::now();
        let one = LayoutNode::leaf("a");
        assert!(!a.observe(Some("w"), Some(&one), t0), "the first frame never animates");
        let t = two();
        assert!(a.observe(Some("w"), Some(&t), t0));
        assert_eq!(a.ratio(&[], t0), Some(1.), "the new second pane starts at nothing");
        assert!(a.display_tree().is_none());
        let mid = a.ratio(&[], t0 + ms(100)).unwrap();
        assert!(mid > 0.5 && mid < 1.);
        assert!(!a.observe(Some("w"), Some(&t), t0 + SPLIT_ANIM));
        assert_eq!(a.ratio(&[], t0 + SPLIT_ANIM), None);
    }

    #[test]
    fn nested_splits_tween_together() {
        let mut a = LayoutAnim::default();
        let t0 = Instant::now();
        a.observe(Some("w"), Some(&LayoutNode::leaf("a")), t0);
        let t = two();
        a.observe(Some("w"), Some(&t), t0);
        let mut nested = t.clone();
        nested.split_leaf("b", "c", Direction::Vertical);
        assert!(a.observe(Some("w"), Some(&nested), t0 + ms(50)));
        assert!(a.ratio(&[], t0 + ms(60)).is_some(), "the first split keeps going");
        assert_eq!(a.ratio(&[true], t0 + ms(50)), Some(1.));
        assert!(a.observe(Some("w"), Some(&nested), t0 + ms(210)), "the second is still running");
        assert_eq!(a.ratio(&[], t0 + ms(210)), None);
        assert!(!a.observe(Some("w"), Some(&nested), t0 + ms(250)));
    }

    #[test]
    fn close_shrinks_a_ghost_then_drops_it() {
        let mut a = LayoutAnim::default();
        let t0 = Instant::now();
        let mut t = two();
        if let LayoutNode::Split(s) = &mut t {
            s.ratio = 0.7;
        }
        a.observe(Some("w"), Some(&t), t0);
        let after = t.clone().remove_leaf("a").unwrap();
        assert!(a.observe(Some("w"), Some(&after), t0));
        assert_eq!(a.display_tree(), Some(&t));
        assert!(a.is_ghost() && a.is_closing("a") && !a.is_closing("b"));
        assert!((a.ratio(&[], t0).unwrap() - 0.7).abs() < 1e-6, "from where it was");
        assert!((a.ratio(&[], t0 + CLOSE_ANIM).unwrap()).abs() < 1e-4, "the closed first pane shrinks to 0");
        assert!(!a.observe(Some("w"), Some(&after), t0 + CLOSE_ANIM));
        assert!(a.display_tree().is_none() && !a.is_closing("a"));
    }

    #[test]
    fn close_mid_split_starts_from_the_drawn_ratio() {
        let mut a = LayoutAnim::default();
        let t0 = Instant::now();
        a.observe(Some("w"), Some(&LayoutNode::leaf("a")), t0);
        a.observe(Some("w"), Some(&two()), t0);
        let drawn = a.ratio(&[], t0 + ms(40)).unwrap();
        a.observe(Some("w"), Some(&LayoutNode::leaf("a")), t0 + ms(40));
        assert!((a.ratio(&[], t0 + ms(40)).unwrap() - drawn).abs() < 1e-6);
        assert!(a.is_closing("b"));
    }

    #[test]
    fn switching_workspaces_and_other_edits_snap() {
        let mut a = LayoutAnim::default();
        let t0 = Instant::now();
        a.observe(Some("w"), Some(&LayoutNode::leaf("a")), t0);
        assert!(!a.observe(Some("v"), Some(&two()), t0), "another workspace");
        // A split in flight is dropped by an unrelated edit (a swap).
        a.observe(Some("v"), Some(&LayoutNode::leaf("a")), t0);
        let close = a.observe(Some("v"), Some(&two()), t0);
        assert!(close);
        let mut swapped = two();
        swapped.swap_leaves("a", "b");
        assert!(!a.observe(Some("v"), Some(&swapped), t0 + ms(10)));
        // Closing the last pane: nothing to animate.
        assert!(!a.observe(Some("v"), None, t0 + ms(20)));
        assert!(!a.observe(None, None, t0 + ms(30)));
    }
}
