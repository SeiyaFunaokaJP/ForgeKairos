use std::{cell::RefCell, cmp::Reverse, rc::Rc, sync::Arc};

use acp_thread::AgentSessionConfigOptions;
use agent_client_protocol::schema::v1 as acp;
use agent_servers::AgentServer;

use collections::{HashMap, HashSet};
use fs::Fs;
use fuzzy::StringMatchCandidate;
use gpui::{
    App, BackgroundExecutor, Context, DismissEvent, Entity, Focusable, Subscription, Task, Window,
    prelude::*,
};
use ordered_float::OrderedFloat;
use picker::popover_menu::PickerPopoverMenu;
use picker::{Picker, PickerDelegate};
use settings::{AgentConfigOptionValue, SettingsStore};
use ui::{
    ElevationIndex, IconButton, KeyBinding, ListItem, ListItemSpacing, PopoverMenuHandle, Switch,
    SwitchLabelPosition, ToggleState, Tooltip, prelude::*,
};
use unicode_segmentation::UnicodeSegmentation;
use util::ResultExt as _;
use zed_actions::agent::ToggleModelSelector;

use crate::model_selector::{
    AgentLaunchEntry, AgentLaunchSource, GroupCollapseState, PickerGroup, PickerGroupKey,
    PickerItem, PickerRow, RowAnchor, anchor_row_index, build_picker_rows,
    filter_agent_launch_entries, group_row_index, grouped_search_text, initial_row_index,
    refresh_rows_after_fold, render_agent_launch_row, render_group_header, row_anchor,
};

type OptionCollapseState = Rc<RefCell<GroupCollapseState<acp::SessionConfigValueId>>>;
use crate::ui::documentation_aside_side;
use crate::{
    CycleFavoriteModels, CycleModeSelector, CycleThinkingEffort, ToggleProfileSelector,
    ToggleThinkingEffortMenu,
};

const PICKER_THRESHOLD: usize = 5;

/// ACP エージェントが返す設定オプションのうち、思考の深さ (reasoning effort) に
/// あたるものを見分ける。
///
/// ACP スレッドには `LanguageModel` が無いのでツールバーのネイティブな思考
/// コントロールは出ず、代わりにこの `ConfigOptionsView` が同じ位置に置かれる。
/// エージェントは `name` を英語で返してくるので、**表示だけ**日本語にする。
/// `id` と値 (`SessionConfigValueId`) はプロトコル上の比較対象なので触らない。
fn is_thought_level_option(option: &acp::SessionConfigOption) -> bool {
    if option.category.as_ref() == Some(&acp::SessionConfigOptionCategory::ThoughtLevel) {
        return true;
    }

    // カテゴリを付けてこないエージェント (Claude Code / Codex の一部バージョン) 向けの
    // 保険。id は安定した機械可読の識別子なので、これで拾う。
    let id = option.id.0.to_ascii_lowercase();
    ["effort", "reasoning", "thinking", "thought"]
        .iter()
        .any(|needle| id.contains(needle))
}

/// The display name of a thinking-effort value. It goes through the one place
/// that names the standard levels (`LanguageModelEffortLevel::display_name`), so
/// it always reads the same as for the native providers; a level of the agent's
/// own keeps the agent's name.
fn thought_level_value_name(value_id: &acp::SessionConfigValueId, name: &str) -> String {
    language_model::LanguageModelEffortLevel {
        name: SharedString::from(name.to_string()),
        value: SharedString::from(value_id.0.to_string()),
        is_default: false,
    }
    .display_name()
    .to_string()
}

/// The option's own name. Only the known kind is translated; any other keeps the
/// agent's wording, since translating an agent's own option names would drift
/// from what they mean.
fn config_option_display_name(option: &acp::SessionConfigOption) -> SharedString {
    if is_thought_level_option(option) {
        return i18n::t!("Thinking Effort").into();
    }
    option.name.clone().into()
}

pub struct ConfigOptionsView {
    config_options: Rc<dyn AgentSessionConfigOptions>,
    selectors: Vec<Entity<ConfigOptionSelector>>,
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    config_option_ids: Vec<acp::SessionConfigId>,
    /// The エージェント section's rows, for the model picker only.
    agent_launchers: Option<AgentLaunchSource>,
    /// Folded groups per option. Kept here because every config update
    /// rebuilds the selectors, and a fold must not spring open with it.
    collapse_states: HashMap<acp::SessionConfigId, OptionCollapseState>,
    _refresh_task: Task<()>,
}

impl ConfigOptionsView {
    pub(crate) fn new(
        config_options: Rc<dyn AgentSessionConfigOptions>,
        agent_server: Rc<dyn AgentServer>,
        fs: Arc<dyn Fs>,
        agent_launchers: Option<AgentLaunchSource>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config_option_ids = Self::config_option_ids(&config_options);

        let rx = config_options.watch(cx);
        let refresh_task = cx.spawn_in(window, async move |this, cx| {
            if let Some(mut rx) = rx {
                while let Ok(()) = rx.recv().await {
                    this.update_in(cx, |this, window, cx| {
                        this.rebuild_selectors(window, cx);
                        cx.notify();
                    })
                    .log_err();
                }
            }
        });

        let mut this = Self {
            config_options,
            selectors: Vec::new(),
            agent_server,
            fs,
            config_option_ids,
            agent_launchers,
            collapse_states: HashMap::default(),
            _refresh_task: refresh_task,
        };
        this.selectors = this.build_selectors(window, cx);
        this
    }

    pub fn toggle_category_picker(
        &mut self,
        category: acp::SessionConfigOptionCategory,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(config_id) = self.first_config_option_id_matching(category, |option| {
            matches!(&option.kind, acp::SessionConfigKind::Select(_))
        }) else {
            return false;
        };

        let Some(selector) = self.selector_for_config_id(&config_id, cx) else {
            return false;
        };

        selector.update(cx, |selector, cx| selector.toggle_picker(window, cx))
    }

    pub fn cycle_category_option(
        &mut self,
        category: acp::SessionConfigOptionCategory,
        favorites_only: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(config_id) = self.first_config_option_id_matching(category, |option| {
            Self::can_cycle_config_option(option, favorites_only)
        }) else {
            return false;
        };

        let Some(next_value) = self.next_value_for_config(&config_id, favorites_only, cx) else {
            return false;
        };
        let default_value = setting_value_for_config_option_value(&next_value);

        self.agent_server.set_default_config_option(
            config_id.0.as_ref(),
            default_value,
            self.fs.clone(),
            cx,
        );

        let task = self
            .config_options
            .set_config_option(config_id, next_value, cx);

        cx.spawn(async move |_, _| {
            if let Err(err) = task.await {
                log::error!("Failed to set config option: {:?}", err);
            }
        })
        .detach();

        true
    }

    fn first_config_option_id_matching(
        &self,
        category: acp::SessionConfigOptionCategory,
        predicate: impl Fn(&acp::SessionConfigOption) -> bool,
    ) -> Option<acp::SessionConfigId> {
        self.config_options
            .config_options()
            .into_iter()
            .find(|option| option.category.as_ref() == Some(&category) && predicate(option))
            .map(|option| option.id)
    }

    fn can_cycle_config_option(option: &acp::SessionConfigOption, favorites_only: bool) -> bool {
        match &option.kind {
            acp::SessionConfigKind::Select(_) => true,
            acp::SessionConfigKind::Boolean(_) => !favorites_only,
            _ => false,
        }
    }

    fn selector_for_config_id(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &App,
    ) -> Option<Entity<ConfigOptionSelector>> {
        self.selectors
            .iter()
            .find(|selector| selector.read(cx).config_id() == config_id)
            .cloned()
    }

