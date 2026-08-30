use anyhow::Context as _;
use copilot::{
    Copilot, GlobalCopilotAuth, Status,
    request::{self, PromptUserDeviceFlow},
};
use copilot_chat::{CopilotChat, CopilotChatStatus, DeviceFlow};
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Element, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement, IntoElement, MouseDownEvent, ParentElement, Render, Styled,
    Subscription, TaskExt, Window, WindowBounds, WindowOptions, div, point,
};
use project::project_settings::ProjectSettings;
use release_channel::ReleaseChannel;
use settings::Settings as _;
use ui::{ButtonLike, CommonAnimationExt, ConfiguredApiCard, Vector, VectorName, prelude::*};
use util::ResultExt as _;
use workspace::{AppState, Toast, Workspace, notifications::NotificationId};

const COPILOT_SIGN_UP_URL: &str = "https://github.com/features/copilot";
const ERROR_LABEL: &str = "Copilot Edit Predictions の起動に問題が発生しました。再インストールして、もう一度サインインしてみてください。";

struct CopilotStatusToast;

pub fn initiate_sign_in(copilot: Entity<Copilot>, window: &mut Window, cx: &mut App) {
    let is_reinstall = false;
    initiate_sign_in_impl(copilot, is_reinstall, window, cx)
}

pub fn initiate_sign_out(copilot: Entity<Copilot>, window: &mut Window, cx: &mut App) {
    copilot_toast(
        Some("Copilot Edit Predictions からサインアウトしています…"),
        window,
        cx,
    );

    let sign_out_task = copilot.update(cx, |copilot, cx| copilot.sign_out(cx));
    window
        .spawn(cx, async move |cx| match sign_out_task.await {
            Ok(()) => cx.update(|window, cx| {
                copilot_toast(
                    Some("Copilot Edit Predictions からサインアウトしました"),
                    window,
                    cx,
                )
            }),
            Err(err) => cx.update(|window, cx| {
                if let Some(workspace) = Workspace::for_window(window, cx) {
                    workspace.update(cx, |workspace, cx| {
                        workspace.show_error(format!("エラー: {err}"), cx);
                    })
                } else {
                    log::error!("{:?}", err);
                }
            }),
        })
        .detach();
}

pub fn reinstall_and_sign_in(copilot: Entity<Copilot>, window: &mut Window, cx: &mut App) {
    let _ = copilot.update(cx, |copilot, cx| copilot.reinstall(cx));
    let is_reinstall = true;
    initiate_sign_in_impl(copilot, is_reinstall, window, cx);
}

fn open_copilot_code_verification_window(copilot: &Entity<Copilot>, window: &Window, cx: &mut App) {
    let current_window_center = window.bounds().center();
    let width = px(450.);
    let height = px(350.);
    let window_bounds = WindowBounds::Windowed(gpui::bounds(
        current_window_center - point(width / 2.0, height / 2.0),
        gpui::size(width, height),
    ));
    let app_id = ReleaseChannel::global(cx).app_id();
    cx.open_window(
        WindowOptions {
            kind: gpui::WindowKind::Floating,
            window_bounds: Some(window_bounds),
            is_resizable: false,
            is_movable: true,
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Zed で GitHub Copilot を使用".into()),
                appears_transparent: true,
                ..Default::default()
            }),
            app_id: Some(app_id.to_owned()),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| CopilotCodeVerification::new(&copilot, window, cx)),
    )
    .context("Failed to open Copilot code verification window")
    .log_err();
}

