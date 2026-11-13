//! SSH surfaces, drawn like the Electron app's: the empty container's type
//! picker, "Connect over SSH" (SSHConnectPanel + ConnectionSelector), the
//! connecting / failed state of an SSH pane, and the connection editor
//! (ConnectionEditorModal).

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::Sizable as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::AppView;
use crate::model::connections::{AuthMethod, SavedConnection};
use crate::model::workspace::{ContentType, new_id};
use crate::ssh::parse_host_key_changed;
use crate::terminal::RemoteStatus;
use crate::theme::*;

const MONO: &str = "JetBrains Mono";

fn accent_mix(a: f32) -> Rgba {
    hexa(0x228df2, a)
}

/// SSHConnectPanel's button styles.
fn ghost_button(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(16.))
        .py(px(8.))
        .rounded(px(9.))
        .border_1()
        .border_color(BORDER)
        .bg(BG_PRIMARY)
        .text_color(TEXT_PRIMARY)
        .text_size(px(12.))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .hover(|d| d.bg(BG_HOVER))
        .child(label)
}

fn primary_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(18.))
        .py(px(9.))
        .rounded(px(9.))
        .bg(ACCENT)
        .text_color(BG_PRIMARY)
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .cursor_pointer()
        .hover(|d| d.opacity(0.9))
        .child(label.into())
}

fn danger_button(id: impl Into<ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px(px(14.))
        .py(px(8.))
        .rounded(px(9.))
        .bg(ACCENT_RED)
        .text_color(hex(0xffffff))
        .text_size(px(12.))
        .font_weight(FontWeight::BOLD)
        .cursor_pointer()
        .hover(|d| d.opacity(0.9))
        .child(label)
}

/// An input in SplitGrid's form style: dark well, accent border + soft ring when focused.
fn field(state: &Entity<InputState>, mono: bool, window: &Window, cx: &App) -> Div {
    let focused = state.read(cx).focus_handle(cx).is_focused(window);
    div()
        .px(px(12.))
        .py(px(5.))
        .rounded(px(9.))
        .bg(BG_PRIMARY)
        .border_1()
        .border_color(if focused { ACCENT } else { BORDER })
        .when(focused, |d| {
            d.shadow(vec![BoxShadow {
                color: c(accent_mix(0.18)),
                offset: point(px(0.), px(0.)),
                blur_radius: px(0.),
                spread_radius: px(3.),
                inset: false,
            }])
        })
        .text_size(px(13.))
        .when(mono, |d| d.font_family(MONO))
        .child(Input::new(state).appearance(false).small())
}

fn field_label(text: impl Into<SharedString>) -> Div {
    div().mb(px(6.)).text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(TEXT_MUTED).child(text.into().to_uppercase())
}

/// The connection editor's state (ConnectionEditorModal).
pub struct ConnectionEditor {
    pub editing: Option<String>,
    pub auth: AuthMethod,
    pub offer: bool,
    pub error: Option<String>,
    pub saving: bool,
    label: Entity<InputState>,
    group: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    username: Entity<InputState>,
    password: Entity<InputState>,
    key_path: Entity<InputState>,
    passphrase: Entity<InputState>,
    sudo: Entity<InputState>,
}

