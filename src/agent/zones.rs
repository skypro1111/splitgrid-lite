//! Agent zones (the Electron app's shared/agent-zones.ts): each agent gets one
//! region of the workspace for the panes it opens, so agents (and their
//! subagents) can't scatter terminals across the user's layout. The first pane
//! an agent opens splits its own terminal and becomes the zone; every later one
//! splits a pane already in it, appended after the zone's last pane, right or
//! down. Background processes go to the zone's tray instead: a thin strip along
//! its bottom. An agent is its terminal plus a zone name (`--zone`), so
//! subagents sharing one terminal each get their own.
//!
//! Pure functions over the layout tree; ids are container ids.

use std::collections::HashMap;

use crate::model::layout::{Direction, LayoutNode, NodePath, ZoneTag};
use crate::model::workspace::Workspace;

pub const DEFAULT_ZONE: &str = "main";
/// The share of a zone (or of the agent's terminal) left above its tray.
pub const TRAY_RATIO: f32 = 0.85;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneSplit {
    Right,
    Down,
}

impl ZoneSplit {
    /// `None` for no value, `Some(None)` for one that isn't a direction.
    pub fn parse(value: Option<&str>) -> Option<Option<ZoneSplit>> {
        let v = value?.to_lowercase();
        Some(match v.as_str() {
            "right" | "h" | "horizontal" => Some(ZoneSplit::Right),
            "down" | "v" | "vertical" => Some(ZoneSplit::Down),
            _ => None,
        })
    }

