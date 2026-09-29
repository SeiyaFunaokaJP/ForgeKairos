use gpui::{Entity, Subscription};
use project::Project;
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};
use zed_actions::OpenRemote;

use crate::icon_for_remote_connection;

/// The leftmost status-bar item, mirroring VS Code's remote indicator: it
/// always offers a one-click way into "リモートプロジェクトを開く" and, while a
/// remote project is open, doubles as the connection status readout.
///
/// The equivalent title-bar affordance
/// (`title_bar::TitleBar::render_remote_project_connection`) only appears once
/// a project is already remote, so it cannot be used to *start* a connection.
pub struct RemoteStatusIndicator {
    project: Entity<Project>,
    _subscriptions: Vec<Subscription>,
}

impl RemoteStatusIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let mut subscriptions = vec![cx.observe(&project, |_, _, cx| cx.notify())];
        // Heartbeat and reconnect transitions are held by the remote client, not
        // by the project, so the status dot would otherwise go stale.
        let remote_client = project.read(cx).remote_client();
        if let Some(remote_client) = remote_client {
            subscriptions.push(cx.observe(&remote_client, |_, _, cx| cx.notify()));
        }

        Self {
            project,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for RemoteStatusIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let project = self.project.read(cx);
        let connection = if project.is_via_remote_server() {
            project.remote_connection_options(cx)
        } else {
            None
        };

        let (icon, icon_color, label, meta) = match &connection {
            Some(options) => {
                let host = options.display_name();
                let (icon_color, meta) = match project.remote_connection_state(cx) {
                    Some(remote::ConnectionState::Connecting) => {
                        (Color::Info, i18n::t!("Connecting to: {host}"))
                    }
                    Some(remote::ConnectionState::Connected) => {
                        (Color::Success, i18n::t!("Connected to: {host}"))
                    }
                    Some(remote::ConnectionState::HeartbeatMissed) => (
                        Color::Warning,
                        i18n::t!("Connection attempt to {host} missed. Retrying..."),
                    ),
                    Some(remote::ConnectionState::Reconnecting) => (
                        Color::Warning,
                        i18n::t!("Lost connection to {host}. Reconnecting..."),
                    ),
                    Some(remote::ConnectionState::Disconnected) => {
                        (Color::Error, i18n::t!("Disconnected from {host}"))
                    }
                    None => (Color::Muted, i18n::t!("Connecting to: {host}")),
                };
                (
                    icon_for_remote_connection(Some(options)),
                    icon_color,
                    Some(SharedString::from(host)),
                    SharedString::from(meta),
                )
            }
            None => (
                IconName::Server,
                Color::Muted,
                None,
                SharedString::from(i18n::t!(
                    "Open projects over SSH, in WSL, or in Dev Containers."
                )),
            ),
        };

        let aria_label = match &label {
            Some(host) => i18n::t!("Remote: {host}"),
            None => i18n::t!("Connect to Remote").to_string(),
        };

        ButtonLike::new("remote-status-indicator")
            .tab_index(0isize)
            .aria_label(aria_label)
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    i18n::t!("Connect to a remote host"),
                    Some(&OpenRemote::default()),
                    meta.clone(),
                    cx,
                )
            })
            .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenRemote::default()), cx))
            .child(
                h_flex()
                    .gap_1()
                    .max_w_32()
                    .child(Icon::new(icon).size(IconSize::Small).color(icon_color))
                    .when_some(label, |el, host| {
                        el.child(Label::new(host).size(LabelSize::Small).truncate())
                    }),
            )
    }
}

impl StatusItemView for RemoteStatusIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Hiding this would remove the only always-available entry point to a
        // remote connection while a local project is open.
        None
    }
}
