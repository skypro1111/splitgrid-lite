//! Settings → General → Appearance: the background colour (the Electron app's
//! BackgroundPicker: presets plus a free colour, clamped into a dark range) and
//! Lite's own font settings: the interface font, and the terminal and
//! editor/SQL fonts with the sizes new panes start at.
//!
//! The selects and the colour field have state of their own, made once a
//! window is at hand (`ensure_appearance_controls`, from AppView::render).

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableListItem;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::{IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{card, row, set_values};
use crate::app::AppView;
use crate::app_settings::{self as prefs, FONT_SIZES};
use crate::background;
use crate::theme::*;
use crate::ui::px;

/// One font in a select: its family, drawn in itself.
#[derive(Clone)]
pub struct FontChoice {
    title: SharedString,
    family: SharedString,
}

impl SearchableListItem for FontChoice {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.title.clone()
    }

    fn render(&self, _: &mut Window, _: &mut App) -> impl IntoElement {
        div().font_family(self.family.clone()).child(self.title.clone())
    }

    fn value(&self) -> &SharedString {
        &self.family
    }
}

type FontSelect = Entity<SelectState<Vec<FontChoice>>>;

#[derive(Default)]
pub struct AppearanceUi {
    color: Option<Entity<InputState>>,
    ui_font: Option<FontSelect>,
    terminal_font: Option<FontSelect>,
    editor_font: Option<FontSelect>,
    /// The typed colour isn't one.
    bad_color: bool,
    _subs: Vec<Subscription>,
}

/// The installed families (once per run): all of them, and the monospaced ones.
fn families(cx: &App) -> (Vec<SharedString>, Vec<SharedString>) {
    thread_local!(static CACHE: std::cell::OnceCell<(Vec<SharedString>, Vec<SharedString>)> = const { std::cell::OnceCell::new() });
    CACHE.with(|c| {
        c.get_or_init(|| {
            use std::sync::atomic::Ordering;
            let text = cx.text_system();
            FONT_PROBE.store(true, Ordering::Relaxed);
            // What a family that doesn't load resolves to.
            let fallback = text.resolve_font(&font("SplitGrid-no-such-font"));
            let mut names: Vec<String> = text.all_font_names().into_iter().filter(|n| !n.starts_with('.')).collect();
            names.sort_by_key(|n| n.to_lowercase());
            names.dedup();
            let size = gpui_kit::px(13.);
            let mut all = Vec::new();
            let mut mono = Vec::new();
            for name in names {
                let name = SharedString::from(name);
                let id = text.resolve_font(&font(name.clone()));
                // Families with no regular face, or no Latin letters, can't be used here.
                if id == fallback {
                    continue;
                }
                let w = |ch| text.advance(id, size, ch).map(|s| f32::from(s.width)).unwrap_or(0.);
                let (i, m) = (w('i'), w('M'));
                if i <= 0. || m <= 0. {
                    continue;
                }
                if (i - m).abs() < 0.01 {
                    mono.push(name.clone());
                }
                all.push(name);
            }
            FONT_PROBE.store(false, Ordering::Relaxed);
            (all, mono)
        })
        .clone()
    })
}

/// A family the settings name may not be installed (another Mac's settings): list it anyway.
fn choices(list: &[SharedString], current: &SharedString, system: bool) -> Vec<FontChoice> {
    let mut out: Vec<FontChoice> = Vec::new();
    if system {
        out.push(FontChoice { title: "System (SF Pro)".into(), family: UI_FONT.into() });
    }
    if !list.contains(current) && current.as_ref() != UI_FONT {
        out.push(FontChoice { title: current.clone(), family: current.clone() });
    }
    out.extend(list.iter().map(|f| FontChoice { title: f.clone(), family: f.clone() }));
    out
}

fn select_index(items: &[FontChoice], current: &SharedString) -> Option<IndexPath> {
    items.iter().position(|c| &c.family == current).map(IndexPath::new)
}

impl AppView {
    /// The colour field and the font selects, made the first time General shows.
    pub(crate) fn ensure_appearance_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.manager.appearance.color.is_some() {
            return;
        }
        let all = self.store.app_settings();
        let fonts = prefs::fonts(&all);
        let (every, mono) = families(cx);
        let mut subs = Vec::new();

