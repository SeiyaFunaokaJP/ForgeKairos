use gpui::{
    Action, AnyElement, App, Context, Entity, FontWeight, Render, Subscription, WeakEntity, Window,
};
use settings::{EditingModeContent, SettingsStore};
use ui::{ButtonLike, ContextMenu, PopoverMenu, Tooltip, text_for_keystrokes};
use vim_mode_setting::{HelixModeSetting, current_editing_mode, write_editing_mode};
use workspace::{HideStatusItem, StatusItemView, item::ItemHandle, ui::prelude::*};

use crate::{ToggleHelixMode, ToggleVimMode, Vim, VimEvent, VimGlobals};

const DISABLE_TITLE: &str = i18n::msg!("Disable Vim Mode");
const DISABLE_META: &str =
    i18n::msg!("When disabled, you can type as soon as you open a file, just like in VSCode.");
const VIM_SHORTCUT_META: &str =
    i18n::msg!("Choose Normal, Vim, Helix or Emacs. The shortcut turns Vim mode on or off.");
const HELIX_SHORTCUT_META: &str =
    i18n::msg!("Choose Normal, Vim, Helix or Emacs. The shortcut turns Helix mode on or off.");

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

    fn editing_mode_label(mode: EditingModeContent) -> SharedString {
        match mode {
            EditingModeContent::Normal => i18n::t!("Normal").into(),
            EditingModeContent::Vim => "Vim".into(),
            EditingModeContent::Helix => "Helix".into(),
            EditingModeContent::Emacs => "Emacs".into(),
        }
    }

    fn editing_mode_menu(menu: ContextMenu, cx: &App) -> ContextMenu {
        let current_mode = current_editing_mode(cx);
        [
            EditingModeContent::Normal,
            EditingModeContent::Vim,
            EditingModeContent::Helix,
            EditingModeContent::Emacs,
        ]
        .into_iter()
        .fold(menu, |menu, mode| {
            let shortcut: Option<Box<dyn Action>> = match mode {
                EditingModeContent::Vim => Some(ToggleVimMode.boxed_clone()),
                EditingModeContent::Helix => Some(ToggleHelixMode.boxed_clone()),
                EditingModeContent::Normal | EditingModeContent::Emacs => None,
            };
            menu.toggleable_entry(
                Self::editing_mode_label(mode),
                mode == current_mode,
                IconPosition::Start,
                shortcut,
                move |_, cx| {
                    write_editing_mode(mode, cx);
                },
            )
        })
    }

    /// The editing mode switch, shown at all times next to the modal editing
    /// mode (if any), so the status bar always has an obvious, mouse-only way
    /// to change how keys edit text, including back to plain VSCode-style
    /// editing.
    fn render_editing_mode_menu(cx: &App) -> AnyElement {
        let current_mode = current_editing_mode(cx);
        let label = Self::editing_mode_label(current_mode);
        let tooltip_action = Self::toggle_action(cx);
        let tooltip_meta = i18n::tr(if HelixModeSetting::is_enabled(cx) {
            HELIX_SHORTCUT_META
        } else {
            VIM_SHORTCUT_META
        });

        PopoverMenu::new("editing-mode-menu")
            .anchor(gpui::Anchor::BottomRight)
            .menu(|window, cx| {
                Some(ContextMenu::build(window, cx, |menu, _, cx| {
                    Self::editing_mode_menu(menu, cx)
                }))
            })
            .trigger_with_tooltip(
                Button::new("editing-mode-button", label.clone())
                    .label_size(LabelSize::Small)
                    .color(if current_mode == EditingModeContent::Normal {
                        Color::Muted
                    } else {
                        Color::Default
                    })
                    .tab_index(0isize)
                    .aria_label(i18n::t!("Editing Mode: {mode}", mode = label)),
                move |_window, cx| {
                    Tooltip::with_meta(
                        i18n::t!("Editing Mode"),
                        Some(&*tooltip_action),
                        tooltip_meta,
                        cx,
                    )
                },
            )
            .into_any_element()
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
        let mode_display = if Vim::enabled(cx) {
            self.vim().map(|vim| self.render_mode_display(vim, cx))
        } else {
            None
        };

        h_flex().gap_0p5().children(mode_display).child(
            div()
                .debug_selector(|| "editing-mode-menu".into())
                .child(Self::render_editing_mode_menu(cx)),
        )
    }
}