fn open_copilot_chat_code_verification_window(
    copilot_chat: &Entity<CopilotChat>,
    window: &Window,
    cx: &mut App,
) {
    let current_window_center = window.bounds().center();
    let height = px(450.);
    let width = px(350.);
    let window_bounds = WindowBounds::Windowed(gpui::bounds(
        current_window_center - point(height / 2.0, width / 2.0),
        gpui::size(height, width),
    ));
    cx.open_window(
        WindowOptions {
            kind: gpui::WindowKind::Floating,
            window_bounds: Some(window_bounds),
            is_resizable: false,
            is_movable: true,
            titlebar: Some(gpui::TitlebarOptions {
                appears_transparent: true,
                ..Default::default()
            }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| CopilotChatCodeVerification::new(&copilot_chat, window, cx)),
    )
    .context("Failed to open Copilot Chat code verification window")
    .log_err();
}

fn copilot_toast(message: Option<&'static str>, window: &Window, cx: &mut App) {
    const NOTIFICATION_ID: NotificationId = NotificationId::unique::<CopilotStatusToast>();

    let Some(workspace) = Workspace::for_window(window, cx) else {
        return;
    };

    cx.defer(move |cx| {
        workspace.update(cx, |workspace, cx| match message {
            Some(message) => workspace.show_toast(Toast::new(NOTIFICATION_ID, message), cx),
            None => workspace.dismiss_toast(&NOTIFICATION_ID, cx),
        });
    })
}

pub fn initiate_sign_in_impl(
    copilot: Entity<Copilot>,
    is_reinstall: bool,
    window: &mut Window,
    cx: &mut App,
) {
    if matches!(copilot.read(cx).status(), Status::Disabled) {
        copilot.update(cx, |copilot, cx| copilot.start_copilot(false, true, cx));
    }
    match copilot.read(cx).status() {
        Status::Starting { task } => {
            copilot_toast(
                Some(if is_reinstall {
                    "Copilot Edit Predictions を再インストールしています…"
                } else {
                    "Copilot Edit Predictions を起動しています…"
                }),
                window,
                cx,
            );

            window
                .spawn(cx, async move |cx| {
                    task.await;
                    cx.update(|window, cx| match copilot.read(cx).status() {
                        Status::Authorized => copilot_toast(
                            Some("Copilot Edit Predictions が起動しました。"),
                            window,
                            cx,
                        ),
                        _ => {
                            copilot_toast(None, window, cx);
                            copilot
                                .update(cx, |copilot, cx| copilot.sign_in(cx))
                                .detach_and_log_err(cx);
                            open_copilot_code_verification_window(&copilot, window, cx);
                        }
                    })
                    .log_err();
                })
                .detach();
        }
        _ => {
            copilot
                .update(cx, |copilot, cx| copilot.sign_in(cx))
                .detach();
            open_copilot_code_verification_window(&copilot, window, cx);
        }
    }
}

pub struct CopilotCodeVerification {
    status: Status,
    connect_clicked: bool,
    focus_handle: FocusHandle,
    copilot: Entity<Copilot>,
    _subscription: Subscription,
    sign_up_url: Option<String>,
}

impl Focusable for CopilotCodeVerification {
    fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for CopilotCodeVerification {}

impl CopilotCodeVerification {
    pub fn new(copilot: &Entity<Copilot>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        window.on_window_should_close(cx, |window, cx| {
            if let Some(this) = window.root::<CopilotCodeVerification>().flatten() {
                this.update(cx, |this, cx| {
                    this.before_dismiss(cx);
                });
            }
            true
        });
        cx.subscribe_in(
            &cx.entity(),
            window,
            |this, _, _: &DismissEvent, window, cx| {
                window.remove_window();
                this.before_dismiss(cx);
            },
        )
        .detach();

        let status = copilot.read(cx).status();
        Self {
            status,
            connect_clicked: false,
            focus_handle: cx.focus_handle(),
            copilot: copilot.clone(),
            sign_up_url: None,
            _subscription: cx.observe(copilot, |this, copilot, cx| {
                let status = copilot.read(cx).status();
                match status {
                    Status::Authorized | Status::Unauthorized | Status::SigningIn { .. } => {
                        this.set_status(status, cx)
                    }
                    _ => cx.emit(DismissEvent),
                }
            }),
        }
    }

    pub fn set_status(&mut self, status: Status, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn render_device_code(data: &PromptUserDeviceFlow, cx: &mut Context<Self>) -> impl IntoElement {
        let copied = cx
            .read_from_clipboard()
            .map(|item| item.text().as_ref() == Some(&data.user_code))
            .unwrap_or(false);

        ButtonLike::new("copy-button")
            .full_width()
            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .p_1()
                    .justify_between()
                    .child(Label::new(data.user_code.clone()))
                    .child(Label::new(if copied {
                        "コピーしました"
                    } else {
                        "コピー"
                    })),
            )
            .on_click({
                let user_code = data.user_code.clone();
                move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
                    window.refresh();
                }
            })
    }

    fn render_prompting_modal(
        copilot: Entity<Copilot>,
        connect_clicked: bool,
        data: &PromptUserDeviceFlow,
        cx: &mut Context<Self>,
    ) -> impl Element {
        let connect_button_label = if connect_clicked {
            "接続を待機しています…"
        } else {
            "GitHub に接続"
        };

        v_flex()
            .flex_1()
            .gap_2p5()
            .items_center()
            .text_center()
            .child(
                Headline::new("Zed で GitHub Copilot Edit Predictions を使用")
                    .size(HeadlineSize::Large),
            )
            .child(
                Label::new(
                    "Copilot の編集予測を使用するには、GitHub の有効なサブスクリプションが必要です。",
                )
                .color(Color::Muted),
            )
            .child(Self::render_device_code(data, cx))
            .child(
                Label::new("下のボタンをクリックした後、このコードを GitHub に貼り付けてください。")
                    .color(Color::Muted),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Button::new("connect-button", connect_button_label)
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Medium)
                            .on_click({
                                let command = data.command.clone();
                                cx.listener(move |this, _, _window, cx| {
                                    let command = command.clone();
                                    let copilot_clone = copilot.clone();
                                    let request_timeout = ProjectSettings::get_global(cx)
                                        .global_lsp_settings
                                        .get_request_timeout();
                                    copilot.update(cx, |copilot, cx| {
                                        if let Some(server) = copilot.language_server() {
                                            let server = server.clone();
                                            cx.spawn(async move |_, cx| {
                                                let result = server
                                                    .request::<lsp::request::ExecuteCommand>(
                                                        lsp::ExecuteCommandParams {
                                                            command: command.command.clone(),
                                                            arguments: command
                                                                .arguments
                                                                .clone()
                                                                .unwrap_or_default(),
                                                            ..Default::default()
                                                        },
                                                        request_timeout,
                                                    )
                                                    .await
                                                    .into_response()
                                                    .ok()
                                                    .flatten();
                                                if let Some(value) = result {
                                                    if let Ok(status) = serde_json::from_value::<
                                                        request::SignInStatus,
                                                    >(
                                                        value
                                                    ) {
                                                        copilot_clone.update(cx, |copilot, cx| {
                                                            copilot
                                                                .update_sign_in_status(status, cx);
                                                        });
                                                    }
                                                }
                                            })
                                            .detach();
                                        }
                                    });

                                    this.connect_clicked = true;
                                })
                            }),
                    )
                    .child(
                        Button::new("copilot-enable-cancel-button", "キャンセル")
                            .full_width()
                            .size(ButtonSize::Medium)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(DismissEvent);
                            })),
                    ),
            )
    }

    fn render_enabled_modal(cx: &mut Context<Self>) -> impl Element {
        v_flex()
            .gap_2()
            .text_center()
            .justify_center()
            .child(
                Headline::new("Copilot Edit Predictions が有効になりました")
                    .size(HeadlineSize::Large),
            )
            .child(Label::new("Copilot の編集予測を使用する準備が整いました。").color(Color::Muted))
            .child(
                Button::new("copilot-enabled-done-button", "完了")
                    .full_width()
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }

    fn render_unauthorized_modal(&self, cx: &mut Context<Self>) -> impl Element {
        let sign_up_url = self
            .sign_up_url
            .as_deref()
            .unwrap_or(COPILOT_SIGN_UP_URL)
            .to_owned();
        let description = "サブスクリプションの購入または更新後、既存のライセンスを接続して Copilot の編集予測を有効にしてください。";

        v_flex()
            .gap_2()
            .text_center()
            .justify_center()
            .child(
                Headline::new("有効な GitHub Copilot のサブスクリプションが必要です")
                    .size(HeadlineSize::Large),
            )
            .child(Label::new(description).color(Color::Warning))
            .child(
                Button::new(
                    "copilot-subscribe-button",
                    "GitHub でサブスクリプションを購入",
                )
                .full_width()
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Medium)
                .on_click(move |_, _, cx| cx.open_url(&sign_up_url)),
            )
            .child(
                Button::new("copilot-subscribe-cancel-button", "キャンセル")
                    .full_width()
                    .size(ButtonSize::Medium)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }

    fn render_error_modal(copilot: Entity<Copilot>, _cx: &mut Context<Self>) -> impl Element {
        v_flex()
            .gap_2()
            .text_center()
            .justify_center()
            .child(Headline::new("エラーが発生しました").size(HeadlineSize::Large))
            .child(Label::new(ERROR_LABEL).color(Color::Muted))
            .child(
                Button::new(
                    "copilot-subscribe-button",
                    "Copilot Edit Predictions を再インストールしてサインイン",
                )
                .full_width()
                .style(ButtonStyle::Outlined)
                .size(ButtonSize::Medium)
                .start_icon(
                    Icon::new(IconName::Download)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(move |_, window, cx| reinstall_and_sign_in(copilot.clone(), window, cx)),
            )
    }

    fn before_dismiss(
        &mut self,
        cx: &mut Context<'_, CopilotCodeVerification>,
    ) -> workspace::DismissDecision {
        self.copilot.update(cx, |copilot, cx| {
            if matches!(copilot.status(), Status::SigningIn { .. }) {
                copilot.sign_out(cx).detach_and_log_err(cx);
            }
        });
        workspace::DismissDecision::Dismiss(true)
    }
}

impl Render for CopilotCodeVerification {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = match &self.status {
            Status::SigningIn { prompt: None } => Icon::new(IconName::ArrowCircle)
                .color(Color::Muted)
                .with_rotate_animation(2)
                .into_any_element(),
            Status::SigningIn {
                prompt: Some(prompt),
            } => {
                Self::render_prompting_modal(self.copilot.clone(), self.connect_clicked, prompt, cx)
                    .into_any_element()
            }
            Status::Unauthorized => {
                self.connect_clicked = false;
                self.render_unauthorized_modal(cx).into_any_element()
            }
            Status::Authorized => {
                self.connect_clicked = false;
                Self::render_enabled_modal(cx).into_any_element()
            }
            Status::Error(..) => {
                Self::render_error_modal(self.copilot.clone(), cx).into_any_element()
            }
            _ => div().into_any_element(),
        };

        v_flex()
            .id("copilot_code_verification")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .px_4()
            .py_8()
            .gap_2()
            .items_center()
            .justify_center()
            .elevation_3(cx)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .child(
                Vector::new(VectorName::ZedXCopilot, rems(8.), rems(4.))
                    .color(Color::Custom(cx.theme().colors().icon)),
            )
            .child(prompt)
    }
}