    fn direction(self) -> Direction {
        match self {
            ZoneSplit::Right => Direction::Horizontal,
            ZoneSplit::Down => Direction::Vertical,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneOwner {
    pub terminal: String,
    pub name: String,
}

impl ZoneOwner {
    pub fn new(terminal: &str, zone: Option<&str>) -> Self {
        Self { terminal: terminal.to_owned(), name: zone.unwrap_or(DEFAULT_ZONE).to_owned() }
    }

    fn owns(&self, tag: Option<&ZoneTag>) -> bool {
        tag.is_some_and(|t| t.terminal == self.terminal && t.name == self.name)
    }
}

/// The path to the node the owner's zone is rooted at.
pub fn find_zone(tree: &LayoutNode, owner: &ZoneOwner) -> Option<NodePath> {
    fn walk(node: &LayoutNode, owner: &ZoneOwner, path: &mut NodePath) -> bool {
        if owner.owns(node.zone()) {
            return true;
        }
        let LayoutNode::Split(s) = node else { return false };
        for (side, child) in [(false, &s.first), (true, &s.second)] {
            path.push(side);
            if walk(child, owner, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    walk(tree, owner, &mut path).then_some(path)
}

#[derive(Debug, Default, PartialEq)]
pub struct ZoneLayout {
    /// The zone's regular panes, in reading order.
    pub main: Vec<String>,
    /// Its tray panes, left to right.
    pub tray: Vec<String>,
}

pub fn zone_layout(tree: Option<&LayoutNode>, owner: &ZoneOwner) -> ZoneLayout {
    // The zone's own panes: another agent's zone nested inside belongs to that agent.
    fn collect(node: &LayoutNode, owner: &ZoneOwner, is_root: bool, mut in_tray: bool, out: &mut ZoneLayout) {
        if !is_root && node.zone().is_some() && !owner.owns(node.zone()) {
            return;
        }
        in_tray |= node.zone_tray();
        match node {
            LayoutNode::Leaf(l) => (if in_tray { &mut out.tray } else { &mut out.main }).push(l.container_id.clone()),
            LayoutNode::Split(s) => {
                collect(&s.first, owner, false, in_tray, out);
                collect(&s.second, owner, false, in_tray, out);
            }
        }
    }
    let mut out = ZoneLayout::default();
    if let Some(tree) = tree
        && let Some(path) = find_zone(tree, owner)
    {
        collect(tree.at(&path).expect("found path"), owner, true, false, &mut out);
    }
    out
}

/// Every pane of the zone, tray included.
pub fn zone_pane_ids(tree: Option<&LayoutNode>, owner: &ZoneOwner) -> Vec<String> {
    let ZoneLayout { mut main, tray } = zone_layout(tree, owner);
    main.extend(tray);
    main
}

/// Without an explicit split, alternate against the target's parent so a run
/// of opens tiles the zone (right, down, right, …).
fn default_direction(zone: &LayoutNode, target: &str) -> Direction {
    fn parent_dir(node: &LayoutNode, target: &str) -> Option<Direction> {
        let LayoutNode::Split(s) = node else { return None };
        for child in [&s.first, &s.second] {
            if matches!(child.as_ref(), LayoutNode::Leaf(l) if l.container_id == target) {
                return Some(s.direction);
            }
            if let Some(d) = parent_dir(child, target) {
                return Some(d);
            }
        }
        None
    }
    match parent_dir(zone, target) {
        Some(Direction::Horizontal) => Direction::Vertical,
        _ => Direction::Horizontal,
    }
}

pub struct OpenArgs<'a> {
    pub owner: &'a ZoneOwner,
    /// Frame header text, used when this open creates the zone.
    pub label: &'a str,
    pub new_id: &'a str,
    /// The pane to split when the zone doesn't exist yet: the agent's own terminal.
    pub anchor_id: Option<&'a str>,
    /// A pane in the zone to split; defaults to the zone's last pane.
    pub at: Option<&'a str>,
    pub split: Option<ZoneSplit>,
    /// Put the pane in the zone's tray (a background process).
    pub background: bool,
}

/// Place a new leaf in the owner's zone (creating it beside the anchor when
/// there is none). Err with a message when `at` isn't one of the zone's panes.
pub fn open_in_zone(tree: Option<LayoutNode>, a: &OpenArgs) -> Result<LayoutNode, String> {
    let mut new_leaf = LayoutNode::leaf(a.new_id);
    let Some(mut tree) = tree else {
        new_leaf.set_zone(Some(tag(a)));
        new_leaf.set_zone_tray(a.background);
        return Ok(new_leaf);
    };

    if let Some(zone_path) = find_zone(&tree, a.owner) {
        let layout = zone_layout(Some(&tree), a.owner);
        // The pool this pane joins: the tray for a background process, else the regular panes.
        let panes = if a.background { &layout.tray } else { &layout.main };
        if let Some(last) = panes.last() {
            let target = a.at.unwrap_or(last);
            if !panes.iter().any(|p| p == target) {
                let which = if a.background { "tray" } else { "regular" };
                return Err(format!("{target} is not one of your zone's {which} panes; pass --at with one of: {}", panes.join(", ")));
            }
            let zone = tree.at(&zone_path).expect("found path");
            // Tray panes line up left to right; regular ones alternate.
            let direction = match a.split {
                Some(s) => s.direction(),
                None if a.background => Direction::Horizontal,
                None => default_direction(zone, target),
            };
            let split = LayoutNode::split(direction, 0.5, LayoutNode::leaf(target), new_leaf);
            let target = target.to_owned();
            tree.replace_leaf(&target, split);
            return Ok(tree);
        }

        // First pane of its kind: the zone splits into regular panes above, tray below.
        let mut rest = tree.at(&zone_path).expect("found path").clone();
        let zone_tag = rest.zone().cloned();
        rest.set_zone(None);
        if a.background {
            new_leaf.set_zone_tray(true);
        }
        let mut split = if a.background {
            LayoutNode::split(Direction::Vertical, TRAY_RATIO, rest, new_leaf)
        } else {
            LayoutNode::split(Direction::Vertical, TRAY_RATIO, new_leaf, rest)
        };
        split.set_zone(zone_tag);

        // A zone that so far is only a tray strip under the agent's terminal
        // grows to half of that space for its first regular pane.
        if !a.background
            && let Some((true, parent_path)) = zone_path.split_last()
            && let Some(LayoutNode::Split(parent)) = tree.at_mut(parent_path)
            && parent.direction == Direction::Vertical
            && parent.ratio > 0.5
        {
            parent.ratio = 0.5;
            *parent.second = split;
            return Ok(tree);
        }
        *tree.at_mut(&zone_path).expect("found path") = split;
        return Ok(tree);
    }

    new_leaf.set_zone(Some(tag(a)));
    new_leaf.set_zone_tray(a.background);
    let ids = tree.leaf_ids();
    let anchor = a.anchor_id.filter(|id| ids.iter().any(|i| i == id)).unwrap_or(tree.first_leaf_id()).to_owned();
    // A background process starts as a thin tray under the agent's terminal.
    let split = if a.background {
        LayoutNode::split(Direction::Vertical, TRAY_RATIO, LayoutNode::leaf(&anchor), new_leaf)
    } else {
        let direction = a.split.unwrap_or(ZoneSplit::Right).direction();
        LayoutNode::split(direction, 0.5, LayoutNode::leaf(&anchor), new_leaf)
    };
    // An agent running inside another agent's zone gets its zone nested there.
    tree.replace_leaf(&anchor, split);
    Ok(tree)
}

fn tag(a: &OpenArgs) -> ZoneTag {
    ZoneTag { terminal: a.owner.terminal.clone(), name: a.owner.name.clone(), label: a.label.to_owned() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZonePlace {
    Main,
    Tray,
}

/// Where each zone pane sits; panes outside every zone are left out.
pub fn zone_places(tree: Option<&LayoutNode>) -> HashMap<String, ZonePlace> {
    fn walk(node: &LayoutNode, mut in_zone: bool, mut in_tray: bool, out: &mut HashMap<String, ZonePlace>) {
        if node.zone().is_some() {
            in_zone = true;
            in_tray = false;
        }
        in_tray |= node.zone_tray();
        match node {
            LayoutNode::Leaf(l) => {
                if in_zone {
                    out.insert(l.container_id.clone(), if in_tray { ZonePlace::Tray } else { ZonePlace::Main });
                }
            }
            LayoutNode::Split(s) => {
                walk(&s.first, in_zone, in_tray, out);
                walk(&s.second, in_zone, in_tray, out);
            }
        }
    }
    let mut out = HashMap::new();
    if let Some(t) = tree {
        walk(t, false, false, &mut out);
    }
    out
}

/// The innermost zone around a pane.
pub fn zone_of_pane(tree: Option<&LayoutNode>, container_id: &str) -> Option<ZoneTag> {
    fn walk<'a>(node: &'a LayoutNode, tag: Option<&'a ZoneTag>, id: &str) -> Option<Option<&'a ZoneTag>> {
        let here = node.zone().or(tag);
        match node {
            LayoutNode::Leaf(l) => (l.container_id == id).then_some(here),
            LayoutNode::Split(s) => walk(&s.first, here, id).or_else(|| walk(&s.second, here, id)),
        }
    }
    walk(tree?, None, container_id).flatten().cloned()
}

/// Move a zone pane into its tray (`to_tray`) or back out among the regular
/// panes, landing where a new pane of that kind would. A zone left with only
/// its tray shrinks back to a strip under the agent's terminal (`anchor_id`).
/// None when the pane is in no zone or is already there.
pub fn move_zone_pane(tree: Option<&LayoutNode>, container_id: &str, to_tray: bool, anchor_id: Option<&str>) -> Option<LayoutNode> {
    let tag = zone_of_pane(tree, container_id)?;
    let owner = ZoneOwner { terminal: tag.terminal.clone(), name: tag.name.clone() };
    if zone_layout(tree, &owner).tray.iter().any(|i| i == container_id) == to_tray {
        return None;
    }
    let mut next = tree?.clone().remove_leaf(container_id);
    let mut moving = vec![container_id.to_owned()];
    if to_tray {
        let left = zone_layout(next.as_ref(), &owner);
        if left.main.is_empty() && !left.tray.is_empty() {
            for id in &left.tray {
                next = next.and_then(|n| n.remove_leaf(id));
            }
            moving = left.tray.into_iter().chain(moving).collect();
        }
    }
    for id in &moving {
        let args = OpenArgs { owner: &owner, label: &tag.label, new_id: id, anchor_id, at: None, split: None, background: to_tray };
        next = Some(open_in_zone(next, &args).ok()?);
    }
    next
}

/// Expand a collapsed pane (model::collapse) across `direction`. One that
/// belongs to an agent's zone goes back into that zone (so the agent still
/// owns it), started beside its parent when the zone is gone; any other one
/// splits its parent. False when it isn't collapsed.
pub fn expand_collapsed(ws: &mut Workspace, id: &str, direction: Direction) -> bool {
    let Some(parent) = ws.collapsed_parent(id) else { return false };
    if let Some(tag) = ws.collapsed_zone(id) {
        let owner = ZoneOwner::new(&tag.terminal, Some(&tag.name));
        let split = Some(if direction == Direction::Vertical { ZoneSplit::Down } else { ZoneSplit::Right });
        let args = OpenArgs { owner: &owner, label: &tag.label, new_id: id, anchor_id: Some(&parent), at: None, split, background: false };
        if let Ok(tree) = open_in_zone(ws.layout_tree.clone(), &args) {
            ws.layout_tree = Some(tree);
            ws.uncollapse(id);
            return true;
        }
    }
    ws.expand(id, direction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> ZoneOwner {
        ZoneOwner::new("me", None)
    }

    fn open(tree: Option<LayoutNode>, id: &str, background: bool) -> LayoutNode {
        let o = owner();
        let args = OpenArgs { owner: &o, label: "Agent", new_id: id, anchor_id: Some("me"), at: None, split: None, background };
        open_in_zone(tree, &args).unwrap()
    }

    #[test]
    fn first_pane_splits_the_agent_terminal_and_later_ones_stay_inside() {
        let mut t = LayoutNode::leaf("user");
        t.split_leaf("user", "me", Direction::Horizontal);
        let t = open(Some(t), "a", false);
        let t = open(Some(t), "b", false);
        let t = open(Some(t), "c", false);
        assert_eq!(t.leaf_ids(), ["user", "me", "a", "b", "c"]);
        assert_eq!(zone_pane_ids(Some(&t), &owner()), ["a", "b", "c"]);
        // Alternating: a|b side by side, then c below b.
        let path = find_zone(&t, &owner()).unwrap();
        let LayoutNode::Split(z) = t.at(&path).unwrap() else { panic!() };
        assert_eq!(z.direction, Direction::Horizontal);
        let LayoutNode::Split(bc) = z.second.as_ref() else { panic!() };
        assert_eq!(bc.direction, Direction::Vertical);
        // Another agent's zone isn't ours.
        assert!(zone_pane_ids(Some(&t), &ZoneOwner::new("me", Some("review"))).is_empty());
    }

    #[test]
    fn background_panes_go_to_the_tray() {
        let t = open(Some(LayoutNode::leaf("me")), "dev", true);
        let t = open(Some(t), "logs", true);
        let layout = zone_layout(Some(&t), &owner());
        assert_eq!(layout.tray, ["dev", "logs"]);
        assert!(layout.main.is_empty());
        let LayoutNode::Split(root) = &t else { panic!() };
        assert_eq!(root.ratio, TRAY_RATIO, "a thin strip under the agent's terminal");
        // The first regular pane grows the zone to half the space, tray kept below.
        let t = open(Some(t), "tests", false);
        let layout = zone_layout(Some(&t), &owner());
        assert_eq!((layout.main, layout.tray), (vec!["tests".to_owned()], vec!["dev".to_owned(), "logs".to_owned()]));
        let LayoutNode::Split(root) = &t else { panic!() };
        assert_eq!(root.ratio, 0.5);
        assert_eq!(zone_places(Some(&t)).get("dev"), Some(&ZonePlace::Tray));
    }

    #[test]
    fn at_must_name_a_zone_pane() {
        let t = open(Some(LayoutNode::leaf("me")), "a", false);
        let o = owner();
        let args = OpenArgs { owner: &o, label: "Agent", new_id: "x", anchor_id: Some("me"), at: Some("me"), split: None, background: false };
        assert!(open_in_zone(Some(t), &args).is_err());
    }

    #[test]
    fn panes_move_between_tray_and_zone() {
        let t = open(Some(LayoutNode::leaf("me")), "a", false);
        let t = open(Some(t), "b", false);
        let t = move_zone_pane(Some(&t), "b", true, Some("me")).unwrap();
        let layout = zone_layout(Some(&t), &owner());
        assert_eq!((layout.main.len(), layout.tray.len()), (1, 1));
        assert!(move_zone_pane(Some(&t), "b", true, Some("me")).is_none(), "already there");
        // The last regular pane into the tray: the zone shrinks to a strip.
        let t = move_zone_pane(Some(&t), "a", true, Some("me")).unwrap();
        let layout = zone_layout(Some(&t), &owner());
        assert_eq!(layout.tray, ["b", "a"]);
        assert!(layout.main.is_empty());
        let t = move_zone_pane(Some(&t), "a", false, Some("me")).unwrap();
        assert_eq!(zone_layout(Some(&t), &owner()).main, ["a"]);
        assert_eq!(zone_of_pane(Some(&t), "a").unwrap().label, "Agent");
        assert!(zone_of_pane(Some(&t), "me").is_none());
    }

    #[test]
    fn an_expanded_background_pane_stays_in_its_agents_zone() {
        use crate::model::workspace::{Container, ContainerContent};
        let mut ws = Workspace::new("W", None);
        ws.containers = ["user", "me"].map(|id| Container { id: id.into(), content: ContainerContent::local_terminal(None) }).to_vec();
        ws.layout_tree = Some(LayoutNode::split(Direction::Horizontal, 0.5, LayoutNode::leaf("user"), LayoutNode::leaf("me")));
        let tag = ZoneTag { terminal: "me".into(), name: DEFAULT_ZONE.into(), label: "Agent".into() };
        let bg = Container { id: "bg".into(), content: ContainerContent::local_terminal(None) };
        // `open --background`: collapsed into the agent's terminal, which is outside its zone.
        assert!(ws.add_collapsed(bg, "me", Some(&tag)));
        assert!(expand_collapsed(&mut ws, "bg", Direction::Vertical));
        assert!(!ws.is_collapsed("bg"));
        assert_eq!(zone_pane_ids(ws.layout_tree.as_ref(), &owner()), ["bg"], "the agent still owns it");
        assert_eq!(ws.focused_container_id.as_deref(), Some("bg"));
        // Collapsed again from its zone and expanded: back in the same zone.
        let zone = zone_of_pane(ws.layout_tree.as_ref(), "bg");
        assert!(ws.collapse("bg", zone.as_ref()).is_some());
        assert!(expand_collapsed(&mut ws, "bg", Direction::Horizontal));
        assert_eq!(zone_pane_ids(ws.layout_tree.as_ref(), &owner()), ["bg"]);
        // A pane of no zone just splits its parent.
        assert!(ws.collapse("user", None).is_some());
        assert!(expand_collapsed(&mut ws, "user", Direction::Horizontal));
        assert!(ws.leaves().contains(&"user".to_owned()));
        assert!(!expand_collapsed(&mut ws, "user", Direction::Horizontal), "not collapsed");
    }
}