impl ConnectionEditor {
    fn new(existing: Option<&SavedConnection>, default_group: Option<String>, window: &mut Window, cx: &mut Context<AppView>) -> Self {
        let editing = existing.is_some();
        let masked = |ph: &'static str, window: &mut Window, cx: &mut Context<AppView>| {
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(if editing { "••••••••" } else { ph }))
        };
        let text = |value: Option<String>, ph: &'static str, window: &mut Window, cx: &mut Context<AppView>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(value.unwrap_or_default()))
        };
        let e = existing;
        Self {
            editing: e.map(|c| c.id.clone()),
            auth: e.map(|c| c.auth_method).unwrap_or(AuthMethod::Password),
            offer: e.and_then(|c| c.offer_saved_password).unwrap_or(false),
            error: None,
            saving: false,
            label: text(e.map(|c| c.label.clone()), "My Server", window, cx),
            group: text(e.and_then(|c| c.group.clone()).or(default_group), "e.g. Production", window, cx),
            host: text(e.map(|c| c.host.clone()), "192.168.1.1", window, cx),
            port: text(Some(e.map(|c| c.port).unwrap_or(22).to_string()), "22", window, cx),
            username: text(e.map(|c| c.username.clone()), "root", window, cx),
            password: masked("Enter password", window, cx),
            key_path: text(e.and_then(|c| c.private_key_path.clone()), "~/.ssh/id_rsa", window, cx),
            passphrase: masked("Key passphrase", window, cx),
            sudo: masked("sudo password", window, cx),
        }
    }
}

impl AppView {
    // ── Editor ──────────────────────────────────────────────────────────────

    pub(crate) fn open_editor(&mut self, existing: Option<String>, group: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let conn = existing.and_then(|id| self.connections.lock().unwrap().get(&id).cloned());
        let editor = ConnectionEditor::new(conn.as_ref(), group, window, cx);
        editor.label.update(cx, |i, cx| i.focus(window, cx));
        self.editor = Some(editor);
        cx.notify();
    }

    fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    fn save_editor(&mut self, cx: &mut Context<Self>) {
        let Some(ed) = self.editor.as_mut() else { return };
        let val = |s: &Entity<InputState>, cx: &App| s.read(cx).value().trim().to_string();
        let (label, group, host, port, username) = (val(&ed.label, cx), val(&ed.group, cx), val(&ed.host, cx), val(&ed.port, cx), val(&ed.username, cx));
        let (password, key_path, passphrase, sudo) = (
            ed.password.read(cx).value().to_string(),
            val(&ed.key_path, cx),
            ed.passphrase.read(cx).value().to_string(),
            ed.sudo.read(cx).value().to_string(),
        );
        let creating = ed.editing.is_none();
        // ConnectionEditorModal's checks, in its order.
        let error = if host.is_empty() || username.is_empty() {
            Some("Host and username are required")
        } else if ed.auth == AuthMethod::Password && creating && password.is_empty() {
            Some("Password is required")
        } else if ed.auth == AuthMethod::PrivateKey && key_path.is_empty() {
            Some("Private key file is required")
        } else {
            None
        };
        if let Some(e) = error {
            ed.error = Some(e.into());
            cx.notify();
            return;
        }

        let mut store = self.connections.lock().unwrap();
        let seal = |v: &str| (!v.is_empty()).then(|| store.seal(v));
        let (sealed_pw, sealed_pp, sealed_sudo) = (seal(&password), seal(&passphrase), seal(&sudo));
        let existing = ed.editing.as_ref().and_then(|id| store.get(id).cloned());
        let mut conn = existing.unwrap_or_else(|| SavedConnection {
            id: new_id("conn"),
            label: String::new(),
            host: String::new(),
            port: 22,
            username: String::new(),
            auth_method: AuthMethod::Password,
            private_key_path: None,
            encrypted_password: None,
            encrypted_passphrase: None,
            encrypted_sudo_password: None,
            offer_saved_password: None,
            group: None,
            extra: Default::default(),
        });
        conn.label = if label.is_empty() { format!("{username}@{host}") } else { label };
        conn.host = host;
        conn.port = port.parse().unwrap_or(22);
        conn.username = username;
        conn.auth_method = ed.auth;
        conn.group = (!group.is_empty()).then_some(group.clone());
        conn.offer_saved_password = Some(ed.offer);
        // An empty secret keeps the stored one.
        match ed.auth {
            AuthMethod::Password => {
                if sealed_pw.is_some() {
                    conn.encrypted_password = sealed_pw;
                }
            }
            AuthMethod::PrivateKey => {
                conn.private_key_path = Some(key_path);
                if sealed_pp.is_some() {
                    conn.encrypted_passphrase = sealed_pp;
                }
            }
        }
        if sealed_sudo.is_some() {
            conn.encrypted_sudo_password = sealed_sudo;
        }
        match store.connections.iter_mut().find(|c| c.id == conn.id) {
            Some(slot) => *slot = conn,
            None => store.connections.push(conn),
        }
        if !group.is_empty() && !store.groups.groups.contains(&group) {
            store.groups.groups.push(group);
        }
        let saved = store.save();
        drop(store);
        match saved {
            Ok(()) => self.close_editor(cx),
            Err(e) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.error = Some(format!("Failed to save connection: {e:#}"));
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn delete_connection(&mut self, id: &str, cx: &mut Context<Self>) {
        let mut store = self.connections.lock().unwrap();
        store.connections.retain(|c| c.id != id);
        if let Err(e) = store.save() {
            log::error!("saving connections: {e:#}");
        }
        drop(store);
        cx.notify();
    }

    fn toggle_group(&mut self, group: &str, cx: &mut Context<Self>) {
        let mut store = self.connections.lock().unwrap();
        let collapsed = &mut store.groups.collapsed;
        if let Some(i) = collapsed.iter().position(|g| g == group) {
            collapsed.remove(i);
        } else {
            collapsed.push(group.to_owned());
        }
        let _ = store.save();
        drop(store);
        cx.notify();
    }

    fn browse_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let answer = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Select SSH Private Key".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = answer.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let _ = this.update_in(cx, |this, window, cx| {
                if let Some(ed) = this.editor.as_ref() {
                    let value = crate::grid::tilde(&path.to_string_lossy());
                    ed.key_path.update(cx, |i, cx| i.set_value(value, window, cx));
                }
            });
        })
        .detach();
    }

    pub(crate) fn render_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ed = self.editor.as_ref()?;
        let editing = ed.editing.is_some();
        let keep = |base: &str| if editing { format!("{base} — leave empty to keep current") } else { base.to_owned() };

        let segment = |id: &'static str, label: &'static str, which: AuthMethod, cx: &mut Context<Self>| {
            let active = ed.auth == which;
            div()
                .id(id)
                .flex_1()
                .flex()
                .justify_center()
                .px(px(12.))
                .py(px(8.))
                .rounded(px(9.))
                .border_1()
                .text_size(px(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .map(|d| {
                    if active {
                        d.border_color(ACCENT).bg(accent_mix(0.14)).text_color(ACCENT)
                    } else {
                        d.border_color(BORDER).bg(BG_PRIMARY).text_color(TEXT_PRIMARY)
                    }
                })
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(ed) = this.editor.as_mut() {
                        ed.auth = which;
                    }
                    cx.notify();
                }))
        };

        let sudo_suffix = if editing {
            " — leave empty to keep current"
        } else if ed.auth == AuthMethod::Password {
            " (optional — otherwise login password)"
        } else {
            " (required for key-auth)"
        };

        let body = div()
            .id("editor-body")
            .px(px(24.))
            .py(px(20.))
            .flex()
            .flex_col()
            .gap(px(16.))
            .overflow_y_scroll()
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(field_label("Label")).child(field(&ed.label, true, window, cx)))
                    .child(div().flex_1().child(field_label("Group (optional)")).child(field(&ed.group, true, window, cx))),
            )
            .child(
                div()
                    .flex()
                    .gap(px(12.))
                    .child(div().flex_1().child(field_label("Host")).child(field(&ed.host, true, window, cx)))
                    .child(div().w(px(96.)).child(field_label("Port")).child(field(&ed.port, true, window, cx))),
            )
            .child(div().child(field_label("Username")).child(field(&ed.username, true, window, cx)))
            .child(
                div().child(field_label("Authentication")).child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(segment("auth-password", "Password", AuthMethod::Password, cx))
                        .child(segment("auth-key", "Private key", AuthMethod::PrivateKey, cx)),
                ),
            )
            .map(|d| match ed.auth {
                AuthMethod::Password => d.child(div().child(field_label(keep("Password"))).child(field(&ed.password, true, window, cx))),
                AuthMethod::PrivateKey => d
                    .child(
                        div().child(field_label("Private key file")).child(
                            div()
                                .flex()
                                .gap(px(8.))
                                .child(div().flex_1().child(field(&ed.key_path, true, window, cx)))
                                .child(
                                    ghost_button("browse-key", "Browse")
                                        .on_click(cx.listener(|this, _, window, cx| this.browse_key(window, cx))),
                                ),
                        ),
                    )
                    .child(
                        div()
                            .child(field_label(if editing { "Passphrase — leave empty to keep current".to_owned() } else { "Passphrase (optional)".to_owned() }))
                            .child(field(&ed.passphrase, true, window, cx)),
                    ),
            })
            .child(
                Checkbox::new("offer-saved")
                    .checked(ed.offer)
                    .label("Offer saved password at prompts (sudo / password)")
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        if let Some(ed) = this.editor.as_mut() {
                            ed.offer = *checked;
                        }
                        cx.notify();
                    })),
            )
            .when(ed.offer, |d| {
                d.child(
                    div()
                        .child(field_label(format!("Sudo password{sudo_suffix}")))
                        .child(field(&ed.sudo, true, window, cx))
                        .child(
                            div()
                                .mt(px(6.))
                                .text_size(px(11.))
                                .line_height(relative(1.5))
                                .text_color(TEXT_MUTED)
                                .child("Encrypted on this Mac; only sent after your confirmation (Enter). Never entered automatically."),
                        ),
                )
            })
            .when_some(ed.error.clone(), |d, e| {
                d.child(
                    div()
                        .px(px(12.))
                        .py(px(9.))
                        .rounded(px(8.))
                        .bg(hexa(0xf14c4c, 0.10))
                        .border_1()
                        .border_color(ACCENT_RED)
                        .text_size(px(12.))
                        .text_color(ACCENT_RED)
                        .child(e),
                )
            });

        let submit = if ed.saving { "Saving…" } else if editing { "Save changes" } else { "Save connection" };
        let modal = div()
            .id("editor")
            .w(px(480.))
            .max_h(relative(0.88))
            .flex()
            .flex_col()
            .bg(BG_SURFACE)
            .border_1()
            .border_color(BORDER)
            .rounded(px(16.))
            .shadow(vec![BoxShadow {
                color: c(hexa(0, 0.55)),
                offset: point(px(0.), px(24.)),
                blur_radius: px(70.),
                spread_radius: px(0.),
                inset: false,
            }])
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .px(px(24.))
                    .pt(px(20.))
                    .pb(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(14.))
                    .border_b_1()
                    .border_color(BORDER)
                    .child(
                        div()
                            .size(px(40.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(11.))
                            .bg(accent_mix(0.16))
                            .text_color(ACCENT)
                            .text_size(px(18.))
                            .font_weight(FontWeight::BOLD)
                            .font_family(MONO)
                            .child("~"),
                    )
                    .child(
                        div()
                            .child(div().text_size(px(16.)).font_weight(FontWeight::BOLD).child(if editing { "Edit connection" } else { "New SSH connection" }))
                            .child(div().mt(px(2.)).text_size(px(12.)).text_color(TEXT_MUTED).child(if editing {
                                "Update the saved connection details"
                            } else {
                                "Save a reusable SSH connection"
                            })),
                    ),
            )
            .child(body)
            .child(
                div()
                    .px(px(24.))
                    .pt(px(14.))
                    .pb(px(20.))
                    .flex()
                    .justify_end()
                    .gap(px(10.))
                    .border_t_1()
                    .border_color(BORDER)
                    .child(ghost_button("editor-cancel", "Cancel").on_click(cx.listener(|this, _, _, cx| this.close_editor(cx))))
                    .child(primary_button("editor-save", submit).on_click(cx.listener(|this, _, _, cx| this.save_editor(cx)))),
            );

        Some(
            div()
                .id("editor-overlay")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(20.))
                .py(px(40.))
                .bg(hexa(0, 0.55))
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| this.close_editor(cx)))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    if ev.keystroke.key == "escape" {
                        this.close_editor(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(modal)
                .into_any_element(),
        )
    }

    // ── Empty container: pick what it becomes ───────────────────────────────

    pub(crate) fn render_type_picker(&self, id: &str, cx: &mut Context<Self>) -> AnyElement {
        let item = |key: &'static str, label: &'static str, icon: &'static str, color: Rgba, kind: ContentType, cx: &mut Context<Self>| {
            let cid = id.to_owned();
            let group = SharedString::from(format!("pick-{key}"));
            div()
                .id(SharedString::from(format!("pick-{key}-{id}")))
                .w_full()
                .max_w(px(160.))
                .px(px(12.))
                .py(px(6.))
                .rounded(px(5.))
                .flex()
                .items_center()
                .gap(px(8.))
                .border_1()
                .border_color(BORDER)
                .text_size(px(11.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(TEXT_PRIMARY)
                .cursor_pointer()
                .hover(move |d| d.border_color(color))
                .group(group)
                .child(div().w(px(20.)).flex_none().flex().justify_center().text_size(px(12.)).text_color(color).child(icon))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if kind == ContentType::Ide {
                        this.fill_ide(cid.clone(), cx)
                    } else {
                        this.fill_container(&cid, kind, cx)
                    }
                }))
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(4.))
            .p(px(12.))
            .child(item("terminal", "Terminal", "$", ACCENT_GREEN, ContentType::Terminal, cx))
            .child(item("ssh", "SSH", "~", ACCENT, ContentType::SshConnect, cx))
            .child(item("ide", "IDE", "</>", ACCENT_YELLOW, ContentType::Ide, cx))
            .child(item("browser", "Browser", "\u{2605}", ACCENT_PURPLE, ContentType::Browser, cx))
            .child(item("sftp", "SFTP", "⇆", hex(0xea7620), ContentType::Sftp, cx))
            .child(item("sql", "SQL", "⛁", ACCENT_BLUE, ContentType::Sql, cx))
            .into_any_element()
    }

    // ── "Connect over SSH" ──────────────────────────────────────────────────

    /// The connection picker: "Connect over SSH", or (`sftp`) the SFTP pane's "Choose a connection".
    pub(crate) fn render_ssh_picker(&self, id: &str, sftp: bool, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let store = self.connections.lock().unwrap();
        let connections = store.connections.clone();
        let groups = store.groups.clone();
        drop(store);
        let connecting = self.ssh_pending.get(id).cloned();
        let error = self.ssh_errors.get(id).cloned();
        let query = self.ssh_search.read(cx).value().trim().to_lowercase();
        let recent_ids = self.state.active().map(|w| w.recent_connections()).unwrap_or_default();

        let matches = |c: &SavedConnection| {
            query.is_empty() || c.label.to_lowercase().contains(&query) || format!("{}@{}:{}", c.username, c.host, c.port).to_lowercase().contains(&query)
        };
        let recent: Vec<&SavedConnection> =
            recent_ids.iter().filter_map(|rid| connections.iter().find(|c| &c.id == rid)).filter(|c| matches(c)).collect();

        // Sections: recent, then by group (or one flat list).
        let mut sections: Vec<(String, Option<String>, Vec<&SavedConnection>)> = Vec::new();
        if !recent.is_empty() {
            sections.push(("Recently used here".into(), None, recent.clone()));
        }
        if connections.iter().any(|c| c.group.is_some()) {
            let mut names = groups.groups.clone();
            for c in &connections {
                if let Some(g) = &c.group
                    && !names.contains(g)
                {
                    names.push(g.clone());
                }
            }
            for g in names {
                let items: Vec<_> = connections.iter().filter(|c| c.group.as_ref() == Some(&g) && matches(c)).collect();
                if !items.is_empty() {
                    sections.push((g.clone(), Some(g), items));
                }
            }
            let ungrouped: Vec<_> = connections.iter().filter(|c| c.group.is_none() && matches(c)).collect();
            if !ungrouped.is_empty() {
                sections.push(("Ungrouped".into(), Some("\u{0}ungrouped".into()), ungrouped));
            }
        } else {
            let rest: Vec<_> = connections.iter().filter(|c| !recent_ids.contains(&c.id) && matches(c)).collect();
            if !rest.is_empty() {
                let title = if recent.is_empty() { "Saved connections" } else { "All connections" };
                sections.push((title.into(), None, rest));
            }
        }

        let row = |c: &SavedConnection, cx: &mut Context<Self>| {
            let (cid, conn_id, del_id) = (id.to_owned(), c.id.clone(), c.id.clone());
            let this_connecting = connecting.as_deref() == Some(c.id.as_str());
            let busy = connecting.is_some();
            let sub = c.address();
            let group = SharedString::from(format!("conn-{}-{}", id, c.id));
            div()
                .id(SharedString::from(format!("conn-{}-{}", id, c.id)))
                .flex()
                .items_center()
                .gap(px(11.))
                .px(px(11.))
                .py(px(9.))
                .rounded(px(9.))
                .bg(BG_PRIMARY)
                .border_1()
                .border_color(BORDER)
                .when(busy && !this_connecting, |d| d.opacity(0.5))
                .when(!busy, |d| d.cursor_pointer().hover(|d| d.bg(BG_HOVER).border_color(hex(0x2f5a82))))
                .group(group.clone())
                .child(div().size(px(7.)).flex_none().rounded_full().bg(ACCENT))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_size(px(13.)).font_weight(FontWeight::SEMIBOLD).child(c.label.clone()))
                        .child(div().truncate().text_size(px(11.)).text_color(TEXT_MUTED).child(sub)),
                )
                .map(|d| {
                    if this_connecting {
                        d.child(div().text_size(px(11.)).font_weight(FontWeight::SEMIBOLD).text_color(ACCENT).child("Connecting…"))
                    } else if !busy && !sftp {
                        d.child(
                            div()
                                .id(SharedString::from(format!("conn-del-{}-{}", id, c.id)))
                                .size(px(22.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.))
                                .text_size(px(14.))
                                .text_color(TEXT_MUTED)
                                .invisible()
                                .group_hover(group, |s| s.visible())
                                .hover(|d| d.bg(ACCENT_RED).text_color(BG_PRIMARY))
                                .child("×")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.delete_connection(&del_id, cx);
                                })),
                        )
                    } else {
                        d
                    }
                })
                .when(!busy, |d| {
                    d.on_click(cx.listener(move |this, _, window, cx| {
                        if sftp {
                            this.choose_sftp(&cid, &conn_id, cx)
                        } else {
                            this.connect_saved(cid.clone(), conn_id.clone(), window, cx)
                        }
                    }))
                })
                .into_any_element()
        };

        let searching = !query.is_empty();
        let list: Vec<AnyElement> = sections
            .iter()
            .map(|(title, group_key, items)| {
                let collapsible = group_key.is_some();
                let key = group_key.clone().unwrap_or_default();
                let collapsed = collapsible && !searching && groups.collapsed.contains(&key);
                let toggle_key = key.clone();
                let header = div()
                    .id(SharedString::from(format!("sec-{id}-{title}")))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(8.))
                    .pt(px(4.))
                    .pb(px(8.))
                    .text_size(px(11.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(TEXT_MUTED)
                    .when(collapsible, |d| {
                        d.cursor_pointer()
                            .child(div().w(px(10.)).child(if collapsed { "▸" } else { "▾" }))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_group(&toggle_key, cx)))
                    })
                    .child(title.to_uppercase())
                    .when(collapsible, |d| d.child(div().opacity(0.6).font_weight(FontWeight::MEDIUM).child(items.len().to_string())));
                let rows: Vec<AnyElement> = if collapsed { Vec::new() } else { items.iter().map(|c| row(c, cx)).collect() };
                div().mb(px(12.)).child(header).child(div().flex().flex_col().gap(px(4.)).children(rows)).into_any_element()
            })
            .collect();

        let host_key = error.as_deref().and_then(parse_host_key_changed);
        let cid_cancel = id.to_owned();
        let empty = connections.is_empty();

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .px(px(16.))
                    .pt(px(16.))
                    .pb(px(10.))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(div().text_size(px(13.)).font_weight(FontWeight::BOLD).child(if sftp { "Choose a connection" } else { "Connect over SSH" }))
                    .child(div().mt(px(2.)).text_size(px(11.)).text_color(TEXT_MUTED).child(if sftp {
                        "Pick a saved SSH connection to browse over SFTP"
                    } else {
                        "Pick a saved connection to open in this container"
                    })),
            )
            .map(|d| match (&host_key, &error) {
                (Some((host, port, expected, actual)), _) => d.child(self.host_key_card(id, host, *port, expected, actual, cx)),
                (None, Some(msg)) => d.child(
                    div()
                        .mx(px(16.))
                        .mb(px(10.))
                        .px(px(11.))
                        .py(px(8.))
                        .rounded(px(8.))
                        .bg(hexa(0xf14c4c, 0.10))
                        .text_size(px(12.))
                        .text_color(ACCENT_RED)
                        .child(if msg.is_empty() { "Connection failed".to_owned() } else { msg.clone() }),
                ),
                _ => d,
            })
            .map(|d| {
                if empty && sftp {
                    d.child(
                        div()
                            .p(px(24.))
                            .flex()
                            .justify_center()
                            .text_size(px(12.))
                            .line_height(relative(1.5))
                            .text_color(TEXT_MUTED)
                            .child("No saved SSH connections. Add one in Settings."),
                    )
                } else if empty {
                    d.child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .items_center()
                            .justify_center()
                            .gap(px(12.))
                            .p(px(24.))
                            .child(div().text_size(px(12.)).text_color(TEXT_MUTED).child("No saved connections yet."))
                            .child(primary_button("ssh-first", "+ New connection").on_click(cx.listener(|this, _, window, cx| this.open_editor(None, None, window, cx)))),
                    )
                } else {
                    d.child(div().px(px(16.)).pb(px(10.)).child(field(&self.ssh_search, false, window, cx)))
                        .child(
                            div()
                                .id(SharedString::from(format!("ssh-list-{id}")))
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .px(px(16.))
                                .pb(px(14.))
                                .when(list.is_empty(), |d| {
                                    d.child(
                                        div()
                                            .px(px(12.))
                                            .py(px(26.))
                                            .flex()
                                            .justify_center()
                                            .text_size(px(12.))
                                            .text_color(TEXT_MUTED)
                                            .child(format!("No connections match “{query}”.")),
                                    )
                                })
                                .children(list),
                        )
                }
            })
            .when(!sftp, |d| d.child(
                div()
                    .px(px(16.))
                    .py(px(12.))
                    .flex()
                    .justify_between()
                    .border_t_1()
                    .border_color(BORDER)
                    .child(ghost_button(SharedString::from(format!("ssh-new-{id}")), "+ New connection").on_click(cx.listener(|this, _, window, cx| this.open_editor(None, None, window, cx))))
                    .child(ghost_button(SharedString::from(format!("ssh-cancel-{id}")), "Cancel").on_click(cx.listener(move |this, _, _, cx| this.empty_container(&cid_cancel, cx)))),
            ))
            .into_any_element()
    }

    fn host_key_card(&self, id: &str, host: &str, port: u16, expected: &str, actual: &str, cx: &mut Context<Self>) -> AnyElement {
        let (cid, cid2) = (id.to_owned(), id.to_owned());
        let host_s = host.to_owned();
        let fp = |label: &'static str, value: &str| {
            div()
                .flex()
                .gap(px(4.))
                .font_family(MONO)
                .text_size(px(10.5))
                .child(div().text_color(TEXT_MUTED).child(label))
                .child(div().text_color(TEXT_PRIMARY).child(value.to_owned()))
        };
        div()
            .mx(px(16.))
            .mb(px(10.))
            .px(px(14.))
            .py(px(12.))
            .rounded(px(8.))
            .bg(hexa(0xf14c4c, 0.08))
            .border_1()
            .border_color(hexa(0xf14c4c, 0.35))
            .child(div().mb(px(6.)).text_size(px(12.)).font_weight(FontWeight::BOLD).text_color(ACCENT_RED).child(format!("⚠ Host key changed for {host}:{port}")))
            .child(
                div().text_size(px(11.)).line_height(relative(1.5)).child(
                    "The server is presenting a different key than the one pinned when you first connected. This is normal if the \
                     server was reinstalled or its SSH keys were rotated — but it can also mean the connection is being intercepted. \
                     Only continue if you trust this change.",
                ),
            )
            .child(div().mt(px(8.)).flex().flex_col().gap(px(4.)).child(fp("Pinned", expected)).child(fp("New", actual)))
            .child(
                div()
                    .mt(px(10.))
                    .flex()
                    .gap(px(8.))
                    .child(danger_button(SharedString::from(format!("hk-accept-{id}")), "Accept new key & reconnect").on_click(cx.listener(
                        move |this, _, window, cx| this.accept_new_host_key(&cid, &host_s, port, window, cx),
                    )))
                    .child(ghost_button(SharedString::from(format!("hk-cancel-{id}")), "Cancel").on_click(cx.listener(move |this, _, _, cx| {
                        this.ssh_errors.remove(&cid2);
                        cx.notify();
                    }))),
            )
            .into_any_element()
    }

    // ── An SSH pane that isn't showing its terminal ─────────────────────────

    /// "Connecting..." while the session starts, or "Connection failed" with
    /// Retry (and the host-key card when that's why). None: show the terminal.
    pub(crate) fn render_ssh_state(&self, id: &str, status: Option<&RemoteStatus>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let failure = self.ssh_errors.get(id).cloned().or_else(|| match status {
            Some(RemoteStatus::Failed(m)) => Some(m.clone()),
            _ => None,
        });
        let connecting = matches!(status, Some(RemoteStatus::Connecting));
        if failure.is_none() && !connecting {
            return None;
        }
        let base = div().size_full().flex().flex_col().items_center().justify_center().gap(px(6.));
        Some(match failure {
            None => base.child(div().text_size(px(12.)).text_color(ACCENT_YELLOW).child("Connecting...")).into_any_element(),
            Some(msg) => {
                let cid = id.to_owned();
                let card = parse_host_key_changed(&msg).map(|(h, p, e, a)| self.host_key_card(id, &h, p, &e, &a, cx));
                base.child(div().text_size(px(12.)).text_color(ACCENT_RED).child("Connection failed"))
                    .map(|d| match card {
                        Some(card) => d.child(div().max_w(px(460.)).child(card)),
                        None => d.child(
                            div()
                                .max_w(px(300.))
                                .text_size(px(11.))
                                .text_color(TEXT_MUTED)
                                .line_height(relative(1.4))
                                .text_center()
                                .child(msg),
                        ),
                    })
                    .child(
                        div()
                            .id(SharedString::from(format!("retry-{id}")))
                            .mt(px(4.))
                            .px(px(12.))
                            .py(px(3.))
                            .rounded(px(4.))
                            .border_1()
                            .border_color(BORDER)
                            .bg(BG_HOVER)
                            .text_size(px(11.))
                            .cursor_pointer()
                            .child("Retry")
                            .on_click(cx.listener(move |this, _, _, cx| this.reconnect(&cid, cx))),
                    )
                    .into_any_element()
            }
        })
    }
}