pub struct CopilotChatCodeVerification {
    status: CopilotChatStatus,
    connect_clicked: bool,
    focus_handle: FocusHandle,
    copilot_chat: Entity<CopilotChat>,
    _subscription: Subscription,
}

impl Focusable for CopilotChatCodeVerification {
    fn focus_handle(&self, _: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for CopilotChatCodeVerification {}

impl CopilotChatCodeVerification {
    pub fn new(
        copilot_chat: &Entity<CopilotChat>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        window.on_window_should_close(cx, |window, cx| {
            if let Some(this) = window.root::<CopilotChatCodeVerification>().flatten() {
                this.update(cx, |this, cx| {
                    this.before_dismiss(cx);
                });
            }
            true
        });
        cx.subscribe_in(
            &cx.entity(),
            window,
            |this, _, _: &DismissEvent, window, cx| {
                window.remove_window();
                this.before_dismiss(cx);
            },
        )
        .detach();

        let status = copilot_chat.read(cx).status();
        Self {
            status,
            connect_clicked: false,
            focus_handle: cx.focus_handle(),
            copilot_chat: copilot_chat.clone(),
            _subscription: cx.observe(copilot_chat, |this, copilot_chat, cx| {
                let status = copilot_chat.read(cx).status();
                let should_dismiss = matches!(
                    status,
                    CopilotChatStatus::Authorized | CopilotChatStatus::Error(_)
                );
                this.status = status;
                cx.notify();
                if should_dismiss {
                    cx.emit(DismissEvent);
                }
            }),
        }
    }

    fn render_device_code(user_code: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let copied = cx
            .read_from_clipboard()
            .map(|item| item.text().as_deref() == Some(user_code))
            .unwrap_or(false);
        let user_code = user_code.to_owned();

        ButtonLike::new("copy-button")
            .full_width()
            .style(ButtonStyle::Tinted(ui::TintColor::Accent))
            .size(ButtonSize::Medium)
            .child(
                h_flex()
                    .w_full()
                    .p_1()
                    .justify_between()
                    .child(Label::new(user_code.clone()))
                    .child(Label::new(if copied {
                        "コピーしました"
                    } else {
                        "コピー"
                    })),
            )
            .on_click(move |_, window, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(user_code.clone()));
                window.refresh();
            })
    }

