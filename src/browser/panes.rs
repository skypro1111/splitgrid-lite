//! The app's side of browser panes: making them for the window's containers,
//! saving their URL, and deciding each frame which web views may show.

use std::collections::HashSet;

use gpui_kit::*;

use super::{BrowserEvent, BrowserPane, BrowserPaneEvent, DEFAULT_PARTITION, NEW_PANE_URL};
use crate::app::{AppView, Drag};
use crate::model::workspace::{ContainerContent, ContentType};

/// A live browser pane and the app's subscriptions to it.
pub(crate) struct Entry {
    pub pane: Entity<BrowserPane>,
    _subscriptions: [Subscription; 2],
}

/// What "Browser" in an empty container becomes (Electron's handleAddBrowser).
pub(crate) fn new_content() -> ContainerContent {
    let mut c = ContainerContent { kind: ContentType::Browser, label: Some("Browser".into()), ..ContainerContent::empty() };
    c.set_extra_str("browserUrl", Some(NEW_PANE_URL.into()));
    c.set_extra_str("browserPartition", Some(DEFAULT_PARTITION.into()));
    c
}

/// Whether one pane's web view may be on screen this frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scene<'a> {
    /// Anything gpui draws over the grid: popovers, pickers, dialogs, menus, a pane drag.
    pub overlay: bool,
    /// The pane shown alone (focus mode), and whether its reveal is still animating.
    pub focus_mode: Option<&'a str>,
    pub focus_animating: bool,
}

impl Scene<'_> {
    pub fn visible(&self, id: &str, in_active_workspace: bool) -> bool {
        if self.overlay || !in_active_workspace {
            return false;
        }
        match self.focus_mode {
            None => true,
            // Only the lifted pane, once it has settled at full size (the
            // others are under its backdrop; the reveal is a gpui clip).
            Some(f) => f == id && !self.focus_animating,
        }
    }
}

impl AppView {
    /// This window's browser pane for a container.
    pub(crate) fn browser_pane(&self, container_id: &str) -> Option<Entity<BrowserPane>> {
        self.browser_panes.get(container_id).map(|e| e.pane.clone())
    }

