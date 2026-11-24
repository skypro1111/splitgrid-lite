//! Collapsed panes: a container taken out of the layout but kept running,
//! shown as a chip in the header of the pane that took its space (its
//! "parent"), and expanded back by splitting that pane. Agents open their
//! background terminals this way.
//!
//! The parent is kept on the container (`collapsedInto`), with the agent zone
//! the pane belonged to (`collapsedZone`), so an agent still owns it.

use serde_json::Value;

use super::layout::{Direction, LayoutNode, ZoneTag};
use super::workspace::{Workspace, now_ms};

const INTO: &str = "collapsedInto";
const ZONE: &str = "collapsedZone";

impl Workspace {
    /// Container ids in the layout, in reading order.
    pub fn leaves(&self) -> Vec<String> {
        self.layout_tree.as_ref().map(|t| t.leaf_ids()).unwrap_or_default()
    }

    fn is_leaf(&self, id: &str) -> bool {
        self.layout_tree.as_ref().is_some_and(|t| t.leaf_ids().iter().any(|l| l == id))
    }

    /// The pane `id` is collapsed into.
    pub fn collapsed_parent(&self, id: &str) -> Option<String> {
        self.container(id)?.content.extra_str(INTO)
    }

    pub fn is_collapsed(&self, id: &str) -> bool {
        self.collapsed_parent(id).is_some()
    }

    /// Panes collapsed into `parent`, oldest first.
    pub fn collapsed_in(&self, parent: &str) -> Vec<String> {
        self.containers.iter().filter(|c| c.content.extra_str(INTO).as_deref() == Some(parent)).map(|c| c.id.clone()).collect()
    }

    /// The agent zone a collapsed pane belonged to.
    pub fn collapsed_zone(&self, id: &str) -> Option<ZoneTag> {
        serde_json::from_value(self.container(id)?.content.extra.get(ZONE)?.clone()).ok()
    }

    /// Every container that should keep running: the layout's and the collapsed ones.
    pub fn live_ids(&self) -> Vec<String> {
        let mut ids = self.leaves();
        ids.extend(self.containers.iter().filter(|c| c.content.extra_str(INTO).is_some()).map(|c| c.id.clone()));
        ids
    }

    /// A pane may collapse unless it is the only one.
    pub fn can_collapse(&self, id: &str) -> bool {
        self.layout_tree.as_ref().and_then(|t| t.absorber(id)).is_some()
    }

    fn mark(&mut self, id: &str, parent: &str, zone: Option<&ZoneTag>) {
        if let Some(c) = self.container_mut(id) {
            c.content.extra.insert(INTO.into(), Value::from(parent));
            match zone.and_then(|z| serde_json::to_value(z).ok()) {
                Some(z) => c.content.extra.insert(ZONE.into(), z),
                None => c.content.extra.remove(ZONE),
            };
        }
    }

    fn unmark(&mut self, id: &str) {
        if let Some(c) = self.container_mut(id) {
            c.content.extra.remove(INTO);
            c.content.extra.remove(ZONE);
        }
    }

    /// Take a pane out of the layout; the neighbour that grows into its space
    /// becomes its parent (and adopts the panes collapsed into it). Returns the
    /// parent, or None when it is the only pane (or not in the layout).
    pub fn collapse(&mut self, id: &str, zone: Option<&ZoneTag>) -> Option<String> {
        let parent = self.layout_tree.as_ref()?.absorber(id)?;
        self.layout_tree = self.layout_tree.take().and_then(|t| t.remove_leaf(id));
        self.mark(id, &parent, zone);
        for child in self.collapsed_in(id) {
            let z = self.collapsed_zone(&child);
            self.mark(&child, &parent, z.as_ref());
        }
        if self.focused_container_id.as_deref() == Some(id) {
            self.focused_container_id = Some(parent.clone());
        }
        self.updated_at = now_ms();
        Some(parent)
    }

    /// Add a new container straight into `parent` as a collapsed pane (an
    /// agent's background terminal). False when `parent` isn't in the layout.
    pub fn add_collapsed(&mut self, container: super::workspace::Container, parent: &str, zone: Option<&ZoneTag>) -> bool {
        if !self.is_leaf(parent) {
            return false;
        }
        let id = container.id.clone();
        self.containers.push(container);
        self.mark(&id, parent, zone);
        self.updated_at = now_ms();
        true
    }

    /// Show a collapsed pane in another parent's header. False unless it is
    /// collapsed and `parent` is in the layout.
    pub fn move_collapsed(&mut self, id: &str, parent: &str) -> bool {
        if !self.is_collapsed(id) || !self.is_leaf(parent) {
            return false;
        }
        let zone = self.collapsed_zone(id);
        self.mark(id, parent, zone.as_ref());
        true
    }

    /// Forget that a pane is collapsed, once the caller has put it back in the
    /// layout itself (into its agent zone, agent::zones); it takes the focus.
    pub fn uncollapse(&mut self, id: &str) {
        self.unmark(id);
        self.focused_container_id = Some(id.to_owned());
        self.updated_at = now_ms();
    }

    /// Put a collapsed pane back: it splits its parent (`direction`) and takes
    /// the second half. False when it isn't collapsed.
    pub fn expand(&mut self, id: &str, direction: Direction) -> bool {
        let Some(parent) = self.collapsed_parent(id) else { return false };
        self.unmark(id);
        let placed = match self.layout_tree.as_mut() {
            Some(tree) => {
                let at = if tree.leaf_ids().contains(&parent) { parent } else { tree.last_leaf_id().to_owned() };
                tree.split_leaf(&at, id, direction)
            }
            None => false,
        };
        if !placed {
            self.layout_tree = Some(LayoutNode::leaf(id));
        }
        self.focused_container_id = Some(id.to_owned());
        self.updated_at = now_ms();
        true
    }