    fn render_prompting_modal(
        connect_clicked: bool,
        device_flow: &DeviceFlow,
        cx: &mut Context<Self>,
    ) -> impl Element {
        let connect_button_label = if connect_clicked {
            "接続を待機しています…"
        } else {
            "GitHub に接続"
        };

        v_flex()
            .flex_1()
            .gap_2p5()
            .items_center()
            .text_center()
            .child(Headline::new("Zed で GitHub Copilot Chat を使用").size(HeadlineSize::Large))
            .child(
                Label::new(
                    "Copilot Chat を使用するには、GitHub の有効なサブスクリプションが必要です。",
                )
                .color(Color::Muted),
            )
            .child(Self::render_device_code(&device_flow.user_code, cx))
            .child(
                Label::new(
                    "下のボタンをクリックした後、このコードを GitHub に貼り付けてください。",
                )
                .color(Color::Muted),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        Button::new("connect-button", connect_button_label)
                            .full_width()
                            .style(ButtonStyle::Outlined)
                            .size(ButtonSize::Medium)
                            .on_click({
                                let verification_uri = device_flow.verification_uri.clone();
                                cx.listener(move |this, _, _window, cx| {
                                    cx.open_url(&verification_uri);
                                    this.connect_clicked = true;
                                })
                            }),
                    )
                    .child(
                        Button::new("copilot-chat-enable-cancel-button", "キャンセル")
                            .full_width()
                            .size(ButtonSize::Medium)
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(DismissEvent);
                            })),
                    ),
            )
    }

    fn render_enabled_modal(cx: &mut Context<Self>) -> impl Element {
        v_flex()
            .gap_2()
            .text_center()
            .justify_center()
            .child(Headline::new("Copilot Chat が有効になりました").size(HeadlineSize::Large))
            .child(Label::new("Copilot Chat を使用する準備が整いました。").color(Color::Muted))
            .child(
                Button::new("copilot-chat-enabled-done-button", "完了")
                    .full_width()
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }

    fn render_error_modal(message: &str, cx: &mut Context<Self>) -> impl Element {
        v_flex()
            .gap_2()
            .text_center()
            .justify_center()
            .child(Headline::new("エラーが発生しました").size(HeadlineSize::Large))
            .child(Label::new(message.to_owned()).color(Color::Muted))
            .child(
                Button::new("copilot-chat-error-cancel-button", "キャンセル")
                    .full_width()
                    .style(ButtonStyle::Outlined)
                    .size(ButtonSize::Medium)
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(DismissEvent))),
            )
    }

    fn before_dismiss(&mut self, cx: &mut Context<Self>) -> workspace::DismissDecision {
        self.copilot_chat.update(cx, |chat, cx| {
            if matches!(
                chat.status(),
                CopilotChatStatus::SigningIn { .. } | CopilotChatStatus::Starting
            ) {
                chat.sign_out(cx).detach_and_log_err(cx);
            }
        });
        workspace::DismissDecision::Dismiss(true)
    }
}

