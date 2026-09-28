use gpui::{Action, App, Context, Entity, FontWeight, Render, Subscription, WeakEntity, Window};
use settings::SettingsStore;
use ui::{ButtonLike, Tooltip, text_for_keystrokes};
use vim_mode_setting::HelixModeSetting;
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle, ui::prelude::*};

use crate::{ToggleHelixMode, ToggleVimMode, Vim, VimEvent, VimGlobals};

const DISABLE_TITLE: &str = "Vim モードを無効にします";
const DISABLE_META: &str =
    "無効にすると VSCode と同じように、ファイルを開いた直後からそのまま文字を入力できます。";
const ENABLE_TITLE: &str = "Vim モードを有効にします";
const ENABLE_META: &str = "有効にすると Vim のキーバインドで操作します。";

/// The ModeIndicator displays the current mode in the status bar.
pub struct ModeIndicator {
    vim: Option<WeakEntity<Vim>>,
    pending_keys: Option<String>,
    vim_subscription: Option<Subscription>,
    _settings_subscription: Subscription,
}

impl ModeIndicator {
    /// Construct a new mode indicator in this window.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_pending_input(window, |this: &mut Self, window, cx| {
            this.update_pending_keys(window, cx);
            cx.notify();
        })
        .detach();

        let handle = cx.entity();
        let window_handle = window.window_handle();
        cx.observe_new::<Vim>(move |_, window, cx| {
            let Some(window) = window else {
                return;
            };
            if window.window_handle() != window_handle {
                return;
            }
            let vim = cx.entity();
            handle.update(cx, |_, cx| {
                cx.subscribe(&vim, |mode_indicator, vim, event, cx| match event {
                    VimEvent::Focused => {
                        mode_indicator.vim_subscription =
                            Some(cx.observe(&vim, |_, _, cx| cx.notify()));
                        mode_indicator.vim = Some(vim.downgrade());
                    }
                })
                .detach()
            })
        })
        .detach();

        // Turning Vim mode off drops the `Vim` entity without notifying this
        // view, so without this the toggle would keep showing the last mode
        // until something else forced a redraw.
        let settings_subscription = cx.observe_global::<SettingsStore>(|_, cx| cx.notify());

        Self {
            vim: None,
            pending_keys: None,
            vim_subscription: None,
            _settings_subscription: settings_subscription,
        }
    }

    fn update_pending_keys(&mut self, window: &mut Window, cx: &App) {
        self.pending_keys = window
            .pending_input_keystrokes()
            .map(|keystrokes| text_for_keystrokes(keystrokes, cx));
    }

    fn vim(&self) -> Option<Entity<Vim>> {
        self.vim.as_ref().and_then(|vim| vim.upgrade())
    }

    /// The action that flips modal editing on or off. `ToggleVimMode` would
    /// move a Helix user over to Vim instead of letting them type normally, so
    /// while Helix is the active mode the button has to go through Helix's own
    /// toggle (which clears `vim_mode` too).
    fn toggle_action(cx: &App) -> Box<dyn Action> {
        if HelixModeSetting::is_enabled(cx) {
            ToggleHelixMode.boxed_clone()
        } else {
            ToggleVimMode.boxed_clone()
        }
    }

    /// The compact button shown whenever there is no mode to display: modal
    /// editing is off, or it is on but no editor has been focused yet. Either
    /// way the status bar keeps an obvious, mouse-only way back to plain
    /// VSCode-style editing.
    fn render_toggle_button(enabled: bool, cx: &App) -> Button {
        let action = Self::toggle_action(cx);
        let tooltip_action = action.boxed_clone();
        let (label, title, meta) = if enabled {
            ("Vim: オン", DISABLE_TITLE, DISABLE_META)
        } else {
            ("Vim: オフ", ENABLE_TITLE, ENABLE_META)
        };

        Button::new("vim-mode-toggle", label)
            .label_size(LabelSize::Small)
            .color(if enabled {
                Color::Default
            } else {
                Color::Muted
            })
            .tab_index(0isize)
            .aria_label(title)
            .tooltip(move |_window, cx| Tooltip::with_meta(title, Some(&*tooltip_action), meta, cx))
            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
    }

    fn current_operators_description(&self, vim: Entity<Vim>, cx: &mut Context<Self>) -> String {
        let recording = Vim::globals(cx)
            .recording_register
            .map(|reg| format!("recording @{reg} "))
            .into_iter();

        let vim = vim.read(cx);
        recording
            .chain(
                cx.global::<VimGlobals>()
                    .pre_count
                    .map(|count| format!("{}", count)),
            )
            .chain(vim.selected_register.map(|reg| format!("\"{reg}")))
            .chain(vim.operator_stack.iter().map(|item| item.status()))
            .chain(
                cx.global::<VimGlobals>()
                    .post_count
                    .map(|count| format!("{}", count)),
            )
            .collect::<Vec<_>>()
            .concat()
    }
}