impl ModeIndicator {
    fn render_mode_display(&self, vim: Entity<Vim>, cx: &mut Context<Self>) -> AnyElement {
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
            .aria_label(i18n::tr(DISABLE_TITLE))
            .tooltip(move |_window, cx| {
                Tooltip::with_meta(
                    i18n::tr(DISABLE_TITLE),
                    Some(&*tooltip_action),
                    i18n::tr(DISABLE_META),
                    cx,
                )
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use gpui::{BorrowAppContext, TestAppContext, VisualTestContext};
    use project::{FakeFs, Fs};
    use settings::{BaseKeymap, EditingModeContent, Settings, SettingsContent, SettingsStore};
    use ui::ContextMenu;
    use vim_mode_setting::{HelixModeSetting, VimModeSetting, current_editing_mode};

    use super::ModeIndicator;

    async fn init_with_user_settings(
        user_settings: &str,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        let settings_dir = paths::settings_file()
            .parent()
            .expect("the settings file is in a directory");
        fs.create_dir(settings_dir)
            .await
            .expect("the fake settings directory is created");
        fs.insert_file(paths::settings_file(), user_settings.as_bytes().to_vec())
            .await;
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            <dyn Fs>::set_global(fs.clone(), cx);
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.set_user_settings(user_settings, cx).result()
            })
        })
        .expect("the test's user settings parse");
        let window = cx.add_empty_window().clone();
        (fs, window)
    }

    /// Opens the status bar's menu, checks which mode it marks as current,
    /// picks `mode` from it and loads what was written as the user settings.
    async fn choose_from_menu(
        mode: EditingModeContent,
        fs: &FakeFs,
        cx: &mut VisualTestContext,
    ) -> (SettingsContent, EditingModeContent) {
        let modes = [
            EditingModeContent::Normal,
            EditingModeContent::Vim,
            EditingModeContent::Helix,
            EditingModeContent::Emacs,
        ];
        let menu = cx.update(|window, cx| {
            ContextMenu::build(window, cx, |menu, _, cx| {
                ModeIndicator::editing_mode_menu(menu, cx)
            })
        });
        let marked_mode = menu.update_in(cx, |menu, window, cx| {
            menu.select_toggled_or_first(window, cx);
            menu.selected_index()
                .and_then(|index| modes.get(index).copied())
        });
        let marked_mode = marked_mode.expect("the menu marks a mode");
        assert_eq!(
            Some(marked_mode),
            cx.update(|_, cx| Some(current_editing_mode(cx))),
            "the menu marks the current mode"
        );

        let target_index = modes
            .iter()
            .position(|candidate| *candidate == mode)
            .expect("every mode is in the menu");
        menu.update_in(cx, |menu, window, cx| {
            menu.select_first(&menu::SelectFirst, window, cx);
            for _ in 0..target_index {
                menu.select_next(&menu::SelectNext, window, cx);
            }
            menu.confirm(&menu::Confirm, window, cx);
        });
        cx.run_until_parked();

        let written = fs
            .load(paths::settings_file())
            .await
            .expect("the settings file was written");
        cx.update(|_, cx| {
            cx.update_global(|store: &mut SettingsStore, cx| {
                store.set_user_settings(&written, cx).result()
            })
        })
        .expect("the written settings parse");
        let content: SettingsContent =
            settings::parse_json_with_comments(&written).expect("the written settings parse");
        (content, marked_mode)
    }

    fn resolved(cx: &mut VisualTestContext) -> (bool, bool, BaseKeymap) {
        cx.update(|_, cx| {
            (
                VimModeSetting::is_enabled(cx),
                HelixModeSetting::is_enabled(cx),
                *BaseKeymap::get_global(cx),
            )
        })
    }

    #[gpui::test]
    async fn leaving_emacs_from_the_status_bar_gives_the_base_keymap_back(cx: &mut TestAppContext) {
        let (fs, mut cx) =
            init_with_user_settings("{\n  \"base_keymap\": \"Emacs\"\n}\n", cx).await;

        let (content, marked) = choose_from_menu(EditingModeContent::Normal, &fs, &mut cx).await;
        assert_eq!(marked, EditingModeContent::Emacs);
        assert_eq!(content.base_keymap, None);
        assert_eq!(content.vim_mode, None);
        assert_eq!(content.helix_mode, None);
        let (vim_mode, helix_mode, base_keymap) = resolved(&mut cx);
        assert!(!vim_mode && !helix_mode);
        assert_ne!(base_keymap, BaseKeymap::Emacs);
        cx.update(|_, cx| assert_eq!(current_editing_mode(cx), EditingModeContent::Normal));
    }

    #[gpui::test]
    async fn moving_from_helix_to_vim_switches_helix_off(cx: &mut TestAppContext) {
        let (fs, mut cx) = init_with_user_settings(
            "{\n  \"helix_mode\": true,\n  \"base_keymap\": \"JetBrains\"\n}\n",
            cx,
        )
        .await;

        let (content, marked) = choose_from_menu(EditingModeContent::Vim, &fs, &mut cx).await;
        assert_eq!(marked, EditingModeContent::Helix);
        assert_eq!(content.vim_mode, Some(true));
        assert_eq!(content.helix_mode, Some(false));
        assert_eq!(
            content.base_keymap,
            Some(settings::BaseKeymapContent::JetBrains),
            "a base keymap other than Emacs stays"
        );
        assert_eq!(resolved(&mut cx), (true, false, BaseKeymap::JetBrains));
    }

    #[gpui::test]
    async fn moving_from_vim_to_emacs_switches_vim_off(cx: &mut TestAppContext) {
        let (fs, mut cx) = init_with_user_settings("{\n  \"vim_mode\": true\n}\n", cx).await;

        let (content, marked) = choose_from_menu(EditingModeContent::Emacs, &fs, &mut cx).await;
        assert_eq!(marked, EditingModeContent::Vim);
        assert_eq!(content.vim_mode, Some(false));
        assert_eq!(content.helix_mode, None);
        assert_eq!(
            content.base_keymap,
            Some(settings::BaseKeymapContent::Emacs)
        );
        assert_eq!(resolved(&mut cx), (false, false, BaseKeymap::Emacs));

        let (content, marked) = choose_from_menu(EditingModeContent::Helix, &fs, &mut cx).await;
        assert_eq!(marked, EditingModeContent::Emacs);
        assert_eq!(content.vim_mode, Some(false));
        assert_eq!(content.helix_mode, Some(true));
        assert_eq!(content.base_keymap, None);
        cx.update(|_, cx| assert_eq!(current_editing_mode(cx), EditingModeContent::Helix));
    }

    // The quick action bar switches Vim on for the session only, while the
    // file says nothing. Choosing Normal must still end it.
    #[gpui::test]
    async fn choosing_normal_ends_a_session_only_vim_mode(cx: &mut TestAppContext) {
        let (fs, mut cx) = init_with_user_settings("{}\n", cx).await;
        cx.update(|_, cx| VimModeSetting::override_global(VimModeSetting(true), cx));

        let (content, marked) = choose_from_menu(EditingModeContent::Normal, &fs, &mut cx).await;
        assert_eq!(marked, EditingModeContent::Vim);
        assert_eq!(content.vim_mode, Some(false));
        assert!(!resolved(&mut cx).0);
    }

    // The switch is the only mouse way out of Vim, so it has to stay next to
    // the mode display instead of replacing it.
    #[gpui::test]
    async fn the_switch_shows_with_and_without_vim(cx: &mut TestAppContext) {
        let mut cx = crate::test::VimTestContext::new(cx, false).await;
        cx.run_until_parked();
        assert!(cx.debug_bounds("editing-mode-menu").is_some());

        cx.enable_vim();
        cx.run_until_parked();
        let shows_vim_mode = cx.workspace(|workspace, _, cx| {
            workspace
                .status_bar()
                .read(cx)
                .item_of_type::<ModeIndicator>()
                .is_some_and(|indicator| indicator.read(cx).vim().is_some())
        });
        assert!(shows_vim_mode, "the mode display is up next to the switch");
        assert!(cx.debug_bounds("editing-mode-menu").is_some());

        cx.disable_vim();
        cx.run_until_parked();
        assert!(cx.debug_bounds("editing-mode-menu").is_some());
    }
}
