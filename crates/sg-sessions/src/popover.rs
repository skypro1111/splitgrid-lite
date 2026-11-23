//! The popover under the menu-bar icon: a header with the live count, the
//! session tree in an outline view (environment → workspace → session, each
//! row with Stop / Close, sessions with View too), and a footer with
//! "Close all sessions", "Open SplitGrid Lite" and Hide.
//!
//! The controller is the outline view's data source and delegate, the
//! popover's delegate, the status item button's target and every row
//! button's target. It owns the status item and the views; AppKit holds it
//! only weakly (delegates, targets), and `main` keeps it for the process.
//!
//! Items handed to the outline view are one `NSString` per node key, cached,
//! so item identity is stable across reloads; expansion state is our own
//! collapsed set (groups start expanded) replayed after each reload.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAppearance, NSAppearanceNameDarkAqua, NSButton, NSColor, NSControlTextEditingDelegate, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate, NSPopover,
    NSPopoverBehavior, NSPopoverDelegate, NSScrollView, NSStatusItem, NSTableColumn, NSTableViewSelectionHighlightStyle, NSTableViewStyle, NSTextAlignment, NSTextField,
    NSView, NSViewController,
};
use objc2_foundation::{NSInteger, NSNotification, NSPoint, NSRectEdge, NSSize, NSString, ns_string};

use crate::model::{self, Kind, Node, Op, Status};
use crate::ui::{self, Mask, rect};
use crate::{actions, scan, status_item, view_window};

const WIDTH: f64 = 380.0;
const MAX_HEIGHT: f64 = 520.0;
const HEADER: f64 = 44.0;
const FOOTER: f64 = 40.0;
const MIN_LIST: f64 = 64.0;
/// Room above and below the rows inside the scroll view.
const LIST_PAD: f64 = 10.0;
const BUTTON: f64 = 24.0;
/// A click on the icon right after the popover closed is the click that
/// closed it (transient: mouse-down outside), not a request to reopen.
const REOPEN_GUARD: Duration = Duration::from_millis(250);

pub struct Ivars {
    item: Retained<NSStatusItem>,
    popover: Retained<NSPopover>,
    outline: Retained<NSOutlineView>,
    scroll: Retained<NSScrollView>,
    count: Retained<NSTextField>,
    empty: Retained<NSTextField>,
    close_all: Retained<NSButton>,
    closed_at: Cell<Option<Instant>>,
    state: RefCell<State>,
}

#[derive(Default)]
struct State {
    nodes: Vec<Node>,
    /// Idle shells left out of the list (model::hide_idle).
    idle: Vec<PathBuf>,
    /// One stable item object per node key.
    items: HashMap<String, Retained<NSString>>,
    collapsed: HashSet<String>,
    /// Replaying expansion: ignore the expand/collapse notifications.
    syncing: bool,
    /// The outline view has been loaded once.
    loaded: bool,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SplitGridSessionsPopover"]
    #[ivars = Ivars]
    pub struct Popover;

    unsafe impl NSObjectProtocol for Popover {}