    /// Make panes for the browser containers of every workspace (hidden ones
    /// too, so an agent can drive them), and drop those whose container is gone.
    pub(crate) fn ensure_browser_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut wanted = Vec::new();
        let mut alive = HashSet::new();
        for ws in &self.state.workspaces {
            for c in ws.containers.iter().filter(|c| c.content.kind == ContentType::Browser) {
                alive.insert(c.id.clone());
                if !self.browser_panes.contains_key(&c.id) {
                    wanted.push((c.id.clone(), c.content.extra_str("browserUrl"), c.content.extra_str("browserPartition"), ws.zoom(&c.id)));
                }
            }
        }
        let gone: Vec<String> = self.browser_panes.keys().filter(|id| !alive.contains(*id)).cloned().collect();
        for id in gone {
            self.drop_browser_pane(&id, cx);
        }
        for (id, url, partition, zoom) in wanted {
            self.create_browser_pane(id, url.unwrap_or_default(), partition, zoom, window, cx);
        }
    }

    /// The pane for a browser container made just now (an agent's `open`), so it
    /// can be driven before the next frame. Its existing pane if it has one.
    pub(crate) fn open_browser_pane(&mut self, container_id: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<Entity<BrowserPane>> {
        if let Some(e) = self.browser_panes.get(container_id) {
            return Some(e.pane.clone());
        }
        let (url, partition, zoom) = self.state.workspaces.iter().find_map(|w| {
            let c = w.container(container_id).filter(|c| c.content.kind == ContentType::Browser)?;
            Some((c.content.extra_str("browserUrl"), c.content.extra_str("browserPartition"), w.zoom(container_id)))
        })?;
        Some(self.create_browser_pane(container_id.to_owned(), url.unwrap_or_default(), partition, zoom, window, cx))
    }

    fn create_browser_pane(&mut self, id: String, url: String, partition: Option<String>, zoom: Option<f32>, window: &mut Window, cx: &mut Context<Self>) -> Entity<BrowserPane> {
        let pane = cx.new(|cx| BrowserPane::new(&url, partition.as_deref(), zoom, window, cx));
        let (a, b) = (id.clone(), id.clone());
        let subscriptions = [
            cx.subscribe(&pane, move |this, _, ev: &BrowserEvent, cx| this.on_browser_event(&a, ev, cx)),
            cx.subscribe_in(&pane, window, move |this, pane, ev: &BrowserPaneEvent, window, cx| match ev {
                BrowserPaneEvent::Focused => {
                    this.set_focused_pane(&b, cx);
                    let handle = pane.read(cx).focus.clone();
                    if !handle.contains_focused(window, cx) {
                        handle.focus(window, cx);
                    }
                    cx.notify();
                }
            }),
        ];
        self.browser_panes.insert(id, Entry { pane: pane.clone(), _subscriptions: subscriptions });
        pane
    }

    fn on_browser_event(&mut self, id: &str, ev: &BrowserEvent, cx: &mut Context<Self>) {
        match ev {
            // Saved so the pane reopens where it was (`browserUrl`).
            BrowserEvent::Navigated(url) => {
                if let Some(c) = self.state.workspaces.iter_mut().find_map(|w| w.container_mut(id))
                    && c.content.extra_str("browserUrl").as_deref() != Some(url.as_str())
                {
                    c.content.set_extra_str("browserUrl", Some(url.clone()));
                    self.changed(cx);
                }
            }
            // The header shows the page title.
            BrowserEvent::TitleChanged(_) => cx.notify(),
            BrowserEvent::LoadStarted | BrowserEvent::LoadFinished | BrowserEvent::Ipc(_) => {}
        }
    }

    /// The pane closed: remove its web view now.
    pub(crate) fn drop_browser_pane(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(entry) = self.browser_panes.remove(id) {
            entry.pane.update(cx, |p, _| p.destroy());
        }
    }

    /// Popovers and context menus open now: browser panes hide only under them.
    fn open_covers(&self, cx: &App) -> Vec<crate::overlay::Cover> {
        let mut open = Vec::new();
        if let Some(p) = &self.popover {
            open.push(crate::overlay::Cover {
                at: p.at,
                slot: 0,
                guess: p.guess,
            });
            if p.kind == crate::app::PopoverKind::AddWorkspace && self.recent_flyout {
                open.push(crate::overlay::Cover {
                    slot: 1,
                    ..crate::overlay::Cover::menu(p.at)
                });
            }
        }
        open.extend(self.procs.pop_cover());
        open.extend(
            self.sftp_panes
                .values()
                .filter_map(|(p, _)| p.read(cx).menu_cover()),
        );
        open.extend(
            self.ide_panes
                .values()
                .filter_map(|(p, _)| p.read(cx).menu_cover()),
        );
        for (p, _) in self.sql_panes.values() {
            open.extend(p.read(cx).menu_covers(cx));
        }
        open
    }

    /// Anything of the app's own drawn over the whole window, hiding every web view.
    fn overlay_open(&self) -> bool {
        self.procs.modal_open()
            || self.env_picker_open
            || self.settings_tab.is_some()
            || self.editor.is_some()
            || self.sync_settings.is_some()
            || matches!(self.drag, Some(Drag::Pane { active: true, .. }))
    }

    /// Decide which web views may show this frame (before the grid paints and
    /// places them): hidden ones go at once.
    pub(crate) fn present_browser_panes(&mut self, cx: &mut Context<Self>) {
        if self.browser_panes.is_empty() {
            return;
        }
        let active = self.state.active();
        let leaves: HashSet<String> = active.and_then(|w| w.layout_tree.as_ref()).map(|t| t.leaf_ids().into_iter().collect()).unwrap_or_default();
        let focused = active.and_then(|w| w.focused_container_id.clone());
        let focus_mode = self.focus_mode.clone().filter(|id| leaves.contains(id));
        let scene = Scene {
            overlay: self.overlay_open(),
            focus_mode: focus_mode.as_deref(),
            focus_animating: self.focus_anim.is_some(),
        };
        let open = self.open_covers(cx);
        let covers = crate::overlay::covered(&open, cx);
        let tiles = self.pane_bounds.borrow();
        for (id, entry) in &self.browser_panes {
            let covered = !covers.is_empty()
                && tiles
                    .get(id)
                    .is_none_or(|t| covers.iter().any(|c| t.intersects(c)));
            let visible = !covered && scene.visible(id, leaves.contains(id));
            let dimmed = focused.as_deref() != Some(id.as_str())
                && focus_mode.as_deref() != Some(id.as_str());
            entry
                .pane
                .update(cx, |p, _| p.set_presentation(visible, dimmed));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ContainerContent, DEFAULT_PARTITION, NEW_PANE_URL, Scene, new_content};

    #[test]
    fn shows_only_in_the_active_workspace_without_overlays() {
        let s = Scene { overlay: false, focus_mode: None, focus_animating: false };
        assert!(s.visible("a", true));
        assert!(!s.visible("a", false));
        let s = Scene { overlay: true, ..s };
        assert!(!s.visible("a", true));
    }

    #[test]
    fn focus_mode_shows_the_lifted_pane_once_settled() {
        let s = Scene { overlay: false, focus_mode: Some("a"), focus_animating: true };
        assert!(!s.visible("a", true));
        assert!(!s.visible("b", true));
        let s = Scene { focus_animating: false, ..s };
        assert!(s.visible("a", true));
        assert!(!s.visible("b", true));
    }

    #[test]
    fn new_content_round_trips_its_fields() {
        let c = new_content();
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["type"], "browser");
        assert_eq!(v["browserUrl"], NEW_PANE_URL);
        assert_eq!(v["browserPartition"], DEFAULT_PARTITION);
        // Fields this build doesn't know survive a load/save.
        let mut v2 = v.clone();
        v2["browserOwnerTerminal"] = "term-1".into();
        let back: ContainerContent = serde_json::from_value(v2.clone()).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), v2);
    }
}