    fn next_value_for_config(
        &self,
        config_id: &acp::SessionConfigId,
        favorites_only: bool,
        cx: &mut Context<Self>,
    ) -> Option<acp::SessionConfigOptionValue> {
        let option = self
            .config_options
            .config_options()
            .into_iter()
            .find(|option| &option.id == config_id)?;

        match &option.kind {
            acp::SessionConfigKind::Select(_) => {
                let mut options = extract_options(&self.config_options, config_id);
                if options.is_empty() {
                    return None;
                }

                if favorites_only {
                    let favorites = self
                        .agent_server
                        .favorite_config_option_value_ids(config_id, cx);
                    options.retain(|option| favorites.contains(&option.value));
                    if options.is_empty() {
                        return None;
                    }
                }

                let current_value = get_current_select_value(&self.config_options, config_id);
                let current_index = current_value
                    .as_ref()
                    .and_then(|current| options.iter().position(|option| &option.value == current))
                    .unwrap_or(usize::MAX);

                let next_index = if current_index == usize::MAX {
                    0
                } else {
                    (current_index + 1) % options.len()
                };

                Some(acp::SessionConfigOptionValue::value_id(
                    options[next_index].value.clone(),
                ))
            }
            acp::SessionConfigKind::Boolean(boolean) => {
                if favorites_only {
                    None
                } else {
                    Some(acp::SessionConfigOptionValue::boolean(
                        !boolean.current_value,
                    ))
                }
            }
            _ => None,
        }
    }

    fn config_option_ids(
        config_options: &Rc<dyn AgentSessionConfigOptions>,
    ) -> Vec<acp::SessionConfigId> {
        config_options
            .config_options()
            .into_iter()
            .map(|option| option.id)
            .collect()
    }

    fn rebuild_selectors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Config option updates can mutate option values for existing IDs (for example,
        // reasoning levels after a model switch). Rebuild to refresh cached picker entries.
        self.config_option_ids = Self::config_option_ids(&self.config_options);
        self.selectors = self.build_selectors(window, cx);
        cx.notify();
    }

    fn build_selectors(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<Entity<ConfigOptionSelector>> {
        let options = self.config_options.config_options();
        // The エージェント section goes to the picker the model keybindings open
        // (`handles_category_keybindings`), not to every model-ish option.
        let model_config_id = options
            .iter()
            .find(|option| {
                option.category.as_ref() == Some(&acp::SessionConfigOptionCategory::Model)
                    && matches!(option.kind, acp::SessionConfigKind::Select(_))
            })
            .map(|option| option.id.clone());
        options
            .into_iter()
            .map(|option| {
                let on_popover_closed: Rc<dyn Fn(&mut Window, &mut App)> = {
                    let view = cx.weak_entity();
                    let config_id = option.id.clone();
                    Rc::new(move |window, cx| {
                        let view = view.clone();
                        let config_id = config_id.clone();
                        // The closing picker is being updated right now, and
                        // it may be the one to refresh.
                        window.defer(cx, move |window, cx| {
                            view.update(cx, |view, cx| {
                                view.release_folds_held_while_open(&config_id, window, cx)
                            })
                            .log_err();
                        });
                    })
                };
                let grouping = PickerGrouping {
                    collapse: self
                        .collapse_states
                        .entry(option.id.clone())
                        .or_default()
                        .clone(),
                    agent_launchers: self
                        .agent_launchers
                        .clone()
                        .filter(|_| model_config_id.as_ref() == Some(&option.id)),
                    on_popover_closed,
                };
                let config_options = self.config_options.clone();
                let agent_server = self.agent_server.clone();
                let fs = self.fs.clone();
                cx.new(|cx| {
                    ConfigOptionSelector::new(
                        config_options,
                        option.id.clone(),
                        agent_server,
                        fs,
                        grouping,
                        window,
                        cx,
                    )
                })
            })
            .collect()
    }

    /// The popover of `config_id` closed: the next opening folds by the
    /// favorites as they are now. The picker to refresh is the current one,
    /// which is not the one that closed if a config update rebuilt the
    /// selectors while it was open.
    fn release_folds_held_while_open(
        &mut self,
        config_id: &acp::SessionConfigId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let released = self
            .collapse_states
            .get(config_id)
            .is_some_and(|collapse| collapse.borrow_mut().release_favorites_at_open());
        if !released {
            return;
        }
        if let Some(picker) = self
            .selector_for_config_id(config_id, cx)
            .and_then(|selector| selector.read(cx).picker.clone())
        {
            picker.update(cx, |picker, cx| picker.refresh(window, cx));
        }
    }
}

impl Render for ConfigOptionsView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        if self.selectors.is_empty() {
            return div().into_any_element();
        }

        h_flex()
            .min_w_0()
            .flex_wrap()
            .gap_1()
            .children(self.selectors.iter().cloned())
            .into_any_element()
    }
}

/// What a selector's picker shares with the [`ConfigOptionsView`] that built
/// it, which outlives the selector being rebuilt on every config update.
struct PickerGrouping {
    collapse: OptionCollapseState,
    agent_launchers: Option<AgentLaunchSource>,
    on_popover_closed: Rc<dyn Fn(&mut Window, &mut App)>,
}

struct ConfigOptionSelector {
    config_options: Rc<dyn AgentSessionConfigOptions>,
    config_id: acp::SessionConfigId,
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    picker_handle: Option<PopoverMenuHandle<Picker<ConfigOptionPickerDelegate>>>,
    picker: Option<Entity<Picker<ConfigOptionPickerDelegate>>>,
    setting_value: bool,
}

impl ConfigOptionSelector {
    pub fn new(
        config_options: Rc<dyn AgentSessionConfigOptions>,
        config_id: acp::SessionConfigId,
        agent_server: Rc<dyn AgentServer>,
        fs: Arc<dyn Fs>,
        grouping: PickerGrouping,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let current_option = config_options
            .config_options()
            .into_iter()
            .find(|opt| opt.id == config_id);
        let option_count = current_option
            .as_ref()
            .map(count_config_options)
            .unwrap_or(0);
        let is_select = current_option
            .as_ref()
            .is_some_and(|option| matches!(&option.kind, acp::SessionConfigKind::Select(_)));
        // The model picker ends with the エージェント section, whose rows are
        // found by typing (「claude」), so it needs a query field however few
        // models the agent offers.
        let is_model = current_option.as_ref().is_some_and(|option| {
            option.category.as_ref() == Some(&acp::SessionConfigOptionCategory::Model)
        });

        let is_searchable = option_count >= PICKER_THRESHOLD || is_model;

        let (picker_handle, picker) = if is_select {
            let config_options = config_options.clone();
            let config_id = config_id.clone();
            let agent_server = agent_server.clone();
            let fs = fs.clone();
            let picker = cx.new(move |picker_cx| {
                let delegate = ConfigOptionPickerDelegate::new(
                    config_options,
                    config_id,
                    agent_server,
                    fs,
                    grouping,
                    window,
                    picker_cx,
                );

                if is_searchable {
                    Picker::list(delegate, window, picker_cx)
                } else {
                    Picker::nonsearchable_list(delegate, window, picker_cx)
                }
                .show_scrollbar(true)
                .initial_width(rems(20.))
            });
            (Some(PopoverMenuHandle::default()), Some(picker))
        } else {
            (None, None)
        };

        Self {
            config_options,
            config_id,
            agent_server,
            fs,
            picker_handle,
            picker,
            setting_value: false,
        }
    }

    fn current_option(&self) -> Option<acp::SessionConfigOption> {
        self.config_options
            .config_options()
            .into_iter()
            .find(|opt| opt.id == self.config_id)
    }