        let color = cx.new(|cx| InputState::new(window, cx).placeholder(background::DEFAULT_COLOR).default_value(background::from_settings(&all)));
        subs.push(cx.subscribe_in(&color, window, |this, input, ev: &InputEvent, window, cx| {
            if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                return;
            }
            let typed = input.read(cx).value().trim().to_string();
            match background::parse(&typed) {
                Some(c) => {
                    this.manager.appearance.bad_color = false;
                    // Show what it was clamped to.
                    if c != typed {
                        input.update(cx, |i, cx| i.set_value(c.clone(), window, cx));
                    }
                    set_values(this, &[("backgroundColor", c.into())], cx);
                }
                None => {
                    this.manager.appearance.bad_color = !typed.is_empty();
                    cx.notify();
                }
            }
        }));

        let mut make = |key: &'static str, list: &[SharedString], current: &SharedString, system: bool, cx: &mut Context<Self>| {
            let items = choices(list, current, system);
            let selected = select_index(&items, current);
            let state = cx.new(|cx| SelectState::new(items, selected, window, cx).searchable(true));
            subs.push(cx.subscribe(&state, move |this, _, ev: &SelectEvent<Vec<FontChoice>>, cx| {
                let SelectEvent::Confirm(Some(family)) = ev else { return };
                set_values(this, &[(key, Value::String(family.to_string()))], cx);
            }));
            state
        };
        let ui_font = make("uiFontFamily", &every, &fonts.ui, true, cx);
        let terminal_font = make("terminalFontFamily", &mono, &fonts.terminal, false, cx);
        let editor_font = make("editorFontFamily", &mono, &fonts.editor, false, cx);

        self.manager.appearance = AppearanceUi { color: Some(color), ui_font: Some(ui_font), terminal_font: Some(terminal_font), editor_font: Some(editor_font), bad_color: false, _subs: subs };
    }

    /// Follow appearance settings saved elsewhere (another window, the Electron app).
    pub(crate) fn sync_appearance_controls(&mut self, all: &prefs::Settings, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &self.manager.appearance;
        if let Some(input) = &ui.color {
            let c = background::from_settings(all);
            let focused = input.read(cx).focus_handle(cx).is_focused(window);
            if !focused && input.read(cx).value().as_ref() != c {
                input.update(cx, |i, cx| i.set_value(c, window, cx));
            }
        }
        let fonts = prefs::fonts(all);
        for (select, current) in [(&ui.ui_font, &fonts.ui), (&ui.terminal_font, &fonts.terminal), (&ui.editor_font, &fonts.editor)] {
            if let Some(select) = select
                && select.read(cx).selected_value() != Some(current)
            {
                select.update(cx, |s, cx| s.set_selected_value(current, window, cx));
            }
        }
    }

    pub(super) fn render_appearance(&self, window: &mut Window, cx: &mut Context<Self>) -> Div {
        let all = self.store.app_settings();
        let current = background::from_settings(&all);
        let fonts = prefs::fonts(&all);
        let ui = &self.manager.appearance;

        let swatches = div().flex().flex_wrap().gap(px(8.)).children(background::PRESETS.iter().map(|(label, color)| {
            let selected = background::normalize(Some(color)) == current;
            let base = background::derive(Some(color));
            div()
                .id(SharedString::from(format!("bg-{label}")))
                .flex()
                .flex_col()
                .items_center()
                .gap(px(5.))
                .cursor_pointer()
                .child(
                    div()
                        .size(px(34.))
                        .rounded(px(9.))
                        .bg(base.primary)
                        .border(px(if selected { 2. } else { 1. }))
                        .border_color(if selected { ACCENT } else { base.border })
                        .child(div().m(px(9.)).size(px(14.)).rounded(px(4.)).bg(base.hover)),
                )
                .child(div().text_size(px(10.5)).text_color(if selected { TEXT_PRIMARY } else { TEXT_MUTED }).child(*label))
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(input) = &this.manager.appearance.color {
                        input.update(cx, |i, cx| i.set_value(color.to_string(), window, cx));
                    }
                    this.manager.appearance.bad_color = false;
                    set_values(this, &[("backgroundColor", Value::String(color.to_string()))], cx);
                }))
        }));

        let color_field = ui.color.as_ref().map(|input| {
            let focused = input.read(cx).focus_handle(cx).is_focused(window);
            div()
                .w(px(110.))
                .px(px(9.))
                .py(px(5.))
                .rounded(px(8.))
                .bg(bg_primary())
                .border_1()
                .border_color(if focused { ACCENT } else { border() })
                .font_family(fonts.editor.clone())
                .text_size(px(12.))
                .child(Input::new(input).appearance(false).small())
        });

        let size_stepper = |id: &'static str, key: &'static str, size: f32, cx: &mut Context<Self>| {
            let step = |d: f32, name: &'static str, glyph: &'static str, cx: &mut Context<Self>| {
                let next = (size + d).clamp(*FONT_SIZES.start() as f32, *FONT_SIZES.end() as f32);
                let enabled = next != size;
                div()
                    .id(SharedString::from(format!("{id}-{name}")))
                    .w(px(26.))
                    .h(px(26.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(7.))
                    .border_1()
                    .border_color(border())
                    .bg(bg_primary())
                    .text_size(px(13.))
                    .text_color(if enabled { TEXT_PRIMARY } else { TEXT_MUTED })
                    .when(enabled, |d| d.cursor_pointer().hover(|d| d.bg(bg_hover())))
                    .child(glyph)
                    .when(enabled, |d| d.on_click(cx.listener(move |this, _, _, cx| set_values(this, &[(key, Value::from(next as f64))], cx))))
            };
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .child(step(-1., "minus", "−", cx))
                .child(div().w(px(36.)).text_center().text_size(px(12.)).text_color(TEXT_PRIMARY).child(format!("{size:.0} pt")))
                .child(step(1., "plus", "+", cx))
        };

        let select = |s: &Option<FontSelect>, id: &'static str| s.as_ref().map(|s| div().w(px(260.)).child(Select::new(s).id(id).small().menu_max_h(px(320.))));
        let reset_fonts = fonts != prefs::default_fonts();

        card("Appearance", None)
            .child(div().mt(px(14.)))
            .child(row("Background").items_start().child(swatches))
            .child(
                row("Custom colour")
                    .children(color_field)
                    .child(div().text_size(px(11.)).text_color(TEXT_MUTED).child("Press Enter to apply")),
            )
            .child(super::hint(if ui.bad_color {
                "That isn't a colour — use #rrggbb or #rgb."
            } else {
                "Panels, inputs, borders, terminals and editors are all derived from this colour. It is nudged into a dark range so the light text stays readable. Shared with the SplitGrid app."
            }))
            .child(row("Interface font").children(select(&ui.ui_font, "font-ui")))
            .child(row("Terminal font").children(select(&ui.terminal_font, "font-terminal")).child(size_stepper("term-size", "terminalFontSize", fonts.terminal_size, cx)))
            .child(row("Editor & SQL font").children(select(&ui.editor_font, "font-editor")).child(size_stepper("editor-size", "editorFontSize", fonts.editor_size, cx)))
            .child(super::hint("The sizes are where new panes start, and where ⌘0 returns a pane to; ⌘+ / ⌘− still zoom each pane on its own. The font lists for the terminal and the editor show monospaced fonts only."))
            .when(reset_fonts, |d| {
                d.child(
                    super::ghost_button("fonts-reset", "Reset fonts").on_click(cx.listener(|this, _, _, cx| {
                        set_values(
                            this,
                            &[("uiFontFamily", Value::Null), ("terminalFontFamily", Value::Null), ("terminalFontSize", Value::Null), ("editorFontFamily", Value::Null), ("editorFontSize", Value::Null)],
                            cx,
                        )
                    })),
                )
            })
    }

    /// Take a new background or fonts: the palette and fonts every view reads,
    /// then the panes without a zoom of their own move to the new default sizes.
    pub(crate) fn follow_appearance(&mut self, all: &prefs::Settings, cx: &mut Context<Self>) {
        let want = (background::derive(Some(&background::from_settings(all))), prefs::fonts(all));
        if want != (palette(), fonts()) {
            prefs::apply_appearance(all, cx);
        }
        let f = fonts();
        let unzoomed = |this: &Self, id: &str| this.state.workspaces.iter().find(|w| w.container(id).is_some()).is_some_and(|w| w.zoom(id).is_none());
        let terminals: Vec<_> = self.panes.iter().filter(|(id, _)| unzoomed(self, id)).map(|(_, p)| p.view.clone()).collect();
        for view in terminals {
            if view.read(cx).font_size() != f.terminal_size {
                view.update(cx, |v, cx| v.set_font_size(f.terminal_size, cx));
            }
        }
        let ides: Vec<_> = self.ide_panes.iter().filter(|(id, _)| unzoomed(self, id)).map(|(_, (ide, _))| ide.clone()).collect();
        for ide in ides {
            if ide.read(cx).font_size() != f.editor_size {
                ide.update(cx, |v, cx| v.set_font_size(f.editor_size, cx));
            }
        }
        cx.notify();
    }
}

/// The settings keys that change the look.
pub const KEYS: [&str; 6] = ["backgroundColor", "uiFontFamily", "terminalFontFamily", "terminalFontSize", "editorFontFamily", "editorFontSize"];
