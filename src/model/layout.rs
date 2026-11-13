//! The tiling layout tree: the same JSON shape as SplitGrid's `LayoutNode`
//! (shared/types.ts, shared/layout-tree.ts), so imported workspaces keep their
//! layout. `horizontal` = children side by side, `vertical` = stacked.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum LayoutNode {
    Split(Split),
    Leaf(Leaf),
}

/// Marks the node an agent's panes live under: its zone (see agent::zones).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneTag {
    /// The agent's terminal ($SPLITGRID_TERMINAL).
    pub terminal: String,
    pub name: String,
    /// Shown in the zone's frame header.
    pub label: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Split {
    pub direction: Direction,
    /// 0–1: the share of the first child.
    pub ratio: f32,
    pub first: Box<LayoutNode>,
    pub second: Box<LayoutNode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<ZoneTag>,
    /// Roots a zone's tray: the thin strip at its bottom for background processes.
    #[serde(default, skip_serializing_if = "is_false")]
    pub zone_tray: bool,
    /// Fields this build doesn't know, kept as they were.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Leaf {
    pub container_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone: Option<ZoneTag>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub zone_tray: bool,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub const MIN_RATIO: f32 = 0.05;

/// Which child of a split, on the way from the root to a node.
pub type NodePath = Vec<bool>; // false = first, true = second

impl LayoutNode {
    pub fn leaf(container_id: impl Into<String>) -> Self {
        LayoutNode::Leaf(Leaf { container_id: container_id.into(), zone: None, zone_tray: false, extra: Map::new() })
    }

    pub fn split(direction: Direction, ratio: f32, first: LayoutNode, second: LayoutNode) -> Self {
        LayoutNode::Split(Split {
            direction,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
            zone: None,
            zone_tray: false,
            extra: Map::new(),
        })
    }

    pub fn zone(&self) -> Option<&ZoneTag> {
        match self {
            LayoutNode::Leaf(l) => l.zone.as_ref(),
            LayoutNode::Split(s) => s.zone.as_ref(),
        }
    }

    pub fn zone_tray(&self) -> bool {
        match self {
            LayoutNode::Leaf(l) => l.zone_tray,
            LayoutNode::Split(s) => s.zone_tray,
        }
    }

    fn marks_mut(&mut self) -> (&mut Option<ZoneTag>, &mut bool) {
        match self {
            LayoutNode::Leaf(l) => (&mut l.zone, &mut l.zone_tray),
            LayoutNode::Split(s) => (&mut s.zone, &mut s.zone_tray),
        }
    }

    pub fn set_zone(&mut self, zone: Option<ZoneTag>) {
        *self.marks_mut().0 = zone;
    }

    pub fn set_zone_tray(&mut self, tray: bool) {
        *self.marks_mut().1 = tray;
    }

    /// Take over `from`'s zone tag and tray mark when it collapses onto (or is
    /// replaced by) this node, so the zone keeps covering the same region. A
    /// node that already roots a zone of its own keeps that one.
    pub fn inherit_zone(mut self, from_zone: Option<ZoneTag>, from_tray: bool) -> Self {
        let (zone, tray) = self.marks_mut();
        if zone.is_none() {
            *zone = from_zone;
        }
        *tray |= from_tray;
        self
    }

    /// Replace the leaf `target` with `replacement` (inheriting its marks).
    /// Returns false when the leaf isn't in the tree.
    pub fn replace_leaf(&mut self, target: &str, replacement: LayoutNode) -> bool {
        match self {
            LayoutNode::Leaf(l) if l.container_id == target => {
                let (zone, tray) = (l.zone.take(), l.zone_tray);
                *self = replacement.inherit_zone(zone, tray);
                true
            }
            LayoutNode::Leaf(_) => false,
            LayoutNode::Split(s) => s.first.replace_leaf(target, replacement.clone()) || s.second.replace_leaf(target, replacement),
        }
    }

    pub fn first_leaf_id(&self) -> &str {
        match self {
            LayoutNode::Leaf(l) => &l.container_id,
            LayoutNode::Split(s) => s.first.first_leaf_id(),
        }
    }

    /// The node at a path from the root.
    pub fn at(&self, path: &[bool]) -> Option<&LayoutNode> {
        match path.split_first() {
            None => Some(self),
            Some((side, rest)) => match self {
                LayoutNode::Split(s) => (if *side { &s.second } else { &s.first }).at(rest),
                LayoutNode::Leaf(_) => None,
            },
        }
    }

    pub fn at_mut(&mut self, path: &[bool]) -> Option<&mut LayoutNode> {
        match path.split_first() {
            None => Some(self),
            Some((side, rest)) => match self {
                LayoutNode::Split(s) => (if *side { &mut s.second } else { &mut s.first }).at_mut(rest),
                LayoutNode::Leaf(_) => None,
            },
        }
    }

    /// Container ids in reading order.
    pub fn leaf_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<String>) {
        match self {
            LayoutNode::Leaf(l) => out.push(l.container_id.clone()),
            LayoutNode::Split(s) => {
                s.first.collect(out);
                s.second.collect(out);
            }
        }
    }

    /// Split the leaf holding `target`: it keeps the first half, `new_id` gets the second.
    /// Returns false when `target` isn't in the tree.
    pub fn split_leaf(&mut self, target: &str, new_id: &str, direction: Direction) -> bool {
        let split = LayoutNode::split(direction, 0.5, LayoutNode::leaf(target), LayoutNode::leaf(new_id));
        self.replace_leaf(target, split)
    }

    /// Remove a leaf; its sibling takes the parent split's place.
    /// None when the tree held only that leaf.
    pub fn remove_leaf(self, target: &str) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf(l) if l.container_id == target => None,
            leaf @ LayoutNode::Leaf(_) => Some(leaf),
            LayoutNode::Split(mut s) => {
                let first = (*s.first).remove_leaf(target);
                let second = (*s.second).remove_leaf(target);
                match (first, second) {
                    (Some(f), Some(sec)) => {
                        s.first = Box::new(f);
                        s.second = Box::new(sec);
                        Some(LayoutNode::Split(s))
                    }
                    (Some(only), None) | (None, Some(only)) => Some(only.inherit_zone(s.zone, s.zone_tray)),
                    (None, None) => None,
                }
            }
        }
    }

    /// Swap two panes' places (dragging one pane's header onto another, as
    /// SplitGrid's swapLeaves does). Only the container ids trade places: the
    /// splits, their ratios and every zone / tray mark stay where they are, so a
    /// pane dragged into an agent's zone joins it and the one it displaced
    /// leaves it. Returns false (and changes nothing) unless both are in the
    /// tree and differ.
    pub fn swap_leaves(&mut self, a: &str, b: &str) -> bool {
        if a == b {
            return false;
        }
        let ids = self.leaf_ids();
        if !ids.iter().any(|i| i == a) || !ids.iter().any(|i| i == b) {
            return false;
        }
        fn walk(node: &mut LayoutNode, a: &str, b: &str) {
            match node {
                LayoutNode::Leaf(l) if l.container_id == a => l.container_id = b.to_owned(),
                LayoutNode::Leaf(l) if l.container_id == b => l.container_id = a.to_owned(),
                LayoutNode::Leaf(_) => {}
                LayoutNode::Split(s) => {
                    walk(&mut s.first, a, b);
                    walk(&mut s.second, a, b);
                }
            }
        }
        walk(self, a, b);
        true
    }

    pub fn split_at_mut(&mut self, path: &[bool]) -> Option<&mut Split> {
        let LayoutNode::Split(s) = self else { return None };
        match path.split_first() {
            None => Some(s),
            Some((false, rest)) => s.first.split_at_mut(rest),
            Some((true, rest)) => s.second.split_at_mut(rest),
        }
    }

    /// Drop leaves whose container no longer exists (what SplitGrid's pruneLayoutTree does).
    pub fn prune(self, keep: &dyn Fn(&str) -> bool) -> Option<LayoutNode> {
        match self {
            LayoutNode::Leaf(l) => keep(&l.container_id).then_some(LayoutNode::Leaf(l)),
            LayoutNode::Split(mut s) => match ((*s.first).prune(keep), (*s.second).prune(keep)) {
                (Some(f), Some(sec)) => {
                    s.first = Box::new(f);
                    s.second = Box::new(sec);
                    s.ratio = s.ratio.clamp(MIN_RATIO, 1. - MIN_RATIO);
                    Some(LayoutNode::Split(s))
                }
                (Some(only), None) | (None, Some(only)) => Some(only.inherit_zone(s.zone, s.zone_tray)),
                (None, None) => None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_splitgrid_json_and_writes_it_back() {
        let json = r#"{"type":"split","direction":"horizontal","ratio":0.6,
            "first":{"type":"leaf","containerId":"a"},
            "second":{"type":"leaf","containerId":"b","zone":{"terminal":"t","name":"main","label":"x"}}}"#;
        let node: LayoutNode = serde_json::from_str(json).unwrap();
        assert_eq!(node.leaf_ids(), ["a", "b"]);
        let back = serde_json::to_value(&node).unwrap();
        assert_eq!(back["first"]["containerId"], "a");
        assert_eq!(back["second"]["zone"]["name"], "main");
        assert!(back["first"].get("zone").is_none() && back["first"].get("zoneTray").is_none(), "absent marks stay absent");
        assert_eq!(back["direction"], "horizontal");
    }

    #[test]
    fn split_and_remove() {
        let mut t = LayoutNode::leaf("a");
        assert!(t.split_leaf("a", "b", Direction::Horizontal));
        assert!(t.split_leaf("b", "c", Direction::Vertical));
        assert_eq!(t.leaf_ids(), ["a", "b", "c"]);
        assert!(!t.split_leaf("zzz", "d", Direction::Vertical));

        let t = t.remove_leaf("b").unwrap();
        assert_eq!(t.leaf_ids(), ["a", "c"]);
        let t = t.remove_leaf("a").unwrap();
        assert_eq!(t, LayoutNode::leaf("c"));
        assert!(t.remove_leaf("c").is_none());
    }

    #[test]
    fn zone_marks_move_with_their_region() {
        let tag = ZoneTag { terminal: "t".into(), name: "main".into(), label: "x".into() };
        let mut t = LayoutNode::leaf("a");
        t.split_leaf("a", "b", Direction::Horizontal);
        t.at_mut(&[true]).unwrap().set_zone(Some(tag.clone()));
        // Splitting the zone's root leaf moves the tag up to the new split.
        t.split_leaf("b", "c", Direction::Vertical);
        assert_eq!(t.at(&[true]).unwrap().zone(), Some(&tag));
        assert_eq!(t.at(&[true, false]).unwrap().zone(), None);
        // Closing one of its panes hands the tag to the survivor.
        let t = t.remove_leaf("b").unwrap();
        assert_eq!(t.at(&[true]).unwrap().zone(), Some(&tag));
        assert_eq!(t.leaf_ids(), ["a", "c"]);
    }

    #[test]
    fn split_paths_reach_nested_splits() {
        let mut t = LayoutNode::leaf("a");
        t.split_leaf("a", "b", Direction::Horizontal);
        t.split_leaf("b", "c", Direction::Vertical);
        t.split_at_mut(&[]).unwrap().ratio = 0.3;
        t.split_at_mut(&[true]).unwrap().ratio = 0.7;
        assert!(t.split_at_mut(&[false]).is_none(), "a leaf is not a split");
        let LayoutNode::Split(root) = &t else { panic!() };
        assert_eq!(root.ratio, 0.3);
        let LayoutNode::Split(inner) = root.second.as_ref() else { panic!() };
        assert_eq!(inner.ratio, 0.7);
    }

    #[test]
    fn prune_drops_missing_containers() {
        let mut t = LayoutNode::leaf("a");
        t.split_leaf("a", "b", Direction::Horizontal);
        t.split_leaf("b", "c", Direction::Vertical);
        let t = t.prune(&|id| id != "b").unwrap();
        assert_eq!(t.leaf_ids(), ["a", "c"]);
        assert!(LayoutNode::leaf("x").prune(&|_| false).is_none());
    }

    fn sample() -> LayoutNode {
        // a | (b / c), ratios 0.3 and 0.7
        let mut t = LayoutNode::leaf("a");
        t.split_leaf("a", "b", Direction::Horizontal);
        t.split_leaf("b", "c", Direction::Vertical);
        t.split_at_mut(&[]).unwrap().ratio = 0.3;
        t.split_at_mut(&[true]).unwrap().ratio = 0.7;
        t
    }

    #[test]
    fn swap_trades_places_and_keeps_the_shape() {
        let mut t = sample();
        assert!(t.swap_leaves("a", "c"));
        assert_eq!(t.leaf_ids(), ["c", "b", "a"]);
        assert_eq!(t.split_at_mut(&[]).unwrap().ratio, 0.3);
        assert_eq!(t.split_at_mut(&[true]).unwrap().ratio, 0.7);
        assert_eq!(t.split_at_mut(&[]).unwrap().direction, Direction::Horizontal);
        assert_eq!(t.split_at_mut(&[true]).unwrap().direction, Direction::Vertical);
        // Swapping back restores the original tree.
        assert!(t.swap_leaves("c", "a"));
        assert_eq!(t, sample());
    }

    #[test]
    fn swap_with_sibling_and_in_a_two_pane_split() {
        let mut t = sample();
        assert!(t.swap_leaves("b", "c"));
        assert_eq!(t.leaf_ids(), ["a", "c", "b"]);
        let mut two = LayoutNode::leaf("x");
        two.split_leaf("x", "y", Direction::Vertical);
        assert!(two.swap_leaves("y", "x"));
        assert_eq!(two.leaf_ids(), ["y", "x"]);
    }

    #[test]
    fn swap_onto_itself_or_a_missing_pane_is_a_no_op() {
        let mut t = sample();
        assert!(!t.swap_leaves("a", "a"));
        assert!(!t.swap_leaves("a", "zzz"));
        assert!(!t.swap_leaves("zzz", "a"));
        assert_eq!(t, sample());
        let mut single = LayoutNode::leaf("a");
        assert!(!single.swap_leaves("a", "b"));
        assert_eq!(single, LayoutNode::leaf("a"));
    }

    #[test]
    fn swap_leaves_zone_and_tray_marks_in_place() {
        let tag = ZoneTag { terminal: "t".into(), name: "main".into(), label: "x".into() };
        let mut t = sample();
        // The zone covers (b / c); c is its tray; a leaf-rooted zone elsewhere too.
        t.at_mut(&[true]).unwrap().set_zone(Some(tag.clone()));
        t.at_mut(&[true, true]).unwrap().set_zone_tray(true);
        let before_marks = |t: &LayoutNode| {
            [vec![], vec![false], vec![true], vec![true, false], vec![true, true]]
                .map(|p| (t.at(&p).unwrap().zone().cloned(), t.at(&p).unwrap().zone_tray()))
        };
        let marks = before_marks(&t);
        // a moves into the zone's tray, c out of it.
        assert!(t.swap_leaves("a", "c"));
        assert_eq!(before_marks(&t), marks);
        assert_eq!(crate::agent::zones::zone_of_pane(Some(&t), "a"), Some(tag.clone()));
        assert_eq!(crate::agent::zones::zone_of_pane(Some(&t), "c"), None);
        assert_eq!(crate::agent::zones::zone_places(Some(&t)).get("a"), Some(&crate::agent::zones::ZonePlace::Tray));

        // A zone rooted at a leaf keeps its tag on the leaf: whichever pane sits there.
        let mut l = LayoutNode::leaf("p");
        l.split_leaf("p", "q", Direction::Horizontal);
        l.at_mut(&[true]).unwrap().set_zone(Some(tag.clone()));
        assert!(l.swap_leaves("p", "q"));
        assert_eq!(l.at(&[true]).unwrap().zone(), Some(&tag));
        assert_eq!(l.at(&[false]).unwrap().zone(), None);
        assert_eq!(l.leaf_ids(), ["q", "p"]);
    }

    #[test]
    fn swap_keeps_unknown_fields_and_round_trips() {
        let json = r#"{"type":"split","direction":"vertical","ratio":0.25,
            "first":{"type":"leaf","containerId":"a","future":1},
            "second":{"type":"leaf","containerId":"b"}}"#;
        let mut node: LayoutNode = serde_json::from_str(json).unwrap();
        assert!(node.swap_leaves("a", "b"));
        let back = serde_json::to_value(&node).unwrap();
        assert_eq!(back["first"]["containerId"], "b");
        assert_eq!(back["first"]["future"], 1, "the slot's own fields stay with the slot");
        assert_eq!(back["second"]["containerId"], "a");
        assert_eq!(back["ratio"], 0.25);
    }
}