impl Render for CopilotChatCodeVerification {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt = match self.status.clone() {
            CopilotChatStatus::Starting | CopilotChatStatus::SignedOut => {
                Icon::new(IconName::ArrowCircle)
                    .color(Color::Muted)
                    .with_rotate_animation(2)
                    .into_any_element()
            }
            CopilotChatStatus::SigningIn { device_flow } => {
                Self::render_prompting_modal(self.connect_clicked, &device_flow, cx)
                    .into_any_element()
            }
            CopilotChatStatus::Authorized => {
                self.connect_clicked = false;
                Self::render_enabled_modal(cx).into_any_element()
            }
            CopilotChatStatus::Error(message) => {
                self.connect_clicked = false;
                Self::render_error_modal(&message, cx).into_any_element()
            }
        };

        v_flex()
            .id("copilot_chat_code_verification")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .px_4()
            .py_8()
            .gap_2()
            .items_center()
            .justify_center()
            .elevation_3(cx)
            .on_action(cx.listener(|_, _: &menu::Cancel, _, cx| {
                cx.emit(DismissEvent);
            }))
            .on_any_mouse_down(cx.listener(|this, _: &MouseDownEvent, window, cx| {
                window.focus(&this.focus_handle, cx);
            }))
            .child(
                Vector::new(VectorName::ZedXCopilot, rems(8.), rems(4.))
                    .color(Color::Custom(cx.theme().colors().icon)),
            )
            .child(prompt)
    }
}