    /// Collapsed panes whose parent left the layout go to `hint` (when it is
    /// in the layout) or the first pane; with no pane left, the first of them
    /// becomes the layout. Nothing running is left unreachable.
    pub(super) fn rehome_collapsed(&mut self, hint: Option<&str>) {
        let orphans: Vec<String> =
            self.containers.iter().filter(|c| c.content.extra_str(INTO).is_some_and(|p| !self.is_leaf(&p))).map(|c| c.id.clone()).collect();
        for id in orphans {
            let to = hint.filter(|h| self.is_leaf(h)).map(str::to_owned).or_else(|| self.leaves().into_iter().next());
            match to {
                Some(to) => {
                    let z = self.collapsed_zone(&id);
                    self.mark(&id, &to, z.as_ref());
                }
                None => {
                    self.unmark(&id);
                    self.layout_tree = Some(LayoutNode::leaf(&id));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::workspace::{Container, ContainerContent, Workspace};
    use super::*;

    fn ws3() -> Workspace {
        // a | (b / c)
        let mut ws = Workspace::new("W", None);
        ws.containers = ["a", "b", "c"].map(|id| Container { id: id.into(), content: ContainerContent::local_terminal(None) }).to_vec();
        let bc = LayoutNode::split(Direction::Vertical, 0.5, LayoutNode::leaf("b"), LayoutNode::leaf("c"));
        ws.layout_tree = Some(LayoutNode::split(Direction::Horizontal, 0.5, LayoutNode::leaf("a"), bc));
        ws.focused_container_id = Some("c".into());
        ws
    }

    #[test]
    fn collapse_gives_the_space_to_the_neighbour_and_expand_splits_it_back() {
        let mut ws = ws3();
        assert_eq!(ws.collapse("c", None).as_deref(), Some("b"));
        assert_eq!(ws.leaves(), ["a", "b"]);
        assert_eq!(ws.collapsed_in("b"), ["c"]);
        assert_eq!(ws.focused_container_id.as_deref(), Some("b"));
        assert_eq!(ws.live_ids(), ["a", "b", "c"]);
        assert!(ws.expand("c", Direction::Vertical));
        assert_eq!(ws.leaves(), ["a", "b", "c"]);
        assert!(!ws.is_collapsed("c"));
        assert_eq!(ws.focused_container_id.as_deref(), Some("c"));
    }

    #[test]
    fn the_last_pane_cannot_collapse() {
        let mut ws = ws3();
        ws.collapse("a", None);
        ws.collapse("b", None);
        assert!(!ws.can_collapse("c"));
        assert_eq!(ws.collapse("c", None), None);
        // b took a with it: both are in c now.
        assert_eq!(ws.collapsed_in("c"), ["a", "b"]);
    }

    #[test]
    fn survives_a_save_and_load() {
        let mut ws = ws3();
        let zone = ZoneTag { terminal: "t".into(), name: "main".into(), label: "x".into() };
        ws.collapse("a", Some(&zone));
        let mut back: Workspace = serde_json::from_value(serde_json::to_value(&ws).unwrap()).unwrap();
        back.normalize();
        assert_eq!(back.leaves(), ["b", "c"]);
        assert_eq!(back.collapsed_in("b"), ["a"]);
        assert_eq!(back.collapsed_zone("a"), Some(zone));
    }

    #[test]
    fn closing_a_parent_rehomes_its_collapsed_panes() {
        let mut ws = ws3();
        ws.collapse("c", None);
        ws.close("b");
        assert_eq!(ws.leaves(), ["a"]);
        assert_eq!(ws.collapsed_in("a"), ["c"]);
        // The last pane closes: the collapsed one takes the layout.
        ws.close("a");
        assert_eq!(ws.leaves(), ["c"]);
        assert!(!ws.is_collapsed("c"));
        assert_eq!(ws.focused_container_id.as_deref(), Some("c"));
    }

    #[test]
    fn closing_a_collapsed_pane_leaves_the_layout_alone() {
        let mut ws = ws3();
        ws.collapse("a", None);
        ws.close("a");
        assert_eq!(ws.leaves(), ["b", "c"]);
        assert!(ws.container("a").is_none());
    }

    #[test]
    fn a_dangling_parent_is_fixed_on_load() {
        let mut ws = ws3();
        ws.collapse("a", None);
        ws.container_mut("a").unwrap().content.set_extra_str(INTO, Some("gone".into()));
        ws.normalize();
        assert_eq!(ws.collapsed_parent("a").as_deref(), Some("b"));
    }

    #[test]
    fn background_panes_join_a_parent_directly() {
        let mut ws = ws3();
        let bg = Container { id: "bg".into(), content: ContainerContent::local_terminal(None) };
        assert!(!ws.add_collapsed(bg.clone(), "nope", None));
        assert!(ws.add_collapsed(bg, "a", None));
        assert_eq!(ws.collapsed_in("a"), ["bg"]);
        assert_eq!(ws.leaves(), ["a", "b", "c"]);
        assert!(ws.move_collapsed("bg", "c"));
        assert_eq!(ws.collapsed_in("c"), ["bg"]);
        assert!(!ws.move_collapsed("a", "c"), "a is not collapsed");
    }
}