    unsafe impl NSOutlineViewDataSource for Popover {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn number_of_children(&self, _ov: &NSOutlineView, item: Option<&AnyObject>) -> NSInteger {
            let key = item.map(key_of);
            let state = self.ivars().state.borrow();
            match key {
                None => state.nodes.len() as NSInteger,
                Some(k) => model::find(&state.nodes, &k).map_or(0, |n| n.children.len() as NSInteger),
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child(&self, _ov: &NSOutlineView, index: NSInteger, item: Option<&AnyObject>) -> Retained<AnyObject> {
            let key = item.map(key_of);
            let child_key = {
                let state = self.ivars().state.borrow();
                let siblings = match &key {
                    None => Some(&state.nodes[..]),
                    Some(k) => model::find(&state.nodes, k).map(|n| &n.children[..]),
                };
                siblings.and_then(|s| s.get(index as usize)).map(|n| n.key.clone()).unwrap_or_default()
            };
            Retained::into_super(Retained::into_super(self.item_for(&child_key)))
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn expandable(&self, _ov: &NSOutlineView, item: &AnyObject) -> bool {
            let state = self.ivars().state.borrow();
            model::find(&state.nodes, &key_of(item)).is_some_and(|n| n.is_group() && !n.children.is_empty())
        }
    }

    unsafe impl NSControlTextEditingDelegate for Popover {}

    unsafe impl NSOutlineViewDelegate for Popover {
        #[unsafe(method_id(outlineView:viewForTableColumn:item:))]
        fn view_for(&self, _ov: &NSOutlineView, _column: Option<&NSTableColumn>, item: &AnyObject) -> Option<Retained<NSView>> {
            let node = {
                let state = self.ivars().state.borrow();
                model::find(&state.nodes, &key_of(item)).cloned()
            };
            node.map(|n| self.row_view(&n))
        }

        #[unsafe(method(outlineView:heightOfRowByItem:))]
        fn height_of(&self, _ov: &NSOutlineView, item: &AnyObject) -> f64 {
            let state = self.ivars().state.borrow();
            model::find(&state.nodes, &key_of(item)).map_or(model::SESSION_ROW, Node::row_height)
        }

        #[unsafe(method(outlineView:shouldSelectItem:))]
        fn should_select(&self, _ov: &NSOutlineView, _item: &AnyObject) -> bool {
            false
        }

        #[unsafe(method(outlineViewItemDidExpand:))]
        fn did_expand(&self, notification: &NSNotification) {
            self.expansion_changed(notification, true);
        }

        #[unsafe(method(outlineViewItemDidCollapse:))]
        fn did_collapse(&self, notification: &NSNotification) {
            self.expansion_changed(notification, false);
        }
    }

    unsafe impl NSPopoverDelegate for Popover {
        #[unsafe(method(popoverDidClose:))]
        fn popover_did_close(&self, _notification: &NSNotification) {
            self.ivars().closed_at.set(Some(Instant::now()));
            scan::set_fast(false);
        }
    }

    impl Popover {
        /// The status item's button.
        #[unsafe(method(toggle:))]
        fn toggle(&self, _sender: Option<&AnyObject>) {
            let iv = self.ivars();
            if iv.popover.isShown() {
                close(&iv.popover);
                return;
            }
            if iv.closed_at.get().is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
                return;
            }
            let Some(button) = iv.item.button(self.mtm()) else { return };
            scan::set_fast(true);
            scan::rescan_soon();
            self.refresh();
            // An accessory app must be active for the transient popover to
            // take key (Escape) and to close on a click elsewhere.
            actions::bring_forward(self.mtm());
            iv.popover.showRelativeToRect_ofView_preferredEdge(button.bounds(), &button, NSRectEdge::MinY);
        }

        #[unsafe(method(rowStop:))]
        fn row_stop(&self, sender: &NSView) {
            self.row_action(sender, Op::Stop);
        }

        #[unsafe(method(rowClose:))]
        fn row_close(&self, sender: &NSView) {
            self.row_action(sender, Op::Close);
        }

        #[unsafe(method(rowView:))]
        fn row_view_action(&self, sender: &NSView) {
            let row = self.ivars().outline.rowForView(sender);
            self.view_row(row);
        }

        #[unsafe(method(rowDoubleClick:))]
        fn row_double_click(&self, _sender: Option<&AnyObject>) {
            let row = self.ivars().outline.clickedRow();
            self.view_row(row);
        }

        #[unsafe(method(closeAll:))]
        fn close_all(&self, _sender: Option<&AnyObject>) {
            let (sockets, (msg, why)) = {
                let state = self.ivars().state.borrow();
                let mut sockets = model::targets(&state.nodes, Op::Close);
                sockets.extend(state.idle.iter().cloned());
                (sockets, model::confirm_all(&state.nodes, state.idle.len()))
            };
            if !sockets.is_empty() && self.ask(&msg, &why) {
                actions::close(sockets);
            }
        }

        #[unsafe(method(openLite:))]
        fn open_lite(&self, _sender: Option<&AnyObject>) {
            close(&self.ivars().popover);
            actions::open_lite();
        }

        #[unsafe(method(hide:))]
        fn hide(&self, _sender: Option<&AnyObject>) {
            actions::quit(self.mtm());
        }
    }
);

fn close(popover: &NSPopover) {
    // SAFETY: no sender.
    unsafe { popover.performClose(None) };
}

/// The key an outline item stands for (our items are NSStrings).
fn key_of(item: &AnyObject) -> String {
    item.downcast_ref::<NSString>().map(|s| s.to_string()).unwrap_or_default()
}

impl Popover {
    /// Build the popover and take over the status item's button.
    pub fn new(item: Retained<NSStatusItem>, mtm: MainThreadMarker) -> Retained<Self> {
        let height = HEADER + MIN_LIST + FOOTER;
        let root = NSView::initWithFrame(NSView::alloc(mtm), rect(0., 0., WIDTH, height));

        let title = ui::label("Sessions", &ui::font(14., ui::semibold()), &NSColor::labelColor(), rect(16., height - 31., 140., 19.), Mask::ViewMinYMargin, mtm);
        let count = ui::label("", &ui::font(12., ui::regular()), &NSColor::secondaryLabelColor(), rect(WIDTH - 196., height - 30., 180., 16.), Mask::ViewMinYMargin | Mask::ViewMinXMargin, mtm);
        count.setAlignment(NSTextAlignment::Right);
        let top_rule = ui::separator(rect(0., height - HEADER, WIDTH, 1.), Mask::ViewMinYMargin | Mask::ViewWidthSizable, mtm);

        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), rect(0., FOOTER, WIDTH, height - HEADER - FOOTER));
        scroll.setAutoresizingMask(Mask::ViewWidthSizable | Mask::ViewHeightSizable);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        let outline = NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), rect(0., 0., WIDTH, height - HEADER - FOOTER));
        let column = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), ns_string!("session"));
        column.setResizingMask(objc2_app_kit::NSTableColumnResizingOptions::AutoresizingMask);
        outline.addTableColumn(&column);
        // SAFETY: a column of this outline view.
        unsafe { outline.setOutlineTableColumn(Some(&column)) };
        outline.setHeaderView(None);
        outline.setStyle(NSTableViewStyle::SourceList);
        outline.setSelectionHighlightStyle(NSTableViewSelectionHighlightStyle::None);
        outline.setBackgroundColor(&NSColor::clearColor());
        outline.setIntercellSpacing(NSSize::new(0., model::ROW_GAP));
        outline.setIndentationPerLevel(14.);
        outline.setFloatsGroupRows(false);
        outline.setAutoresizesOutlineColumn(false);
        outline.setColumnAutoresizingStyle(objc2_app_kit::NSTableViewColumnAutoresizingStyle::FirstColumnOnlyAutoresizingStyle);
        outline.setAutoresizingMask(Mask::ViewWidthSizable);
        // SAFETY: plain flag; our items are kept alive by `State::items` too.
        unsafe { outline.setStronglyReferencesItems(true) };
        scroll.setDocumentView(Some(&outline));
        outline.sizeLastColumnToFit();

        let empty = ui::label(
            "Nothing is running",
            &ui::font(12., ui::regular()),
            &NSColor::secondaryLabelColor(),
            rect(0., FOOTER + 22., WIDTH, 18.),
            Mask::ViewWidthSizable | Mask::ViewMinYMargin | Mask::ViewMaxYMargin,
            mtm,
        );
        empty.setAlignment(NSTextAlignment::Center);
        empty.setHidden(true);

        let bottom_rule = ui::separator(rect(0., FOOTER - 1., WIDTH, 1.), Mask::ViewMaxYMargin | Mask::ViewWidthSizable, mtm);

        let popover = NSPopover::new(mtm);
        popover.setBehavior(NSPopoverBehavior::Transient);
        popover.setAnimates(true);
        // SAFETY: an AppKit constant.
        popover.setAppearance(NSAppearance::appearanceNamed(unsafe { NSAppearanceNameDarkAqua }).as_deref());
        popover.setContentSize(NSSize::new(WIDTH, height));

        // Footer buttons; their target (the controller) is set after init.
        let close_all = ui::text_button(
            "Close all sessions…",
            "xmark.circle",
            &NSColor::systemRedColor(),
            sel!(closeAll:),
            rect(10., 8., 160., 24.),
            mtm,
        );
        close_all.setAutoresizingMask(Mask::ViewMaxYMargin | Mask::ViewMaxXMargin);
        let open = ui::text_button(
            "Open SplitGrid Lite",
            "arrow.up.forward.app",
            &NSColor::secondaryLabelColor(),
            sel!(openLite:),
            rect(WIDTH - 10. - 28. - 156., 8., 156., 24.),
            mtm,
        );
        open.setAutoresizingMask(Mask::ViewMaxYMargin | Mask::ViewMinXMargin);
        let hide = ui::icon_button(
            "power",
            "Hide the icon until terminals start again",
            None,
            sel!(hide:),
            rect(WIDTH - 10. - 26., 8., 26., 24.),
            mtm,
        );
        hide.setAutoresizingMask(Mask::ViewMaxYMargin | Mask::ViewMinXMargin);
        for v in [
            &*title as &NSView,
            &count,
            &top_rule,
            &scroll,
            &empty,
            &bottom_rule,
            &close_all,
            &open,
            &hide,
        ] {
            root.addSubview(v);
        }

        let this = Self::alloc(mtm).set_ivars(Ivars {
            item,
            popover,
            outline,
            scroll,
            count,
            empty,
            close_all,
            closed_at: Cell::new(None),
            state: RefCell::default(),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        let iv = this.ivars();
        let target: &AnyObject = this.as_ref();
        for b in [&iv.close_all, &open, &hide] {
            // SAFETY: the controller lives for the process.
            unsafe { b.setTarget(Some(target)) };
        }

        let vc = NSViewController::new(mtm);
        vc.setView(&root);
        iv.popover.setContentViewController(Some(&vc));
        iv.popover.setDelegate(Some(ProtocolObject::from_ref(&*this)));

        // SAFETY: we are the outline view's data source and delegate for good.
        unsafe {
            iv.outline.setDataSource(Some(ProtocolObject::from_ref(&*this)));
            iv.outline.setDelegate(Some(ProtocolObject::from_ref(&*this)));
            iv.outline.setTarget(Some(target));
            iv.outline.setDoubleAction(Some(sel!(rowDoubleClick:)));
        }
        if let Some(button) = iv.item.button(mtm) {
            // SAFETY: the controller lives for the process.
            unsafe {
                button.setTarget(Some(target));
                button.setAction(Some(sel!(toggle:)));
            }
        }
        this
    }

    fn item_for(&self, key: &str) -> Retained<NSString> {
        let mut state = self.ivars().state.borrow_mut();
        state.items.entry(key.to_owned()).or_insert_with(|| NSString::from_str(key)).clone()
    }

    /// Every second: the status item's count, and the list (reloaded only
    /// when it changed, so a closed popover opens already filled).
    pub fn tick(&self) {
        self.refresh();
    }

    /// Rebuild the tree from the last scan; reload the outline view only when
    /// something changed, keeping expansion and scroll position.
    pub fn refresh(&self) {
        let iv = self.ivars();
        let scan = scan::last();
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        let home = std::env::var("HOME").ok();
        let (nodes, idle) = model::hide_idle(model::build(
            scan.sessions,
            &scan.broken,
            now_ms,
            home.as_deref(),
        ));
        let total = model::total(&nodes);
        status_item::set_count(&iv.item, total, self.mtm());
        let busy: usize = nodes.iter().map(Node::busy).sum();
        let mut count_text = match (total, busy) {
            (0, _) => String::new(),
            (n, 0) => format!("{n} running"),
            (n, b) => format!("{n} running  ·  {b} busy"),
        };
        if !idle.is_empty() {
            let sep = if count_text.is_empty() { "" } else { "  ·  " };
            count_text.push_str(&format!("{sep}{} idle hidden", idle.len()));
        }
        iv.count.setStringValue(&NSString::from_str(&count_text));
        iv.empty.setHidden(total != 0);
        iv.close_all.setEnabled(total + idle.len() != 0);
        iv.state.borrow_mut().idle = idle;

        let changed = {
            let mut state = iv.state.borrow_mut();
            if state.loaded && state.nodes == nodes {
                false
            } else {
                model::prune_collapsed(&nodes, &mut state.collapsed);
                state.nodes = nodes;
                state.loaded = true;
                true
            }
        };
        if !changed {
            return;
        }
        let (list, expansion) = {
            let state = iv.state.borrow();
            (model::list_height(&state.nodes, &state.collapsed), model::expansion(&state.nodes, &state.collapsed))
        };
        self.fit(list);

        let clip = iv.scroll.contentView();
        let origin = clip.bounds().origin;
        iv.outline.reloadData();
        self.replay(&expansion);
        // Forget items of nodes that are gone (the reload has let go of them).
        {
            let mut state = iv.state.borrow_mut();
            let live: HashSet<String> = model::visible(&state.nodes, &HashSet::new()).into_iter().map(|(_, n)| n.key.clone()).collect();
            state.items.retain(|k, _| live.contains(k));
        }
        let max_y = (iv.outline.frame().size.height - clip.bounds().size.height).max(0.);
        clip.scrollToPoint(NSPoint::new(origin.x, origin.y.clamp(0., max_y)));
        iv.scroll.reflectScrolledClipView(&clip);
    }

    /// Size the popover to the list (clamped).
    fn fit(&self, list: f64) {
        let iv = self.ivars();
        let body = (list + LIST_PAD).clamp(MIN_LIST, MAX_HEIGHT - HEADER - FOOTER);
        let size = NSSize::new(WIDTH, HEADER + body + FOOTER);
        let now = iv.popover.contentSize();
        if (now.height - size.height).abs() > 0.5 || (now.width - size.width).abs() > 0.5 {
            iv.popover.setContentSize(size);
        }
    }

    /// Expand / collapse groups as recorded (parents first).
    fn replay(&self, expansion: &[(String, bool)]) {
        let iv = self.ivars();
        iv.state.borrow_mut().syncing = true;
        for (key, open) in expansion {
            let item = self.item_for(key);
            let any: &AnyObject = item.as_ref();
            // SAFETY: an item of this outline view.
            unsafe {
                if *open {
                    iv.outline.expandItem(Some(any));
                } else {
                    iv.outline.collapseItem(Some(any));
                }
            }
        }
        iv.state.borrow_mut().syncing = false;
    }

    /// The user expanded or collapsed a group: remember it; on expand, also
    /// open its subgroups as they were (AppKit shows them collapsed).
    fn expansion_changed(&self, notification: &NSNotification, open: bool) {
        let iv = self.ivars();
        if iv.state.borrow().syncing {
            return;
        }
        let Some(item) = notification.userInfo().and_then(|info| info.objectForKey(ns_string!("NSObject"))) else { return };
        let key = key_of(&item);
        let sub = {
            let mut state = iv.state.borrow_mut();
            if open {
                state.collapsed.remove(&key);
            } else {
                state.collapsed.insert(key.clone());
            }
            let sub = if open { model::find(&state.nodes, &key).map(|n| model::expansion(&n.children, &state.collapsed)).unwrap_or_default() } else { Vec::new() };
            (sub, model::list_height(&state.nodes, &state.collapsed))
        };
        self.replay(&sub.0);
        self.fit(sub.1);
    }

    /// Stop or Close on the row holding `sender`.
    fn row_action(&self, sender: &NSView, op: Op) {
        let row = self.ivars().outline.rowForView(sender);
        let Some(node) = self.node_at(row) else { return };
        let sockets = model::targets([&node], op);
        if sockets.is_empty() {
            return;
        }
        if let Some((msg, why)) = model::confirm(&node, op)
            && !self.ask(&msg, &why)
        {
            return;
        }
        match op {
            Op::Stop => actions::stop(sockets),
            Op::Close => actions::close(sockets),
        }
    }

    fn view_row(&self, row: NSInteger) {
        let Some(node) = self.node_at(row) else {
            return;
        };
        let Some(s) = node.session().filter(|s| s.reachable) else {
            return;
        };
        close(&self.ivars().popover);
        view_window::open(s.socket.clone(), &s.title, self.mtm());
    }

    fn node_at(&self, row: NSInteger) -> Option<Node> {
        if row < 0 {
            return None;
        }
        let item = self.ivars().outline.itemAtRow(row)?;
        let state = self.ivars().state.borrow();
        model::find(&state.nodes, &key_of(&item)).cloned()
    }

    /// Ask with the popover held open (a transient one would close when the
    /// alert takes focus); no state borrow is held across the modal loop.
    fn ask(&self, message: &str, informative: &str) -> bool {
        let popover = &self.ivars().popover;
        popover.setBehavior(NSPopoverBehavior::ApplicationDefined);
        let yes = actions::ask_end(message, informative, self.mtm());
        popover.setBehavior(NSPopoverBehavior::Transient);
        yes
    }

    /// One row's view: built fresh (rows are few), laid out with
    /// autoresizing so it follows the column's width.
    fn row_view(&self, node: &Node) -> Retained<NSView> {
        let mtm = self.mtm();
        let target: &AnyObject = self.as_ref();
        let h = node.row_height();
        let w = WIDTH - 40.;
        let cell = NSView::initWithFrame(NSView::alloc(mtm), rect(0., 0., w, h));
        cell.setAutoresizingMask(Mask::ViewWidthSizable);

        // Buttons from the right: Close, Stop, then View on sessions.
        let mut right = w - 2.;
        let mut button = |symbol: &str, tip: &str, action, enabled: bool| {
            right -= BUTTON;
            let b = ui::icon_button(
                symbol,
                tip,
                Some(target),
                action,
                rect(right, (h - 22.) / 2., BUTTON, 22.),
                mtm,
            );
            b.setAutoresizingMask(Mask::ViewMinXMargin);
            b.setEnabled(enabled);
            cell.addSubview(&b);
        };
        let can_stop = !model::targets([node], Op::Stop).is_empty();
        let what = match node.kind {
            Kind::Env => "in this environment",
            Kind::Workspace => "in this workspace",
            Kind::Session(_) => "",
        };
        if node.is_group() {
            button("xmark.circle", &format!("End every terminal {what}"), sel!(rowClose:), true);
            button("stop.circle", &format!("Interrupt every running job {what} (Ctrl-C)"), sel!(rowStop:), can_stop);
        } else {
            let reachable = node.session().is_some_and(|s| s.reachable);
            button("xmark.circle", "End this terminal", sel!(rowClose:), true);
            button("stop.circle", "Interrupt the running job (Ctrl-C)", sel!(rowStop:), can_stop);
            button("eye", "View its output (read-only)", sel!(rowView:), reachable);
        }
        right -= 4.;

        match &node.kind {
            Kind::Session(s) => {
                let (dot, tip) = match s.status {
                    Status::Busy => (NSColor::systemGreenColor(), "Running a job"),
                    Status::Idle => (NSColor::tertiaryLabelColor(), "Idle at the shell prompt"),
                    Status::Unknown => (NSColor::systemOrangeColor(), "From another SplitGrid version"),
                };
                let d = ui::label("●", &ui::font(9., ui::regular()), &dot, rect(0., (h - 12.) / 2., 12., 12.), Mask::ViewNotSizable, mtm);
                d.setToolTip(Some(&NSString::from_str(tip)));
                cell.addSubview(&d);
                if s.attached {
                    right -= 18.;
                    if let Some(image) = ui::symbol("macwindow", 11., ui::regular()) {
                        let iv = objc2_app_kit::NSImageView::imageViewWithImage(&image, mtm);
                        iv.setContentTintColor(Some(&NSColor::secondaryLabelColor()));
                        iv.setFrame(rect(right, (h - 16.) / 2., 16., 16.));
                        iv.setAutoresizingMask(Mask::ViewMinXMargin);
                        iv.setToolTip(Some(ns_string!("Open in SplitGrid Lite")));
                        cell.addSubview(&iv);
                    }
                }
                let text_w = (right - 16.).max(40.);
                let title = ui::label(&s.title, &ui::font(13., ui::regular()), &NSColor::labelColor(), rect(16., h / 2., text_w, 17.), Mask::ViewWidthSizable, mtm);
                let detail = ui::label(&s.detail, &ui::font(11., ui::regular()), &NSColor::secondaryLabelColor(), rect(16., h / 2. - 15., text_w, 14.), Mask::ViewWidthSizable, mtm);
                detail.setToolTip(Some(&NSString::from_str(&s.detail)));
                cell.addSubview(&title);
                cell.addSubview(&detail);
            }
            Kind::Env | Kind::Workspace => {
                let env = matches!(node.kind, Kind::Env);
                let n = node.count();
                let count = ui::label(&n.to_string(), &ui::font(11., ui::regular()), &NSColor::secondaryLabelColor(), rect(right - 30., (h - 14.) / 2., 30., 14.), Mask::ViewMinXMargin, mtm);
                count.setAlignment(NSTextAlignment::Right);
                count.setToolTip(Some(&NSString::from_str(&format!("{n} {}", if n == 1 { "terminal" } else { "terminals" }))));
                let (font, color) = if env { (ui::font(12., ui::semibold()), NSColor::labelColor()) } else { (ui::font(12., ui::medium()), NSColor::secondaryLabelColor()) };
                let name = ui::label(&node.name, &font, &color, rect(2., (h - 16.) / 2., (right - 36.).max(40.), 16.), Mask::ViewWidthSizable, mtm);
                cell.addSubview(&name);
                cell.addSubview(&count);
            }
        }
        cell
    }
}