impl Render for ModeIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !Vim::enabled(cx) {
            return Self::render_toggle_button(false, cx).into_any_element();
        }

        let vim = self.vim();
        let Some(vim) = vim else {
            return Self::render_toggle_button(true, cx).into_any_element();
        };

        let vim_readable = vim.read(cx);
        let status_label = vim_readable.status_label.clone();
        let temp_mode = vim_readable.temp_mode;
        let mode = vim_readable.mode;

        let theme = cx.theme();
        let colors = theme.colors();
        let system_transparent = gpui::hsla(0.0, 0.0, 0.0, 0.0);
        let vim_mode_text = match mode {
            crate::state::Mode::Normal => colors.vim_normal_foreground,
            crate::state::Mode::Insert => colors.vim_insert_foreground,
            crate::state::Mode::Replace => colors.vim_replace_foreground,
            crate::state::Mode::Visual => colors.vim_visual_foreground,
            crate::state::Mode::VisualLine => colors.vim_visual_line_foreground,
            crate::state::Mode::VisualBlock => colors.vim_visual_block_foreground,
            crate::state::Mode::HelixNormal => colors.vim_helix_normal_foreground,
            crate::state::Mode::HelixSelect => colors.vim_helix_select_foreground,
        };
        let bg_color = match mode {
            crate::state::Mode::Normal => colors.vim_normal_background,
            crate::state::Mode::Insert => colors.vim_insert_background,
            crate::state::Mode::Replace => colors.vim_replace_background,
            crate::state::Mode::Visual => colors.vim_visual_background,
            crate::state::Mode::VisualLine => colors.vim_visual_line_background,
            crate::state::Mode::VisualBlock => colors.vim_visual_block_background,
            crate::state::Mode::HelixNormal => colors.vim_helix_normal_background,
            crate::state::Mode::HelixSelect => colors.vim_helix_select_background,
        };

        let (label, mode): (SharedString, Option<SharedString>) = if let Some(label) = status_label
        {
            (label, None)
        } else {
            let mode_str = if temp_mode {
                format!("(insert) {}", mode)
            } else {
                mode.to_string()
            };

            let current_operators_description = self.current_operators_description(vim.clone(), cx);
            let pending = self
                .pending_keys
                .as_ref()
                .unwrap_or(&current_operators_description);
            let mode = if bg_color != system_transparent {
                mode_str.into()
            } else {
                format!("-- {} --", mode_str).into()
            };
            (pending.into(), Some(mode))
        };

        let action = Self::toggle_action(cx);
        let tooltip_action = action.boxed_clone();

        ButtonLike::new("vim-mode-indicator")
            .tab_index(0isize)
            .aria_label(DISABLE_TITLE)
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(DISABLE_TITLE, Some(&*tooltip_action), DISABLE_META, cx)
            })
            .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
            .child(
                h_flex()
                    .h(ButtonSize::Default.rems())
                    .gap_1()
                    .when(!label.is_empty(), |el| {
                        el.child(
                            Label::new(label)
                                .size(LabelSize::Small)
                                .weight(FontWeight::MEDIUM),
                        )
                    })
                    .when_some(mode, |el, mode| {
                        el.child(
                            v_flex()
                                .when(bg_color != system_transparent, |el| el.px_2())
                                // match with other icons at the bottom that use default buttons
                                .h(ButtonSize::Default.rems())
                                .justify_center()
                                .rounded_sm()
                                .bg(bg_color)
                                .child(
                                    Label::new(mode)
                                        .size(LabelSize::Small)
                                        .weight(FontWeight::MEDIUM)
                                        .when(
                                            bg_color != system_transparent
                                                && vim_mode_text != system_transparent,
                                            |el| el.color(Color::Custom(vim_mode_text)),
                                        ),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }
}

impl StatusItemView for ModeIndicator {
    fn set_active_pane_item(
        &mut self,
        _active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // This item doubles as the only GUI switch for Vim mode, so hiding it
        // would strand users who turned Vim on by accident with no way back.
        None
    }
}
