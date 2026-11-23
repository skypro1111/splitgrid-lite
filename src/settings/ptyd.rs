//! Settings → General → Persistent terminals (crate::ptyd): the switch, the
//! menu-bar icon, and the sessions' status with "End all sessions".

use std::sync::Mutex;
use std::time::{Duration, Instant};

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{card, ghost_button, set, toggle};
use crate::app::AppView;
use crate::theme::*;
use crate::ui::px;

/// "End all sessions" asks once more within this long.
const CONFIRM_WINDOW: Duration = Duration::from_secs(5);
static END_ALL_ARMED: Mutex<Option<Instant>> = Mutex::new(None);

fn armed() -> bool {
    END_ALL_ARMED.lock().unwrap().is_some_and(|t| t.elapsed() < CONFIRM_WINDOW)
}

impl AppView {
    pub(super) fn render_ptyd(&self, cx: &mut Context<Self>) -> Div {
        let all = self.store.app_settings();
        let on = all.get(crate::ptyd::SETTING).and_then(serde_json::Value::as_bool).unwrap_or(false);
        let menu = all.get(crate::ptyd::MENU_SETTING).and_then(serde_json::Value::as_bool) != Some(false);
        let st = crate::ptyd::status();

        let mut body = div().mt(px(14.)).child(toggle(
            "persistent-terminals",
            "Keep terminals running after quitting",
            Some("Each local terminal runs in its own small background process (splitgrid-ptyd), so quitting SplitGrid Lite, closing its window or a crash leaves the shell and what runs in it alive; it reconnects with its recent output on the next start. Closing a pane, workspace or environment still ends its terminals. Applies to new terminals: turning it off leaves running ones until they are closed. SSH terminals are not affected."),
            on,
            false,
            |t, v, cx| {
                set(t, &[(crate::ptyd::SETTING, v)], cx);
                if v {
                    crate::ptyd::install_in_background();
                }
            },
            cx,
        ));

        if on || st.live > 0 {
            body = body.child(div().ml(px(22.)).mt(px(14.)).child(toggle(
                "ptyd-menu",
                "Show menu bar icon",
                Some("Lists the running terminals by environment and workspace while SplitGrid Lite is closed, to look at or end them. Shown while terminals are running."),
                menu,
                false,
                |t, v, cx| {
                    set(t, &[(crate::ptyd::MENU_SETTING, v)], cx);
                    if v {
                        std::thread::spawn(crate::ptyd::ensure_menu);
                    }
                },
                cx,
            )));

            let mut lines: Vec<String> = Vec::new();
            lines.push(match &st.installed {
                Some(name) => format!("Installed: {name}"),
                None if on => "Installed: not yet (when the first terminal starts)".into(),
                None => "Installed: —".into(),
            });
            if st.checked.is_some() {
                lines.push(format!("{} running · {} shown in a window", st.live, st.attached));
            }
            if st.incompatible > 0 {
                lines.push(format!("{} from another SplitGrid Lite version (can't be shown; End all sessions ends them)", st.incompatible));
            }
            let orphans = st.orphans.clone();
            let n_orphans = orphans.len();
            let confirm = armed();
            body = body.child(
                div().mt(px(14.)).text_size(px(12.)).line_height(px(18.)).text_color(TEXT_MUTED).children(lines.into_iter().map(|l| div().child(l))).when(
                    n_orphans > 0,
                    |d| {
                        d.child(
                            div()
                                .mt(px(8.))
                                .flex()
                                .items_center()
                                .gap(px(10.))
                                .child(div().flex_1().child(format!("{n_orphans} belong to no workspace any more.")))
                                .child(ghost_button("ptyd-end-orphans", "End those").on_click(cx.listener(move |_, _, _, cx| {
                                    crate::ptyd::end_sockets(orphans.clone());
                                    cx.notify();
                                }))),
                        )
                    },
                ),
            );
            if st.live > 0 {
                body = body.child(div().mt(px(12.)).flex().items_center().gap(px(10.)).child(
                    ghost_button("ptyd-end-all", if confirm { "Click again to end every terminal" } else { "End all sessions" }).on_click(cx.listener(
                        |_, _, _, cx| {
                            let mut a = END_ALL_ARMED.lock().unwrap();
                            if a.is_some_and(|t| t.elapsed() < CONFIRM_WINDOW) {
                                *a = None;
                                crate::ptyd::end_all();
                            } else {
                                *a = Some(Instant::now());
                            }
                            cx.notify();
                        },
                    )),
                ));
            }
        }
        card("Persistent terminals", None).child(body)
    }
}