    fn config_id(&self) -> &acp::SessionConfigId {
        &self.config_id
    }

    fn toggle_picker(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if let Some(picker_handle) = &self.picker_handle {
            picker_handle.toggle(window, cx);
            true
        } else {
            false
        }
    }

    fn current_value_name(&self) -> String {
        let Some(option) = self.current_option() else {
            return i18n::t!("Unknown").to_string();
        };

        match &option.kind {
            acp::SessionConfigKind::Select(select) => {
                let Some(name) = find_option_name(&select.options, &select.current_value) else {
                    return i18n::t!("Unknown").to_string();
                };
                if is_thought_level_option(&option) {
                    thought_level_value_name(&select.current_value, &name)
                } else {
                    name
                }
            }
            _ => i18n::t!("Unknown").to_string(),
        }
    }

    fn handles_category_keybindings(&self, category: &acp::SessionConfigOptionCategory) -> bool {
        self.config_options
            .config_options()
            .into_iter()
            .find(|option| {
                option.category.as_ref() == Some(category)
                    && matches!(&option.kind, acp::SessionConfigKind::Select(_))
            })
            .is_some_and(|option| option.id == self.config_id)
    }

    fn render_trigger_button(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Button {
        let Some(option) = self.current_option() else {
            return Button::new("config-option-trigger", i18n::t!("Unknown"))
                .label_size(LabelSize::Small)
                .color(Color::Muted)
                .disabled(true);
        };

        let picker_deployed = self
            .picker_handle
            .as_ref()
            .is_some_and(|picker_handle| picker_handle.is_deployed());
        let icon = if picker_deployed {
            IconName::ChevronUp
        } else {
            IconName::ChevronDown
        };

        let value_name = self.current_value_name();
        let mut graphemes = value_name.graphemes(true);
        let truncated = graphemes.by_ref().take(32).collect::<String>();
        let mut display_name = if graphemes.next().is_some() {
            format!("{truncated}…")
        } else {
            truncated
        };
        // 値だけだと何のコントロールか分からないので、思考の深さのときは
        // ネイティブの思考コントロール (`thread_view.rs`) と同じ「思考: 高」の形にする。
        if is_thought_level_option(&option) {
            display_name = i18n::t!("Thinking: {level}", level = display_name);
        }

        Button::new(
            ElementId::Name(format!("config-option-{}", option.id.0).into()),
            display_name,
        )
        .label_size(LabelSize::Small)
        .color(Color::Muted)
        .end_icon(Icon::new(icon).size(IconSize::XSmall).color(Color::Muted))
        .disabled(self.setting_value)
    }
}

impl Render for ConfigOptionSelector {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(option) = self.current_option() else {
            return div().into_any_element();
        };