pub struct ConfigurationView {
    copilot_status: Option<Status>,
    chat_status: Option<CopilotChatStatus>,
    is_authenticated: Box<dyn Fn(&mut App) -> bool + 'static>,
    edit_prediction: bool,
    /// When `true`, renders a compact control suitable for an inline settings
    /// row: no explanatory labels (those live in the row's left column) and
    /// content-sized buttons instead of full-width ones.
    compact: bool,
    _subscription: Option<Subscription>,
}

pub enum ConfigurationMode {
    Chat,
    EditPrediction,
}

impl ConfigurationView {
    pub fn new(
        is_authenticated: impl Fn(&mut App) -> bool + 'static,
        mode: ConfigurationMode,
        cx: &mut Context<Self>,
    ) -> Self {
        let edit_prediction = matches!(mode, ConfigurationMode::EditPrediction);

        match mode {
            ConfigurationMode::EditPrediction => {
                let copilot = AppState::try_global(cx)
                    .and_then(|state| GlobalCopilotAuth::try_get_or_init(state, cx));

                Self {
                    copilot_status: copilot.as_ref().map(|copilot| copilot.0.read(cx).status()),
                    chat_status: None,
                    is_authenticated: Box::new(is_authenticated),
                    edit_prediction,
                    _subscription: copilot.as_ref().map(|copilot| {
                        cx.observe(&copilot.0, |this, model, cx| {
                            this.copilot_status = Some(model.read(cx).status());
                            cx.notify();
                        })
                    }),
                    compact: false,
                }
            }
            ConfigurationMode::Chat => {
                let copilot_chat = CopilotChat::global(cx);

                Self {
                    copilot_status: None,
                    chat_status: copilot_chat
                        .as_ref()
                        .map(|copilot_chat| copilot_chat.read(cx).status()),
                    is_authenticated: Box::new(is_authenticated),
                    edit_prediction,
                    _subscription: copilot_chat.as_ref().map(|copilot_chat| {
                        cx.observe(copilot_chat, |this, model, cx| {
                            this.chat_status = Some(model.read(cx).status());
                            cx.notify();
                        })
                    }),
                    compact: false,
                }
            }
        }
    }