        match &option.kind {
            acp::SessionConfigKind::Select(_) => {
                let (Some(picker), Some(picker_handle)) =
                    (self.picker.clone(), self.picker_handle.clone())
                else {
                    return div().into_any_element();
                };

                let trigger_button = self.render_trigger_button(window, cx);

                let show_category_keybindings = option
                    .category
                    .as_ref()
                    .is_some_and(|category| self.handles_category_keybindings(category));
                let option_category = option.category.clone();
                let option_name = config_option_display_name(&option);
                let option_description: Option<SharedString> =
                    option.description.clone().map(Into::into);

                let tooltip = Tooltip::element(move |_window, cx| {
                    let mut content = v_flex().gap_1().child(Label::new(option_name.clone()));
                    if let Some(desc) = option_description.as_ref() {
                        content = content.child(
                            Label::new(desc.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                    }

                    let action_tooltip_container = |label: &str, keybinding: KeyBinding| {
                        h_flex()
                            .pt_1()
                            .gap_2()
                            .justify_between()
                            .border_t_1()
                            .border_color(cx.theme().colors().border_variant)
                            .child(Label::new(label))
                            .child(keybinding)
                    };

                    if show_category_keybindings && let Some(category) = &option_category {
                        match category {
                            acp::SessionConfigOptionCategory::Mode => {
                                content = content
                                    .child(action_tooltip_container(
                                        i18n::t!("Change Mode"),
                                        KeyBinding::for_action(&ToggleProfileSelector, cx),
                                    ))
                                    .child(action_tooltip_container(
                                        i18n::t!("Cycle Through Modes"),
                                        KeyBinding::for_action(&CycleModeSelector, cx),
                                    ));
                            }
                            acp::SessionConfigOptionCategory::Model => {
                                content = content
                                    .child(action_tooltip_container(
                                        i18n::t!("Change Model"),
                                        KeyBinding::for_action(&ToggleModelSelector, cx),
                                    ))
                                    .child(action_tooltip_container(
                                        i18n::t!("Cycle Favorite Models"),
                                        KeyBinding::for_action(&CycleFavoriteModels, cx),
                                    ));
                            }
                            acp::SessionConfigOptionCategory::ThoughtLevel => {
                                content = content
                                    .child(action_tooltip_container(
                                        i18n::t!("Change Thinking Effort"),
                                        KeyBinding::for_action(&ToggleThinkingEffortMenu, cx),
                                    ))
                                    .child(action_tooltip_container(
                                        i18n::t!("Cycle Thinking Effort"),
                                        KeyBinding::for_action(&CycleThinkingEffort, cx),
                                    ));
                            }
                            _ => {}
                        }
                    }
                    content.into_any()
                });

                PickerPopoverMenu::new(
                    picker,
                    trigger_button,
                    tooltip,
                    gpui::Anchor::BottomRight,
                    cx,
                )
                .with_handle(picker_handle)
                .render(window, cx)
                .into_any_element()
            }
            acp::SessionConfigKind::Boolean(boolean) => {
                let option_id = option.id.clone();
                let option_name = config_option_display_name(&option);
                let option_description: Option<SharedString> =
                    option.description.clone().map(Into::into);
                let tooltip_name = option_name.clone();
                let tooltip = Tooltip::element(move |_window, _cx| {
                    let mut content = v_flex().gap_1().child(Label::new(tooltip_name.clone()));
                    if let Some(desc) = option_description.as_ref() {
                        content = content.child(
                            Label::new(desc.clone())
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        );
                    }
                    content.into_any()
                });

                let config_id = self.config_id.clone();
                let config_options = self.config_options.clone();
                let agent_server = self.agent_server.clone();
                let fs = self.fs.clone();
                let current_value = boolean.current_value;
                let toggle_state = if current_value {
                    ToggleState::Selected
                } else {
                    ToggleState::Unselected
                };

                h_flex()
                    .id(ElementId::Name(
                        format!("config-option-{}", option_id.0).into(),
                    ))
                    .pr_1()
                    .tooltip(tooltip)
                    .child(
                        Switch::new(
                            ElementId::Name(format!("config-option-{}-switch", option_id.0).into()),
                            toggle_state,
                        )
                        .label(option_name)
                        .label_position(SwitchLabelPosition::Start)
                        .label_size(LabelSize::Small)
                        .label_color(Color::Muted)
                        .disabled(self.setting_value)
                        .on_click(move |state, _window, cx| {
                            let next_value = matches!(state, ToggleState::Selected);
                            agent_server.set_default_config_option(
                                config_id.0.as_ref(),
                                Some(AgentConfigOptionValue::Boolean(next_value)),
                                fs.clone(),
                                cx,
                            );

                            let task = config_options.set_config_option(
                                config_id.clone(),
                                acp::SessionConfigOptionValue::boolean(next_value),
                                cx,
                            );

                            cx.spawn(async move |_| {
                                if let Err(err) = task.await {
                                    log::error!("Failed to set config option: {:?}", err);
                                }
                            })
                            .detach();
                        }),
                    )
                    .into_any_element()
            }
            _ => div().into_any_element(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct ConfigOptionValue {
    value: acp::SessionConfigValueId,
    name: String,
    description: Option<String>,
    group: Option<String>,
}

impl PickerItem for ConfigOptionValue {
    type Id = acp::SessionConfigValueId;

    fn item_id(&self) -> &acp::SessionConfigValueId {
        &self.value
    }
}

type ConfigOptionPickerEntry = PickerRow<ConfigOptionValue>;

struct ConfigOptionPickerDelegate {
    config_options: Rc<dyn AgentSessionConfigOptions>,
    config_id: acp::SessionConfigId,
    agent_server: Rc<dyn AgentServer>,
    fs: Arc<dyn Fs>,
    filtered_entries: Vec<ConfigOptionPickerEntry>,
    all_options: Vec<ConfigOptionValue>,
    /// The last search's results. Folding a group rebuilds the rows from these
    /// on the spot, because `confirm` cannot call `Picker::refresh`.
    filtered_options: Vec<ConfigOptionValue>,
    filtered_agents: Vec<AgentLaunchEntry>,
    agent_launchers: Option<AgentLaunchSource>,
    collapse: OptionCollapseState,
    /// Keeps the selection where it was when the list is updated for a fold
    /// or a star.
    reselect_row: Option<RowAnchor<acp::SessionConfigValueId>>,
    selected_index: usize,
    selected_description: Option<(usize, SharedString)>,
    favorites: HashSet<acp::SessionConfigValueId>,
    _settings_subscription: Subscription,
    _dismiss_subscription: Subscription,
}

impl ConfigOptionPickerDelegate {
    fn new(
        config_options: Rc<dyn AgentSessionConfigOptions>,
        config_id: acp::SessionConfigId,
        agent_server: Rc<dyn AgentServer>,
        fs: Arc<dyn Fs>,
        grouping: PickerGrouping,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Self {
        let favorites = agent_server.favorite_config_option_value_ids(&config_id, cx);

        let all_options = extract_options(&config_options, &config_id);
        let PickerGrouping {
            collapse,
            agent_launchers,
            on_popover_closed,
        } = grouping;
        // A picker opens with an empty query, whatever the one this replaces
        // was showing.
        collapse.borrow_mut().set_query("");
        let filtered_agents = agent_launchers
            .as_ref()
            .map(|agent_launchers| agent_launchers(cx))
            .unwrap_or_default();

        let agent_server_for_subscription = agent_server.clone();
        let config_id_for_subscription = config_id.clone();
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |picker, window, cx| {
                let new_favorites = agent_server_for_subscription
                    .favorite_config_option_value_ids(&config_id_for_subscription, cx);
                if new_favorites != picker.delegate.favorites {
                    if picker.focus_handle(cx).contains_focused(window, cx) {
                        let delegate = &mut picker.delegate;
                        delegate
                            .collapse
                            .borrow_mut()
                            .hold_favorites_at_open(&delegate.favorites);
                        delegate.reselect_row =
                            row_anchor(&delegate.filtered_entries, delegate.selected_index);
                    }
                    picker.delegate.favorites = new_favorites;
                    picker.refresh(window, cx);
                }
            });

        // Every way the popover closes emits this on the picker.
        let picker = cx.entity();
        let dismiss_subscription = cx.subscribe_in(
            &picker,
            window,
            move |_, _, _: &DismissEvent, window, cx| on_popover_closed(window, cx),
        );

        cx.notify();

        let mut this = Self {
            config_options,
            config_id,
            agent_server,
            fs,
            filtered_entries: Vec::new(),
            filtered_options: all_options.clone(),
            all_options,
            filtered_agents,
            agent_launchers,
            collapse,
            reselect_row: None,
            selected_index: 0,
            selected_description: None,
            favorites,
            _settings_subscription: settings_subscription,
            _dismiss_subscription: dismiss_subscription,
        };
        this.rebuild_entries();
        this.selected_index =
            initial_row_index(&this.filtered_entries, this.current_value().as_ref());
        this
    }

    fn current_value(&self) -> Option<acp::SessionConfigValueId> {
        get_current_select_value(&self.config_options, &self.config_id)
    }

    fn rebuild_entries(&mut self) {
        let current_value = self.current_value();
        self.filtered_entries = options_to_picker_entries(
            &self.filtered_options,
            &self.favorites,
            current_value.as_ref(),
            self.filtered_agents.clone(),
            &self.collapse.borrow(),
        );
    }

    fn toggle_group(
        &mut self,
        key: PickerGroupKey,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.collapse
            .borrow_mut()
            .set_expanded(key.clone(), !expanded);
        self.rebuild_entries();
        if let Some(index) = group_row_index(&self.filtered_entries, &key) {
            self.selected_index = index;
        }
        self.selected_description = None;
        self.reselect_row = Some(RowAnchor::Group(key));
        refresh_rows_after_fold(window, cx);
        cx.notify();
    }
}

impl PickerDelegate for ConfigOptionPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "config options"
    }

    fn match_count(&self) -> usize {
        self.filtered_entries.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected_index = ix.min(self.filtered_entries.len().saturating_sub(1));
        cx.notify();
    }

    fn can_select(&self, ix: usize, _window: &mut Window, _cx: &mut Context<Picker<Self>>) -> bool {
        match self.filtered_entries.get(ix) {
            Some(PickerRow::Item { .. } | PickerRow::Group { .. } | PickerRow::Agent(_)) => true,
            Some(PickerRow::Label(_)) | None => false,
        }
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("Select an option…").into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        let all_options = self.all_options.clone();
        let agents = self
            .agent_launchers
            .as_ref()
            .map(|agent_launchers| agent_launchers(cx))
            .unwrap_or_default();
        let executor = cx.background_executor().clone();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_options = if query.is_empty() {
                all_options
            } else {
                fuzzy_search_options(all_options, &query, executor.clone()).await
            };
            let filtered_agents = filter_agent_launch_entries(agents, &query, executor).await;

            this.update_in(cx, |this, window, cx| {
                let delegate = &mut this.delegate;
                delegate.collapse.borrow_mut().set_query(&query);
                delegate.filtered_options = filtered_options;
                delegate.filtered_agents = filtered_agents;
                delegate.rebuild_entries();
                let current_value = delegate.current_value();
                let new_index = delegate
                    .reselect_row
                    .take()
                    .and_then(|anchor| anchor_row_index(&delegate.filtered_entries, &anchor))
                    .unwrap_or_else(|| {
                        initial_row_index(&delegate.filtered_entries, current_value.as_ref())
                    });

                this.set_selected_index(new_index, Some(picker::Direction::Down), true, window, cx);
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.filtered_entries.get(self.selected_index) {
            Some(PickerRow::Item { item: option, .. }) => {
                self.agent_server.set_default_config_option(
                    self.config_id.0.as_ref(),
                    Some(AgentConfigOptionValue::ValueId(option.value.0.to_string())),
                    self.fs.clone(),
                    cx,
                );
                let task = self.config_options.set_config_option(
                    self.config_id.clone(),
                    acp::SessionConfigOptionValue::value_id(option.value.clone()),
                    cx,
                );

                cx.spawn(async move |_, _| {
                    if let Err(err) = task.await {
                        log::error!("Failed to set config option: {:?}", err);
                    }
                })
                .detach();

                cx.emit(DismissEvent);
            }
            Some(PickerRow::Group { key, expanded, .. }) => {
                let key = key.clone();
                let expanded = *expanded;
                self.toggle_group(key, expanded, window, cx);
            }
            Some(PickerRow::Agent(entry)) => {
                window.dispatch_action(entry.action.to_action(), cx);
                cx.emit(DismissEvent);
            }
            Some(PickerRow::Label(_)) | None => {}
        }
    }

    fn dismissed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.defer_in(window, |picker, window, cx| {
            picker.set_query("", window, cx);
        });
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        match self.filtered_entries.get(ix)? {
            PickerRow::Label(title) => Some(
                div()
                    .when(ix > 0, |this| this.mt_1())
                    .child(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(cx.theme().colors().text_muted)
                            .child(title.clone()),
                    )
                    .into_any_element(),
            ),
            PickerRow::Group { .. } => render_group_header(&self.filtered_entries, ix, selected),
            PickerRow::Agent(entry) => Some(render_agent_launch_row(ix, entry, selected)),
            PickerRow::Item {
                item: option,
                is_favorite,
            } => {
                let current_value = self.current_value();
                let is_selected = current_value.as_ref() == Some(&option.value);

                let is_favorite = *is_favorite;

                let option_name = option.name.clone();
                let description = option.description.clone();

                Some(
                    div()
                        .id(("config-option-picker-item", ix))
                        .when_some(description, |this, desc| {
                            let desc: SharedString = desc.into();
                            this.on_hover(cx.listener(move |menu, hovered, _, cx| {
                                if *hovered {
                                    menu.delegate.selected_description = Some((ix, desc.clone()));
                                } else if matches!(menu.delegate.selected_description, Some((id, _)) if id == ix)
                                {
                                    menu.delegate.selected_description = None;
                                }
                                cx.notify();
                            }))
                        })
                        .child(
                            ListItem::new(ix)
                                .inset(true)
                                .spacing(ListItemSpacing::Sparse)
                                .toggle_state(selected)
                                .child(h_flex().w_full().child(Label::new(option_name).truncate()))
                                .end_slot(div().pr_2().when(is_selected, |this| {
                                    this.child(Icon::new(IconName::Check).color(Color::Accent))
                                }))
                                .end_slot_on_hover(div().pr_1p5().child({
                                    let (icon, color, tooltip) = if is_favorite {
                                        (IconName::StarFilled, Color::Accent, i18n::t!("Unfavorite"))
                                    } else {
                                        (IconName::Star, Color::Default, i18n::t!("Favorite"))
                                    };

                                    let config_id = self.config_id.clone();
                                    let value_id = option.value.clone();
                                    let agent_server = self.agent_server.clone();
                                    let fs = self.fs.clone();

                                    IconButton::new(("toggle-favorite-config-option", ix), icon)
                                        .layer(ElevationIndex::ElevatedSurface)
                                        .icon_color(color)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text(tooltip))
                                        .on_click(move |_, _, cx| {
                                            agent_server.toggle_favorite_config_option_value(
                                                config_id.clone(),
                                                value_id.clone(),
                                                !is_favorite,
                                                fs.clone(),
                                                cx,
                                            );
                                        })
                                })),
                        )
                        .into_any_element(),
                )
            }
        }
    }

    fn documentation_aside(
        &self,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<ui::DocumentationAside> {
        self.selected_description.as_ref().map(|(_, description)| {
            let description = description.clone();

            let side = documentation_aside_side(cx);

            ui::DocumentationAside::new(
                side,
                Rc::new(move |_| Label::new(description.clone()).into_any_element()),
            )
        })
    }

    fn documentation_aside_index(&self) -> Option<usize> {
        self.selected_description.as_ref().map(|(ix, _)| *ix)
    }
}

fn extract_options(
    config_options: &Rc<dyn AgentSessionConfigOptions>,
    config_id: &acp::SessionConfigId,
) -> Vec<ConfigOptionValue> {
    let Some(option) = config_options
        .config_options()
        .into_iter()
        .find(|opt| &opt.id == config_id)
    else {
        return Vec::new();
    };

    // メニューの項目名はトリガーボタンの表示と揃える必要がある
    // (思考の深さだけ日本語のレベル名になる)。
    let is_thought_level = is_thought_level_option(&option);
    let value_name = |value: &acp::SessionConfigValueId, name: &str| {
        if is_thought_level {
            thought_level_value_name(value, name)
        } else {
            name.to_string()
        }
    };

    match &option.kind {
        acp::SessionConfigKind::Select(select) => match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .map(|opt| ConfigOptionValue {
                    name: value_name(&opt.value, &opt.name),
                    value: opt.value.clone(),
                    description: opt.description.clone(),
                    group: None,
                })
                .collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| {
                    group.options.iter().map(|opt| ConfigOptionValue {
                        name: value_name(&opt.value, &opt.name),
                        value: opt.value.clone(),
                        description: opt.description.clone(),
                        group: Some(group.name.clone()),
                    })
                })
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

fn get_current_select_value(
    config_options: &Rc<dyn AgentSessionConfigOptions>,
    config_id: &acp::SessionConfigId,
) -> Option<acp::SessionConfigValueId> {
    config_options
        .config_options()
        .into_iter()
        .find(|opt| &opt.id == config_id)
        .and_then(|opt| match &opt.kind {
            acp::SessionConfigKind::Select(select) => Some(select.current_value.clone()),
            _ => None,
        })
}

fn setting_value_for_config_option_value(
    value: &acp::SessionConfigOptionValue,
) -> Option<AgentConfigOptionValue> {
    match value {
        acp::SessionConfigOptionValue::ValueId { value } => {
            Some(AgentConfigOptionValue::ValueId(value.0.to_string()))
        }
        acp::SessionConfigOptionValue::Boolean { value } => {
            Some(AgentConfigOptionValue::Boolean(*value))
        }
        _ => None,
    }
}

fn options_to_picker_entries(
    options: &[ConfigOptionValue],
    favorites: &HashSet<acp::SessionConfigValueId>,
    current_value: Option<&acp::SessionConfigValueId>,
    agents: Vec<AgentLaunchEntry>,
    collapse: &GroupCollapseState<acp::SessionConfigValueId>,
) -> Vec<ConfigOptionPickerEntry> {
    let mut groups: Vec<PickerGroup<ConfigOptionValue>> = Vec::new();
    for option in options {
        let name = option.group.clone().map(SharedString::from);
        match groups.last_mut() {
            Some(group) if group.name == name => group.items.push(option.clone()),
            _ => groups.push(PickerGroup {
                name,
                items: vec![option.clone()],
            }),
        }
    }
    build_picker_rows(
        groups,
        favorites,
        current_value,
        agents,
        collapse,
        i18n::t!("All Options"),
    )
}

async fn fuzzy_search_options(
    options: Vec<ConfigOptionValue>,
    query: &str,
    executor: BackgroundExecutor,
) -> Vec<ConfigOptionValue> {
    let candidates = options
        .iter()
        .enumerate()
        .map(|(ix, opt)| {
            StringMatchCandidate::new(ix, &grouped_search_text(opt.group.as_deref(), &opt.name))
        })
        .collect::<Vec<_>>();

    let mut matches = fuzzy::match_strings(
        &candidates,
        query,
        false,
        true,
        100,
        &Default::default(),
        executor,
    )
    .await;

    // Matches stay in the agent's group order (best first within a group), so
    // that each group gets one heading to fold.
    let group_rank = |candidate_id: usize| {
        options
            .get(candidate_id)
            .and_then(|option| options.iter().position(|other| other.group == option.group))
    };
    matches.sort_unstable_by_key(|mat| {
        let candidate = &candidates[mat.candidate_id];
        (
            group_rank(mat.candidate_id),
            Reverse(OrderedFloat(mat.score)),
            candidate.id,
        )
    });

    matches
        .into_iter()
        .map(|mat| options[mat.candidate_id].clone())
        .collect()
}

fn find_option_name(
    options: &acp::SessionConfigSelectOptions,
    value_id: &acp::SessionConfigValueId,
) -> Option<String> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(opts) => opts
            .iter()
            .find(|o| &o.value == value_id)
            .map(|o| o.name.clone()),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups.iter().find_map(|group| {
            group
                .options
                .iter()
                .find(|o| &o.value == value_id)
                .map(|o| o.name.clone())
        }),
        _ => None,
    }
}

fn count_config_options(option: &acp::SessionConfigOption) -> usize {
    match &option.kind {
        acp::SessionConfigKind::Select(select) => match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options.len(),
            acp::SessionConfigSelectOptions::Grouped(groups) => {
                groups.iter().map(|g| g.options.len()).sum()
            }
            _ => 0,
        },
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::ExternalAgentMenuItem;
    use crate::model_selector::agent_launch_entries;
    use acp_thread::AgentConnection;
    use fs::FakeFs;
    use gpui::{TestAppContext, VisualTestContext};
    use parking_lot::Mutex;
    use project::{AgentId, Project};
    use std::{any::Any, cell::Cell};

    #[gpui::test]
    fn cycling_config_option_saves_selected_value_as_default(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::select(
                "mode",
                "Mode",
                "auto",
                vec![
                    acp::SessionConfigSelectOption::new("auto", "Auto"),
                    acp::SessionConfigSelectOption::new("manual", "Manual"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Mode),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let config_options: Rc<dyn AgentSessionConfigOptions> = config_options.clone();
            let agent_server: Rc<dyn AgentServer> = agent_server.clone();
            let fs = fs.clone();
            let view = cx.new(|_| ConfigOptionsView {
                config_option_ids: ConfigOptionsView::config_option_ids(&config_options),
                config_options,
                selectors: Vec::new(),
                agent_server,
                fs,
                agent_launchers: None,
                collapse_states: HashMap::default(),
                _refresh_task: Task::ready(()),
            });

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::Mode, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "mode".to_string(),
                Some(AgentConfigOptionValue::ValueId("manual".to_string()))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "mode".to_string(),
                acp::SessionConfigOptionValue::value_id("manual")
            )]
        );
    }

    #[gpui::test]
    fn cycling_boolean_config_option_saves_selected_value_as_default(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::ModelConfig),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let config_options: Rc<dyn AgentSessionConfigOptions> = config_options.clone();
            let agent_server: Rc<dyn AgentServer> = agent_server.clone();
            let fs = fs.clone();
            let view = cx.new(|_| ConfigOptionsView {
                config_option_ids: ConfigOptionsView::config_option_ids(&config_options),
                config_options,
                selectors: Vec::new(),
                agent_server,
                fs,
                agent_launchers: None,
                collapse_states: HashMap::default(),
                _refresh_task: Task::ready(()),
            });

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::ModelConfig, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "web_search".to_string(),
                Some(AgentConfigOptionValue::Boolean(true))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "web_search".to_string(),
                acp::SessionConfigOptionValue::boolean(true)
            )]
        );
    }

    #[gpui::test]
    fn cycling_category_cycles_boolean_config_option_first(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::Model),
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "small",
                vec![
                    acp::SessionConfigSelectOption::new("small", "Small"),
                    acp::SessionConfigSelectOption::new("large", "Large"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());

        cx.update(|cx| {
            let config_options: Rc<dyn AgentSessionConfigOptions> = config_options.clone();
            let agent_server: Rc<dyn AgentServer> = agent_server.clone();
            let fs = fs.clone();
            let view = cx.new(|_| ConfigOptionsView {
                config_option_ids: ConfigOptionsView::config_option_ids(&config_options),
                config_options,
                selectors: Vec::new(),
                agent_server,
                fs,
                agent_launchers: None,
                collapse_states: HashMap::default(),
                _refresh_task: Task::ready(()),
            });

            assert!(view.update(cx, |view, cx| {
                view.cycle_category_option(acp::SessionConfigOptionCategory::Model, false, cx)
            }));
        });

        assert_eq!(
            agent_server.saved_defaults.lock().as_slice(),
            &[(
                "web_search".to_string(),
                Some(AgentConfigOptionValue::Boolean(true))
            )]
        );
        assert_eq!(
            config_options.set_values.borrow().as_slice(),
            &[(
                "web_search".to_string(),
                acp::SessionConfigOptionValue::boolean(true)
            )]
        );
    }

    #[gpui::test]
    fn toggling_category_picker_without_select_config_option_is_unhandled(cx: &mut TestAppContext) {
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::boolean("web_search", "Web Search", false)
                .category(acp::SessionConfigOptionCategory::Model),
        ]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let cx = cx.add_empty_window();
        let view = cx.update({
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = agent_server;
                cx.new(|cx| {
                    ConfigOptionsView::new(config_options, agent_server, fs, None, window, cx)
                })
            }
        });

        let handled = cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.toggle_category_picker(acp::SessionConfigOptionCategory::Model, window, cx)
            })
        });

        assert!(!handled);
    }

    /// ACP スレッド (Claude Code / Codex) の思考の深さは `LanguageModel` ではなく
    /// エージェントの config option として来る。ツールバーの表示だけ日本語に
    /// なることを固定する (値 / id は原文のまま)。
    #[test]
    fn thought_level_config_option_is_labelled_in_japanese() {
        let option = acp::SessionConfigOption::select(
            "reasoning_effort",
            "Reasoning Effort",
            "high",
            vec![
                acp::SessionConfigSelectOption::new("low", "Low"),
                acp::SessionConfigSelectOption::new("medium", "Medium"),
                acp::SessionConfigSelectOption::new("high", "High"),
                acp::SessionConfigSelectOption::new("turbo", "Turbo"),
            ],
        );
        let config_id = option.id.clone();

        assert!(is_thought_level_option(&option));
        assert_eq!(config_option_display_name(&option), "Thinking Effort");

        let config_options: Rc<dyn AgentSessionConfigOptions> =
            Rc::new(TestSessionConfigOptions::new(vec![option]));
        let names = extract_options(&config_options, &config_id)
            .into_iter()
            .map(|option| option.name)
            .collect::<Vec<_>>();

        // 標準の値は日本語、エージェント独自の値は原文のまま。
        assert_eq!(names, vec!["Low", "Medium", "High", "Turbo"]);

        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let names = extract_options(&config_options, &config_id)
            .into_iter()
            .map(|option| option.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["低", "中", "高", "Turbo"]);
    }

    #[test]
    fn other_config_options_keep_the_agents_own_names() {
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "small",
            vec![
                acp::SessionConfigSelectOption::new("small", "Small"),
                acp::SessionConfigSelectOption::new("large", "Large"),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        let config_id = option.id.clone();

        assert!(!is_thought_level_option(&option));
        assert_eq!(config_option_display_name(&option), "Model");

        let config_options: Rc<dyn AgentSessionConfigOptions> =
            Rc::new(TestSessionConfigOptions::new(vec![option]));
        let names = extract_options(&config_options, &config_id)
            .into_iter()
            .map(|option| option.name)
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["Small", "Large"]);
    }

    fn init_picker_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn option_value(value: &str, group: Option<&str>) -> ConfigOptionValue {
        ConfigOptionValue {
            value: acp::SessionConfigValueId::from(value.to_string()),
            name: value.to_string(),
            description: None,
            group: group.map(str::to_string),
        }
    }

    fn searching(query: &str) -> GroupCollapseState<acp::SessionConfigValueId> {
        let mut collapse = GroupCollapseState::default();
        collapse.set_query(query);
        collapse
    }

    /// `# ` marks a label, `▶`/`▼` a folded/unfolded heading with its count,
    /// `@ ` an agent row; anything else is an option's value.
    fn config_entry_labels(entries: &[ConfigOptionPickerEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|entry| match entry {
                PickerRow::Label(label) => format!("# {label}"),
                PickerRow::Group {
                    name,
                    count,
                    expanded,
                    ..
                } => format!("{} {name} ({count})", if *expanded { "▼" } else { "▶" }),
                PickerRow::Item { item, .. } => item.value.0.to_string(),
                PickerRow::Agent(entry) => format!("@ {}", entry.label),
            })
            .collect()
    }

    fn grouped_model_option(current_value: &'static str) -> acp::SessionConfigOption {
        acp::SessionConfigOption::select(
            "model",
            "Model",
            current_value,
            vec![
                acp::SessionConfigSelectGroup::new(
                    "anthropic",
                    "Anthropic",
                    vec![
                        acp::SessionConfigSelectOption::new("sonnet", "Sonnet"),
                        acp::SessionConfigSelectOption::new("opus", "Opus"),
                    ],
                ),
                acp::SessionConfigSelectGroup::new(
                    "openai",
                    "OpenAI",
                    vec![
                        acp::SessionConfigSelectOption::new("gpt-5", "GPT-5"),
                        acp::SessionConfigSelectOption::new("gpt-5-mini", "GPT-5 mini"),
                    ],
                ),
            ],
        )
        .category(acp::SessionConfigOptionCategory::Model)
    }

    fn option_picker(
        view: &Entity<ConfigOptionsView>,
        config_id: &str,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<ConfigOptionPickerDelegate>> {
        view.read_with(cx, |view, cx| {
            view.selectors
                .iter()
                .find(|selector| selector.read(cx).config_id.0.as_ref() == config_id)
                .and_then(|selector| selector.read(cx).picker.clone())
                .expect("a select option has a picker")
        })
    }

    fn picker_labels(
        picker: &Entity<Picker<ConfigOptionPickerDelegate>>,
        cx: &mut VisualTestContext,
    ) -> Vec<String> {
        picker.read_with(cx, |picker, _| {
            config_entry_labels(&picker.delegate.filtered_entries)
        })
    }

    #[gpui::test]
    async fn acp_model_groups_fold_behind_favorites_and_stay_folded_across_rebuilds(
        cx: &mut TestAppContext,
    ) {
        init_picker_test(cx);
        let agent_server = Rc::new(TestAgentServer {
            favorites: RefCell::new(HashSet::from_iter([acp::SessionConfigValueId::from(
                "sonnet",
            )])),
            ..TestAgentServer::default()
        });
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![grouped_model_option(
            "sonnet",
        )]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let (view, cx) = cx.add_window_view({
            let config_options = config_options.clone();
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = agent_server;
                ConfigOptionsView::new(config_options, agent_server, fs, None, window, cx)
            }
        });
        cx.run_until_parked();

        let picker = option_picker(&view, "model", cx);
        assert_eq!(
            picker_labels(&picker, cx),
            vec!["# Favorites", "sonnet", "▶ Anthropic (2)", "▶ OpenAI (2)"]
        );
        let dismissed = Rc::new(Cell::new(false));
        cx.update({
            let dismissed = dismissed.clone();
            |_, cx| {
                cx.subscribe(&picker, move |_, _: &DismissEvent, _| dismissed.set(true))
                    .detach();
            }
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.set_selected_index(3, window, cx);
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        let unfolded = vec![
            "# Favorites",
            "sonnet",
            "▶ Anthropic (2)",
            "▼ OpenAI (2)",
            "gpt-5",
            "gpt-5-mini",
        ];
        assert_eq!(picker_labels(&picker, cx), unfolded);
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 3, "still on the heading");
            assert!(
                picker.delegate.reselect_row.is_none(),
                "the list was updated for the new number of rows"
            );
        });
        assert!(
            config_options.set_values.borrow().is_empty(),
            "a heading is not an option to choose"
        );
        assert!(!dismissed.get(), "and the popover stays open");

        // Every config update rebuilds the selectors, pickers included.
        view.update_in(cx, |view, window, cx| view.rebuild_selectors(window, cx));
        cx.run_until_parked();
        let rebuilt = option_picker(&view, "model", cx);
        assert_ne!(rebuilt.entity_id(), picker.entity_id());
        assert_eq!(picker_labels(&rebuilt, cx), unfolded);
    }

    #[test]
    fn short_ungrouped_acp_lists_are_never_folded() {
        let favorites = HashSet::from_iter([acp::SessionConfigValueId::from("m0")]);
        let short = (0..4)
            .map(|index| option_value(&format!("m{index}"), None))
            .collect::<Vec<_>>();
        assert_eq!(
            config_entry_labels(&options_to_picker_entries(
                &short,
                &favorites,
                None,
                Vec::new(),
                &GroupCollapseState::default(),
            )),
            vec!["# Favorites", "m0", "# All Options", "m0", "m1", "m2", "m3"]
        );

        let long = (0..12)
            .map(|index| option_value(&format!("m{index}"), None))
            .collect::<Vec<_>>();
        assert_eq!(
            config_entry_labels(&options_to_picker_entries(
                &long,
                &favorites,
                None,
                Vec::new(),
                &GroupCollapseState::default(),
            )),
            vec!["# Favorites", "m0", "▶ All Options (12)"]
        );
    }

    #[gpui::test]
    async fn acp_search_names_groups_and_gives_each_group_one_heading(cx: &mut TestAppContext) {
        let options = vec![
            option_value("x", Some("Alpha")),
            option_value("x-with-a-much-longer-name", Some("Alpha")),
            option_value("x-mid", Some("Beta")),
            option_value("gpt-5", Some("OpenAI")),
            option_value("gpt-6", Some("OpenAI")),
        ];

        let found = fuzzy_search_options(options.clone(), "openai", cx.executor()).await;
        assert_eq!(
            found
                .iter()
                .map(|option| option.name.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5", "gpt-6"]
        );

        let found = fuzzy_search_options(options, "x", cx.executor()).await;
        let rows = options_to_picker_entries(
            &found,
            &HashSet::default(),
            None,
            Vec::new(),
            &searching("x"),
        );
        assert_eq!(
            config_entry_labels(&rows),
            vec![
                "▼ Alpha (2)",
                "x",
                "x-with-a-much-longer-name",
                "▼ Beta (1)",
                "x-mid"
            ]
        );
    }

    #[gpui::test]
    async fn an_acp_star_leaves_the_open_list_unfolded_across_rebuilds_until_it_closes(
        cx: &mut TestAppContext,
    ) {
        init_picker_test(cx);
        let agent_server = Rc::new(TestAgentServer::default());
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![grouped_model_option(
            "gpt-5",
        )]));
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let (view, cx) = cx.add_window_view({
            let agent_server = agent_server.clone();
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = agent_server;
                ConfigOptionsView::new(config_options, agent_server, fs, None, window, cx)
            }
        });
        cx.run_until_parked();
        view.update_in(cx, |view, window, cx| {
            assert!(view.toggle_category_picker(
                acp::SessionConfigOptionCategory::Model,
                window,
                cx
            ));
        });
        let picker = option_picker(&view, "model", cx);
        // What the popover does on its next frames once it is shown.
        picker.update_in(cx, |picker, window, cx| {
            window.focus(&picker.focus_handle(cx), cx)
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| picker.focus_handle(cx).contains_focused(window, cx)),
            "the popover is open"
        );
        let unfolded = vec![
            "▼ Anthropic (2)",
            "sonnet",
            "opus",
            "▼ OpenAI (2)",
            "gpt-5",
            "gpt-5-mini",
        ];
        assert_eq!(picker_labels(&picker, cx), unfolded);

        agent_server
            .favorites
            .borrow_mut()
            .insert(acp::SessionConfigValueId::from("opus"));
        cx.update(|_, cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
        cx.run_until_parked();
        let starred = vec![
            "# Favorites",
            "opus",
            "▼ Anthropic (2)",
            "sonnet",
            "opus",
            "▼ OpenAI (2)",
            "gpt-5",
            "gpt-5-mini",
        ];
        assert_eq!(picker_labels(&picker, cx), starred, "nothing folds");

        view.update_in(cx, |view, window, cx| view.rebuild_selectors(window, cx));
        cx.run_until_parked();
        let rebuilt = option_picker(&view, "model", cx);
        assert_ne!(rebuilt.entity_id(), picker.entity_id());
        assert_eq!(
            picker_labels(&rebuilt, cx),
            starred,
            "a config update while open does not fold it either"
        );

        picker.update(cx, |_, cx| cx.emit(DismissEvent));
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&rebuilt, cx),
            vec![
                "# Favorites",
                "opus",
                "▶ Anthropic (2)",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ],
            "the picker the next opening shows folds behind the favorites"
        );
    }

    #[gpui::test]
    async fn the_acp_model_picker_takes_a_query_and_ends_with_the_agent_section(
        cx: &mut TestAppContext,
    ) {
        init_picker_test(cx);
        let launched = Rc::new(RefCell::new(Vec::<AgentId>::new()));
        cx.update(|cx| {
            let launched = launched.clone();
            cx.on_action(move |action: &crate::NewExternalAgentThread, _| {
                launched.borrow_mut().push(action.agent.clone());
            });
        });
        let config_options = Rc::new(TestSessionConfigOptions::new(vec![
            acp::SessionConfigOption::select(
                "mode",
                "Mode",
                "ask",
                vec![
                    acp::SessionConfigSelectOption::new("ask", "Ask"),
                    acp::SessionConfigSelectOption::new("code", "Code"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Mode),
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "default",
                vec![
                    acp::SessionConfigSelectOption::new("default", "Default"),
                    acp::SessionConfigSelectOption::new("opus", "Opus"),
                    acp::SessionConfigSelectOption::new("haiku", "Haiku"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ]));
        let agent_launchers: AgentLaunchSource = Rc::new(|_| {
            agent_launch_entries(
                &AgentId::new("test-agent"),
                true,
                vec![ExternalAgentMenuItem {
                    id: AgentId::new("codex-acp"),
                    display_name: "Codex".into(),
                    icon_path: None,
                }],
            )
        });
        let fs: Arc<dyn Fs> = FakeFs::new(cx.executor());
        let (view, cx) = cx.add_window_view({
            let config_options = config_options.clone();
            move |window, cx| {
                let config_options: Rc<dyn AgentSessionConfigOptions> = config_options;
                let agent_server: Rc<dyn AgentServer> = Rc::new(TestAgentServer::default());
                ConfigOptionsView::new(
                    config_options,
                    agent_server,
                    fs,
                    Some(agent_launchers),
                    window,
                    cx,
                )
            }
        });
        cx.run_until_parked();

        let model = option_picker(&view, "model", cx);
        let mode = option_picker(&view, "mode", cx);
        assert_eq!(
            picker_labels(&model, cx),
            vec!["default", "opus", "haiku", "▶ Agents (3)"]
        );
        assert_eq!(
            picker_labels(&mode, cx),
            vec!["ask", "code"],
            "only the model picker carries the agents"
        );

        model.update_in(cx, |picker, window, cx| {
            picker.set_query("codex", window, cx)
        });
        mode.update_in(cx, |picker, window, cx| {
            picker.set_query("codex", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            model.read_with(cx, |picker, cx| picker.query(cx)),
            "codex",
            "three models still get a query field"
        );
        assert_eq!(
            mode.read_with(cx, |picker, cx| picker.query(cx)),
            "",
            "other short lists keep their plain menu"
        );
        assert_eq!(
            picker_labels(&model, cx),
            vec!["▼ Agents (1)", "@ New Thread with Codex"]
        );

        let dismissed = Rc::new(Cell::new(false));
        cx.update({
            let dismissed = dismissed.clone();
            |window, cx| {
                cx.subscribe(&model, move |_, _: &DismissEvent, _| dismissed.set(true))
                    .detach();
                window.refresh();
            }
        });
        cx.run_until_parked();
        model.update_in(cx, |picker, window, cx| {
            assert_eq!(picker.delegate.selected_index, 1);
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(launched.borrow().as_slice(), &[AgentId::new("codex-acp")]);
        assert!(dismissed.get());
        assert!(
            config_options.set_values.borrow().is_empty(),
            "starting an agent leaves this thread's model alone"
        );
    }

    #[derive(Default)]
    struct TestAgentServer {
        saved_defaults: Arc<Mutex<Vec<(String, Option<AgentConfigOptionValue>)>>>,
        favorites: RefCell<HashSet<acp::SessionConfigValueId>>,
    }

    impl AgentServer for TestAgentServer {
        fn logo(&self) -> IconName {
            IconName::ZedAssistant
        }

        fn agent_id(&self) -> AgentId {
            AgentId::new("test-agent")
        }

        fn connect(
            &self,
            _delegate: agent_servers::AgentServerDelegate,
            _project: Entity<Project>,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Rc<dyn AgentConnection>>> {
            Task::ready(Err(anyhow::anyhow!("test agent server cannot connect")))
        }

        fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
            self
        }

        fn set_default_config_option(
            &self,
            config_id: &str,
            value: Option<AgentConfigOptionValue>,
            _fs: Arc<dyn Fs>,
            _cx: &mut App,
        ) {
            self.saved_defaults
                .lock()
                .push((config_id.to_string(), value));
        }

        fn favorite_config_option_value_ids(
            &self,
            _config_id: &acp::SessionConfigId,
            _cx: &mut App,
        ) -> HashSet<acp::SessionConfigValueId> {
            self.favorites.borrow().clone()
        }
    }

    struct TestSessionConfigOptions {
        options: RefCell<Vec<acp::SessionConfigOption>>,
        set_values: RefCell<Vec<(String, acp::SessionConfigOptionValue)>>,
    }

    impl TestSessionConfigOptions {
        fn new(options: Vec<acp::SessionConfigOption>) -> Self {
            Self {
                options: RefCell::new(options),
                set_values: RefCell::new(Vec::new()),
            }
        }
    }

    impl AgentSessionConfigOptions for TestSessionConfigOptions {
        fn config_options(&self) -> Vec<acp::SessionConfigOption> {
            self.options.borrow().clone()
        }

        fn set_config_option(
            &self,
            config_id: acp::SessionConfigId,
            value: acp::SessionConfigOptionValue,
            _cx: &mut App,
        ) -> Task<anyhow::Result<Vec<acp::SessionConfigOption>>> {
            self.set_values
                .borrow_mut()
                .push((config_id.0.to_string(), value.clone()));

            let options = {
                let mut options = self.options.borrow_mut();
                if let Some(option) = options.iter_mut().find(|option| option.id == config_id) {
                    match (&mut option.kind, value) {
                        (
                            acp::SessionConfigKind::Select(select),
                            acp::SessionConfigOptionValue::ValueId { value },
                        ) => {
                            select.current_value = value;
                        }
                        (
                            acp::SessionConfigKind::Boolean(boolean),
                            acp::SessionConfigOptionValue::Boolean { value },
                        ) => {
                            boolean.current_value = value;
                        }
                        _ => {}
                    }
                }
                options.clone()
            };

            Task::ready(Ok(options))
        }
    }
}