    /// Renders the view compactly for an inline settings row (no labels,
    /// content-sized buttons). The explanatory copy is expected to be shown
    /// elsewhere (e.g. the row's left column).
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }
}

impl ConfigurationView {
    fn is_starting(&self) -> bool {
        if self.edit_prediction {
            matches!(&self.copilot_status, Some(Status::Starting { .. }))
        } else {
            matches!(&self.chat_status, Some(CopilotChatStatus::Starting))
        }
    }

    fn is_signing_in(&self) -> bool {
        if self.edit_prediction {
            matches!(
                &self.copilot_status,
                Some(Status::SigningIn { .. })
                    | Some(Status::SignedOut {
                        awaiting_signing_in: true
                    })
            )
        } else {
            matches!(
                &self.chat_status,
                Some(CopilotChatStatus::SigningIn { .. } | CopilotChatStatus::Starting)
            )
        }
    }

    fn is_error(&self) -> bool {
        if self.edit_prediction {
            matches!(&self.copilot_status, Some(Status::Error(_)))
        } else {
            matches!(&self.chat_status, Some(CopilotChatStatus::Error(_)))
        }
    }

    fn has_no_status(&self) -> bool {
        if self.edit_prediction {
            self.copilot_status.is_none()
        } else {
            self.chat_status.is_none()
        }
    }

    fn loading_message(&self) -> Option<SharedString> {
        let product = if self.edit_prediction {
            "Copilot Edit Predictions"
        } else {
            "Copilot Chat"
        };
        if self.is_starting() {
            Some(format!("{product} を起動しています…").into())
        } else if self.is_signing_in() {
            Some(format!("{product} にサインインしています…").into())
        } else {
            None
        }
    }

    fn render_loading_button(
        &self,
        label: impl Into<SharedString>,
        edit_prediction: bool,
    ) -> impl IntoElement {
        Button::new("loading_button", label)
            .map(|this| {
                if edit_prediction || self.compact {
                    this.size(ButtonSize::Medium)
                } else {
                    this.full_width()
                }
            })
            .disabled(true)
            .loading(true)
            .style(ButtonStyle::Outlined)
    }

    fn render_sign_in_button(&self, edit_prediction: bool) -> impl IntoElement {
        let label = if edit_prediction {
            "GitHub にサインイン"
        } else {
            "サインイン"
        };

        Button::new("sign_in", label)
            .map(|this| {
                if edit_prediction || self.compact {
                    this.size(ButtonSize::Medium)
                } else {
                    this.full_width()
                }
            })
            .style(ButtonStyle::Outlined)
            .when(edit_prediction, |this| this.tab_index(0isize))
            .on_click(move |_, window, cx| {
                if edit_prediction {
                    let app_state = AppState::global(cx);
                    if let Some(copilot) = GlobalCopilotAuth::try_get_or_init(app_state, cx) {
                        initiate_sign_in(copilot.0, window, cx)
                    }
                } else if let Some(copilot_chat) = CopilotChat::global(cx) {
                    copilot_chat.update(cx, |copilot_chat, cx| copilot_chat.sign_in(cx));
                    open_copilot_chat_code_verification_window(&copilot_chat, window, cx);
                }
            })
    }

    fn render_reinstall_button(&self, edit_prediction: bool) -> impl IntoElement {
        let label = if edit_prediction {
            "再インストールしてサインイン"
        } else {
            "Copilot を再インストールしてサインイン"
        };

        Button::new("reinstall_and_sign_in", label)
            .map(|this| {
                if edit_prediction || self.compact {
                    this.size(ButtonSize::Medium)
                } else {
                    this.full_width()
                }
            })
            .style(ButtonStyle::Outlined)
            .start_icon(
                Icon::new(IconName::Download)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .on_click(|_, window, cx| {
                let app_state = AppState::global(cx);
                if let Some(copilot) = GlobalCopilotAuth::try_get_or_init(app_state, cx) {
                    reinstall_and_sign_in(copilot.0, window, cx);
                }
            })
    }

    fn render_for_edit_prediction(&self) -> impl IntoElement {
        let container = |description: SharedString, action: AnyElement| {
            h_flex()
                .pt_2p5()
                .w_full()
                .justify_between()
                .child(
                    v_flex()
                        .w_full()
                        .max_w_1_2()
                        .child(Label::new("認証が必要"))
                        .child(
                            Label::new(description)
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                        ),
                )
                .child(action)
        };

        let start_label = "編集予測に Copilot を使用するには、GitHub にログインしている必要があります。GitHub アカウントに有効な Copilot のサブスクリプションが必要な点にご注意ください。".into();
        let no_status_label = "Copilot Edit Predictions には有効な GitHub Copilot のサブスクリプションが必要です。Copilot Edit Predictions が設定済みであることを確認してもう一度お試しいただくか、別の編集予測プロバイダーを使用してください。".into();

        if let Some(msg) = self.loading_message() {
            container(
                start_label,
                self.render_loading_button(msg, true).into_any_element(),
            )
            .into_any_element()
        } else if self.is_error() {
            container(
                ERROR_LABEL.into(),
                self.render_reinstall_button(true).into_any_element(),
            )
            .into_any_element()
        } else if self.has_no_status() {
            container(
                no_status_label,
                self.render_sign_in_button(true).into_any_element(),
            )
            .into_any_element()
        } else {
            container(
                start_label,
                self.render_sign_in_button(true).into_any_element(),
            )
            .into_any_element()
        }
    }

    fn render_for_chat(&self) -> impl IntoElement {
        let start_label = "Zed のエージェントで GitHub Copilot Chat を使用するには、GitHub にログインしている必要があります。GitHub アカウントに有効な Copilot Chat のサブスクリプションが必要な点にご注意ください。";
        let no_status_label = "Copilot Chat には有効な GitHub Copilot のサブスクリプションが必要です。Copilot Chat が設定済みであることを確認してもう一度お試しいただくか、別の LLM プロバイダーを使用してください。";

        let (label, button) = if let Some(msg) = self.loading_message() {
            (
                start_label,
                self.render_loading_button(msg, false).into_any_element(),
            )
        } else if self.is_error() {
            (
                "Copilot Chat のサインインで問題が発生しました。もう一度お試しください。",
                self.render_sign_in_button(false).into_any_element(),
            )
        } else if self.has_no_status() {
            (
                no_status_label,
                self.render_sign_in_button(false).into_any_element(),
            )
        } else {
            (
                start_label,
                self.render_sign_in_button(false).into_any_element(),
            )
        };

        v_flex()
            .gap_2()
            .when(!self.compact, |this| this.child(Label::new(label)))
            .child(button)
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_authenticated = &self.is_authenticated;
        let edit_prediction = self.edit_prediction;

        if is_authenticated(cx) {
            return ConfiguredApiCard::new("copilot-authorized", "認証済み")
                .button_label("サインアウト")
                .on_click(move |_, window, cx| {
                    if edit_prediction {
                        if let Some(auth) = GlobalCopilotAuth::try_global(cx) {
                            initiate_sign_out(auth.0.clone(), window, cx);
                        }
                    } else if let Some(copilot_chat) = CopilotChat::global(cx) {
                        copilot_chat
                            .update(cx, |copilot_chat, cx| copilot_chat.sign_out(cx))
                            .detach_and_log_err(cx);
                    }
                })
                .into_any_element();
        }

        if self.edit_prediction {
            self.render_for_edit_prediction().into_any_element()
        } else {
            self.render_for_chat().into_any_element()
        }
    }
}
