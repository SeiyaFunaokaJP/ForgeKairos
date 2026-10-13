use std::{cmp::Reverse, rc::Rc, sync::Arc};

use acp_thread::{
    AgentModelIcon, AgentModelId, AgentModelInfo, AgentModelList, AgentModelSelector,
};

use anyhow::Result;
use collections::{HashMap, HashSet, IndexMap};
use futures::FutureExt;
use fuzzy::{StringMatchCandidate, match_strings};
use gpui::{
    Action, AsyncWindowContext, BackgroundExecutor, DismissEvent, FocusHandle, Focusable,
    Subscription, Task, TaskExt, WeakEntity,
};
use itertools::Itertools;
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate, ScrollBehavior};
use project::{AgentId, Project};
use settings::SettingsStore;
use ui::{DocumentationAside, IntoElement, prelude::*};
use util::ResultExt;
use zed_actions::agent::OpenSettings;

use crate::Agent;
use crate::ai_choices::{
    AiChoice, AiChoiceSection, AiChoiceTarget, AiChoiceTransition, ThreadRunner,
    choice_search_text, choice_transition, gather_ai_choice_inputs, render_ai_choice_picker_row,
    vendor_label,
};
use crate::ui::{
    ModelSelectorFooter, ModelSelectorGroupHeader, ModelSelectorHeader, ModelSelectorListItem,
    documentation_aside_side,
};
use agent_settings::ai_vendor::AiVendor;
use workspace::Workspace;

pub type ModelSelector = Picker<ModelPickerDelegate>;

pub fn acp_model_selector(
    selector: Rc<dyn AgentModelSelector>,
    focus_handle: FocusHandle,
    agent_launchers: Option<AgentLaunchSource>,
    window: &mut Window,
    cx: &mut Context<ModelSelector>,
) -> ModelSelector {
    let delegate = ModelPickerDelegate::new(selector, focus_handle, agent_launchers, window, cx);
    Picker::list(delegate, window, cx)
        .show_scrollbar(true)
        .initial_width(rems(20.))
}

const FAVORITES_LABEL: &str = i18n::msg!("Favorites");
/// A list without a group name of its own (an ACP agent's ungrouped options, a
/// flat model list) is only folded from this many entries on. Claude's ACP
/// model list has a handful: folding it would cost a click without making the
/// popover any shorter.
const MIN_FOLDABLE_UNGROUPED: usize = 10;

/// Identifies a group's fold across rebuilds of the list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PickerGroupKey {
    Named(SharedString),
    /// The list that has no group name of its own, headed 「すべて」.
    Ungrouped,
}

/// The groups the user folded or unfolded, kept for as long as the selector
/// lives and never written to settings.
#[derive(Debug)]
pub(crate) struct GroupCollapseState<Id> {
    /// Decided while the list showed everything. Wins over the defaults.
    browsing: HashMap<PickerGroupKey, bool>,
    /// Decided during the current search, and thrown away when the query
    /// changes: a match must never hide behind a heading folded for another
    /// query.
    searching: HashMap<PickerGroupKey, bool>,
    query: String,
    /// The favorites the popover opened with, once a star was toggled in it.
    /// The default folds follow these until the popover closes: starring
    /// the first model must not fold away the list the user is looking at.
    favorites_at_open: Option<HashSet<Id>>,
}

impl<Id> Default for GroupCollapseState<Id> {
    fn default() -> Self {
        Self {
            browsing: HashMap::default(),
            searching: HashMap::default(),
            query: String::new(),
            favorites_at_open: None,
        }
    }
}

impl<Id: Clone + Eq + std::hash::Hash> GroupCollapseState<Id> {
    /// Called with the favorites as they were before a star was toggled in
    /// the open popover.
    pub(crate) fn hold_favorites_at_open(&mut self, favorites: &HashSet<Id>) {
        if self.favorites_at_open.is_none() {
            self.favorites_at_open = Some(favorites.clone());
        }
    }

    /// Called when the popover closes. Returns whether the rows were built on
    /// held favorites and need building again for the next opening.
    pub(crate) fn release_favorites_at_open(&mut self) -> bool {
        self.favorites_at_open.take().is_some()
    }
}

impl<Id> GroupCollapseState<Id> {
    pub(crate) fn set_query(&mut self, query: &str) {
        if self.query != query {
            self.query = query.to_string();
            self.searching.clear();
        }
    }

    pub(crate) fn is_searching(&self) -> bool {
        !self.query.is_empty()
    }

    pub(crate) fn set_expanded(&mut self, key: PickerGroupKey, expanded: bool) {
        if self.is_searching() {
            self.searching.insert(key, expanded);
        } else {
            self.browsing.insert(key, expanded);
        }
    }

    fn is_expanded(&self, key: &PickerGroupKey, default: bool) -> bool {
        let decided = if self.is_searching() {
            &self.searching
        } else {
            &self.browsing
        };
        decided.get(key).copied().unwrap_or(default)
    }
}

/// An entry of a model or option picker, as far as grouping is concerned.
pub(crate) trait PickerItem: Clone {
    type Id: Clone + Eq + std::hash::Hash;

    fn item_id(&self) -> &Self::Id;

    /// The language model provider of a built-in agent's model, which the
    /// agent writes before a `/` in the id.
    fn native_provider_id(&self) -> Option<&str> {
        None
    }
}

impl PickerItem for AgentModelInfo {
    type Id = AgentModelId;

    fn item_id(&self) -> &AgentModelId {
        &self.id
    }

    fn native_provider_id(&self) -> Option<&str> {
        self.id
            .as_ref()
            .split_once('/')
            .map(|(provider_id, _)| provider_id)
    }
}

pub(crate) struct PickerGroup<T> {
    pub(crate) name: Option<SharedString>,
    pub(crate) items: Vec<T>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PickerRow<T> {
    /// A heading that cannot be selected (お気に入り / すべて).
    Label(SharedString),
    /// A heading that folds or unfolds its group on Enter or a click.
    Group {
        key: PickerGroupKey,
        name: SharedString,
        count: usize,
        expanded: bool,
    },
    Item {
        item: T,
        is_favorite: bool,
    },
    Agent(AgentLaunchEntry),
}

/// Lays out a model or option picker: お気に入り first, then each group behind
/// its own heading, then, where the picker offers other AIs (ADR-F94), one
/// section per vendor in the `+` menu's order.
///
/// In a vendor's section each AI the `+` menu lists is a row that starts it,
/// except the thread's own runner (the built-in agent's providers, or the
/// external agent the thread runs): its models' groups stand in that row's
/// place, so that switching among them stays in this thread. Groups that
/// belong to no single entry (the built-in agent's 「おすすめ」 across
/// providers) stay above the sections.
///
/// With favorites the other groups start folded, since the favorites are what
/// the user picks from; the exception is the first group holding the entry in
/// use when that entry is not a favorite, so that its check mark stays in
/// sight. Without favorites every group starts open, as it always did. While
/// searching every group starts open so that no match is hidden. Whatever the
/// user folded or unfolded wins over all of these.
///
/// The defaults go by the favorites the popover opened with while it is open
/// ([`GroupCollapseState::hold_favorites_at_open`]); the お気に入り section and
/// the stars always show the favorites as they are.
pub(crate) fn build_picker_rows<T: PickerItem>(
    groups: Vec<PickerGroup<T>>,
    favorites: &HashSet<T::Id>,
    active: Option<&T::Id>,
    ai_choices: Option<PickerAiChoices>,
    collapse: &GroupCollapseState<T::Id>,
    ungrouped_name: &'static str,
) -> Vec<PickerRow<T>> {
    let is_searching = collapse.is_searching();

    let favorite_items = {
        let mut seen = HashSet::default();
        groups
            .iter()
            .flat_map(|group| &group.items)
            .filter(|item| favorites.contains(item.item_id()) && seen.insert(item.item_id()))
            .cloned()
            .collect::<Vec<_>>()
    };
    let has_favorites = !favorite_items.is_empty();
    let fold_favorites = collapse.favorites_at_open.as_ref().unwrap_or(favorites);
    let folds_behind_favorites = groups
        .iter()
        .flat_map(|group| &group.items)
        .any(|item| fold_favorites.contains(item.item_id()));
    let active_is_a_folding_favorite = active.is_some_and(|active| fold_favorites.contains(active));
    let group_opened_for_active = if folds_behind_favorites && !active_is_a_folding_favorite {
        active.and_then(|active| {
            groups
                .iter()
                .position(|group| group.items.iter().any(|item| item.item_id() == active))
        })
    } else {
        None
    };

    let owners = groups
        .iter()
        .map(|group| {
            let choices = ai_choices.as_ref()?;
            let owner = group_owner(group, &choices.runner)?;
            choices
                .sections
                .iter()
                .flat_map(|section| &section.choices)
                .any(|choice| choice.target == owner)
                .then_some(owner)
        })
        .collect::<Vec<_>>();

    let push_group = |rows: &mut Vec<PickerRow<T>>,
                      group_index: usize,
                      group: PickerGroup<T>,
                      in_vendor_section: bool| {
        if group.items.is_empty() {
            return;
        }
        let count = group.items.len();
        let (key, name, foldable) = match group.name {
            Some(name) => (PickerGroupKey::Named(name.clone()), name, true),
            None => (
                PickerGroupKey::Ungrouped,
                SharedString::from(ungrouped_name),
                folds_behind_favorites && count >= MIN_FOLDABLE_UNGROUPED,
            ),
        };
        if foldable {
            let expanded_by_default = is_searching
                || !folds_behind_favorites
                || group_opened_for_active == Some(group_index);
            let expanded = collapse.is_expanded(&key, expanded_by_default);
            rows.push(PickerRow::Group {
                key,
                name,
                count,
                expanded,
            });
            if !expanded {
                return;
            }
        } else if has_favorites && !in_vendor_section {
            rows.push(PickerRow::Label(name));
        }
        rows.extend(group.items.into_iter().map(|item| {
            let is_favorite = favorites.contains(item.item_id());
            PickerRow::Item { item, is_favorite }
        }));
    };

    let mut rows = Vec::new();
    if has_favorites {
        rows.push(PickerRow::Label(i18n::tr(FAVORITES_LABEL).into()));
        rows.extend(favorite_items.into_iter().map(|item| PickerRow::Item {
            item,
            is_favorite: true,
        }));
    }

    let mut groups = groups.into_iter().map(Some).collect::<Vec<_>>();
    for (group_index, slot) in groups.iter_mut().enumerate() {
        if owners.get(group_index).is_some_and(Option::is_none)
            && let Some(group) = slot.take()
        {
            push_group(&mut rows, group_index, group, false);
        }
    }

    if let Some(PickerAiChoices { runner, sections }) = ai_choices {
        for section in sections {
            let mut section_rows = Vec::new();
            for choice in section.choices {
                let transition = choice_transition(&choice, Some(&runner));
                if transition == AiChoiceTransition::SameThread {
                    for (group_index, slot) in groups.iter_mut().enumerate() {
                        if owners.get(group_index).and_then(Option::as_ref) == Some(&choice.target)
                            && let Some(group) = slot.take()
                        {
                            push_group(&mut section_rows, group_index, group, true);
                        }
                    }
                } else {
                    section_rows.push(PickerRow::Agent(AgentLaunchEntry {
                        vendor: section.vendor,
                        choice,
                        transition,
                    }));
                }
            }
            if !section_rows.is_empty() {
                rows.push(PickerRow::Label(vendor_label(section.vendor)));
                rows.extend(section_rows);
            }
        }
    }

    rows
}

/// The `+` menu's entry whose models `group` lists, if it is one entry's.
fn group_owner<T: PickerItem>(
    group: &PickerGroup<T>,
    runner: &ThreadRunner,
) -> Option<AiChoiceTarget> {
    match runner {
        ThreadRunner::ExternalAgent(agent_id) => Some(AiChoiceTarget::ExternalAgentThread {
            agent_id: agent_id.clone(),
        }),
        ThreadRunner::BuiltInAgent => {
            let mut providers = group.items.iter().map(PickerItem::native_provider_id);
            let first = providers.next()??;
            providers.all(|provider| provider == Some(first)).then(|| {
                AiChoiceTarget::NativeThread {
                    provider_id: SharedString::from(first.to_string()),
                }
            })
        }
    }
}

/// Where the selection starts: on the entry in use, else on the first entry.
/// Not on a heading while an entry is in sight, or Enter right after typing a
/// query would fold a group instead of picking the best match.
pub(crate) fn initial_row_index<T: PickerItem>(
    rows: &[PickerRow<T>],
    active: Option<&T::Id>,
) -> usize {
    active
        .and_then(|active| {
            rows.iter().position(
                |row| matches!(row, PickerRow::Item { item, .. } if item.item_id() == active),
            )
        })
        // A model before another AI even where that AI's vendor comes first:
        // Enter right after a query must not leave this thread for a new one.
        .or_else(|| {
            rows.iter()
                .position(|row| matches!(row, PickerRow::Item { .. }))
        })
        .or_else(|| {
            rows.iter()
                .position(|row| matches!(row, PickerRow::Agent(_)))
        })
        .unwrap_or(0)
}

pub(crate) fn group_row_index<T>(rows: &[PickerRow<T>], key: &PickerGroupKey) -> Option<usize> {
    rows.iter()
        .position(|row| matches!(row, PickerRow::Group { key: row_key, .. } if row_key == key))
}

/// A row the selection stays on while the list is rebuilt under it: the
/// heading just folded, or the row selected when a star was toggled.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum RowAnchor<Id> {
    Group(PickerGroupKey),
    /// An entry is listed twice once it is a favorite, so this also says
    /// which of the two was selected.
    Item {
        id: Id,
        among_favorites: bool,
    },
    Agent(AiChoiceTarget),
}

fn favorites_section_len<T>(rows: &[PickerRow<T>]) -> usize {
    match rows.first() {
        Some(PickerRow::Label(label)) if label.as_ref() == i18n::tr(FAVORITES_LABEL) => {
            1 + rows
                .iter()
                .skip(1)
                .take_while(|row| matches!(row, PickerRow::Item { .. }))
                .count()
        }
        _ => 0,
    }
}

pub(crate) fn row_anchor<T: PickerItem>(
    rows: &[PickerRow<T>],
    ix: usize,
) -> Option<RowAnchor<T::Id>> {
    match rows.get(ix)? {
        PickerRow::Label(_) => None,
        PickerRow::Group { key, .. } => Some(RowAnchor::Group(key.clone())),
        PickerRow::Item { item, .. } => Some(RowAnchor::Item {
            id: item.item_id().clone(),
            among_favorites: ix < favorites_section_len(rows),
        }),
        PickerRow::Agent(entry) => Some(RowAnchor::Agent(entry.choice.target.clone())),
    }
}

pub(crate) fn anchor_row_index<T: PickerItem>(
    rows: &[PickerRow<T>],
    anchor: &RowAnchor<T::Id>,
) -> Option<usize> {
    match anchor {
        RowAnchor::Group(key) => group_row_index(rows, key),
        RowAnchor::Agent(target) => rows.iter().position(
            |row| matches!(row, PickerRow::Agent(entry) if &entry.choice.target == target),
        ),
        RowAnchor::Item {
            id,
            among_favorites,
        } => {
            let favorites_end = favorites_section_len(rows);
            let position_within = |range: std::ops::Range<usize>| {
                range.into_iter().find(|&ix| {
                    matches!(rows.get(ix), Some(PickerRow::Item { item, .. }) if item.item_id() == id)
                })
            };
            let (preferred, other) = if *among_favorites {
                (0..favorites_end, favorites_end..rows.len())
            } else {
                (favorites_end..rows.len(), 0..favorites_end)
            };
            position_within(preferred).or_else(|| position_within(other))
        }
    }
}

/// Folding a group changes how many rows there are, which the picker's list
/// only learns through an update of the matches. `confirm` runs while the
/// picker is being updated, so it cannot start one itself.
pub(crate) fn refresh_rows_after_fold<D: PickerDelegate>(
    window: &mut Window,
    cx: &mut Context<Picker<D>>,
) {
    cx.defer_in(window, |picker, window, cx| {
        let query = picker.query(cx);
        picker.update_matches_with_options(query, ScrollBehavior::PreserveOffset, window, cx);
    });
}

/// Lets a query name the group as well as the entry (「ollama」 lists the
/// Ollama models), the way the inline assistant's selector matches
/// `provider/model`.
pub(crate) fn grouped_search_text(group_name: Option<&str>, name: &str) -> String {
    match group_name {
        Some(group_name) => format!("{group_name}/{name}"),
        None => name.to_string(),
    }
}

pub(crate) fn render_group_header<T>(
    rows: &[PickerRow<T>],
    ix: usize,
    selected: bool,
) -> Option<AnyElement> {
    let PickerRow::Group {
        name,
        count,
        expanded,
        ..
    } = rows.get(ix)?
    else {
        return None;
    };
    let follows_an_open_group = ix
        .checked_sub(1)
        .and_then(|previous| rows.get(previous))
        .is_some_and(|row| matches!(row, PickerRow::Item { .. } | PickerRow::Agent(_)));
    Some(
        ModelSelectorGroupHeader::new(ix, name.clone(), *count, *expanded)
            .is_focused(selected)
            .has_border(follows_an_open_group)
            .into_any_element(),
    )
}

/// A row of a model picker that starts another AI than the thread's (or
/// opens its settings), as the `+` menu lists it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentLaunchEntry {
    pub(crate) vendor: AiVendor,
    pub(crate) choice: AiChoice,
    pub(crate) transition: AiChoiceTransition,
}

/// The `+` menu's sections, for a picker of a thread `runner` runs.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PickerAiChoices {
    pub(crate) runner: ThreadRunner,
    pub(crate) sections: Vec<AiChoiceSection>,
}

/// What a thread's model picker offers besides the thread's own models.
#[derive(Clone)]
pub(crate) struct AgentLaunchSource {
    /// Asked each time the list is built, so that an agent added while the
    /// thread is open shows up. `None` where nothing else can be started.
    pub(crate) choices: Rc<dyn Fn(&App) -> Option<PickerAiChoices>>,
    pub(crate) open: Rc<dyn Fn(&AiChoiceTarget, &mut Window, &mut App)>,
}

/// The other AIs for a thread of `current_agent` in `project`: the `+` menu's.
///
/// Holds the project and the workspace weakly: the picker that calls this
/// sits behind the thread view's `PopoverMenuHandle`, and nothing reached
/// from there may keep a strong hold on what owns the thread view.
pub(crate) fn agent_launch_source(
    project: WeakEntity<Project>,
    workspace: WeakEntity<Workspace>,
    current_agent: AgentId,
) -> AgentLaunchSource {
    let runner = ThreadRunner::of_agent(&current_agent);
    AgentLaunchSource {
        choices: Rc::new(move |cx| {
            let project = project.upgrade()?;
            // What they start opens in the panel, which refuses it without a
            // folder (`AgentPanel::new_external_agent_thread`).
            if project.read(cx).visible_worktrees(cx).next().is_none() {
                return None;
            }
            let supports_terminal = project.read(cx).supports_terminal(cx);
            // Which AI is "current" only matters to the `+` menu's shortcut.
            let inputs =
                gather_ai_choice_inputs(&project, Agent::NativeAgent, supports_terminal, cx);
            Some(PickerAiChoices {
                runner: runner.clone(),
                sections: crate::ai_choices::build_ai_choice_sections(&inputs),
            })
        }),
        open: Rc::new(move |target, window, cx| {
            crate::ai_choices::open_ai_choice(target, &workspace, window, cx)
        }),
    }
}

/// Keeps the entries `query` matches, and always the thread's own runner's,
/// which only place its models.
pub(crate) async fn filter_agent_launch_entries(
    choices: Option<PickerAiChoices>,
    query: &str,
    executor: BackgroundExecutor,
) -> Option<PickerAiChoices> {
    let mut choices = choices?;
    if query.is_empty() {
        return Some(choices);
    }
    let candidates = choices
        .sections
        .iter()
        .flat_map(|section| {
            section
                .choices
                .iter()
                .map(move |choice| choice_search_text(section.vendor, choice))
        })
        .enumerate()
        .map(|(ix, text)| StringMatchCandidate::new(ix, &text))
        .collect::<Vec<_>>();
    let matched = match_strings(
        &candidates,
        query,
        false,
        true,
        100,
        &Default::default(),
        executor,
    )
    .await
    .into_iter()
    .map(|mat| mat.candidate_id)
    .collect::<HashSet<_>>();
    let mut index = 0;
    let runner = choices.runner.clone();
    for section in &mut choices.sections {
        section.choices.retain(|choice| {
            let keep = matched.contains(&index)
                || choice_transition(choice, Some(&runner)) == AiChoiceTransition::SameThread;
            index += 1;
            keep
        });
    }
    Some(choices)
}

pub(crate) fn render_agent_launch_row(
    ix: usize,
    entry: &AgentLaunchEntry,
    selected: bool,
) -> AnyElement {
    render_ai_choice_picker_row(ix, &entry.choice, entry.transition, selected)
}

/// Starts what `entry` names once the picker that confirmed it is done
/// updating: it may replace the thread view the picker lives in.
pub(crate) fn open_agent_launch_entry(
    source: Option<&AgentLaunchSource>,
    entry: &AgentLaunchEntry,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(source) = source else {
        return;
    };
    let open = source.open.clone();
    let target = entry.choice.target.clone();
    window.defer(cx, move |window, cx| open(&target, window, cx));
}

type ModelPickerEntry = PickerRow<AgentModelInfo>;

pub struct ModelPickerDelegate {
    selector: Rc<dyn AgentModelSelector>,
    filtered_entries: Vec<ModelPickerEntry>,
    models: Option<AgentModelList>,
    /// The last search's results. Folding a group rebuilds the rows from these
    /// on the spot, because `confirm` cannot call `Picker::refresh`.
    filtered_models: AgentModelList,
    filtered_agents: Option<PickerAiChoices>,
    agent_launchers: Option<AgentLaunchSource>,
    collapse: GroupCollapseState<AgentModelId>,
    /// Keeps the selection where it was when the list is updated for a fold
    /// or a star.
    reselect_row: Option<RowAnchor<AgentModelId>>,
    selected_index: usize,
    selected_description: Option<(usize, SharedString)>,
    selected_model: Option<AgentModelInfo>,
    favorites: HashSet<AgentModelId>,
    _refresh_models_task: Task<()>,
    _settings_subscription: Subscription,
    _dismiss_subscription: Subscription,
    focus_handle: FocusHandle,
}

impl ModelPickerDelegate {
    fn new(
        selector: Rc<dyn AgentModelSelector>,
        focus_handle: FocusHandle,
        agent_launchers: Option<AgentLaunchSource>,
        window: &mut Window,
        cx: &mut Context<ModelSelector>,
    ) -> Self {
        let rx = selector.watch(cx);
        let refresh_models_task = {
            cx.spawn_in(window, {
                async move |this, cx| {
                    async fn refresh(
                        this: &WeakEntity<Picker<ModelPickerDelegate>>,
                        cx: &mut AsyncWindowContext,
                    ) -> Result<()> {
                        let (models_task, selected_model_task) = this.update(cx, |this, cx| {
                            (
                                this.delegate.selector.list_models(cx),
                                this.delegate.selector.selected_model(cx),
                            )
                        })?;

                        let (models, selected_model) =
                            futures::join!(models_task, selected_model_task);

                        this.update_in(cx, |this, window, cx| {
                            this.delegate.models = models.ok();
                            this.delegate.selected_model = selected_model.ok();
                            this.refresh(window, cx)
                        })
                    }

                    refresh(&this, cx).await.log_err();
                    if let Some(mut rx) = rx {
                        while let Ok(()) = rx.recv().await {
                            refresh(&this, cx).await.log_err();
                        }
                    }
                }
            })
        };

        let selector_for_subscription = selector.clone();
        let settings_subscription =
            cx.observe_global_in::<SettingsStore>(window, move |picker, window, cx| {
                // Only refresh if the favorites actually changed to avoid redundant work
                // when other settings are modified (e.g., user editing settings.json)
                let new_favorites = selector_for_subscription.favorite_model_ids(cx);
                if new_favorites != picker.delegate.favorites {
                    if picker.focus_handle(cx).contains_focused(window, cx) {
                        let delegate = &mut picker.delegate;
                        delegate
                            .collapse
                            .hold_favorites_at_open(&delegate.favorites);
                        delegate.reselect_row =
                            row_anchor(&delegate.filtered_entries, delegate.selected_index);
                    }
                    picker.delegate.favorites = new_favorites;
                    picker.refresh(window, cx);
                }
            });
        let favorites = selector.favorite_model_ids(cx);

        // Every way the popover closes (a choice, Escape, a click outside,
        // `PopoverMenuHandle::hide`) emits this on the picker.
        let picker = cx.entity();
        let dismiss_subscription = cx.subscribe_in(
            &picker,
            window,
            |picker, _, _: &DismissEvent, window, cx| {
                if picker.delegate.collapse.release_favorites_at_open() {
                    picker.refresh(window, cx);
                }
            },
        );

        Self {
            selector,
            filtered_entries: Vec::new(),
            models: None,
            filtered_models: AgentModelList::Flat(Vec::new()),
            filtered_agents: None,
            agent_launchers,
            collapse: GroupCollapseState::default(),
            reselect_row: None,
            selected_model: None,
            selected_index: 0,
            selected_description: None,
            favorites,
            _refresh_models_task: refresh_models_task,
            _settings_subscription: settings_subscription,
            _dismiss_subscription: dismiss_subscription,
            focus_handle,
        }
    }

    pub fn active_model(&self) -> Option<&AgentModelInfo> {
        self.selected_model.as_ref()
    }

    pub fn favorites_count(&self) -> usize {
        self.favorites.len()
    }

    pub fn cycle_favorite_models(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.favorites.is_empty() {
            return;
        }

        let Some(models) = &self.models else {
            return;
        };

        let all_models: Vec<&AgentModelInfo> = match models {
            AgentModelList::Flat(list) => list.iter().collect(),
            AgentModelList::Grouped(index_map) => index_map.values().flatten().collect(),
        };

        let favorite_models: Vec<_> = all_models
            .into_iter()
            .filter(|model| self.favorites.contains(&model.id))
            .unique_by(|model| &model.id)
            .collect();

        if favorite_models.is_empty() {
            return;
        }

        let current_id = self.selected_model.as_ref().map(|m| &m.id);

        let current_index_in_favorites = current_id
            .and_then(|id| favorite_models.iter().position(|m| &m.id == id))
            .unwrap_or(usize::MAX);

        let next_index = if current_index_in_favorites == usize::MAX {
            0
        } else {
            (current_index_in_favorites + 1) % favorite_models.len()
        };

        let next_model = favorite_models[next_index].clone();

        self.selector
            .select_model(next_model.id.clone(), cx)
            .detach_and_log_err(cx);

        self.selected_model = Some(next_model);

        // Keep the picker selection aligned with the newly-selected model
        if let Some(new_index) = self.filtered_entries.iter().position(|entry| {
            matches!(entry, PickerRow::Item { item, .. } if self.selected_model.as_ref().is_some_and(|selected| item.id == selected.id))
        }) {
            self.set_selected_index(new_index, window, cx);
        } else {
            cx.notify();
        }
    }

    fn rebuild_entries(&mut self) {
        self.filtered_entries = info_list_to_picker_entries(
            self.filtered_models.clone(),
            &self.favorites,
            self.selected_model.as_ref().map(|model| &model.id),
            self.filtered_agents.clone(),
            &self.collapse,
        );
    }

    fn toggle_group(
        &mut self,
        key: PickerGroupKey,
        expanded: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.collapse.set_expanded(key.clone(), !expanded);
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

impl PickerDelegate for ModelPickerDelegate {
    type ListItem = AnyElement;

    fn name() -> &'static str {
        "model selector"
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
        i18n::t!("Select a model…").into()
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim().to_string();
        let models = self.models.clone();
        let agents = self
            .agent_launchers
            .as_ref()
            .and_then(|agent_launchers| (agent_launchers.choices)(cx));
        let executor = cx.background_executor().clone();

        cx.spawn_in(window, async move |this, cx| {
            let filtered_models = match models {
                Some(models) => fuzzy_search(models, query.clone(), executor.clone()).await,
                None => AgentModelList::Flat(Vec::new()),
            };
            let filtered_agents = filter_agent_launch_entries(agents, &query, executor).await;

            this.update_in(cx, |this, window, cx| {
                let delegate = &mut this.delegate;
                delegate.collapse.set_query(&query);
                delegate.filtered_models = filtered_models;
                delegate.filtered_agents = filtered_agents;
                delegate.rebuild_entries();
                let new_index = delegate
                    .reselect_row
                    .take()
                    .and_then(|anchor| anchor_row_index(&delegate.filtered_entries, &anchor))
                    .unwrap_or_else(|| {
                        initial_row_index(
                            &delegate.filtered_entries,
                            delegate.selected_model.as_ref().map(|model| &model.id),
                        )
                    });
                this.set_selected_index(new_index, Some(picker::Direction::Down), true, window, cx);
                cx.notify();
            })
            .ok();
        })
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        match self.filtered_entries.get(self.selected_index) {
            Some(PickerRow::Item {
                item: model_info, ..
            }) => {
                if model_info.disabled.is_some() {
                    return;
                }
                self.selector
                    .select_model(model_info.id.clone(), cx)
                    .detach_and_log_err(cx);
                self.selected_model = Some(model_info.clone());
                let current_index = self.selected_index;
                self.set_selected_index(current_index, window, cx);

                cx.emit(DismissEvent);
            }
            Some(PickerRow::Group { key, expanded, .. }) => {
                let key = key.clone();
                let expanded = *expanded;
                self.toggle_group(key, expanded, window, cx);
            }
            Some(PickerRow::Agent(entry)) => {
                open_agent_launch_entry(self.agent_launchers.as_ref(), entry, window, cx);
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
            PickerRow::Label(title) => {
                Some(ModelSelectorHeader::new(title, ix > 1).into_any_element())
            }
            PickerRow::Group { .. } => render_group_header(&self.filtered_entries, ix, selected),
            PickerRow::Agent(entry) => {
                let description = crate::ai_choices::choice_explanation(&entry.choice);
                Some(
                    div()
                        .id(("ai-choice-row", ix))
                        .on_hover(cx.listener(move |menu, hovered, _, cx| {
                            if *hovered {
                                menu.delegate.selected_description = Some((ix, description.clone()));
                            } else if matches!(menu.delegate.selected_description, Some((id, _)) if id == ix) {
                                menu.delegate.selected_description = None;
                            }
                            cx.notify();
                        }))
                        .child(render_agent_launch_row(ix, entry, selected))
                        .into_any_element(),
                )
            }
            PickerRow::Item {
                item: model_info,
                is_favorite,
            } => {
                let is_selected = Some(model_info) == self.selected_model.as_ref();

                let is_favorite = *is_favorite;
                let handle_action_click = {
                    let model_id = model_info.id.clone();
                    let selector = self.selector.clone();

                    cx.listener(move |_, _, _, cx| {
                        selector.toggle_favorite_model(model_id.clone(), !is_favorite, cx);
                    })
                };

                let model_cost = model_info.cost.clone();

                Some(
                    div()
                        .id(("model-picker-menu-child", ix))
                        .when_some(model_info.description.clone(), |this, description| {
                            this.on_hover(cx.listener(move |menu, hovered, _, cx| {
                                if *hovered {
                                    menu.delegate.selected_description =
                                        Some((ix, description.clone()));
                                } else if matches!(menu.delegate.selected_description, Some((id, _)) if id == ix) {
                                    menu.delegate.selected_description = None;
                                }
                                cx.notify();
                            }))
                        })
                        .child(
                            ModelSelectorListItem::new(ix, model_info.name.clone())
                                .map(|this| match &model_info.icon {
                                    Some(AgentModelIcon::Path(path)) => this.icon_path(path.clone()),
                                    Some(AgentModelIcon::Named(icon)) => this.icon(*icon),
                                    None => this,
                                })
                                .disabled(model_info.disabled.clone())
                                .is_selected(is_selected)
                                .is_focused(selected)
                                .is_latest(model_info.is_latest)
                                .is_favorite(is_favorite)
                                .on_toggle_favorite(handle_action_click)
                                .cost_info(model_cost)
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

            DocumentationAside::new(
                side,
                Rc::new(move |_| Label::new(description.clone()).into_any_element()),
            )
        })
    }

    fn documentation_aside_index(&self) -> Option<usize> {
        self.selected_description.as_ref().map(|(ix, _)| *ix)
    }

    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let focus_handle = self.focus_handle.clone();

        if !self.selector.should_render_footer() {
            return None;
        }

        Some(ModelSelectorFooter::new(OpenSettings.boxed_clone(), focus_handle).into_any_element())
    }
}

fn info_list_to_picker_entries(
    model_list: AgentModelList,
    favorites: &HashSet<AgentModelId>,
    active: Option<&AgentModelId>,
    agents: Option<PickerAiChoices>,
    collapse: &GroupCollapseState<AgentModelId>,
) -> Vec<ModelPickerEntry> {
    let groups = match model_list {
        AgentModelList::Flat(models) => vec![PickerGroup {
            name: None,
            items: models,
        }],
        AgentModelList::Grouped(groups) => groups
            .into_iter()
            .map(|(group_name, models)| PickerGroup {
                name: Some(group_name.0),
                items: models,
            })
            .collect(),
    };
    build_picker_rows(groups, favorites, active, agents, collapse, i18n::t!("All"))
}

async fn fuzzy_search(
    model_list: AgentModelList,
    query: String,
    executor: BackgroundExecutor,
) -> AgentModelList {
    async fn fuzzy_search_list(
        model_list: Vec<AgentModelInfo>,
        group_name: Option<SharedString>,
        query: &str,
        executor: BackgroundExecutor,
    ) -> Vec<AgentModelInfo> {
        let candidates = model_list
            .iter()
            .enumerate()
            .map(|(ix, model)| {
                StringMatchCandidate::new(
                    ix,
                    &grouped_search_text(group_name.as_deref(), &model.name),
                )
            })
            .collect::<Vec<_>>();
        let mut matches = match_strings(
            &candidates,
            query,
            false,
            true,
            100,
            &Default::default(),
            executor,
        )
        .await;

        matches.sort_unstable_by_key(|mat| {
            let candidate = &candidates[mat.candidate_id];
            (Reverse(OrderedFloat(mat.score)), candidate.id)
        });

        matches
            .into_iter()
            .map(|mat| model_list[mat.candidate_id].clone())
            .collect()
    }

    match model_list {
        AgentModelList::Flat(model_list) => {
            AgentModelList::Flat(fuzzy_search_list(model_list, None, &query, executor).await)
        }
        AgentModelList::Grouped(index_map) => {
            let groups =
                futures::future::join_all(index_map.into_iter().map(|(group_name, models)| {
                    fuzzy_search_list(models, Some(group_name.0.clone()), &query, executor.clone())
                        .map(|results| (group_name, results))
                }))
                .await;
            AgentModelList::Grouped(IndexMap::from_iter(
                groups
                    .into_iter()
                    .filter(|(_, results)| !results.is_empty()),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use fs::FakeFs;
    use gpui::{App, Entity, TestAppContext, VisualTestContext};
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::agent_panel::{ExternalAgentMenuItem, external_agent_menu_items};
    use crate::ai_choices::{
        AiChoiceIcon, AiChoiceInputs, AiPlacement, ExternalAgentInput, NativeProviderInput,
        build_ai_choice_sections,
    };

    fn create_model_list(grouped_models: Vec<(&str, Vec<&str>)>) -> AgentModelList {
        AgentModelList::Grouped(IndexMap::from_iter(grouped_models.into_iter().map(
            |(group, models)| {
                (
                    acp_thread::AgentModelGroupName(group.to_string().into()),
                    models
                        .into_iter()
                        .map(|model| acp_thread::AgentModelInfo {
                            id: AgentModelId::new(model),
                            name: model.to_string().into(),
                            description: None,
                            icon: None,
                            is_latest: false,
                            disabled: None,
                            cost: None,
                        })
                        .collect::<Vec<_>>(),
                )
            },
        )))
    }

    fn assert_models_eq(result: AgentModelList, expected: Vec<(&str, Vec<&str>)>) {
        let AgentModelList::Grouped(groups) = result else {
            panic!("Expected LanguageModelInfoList::Grouped, got {:?}", result);
        };

        assert_eq!(
            groups.len(),
            expected.len(),
            "Number of groups doesn't match"
        );

        for (i, (expected_group, expected_models)) in expected.iter().enumerate() {
            let (actual_group, actual_models) = groups.get_index(i).unwrap();
            assert_eq!(
                actual_group.0.as_ref(),
                *expected_group,
                "Group at position {} doesn't match expected group",
                i
            );
            assert_eq!(
                actual_models.len(),
                expected_models.len(),
                "Number of models in group {} doesn't match",
                expected_group
            );

            for (j, expected_model_name) in expected_models.iter().enumerate() {
                assert_eq!(
                    actual_models[j].name, *expected_model_name,
                    "Model at position {} in group {} doesn't match expected model",
                    j, expected_group
                );
            }
        }
    }

    fn create_favorites(models: Vec<&str>) -> HashSet<AgentModelId> {
        models.into_iter().map(AgentModelId::new).collect()
    }

    fn get_entry_model_ids(entries: &[ModelPickerEntry]) -> Vec<&str> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                PickerRow::Item { item, .. } => Some(item.id.as_ref()),
                _ => None,
            })
            .collect()
    }

    /// `# ` marks a label, `▶`/`▼` a folded/unfolded heading with its count,
    /// `@ ` an agent row; anything else is a model id.
    fn entry_labels(entries: &[ModelPickerEntry]) -> Vec<String> {
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
                PickerRow::Item { item, .. } => item.id.to_string(),
                PickerRow::Agent(entry) => format!("@ {}", entry.choice.label),
            })
            .collect()
    }

    fn picker_entries(
        models: AgentModelList,
        favorites: &HashSet<AgentModelId>,
        active: Option<&str>,
        collapse: &GroupCollapseState<AgentModelId>,
    ) -> Vec<ModelPickerEntry> {
        let active = active.map(AgentModelId::new);
        info_list_to_picker_entries(models, favorites, active.as_ref(), None, collapse)
    }

    fn external_agent(id: &str, display_name: &str) -> ExternalAgentMenuItem {
        ExternalAgentMenuItem {
            id: AgentId::new(id),
            display_name: display_name.to_string().into(),
            icon_path: None,
        }
    }

    fn init_picker_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
    }

    fn open_picker(
        selector: Rc<TestModelSelector>,
        agent_launchers: Option<AgentLaunchSource>,
        cx: &mut TestAppContext,
    ) -> (Entity<ModelSelector>, &mut VisualTestContext) {
        let (picker, cx) = cx.add_window_view(move |window, cx| {
            let selector: Rc<dyn AgentModelSelector> = selector;
            acp_model_selector(selector, cx.focus_handle(), agent_launchers, window, cx)
        });
        cx.run_until_parked();
        (picker, cx)
    }

    fn picker_labels(picker: &Entity<ModelSelector>, cx: &mut VisualTestContext) -> Vec<String> {
        picker.read_with(cx, |picker, _| {
            entry_labels(&picker.delegate.filtered_entries)
        })
    }

    #[gpui::test]
    fn confirming_model_selects_model(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });

        let model_selector = Rc::new(TestModelSelector::new());

        let window_handle = cx.add_window({
            let model_selector = model_selector.clone();
            move |window, cx| {
                let selector: Rc<dyn AgentModelSelector> = model_selector;
                acp_model_selector(selector, cx.focus_handle(), None, window, cx)
            }
        });
        cx.run_until_parked();

        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        window_handle
            .update(&mut cx, |picker, window, cx| {
                picker.delegate.set_selected_index(1, window, cx);
                picker.delegate.confirm(false, window, cx);
            })
            .unwrap();

        assert_eq!(
            model_selector.selected_models.borrow().as_slice(),
            &[AgentModelId::new("manual")]
        );
    }

    struct TestModelSelector {
        models: AgentModelList,
        favorites: RefCell<HashSet<AgentModelId>>,
        selected_model: RefCell<AgentModelInfo>,
        selected_models: RefCell<Vec<AgentModelId>>,
    }

    impl TestModelSelector {
        fn new() -> Self {
            let models = vec![
                AgentModelInfo {
                    id: AgentModelId::new("auto"),
                    name: "Auto".into(),
                    description: None,
                    icon: None,
                    is_latest: false,
                    disabled: None,
                    cost: None,
                },
                AgentModelInfo {
                    id: AgentModelId::new("manual"),
                    name: "Manual".into(),
                    description: None,
                    icon: None,
                    is_latest: false,
                    disabled: None,
                    cost: None,
                },
            ];

            Self {
                selected_model: RefCell::new(models[0].clone()),
                models: AgentModelList::Flat(models),
                favorites: RefCell::default(),
                selected_models: RefCell::new(Vec::new()),
            }
        }

        fn grouped(groups: Vec<(&str, Vec<&str>)>, favorites: Vec<&str>, selected: &str) -> Self {
            let models = create_model_list(groups);
            let AgentModelList::Grouped(groups) = &models else {
                unreachable!()
            };
            let selected_model = groups
                .values()
                .flatten()
                .find(|model| model.id.as_ref() == selected)
                .cloned()
                .expect("the selected model is one of the models");
            Self {
                selected_model: RefCell::new(selected_model),
                models,
                favorites: RefCell::new(create_favorites(favorites)),
                selected_models: RefCell::new(Vec::new()),
            }
        }
    }

    impl AgentModelSelector for TestModelSelector {
        fn list_models(&self, _cx: &mut App) -> Task<Result<AgentModelList>> {
            Task::ready(Ok(self.models.clone()))
        }

        fn select_model(&self, model_id: AgentModelId, _cx: &mut App) -> Task<Result<()>> {
            self.selected_models.borrow_mut().push(model_id.clone());
            let all_models: Vec<&AgentModelInfo> = match &self.models {
                AgentModelList::Flat(list) => list.iter().collect(),
                AgentModelList::Grouped(groups) => groups.values().flatten().collect(),
            };
            if let Some(model) = all_models.into_iter().find(|model| model.id == model_id) {
                *self.selected_model.borrow_mut() = model.clone();
            }
            Task::ready(Ok(()))
        }

        fn selected_model(&self, _cx: &mut App) -> Task<Result<AgentModelInfo>> {
            Task::ready(Ok(self.selected_model.borrow().clone()))
        }

        fn favorite_model_ids(&self, _cx: &mut App) -> HashSet<AgentModelId> {
            self.favorites.borrow().clone()
        }
    }

    #[gpui::test]
    async fn test_fuzzy_match(cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            (
                "zed",
                vec![
                    "Claude 3.7 Sonnet",
                    "Claude 3.7 Sonnet Thinking",
                    "gpt-5",
                    "gpt-5-mini",
                ],
            ),
            ("openai", vec!["gpt-3.5-turbo", "gpt-5", "gpt-5-mini"]),
            ("ollama", vec!["mistral", "deepseek"]),
        ]);

        // Results should preserve models order whenever possible.
        // In the case below, `zed/gpt-5-mini` and `openai/gpt-5-mini` have identical
        // similarity scores, but `zed/gpt-5-mini` was higher in the models list,
        // so it should appear first in the results.
        let results = fuzzy_search(models.clone(), "mini".into(), cx.executor()).await;
        assert_models_eq(
            results,
            vec![("zed", vec!["gpt-5-mini"]), ("openai", vec!["gpt-5-mini"])],
        );

        // Fuzzy search - test with specific model name
        let results = fuzzy_search(models.clone(), "mistral".into(), cx.executor()).await;
        assert_models_eq(results, vec![("ollama", vec!["mistral"])]);
    }

    #[gpui::test]
    async fn a_query_can_name_the_group(cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Recommended", vec!["claude-sonnet", "gpt-5"]),
            ("OpenAI", vec!["gpt-5", "gpt-6"]),
            ("Ollama", vec!["qwen3", "llama3"]),
        ]);

        let results = fuzzy_search(models.clone(), "ollama".into(), cx.executor()).await;
        assert_models_eq(results, vec![("Ollama", vec!["qwen3", "llama3"])]);

        let results = fuzzy_search(models, "openai".into(), cx.executor()).await;
        assert_models_eq(results, vec![("OpenAI", vec!["gpt-5", "gpt-6"])]);
    }

    #[gpui::test]
    fn test_favorites_section_appears_when_favorites_exist(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("zed", vec!["zed/claude", "zed/gemini"]),
            ("openai", vec!["openai/gpt-5"]),
        ]);
        let favorites = create_favorites(vec!["zed/gemini"]);

        let entries = picker_entries(models, &favorites, None, &GroupCollapseState::default());

        assert!(matches!(
            entries.first(),
            Some(PickerRow::Label(s)) if s == "Favorites"
        ));

        let model_ids = get_entry_model_ids(&entries);
        assert_eq!(model_ids[0], "zed/gemini");
    }

    #[gpui::test]
    fn test_no_favorites_section_when_no_favorites(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![("zed", vec!["zed/claude", "zed/gemini"])]);
        let favorites = create_favorites(vec![]);

        let entries = picker_entries(models, &favorites, None, &GroupCollapseState::default());

        assert!(matches!(
            entries.first(),
            Some(PickerRow::Group { name, .. }) if name == "zed"
        ));
    }

    #[gpui::test]
    fn test_models_have_correct_actions(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("zed", vec!["zed/claude", "zed/gemini"]),
            ("openai", vec!["openai/gpt-5"]),
        ]);
        let favorites = create_favorites(vec!["zed/claude"]);
        let mut searching = GroupCollapseState::default();
        searching.set_query("e");

        let entries = picker_entries(models, &favorites, None, &searching);

        for entry in &entries {
            if let PickerRow::Item { item, is_favorite } = entry {
                if item.id.as_ref() == "zed/claude" {
                    assert!(is_favorite, "zed/claude should be a favorite");
                } else {
                    assert!(!is_favorite, "{} should not be a favorite", item.id);
                }
            }
        }
    }

    #[gpui::test]
    fn favorites_appear_in_their_groups_once_those_are_open(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("zed", vec!["zed/claude", "zed/gemini"]),
            ("openai", vec!["openai/gpt-5", "openai/gpt-4"]),
        ]);
        let favorites = create_favorites(vec!["zed/gemini", "openai/gpt-5"]);
        let mut searching = GroupCollapseState::default();
        searching.set_query("gpt");

        let entries = picker_entries(models, &favorites, None, &searching);
        let model_ids = get_entry_model_ids(&entries);

        assert_eq!(model_ids[0], "zed/gemini");
        assert_eq!(model_ids[1], "openai/gpt-5");

        assert!(model_ids[2..].contains(&"zed/gemini"));
        assert!(model_ids[2..].contains(&"openai/gpt-5"));
    }

    /// The same model used to be listed up to three times (お気に入り, 推奨 and
    /// its provider). With a favorite, the other groups are a heading each.
    #[gpui::test]
    fn test_favorites_are_not_duplicated_when_repeated_in_other_sections(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Recommended", vec!["zed/claude", "anthropic/claude"]),
            ("Zed", vec!["zed/claude", "zed/gpt-5"]),
            ("Antropic", vec!["anthropic/claude"]),
            ("OpenAI", vec!["openai/gpt-5"]),
        ]);

        let favorites = create_favorites(vec!["zed/claude"]);

        let entries = picker_entries(
            models,
            &favorites,
            Some("zed/claude"),
            &GroupCollapseState::default(),
        );

        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "zed/claude",
                "▶ Recommended (2)",
                "▶ Zed (2)",
                "▶ Antropic (1)",
                "▶ OpenAI (1)",
            ]
        );
    }

    #[gpui::test]
    fn without_favorites_every_group_stays_open(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Recommended", vec!["zed/claude"]),
            ("OpenAI", vec!["openai/gpt-5", "openai/o3"]),
        ]);

        let entries = picker_entries(
            models,
            &HashSet::default(),
            Some("openai/o3"),
            &GroupCollapseState::default(),
        );

        assert_eq!(
            entry_labels(&entries),
            vec![
                "▼ Recommended (1)",
                "zed/claude",
                "▼ OpenAI (2)",
                "openai/gpt-5",
                "openai/o3",
            ]
        );
    }

    #[gpui::test]
    fn searching_opens_every_group_even_one_the_user_folded(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Anthropic", vec!["claude-sonnet"]),
            ("OpenAI", vec!["gpt-5", "gpt-5-mini"]),
        ]);
        let favorites = create_favorites(vec!["claude-sonnet"]);
        let mut collapse = GroupCollapseState::default();
        collapse.set_expanded(PickerGroupKey::Named("OpenAI".into()), false);

        collapse.set_query("gpt");
        let entries = picker_entries(models, &favorites, None, &collapse);

        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▼ Anthropic (1)",
                "claude-sonnet",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ]
        );
    }

    #[gpui::test]
    fn the_group_holding_the_model_in_use_opens(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Recommended", vec!["claude-sonnet", "gpt-5"]),
            ("Anthropic", vec!["claude-sonnet", "claude-opus"]),
            ("OpenAI", vec!["gpt-5", "o3"]),
        ]);
        let favorites = create_favorites(vec!["claude-sonnet"]);

        // Not a favorite: the first group holding it opens, so its check mark
        // is in sight, and only that one (it would be listed twice otherwise).
        let entries = picker_entries(
            models.clone(),
            &favorites,
            Some("o3"),
            &GroupCollapseState::default(),
        );
        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▶ Recommended (2)",
                "▶ Anthropic (2)",
                "▼ OpenAI (2)",
                "gpt-5",
                "o3",
            ]
        );

        // A favorite is already in sight under お気に入り.
        let entries = picker_entries(
            models,
            &favorites,
            Some("claude-sonnet"),
            &GroupCollapseState::default(),
        );
        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▶ Recommended (2)",
                "▶ Anthropic (2)",
                "▶ OpenAI (2)",
            ]
        );
    }

    #[gpui::test]
    fn folds_the_user_made_win_and_search_folds_last_one_query(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Anthropic", vec!["claude-sonnet"]),
            ("OpenAI", vec!["gpt-5"]),
        ]);
        let favorites = create_favorites(vec!["claude-sonnet"]);
        let openai = PickerGroupKey::Named("OpenAI".into());
        let mut collapse = GroupCollapseState::default();

        collapse.set_expanded(openai.clone(), true);
        assert_eq!(
            entry_labels(&picker_entries(models.clone(), &favorites, None, &collapse)),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▶ Anthropic (1)",
                "▼ OpenAI (1)",
                "gpt-5",
            ]
        );

        collapse.set_query("gpt");
        collapse.set_expanded(openai, false);
        assert_eq!(
            entry_labels(&picker_entries(models.clone(), &favorites, None, &collapse)),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▼ Anthropic (1)",
                "claude-sonnet",
                "▶ OpenAI (1)",
            ],
            "a heading can be folded while searching"
        );

        collapse.set_query("gpt-");
        assert!(
            entry_labels(&picker_entries(models.clone(), &favorites, None, &collapse))
                .contains(&"▼ OpenAI (1)".to_string()),
            "a new query opens it again, so no match hides behind it"
        );

        collapse.set_query("");
        assert!(
            entry_labels(&picker_entries(models, &favorites, None, &collapse))
                .contains(&"▼ OpenAI (1)".to_string()),
            "and the fold made before searching is still there"
        );
    }

    #[gpui::test]
    fn test_flat_model_list_with_favorites(_cx: &mut TestAppContext) {
        let models = AgentModelList::Flat(vec![
            acp_thread::AgentModelInfo {
                id: AgentModelId::new("zed/claude"),
                name: "Claude".into(),
                description: None,
                icon: None,
                is_latest: false,
                disabled: None,
                cost: None,
            },
            acp_thread::AgentModelInfo {
                id: AgentModelId::new("zed/gemini"),
                name: "Gemini".into(),
                description: None,
                icon: None,
                is_latest: false,
                disabled: None,
                cost: None,
            },
        ]);
        let favorites = create_favorites(vec!["zed/gemini"]);

        let entries = picker_entries(models, &favorites, None, &GroupCollapseState::default());

        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "zed/gemini",
                "# All",
                "zed/claude",
                "zed/gemini",
            ],
            "a short list without group names is never folded"
        );
    }

    #[gpui::test]
    fn a_long_flat_list_folds_behind_favorites(_cx: &mut TestAppContext) {
        let models = AgentModelList::Flat(
            (0..MIN_FOLDABLE_UNGROUPED)
                .map(|index| AgentModelInfo {
                    id: AgentModelId::new(format!("model-{index}")),
                    name: format!("Model {index}").into(),
                    description: None,
                    icon: None,
                    is_latest: false,
                    disabled: None,
                    cost: None,
                })
                .collect(),
        );
        let favorites = create_favorites(vec!["model-0"]);

        let entries = picker_entries(models, &favorites, None, &GroupCollapseState::default());

        assert_eq!(
            entry_labels(&entries),
            vec!["# Favorites", "model-0", "▶ All (10)"]
        );
    }

    #[gpui::test]
    fn test_favorites_count_returns_correct_count(_cx: &mut TestAppContext) {
        let empty_favorites: HashSet<AgentModelId> = HashSet::default();
        assert_eq!(empty_favorites.len(), 0);

        let one_favorite = create_favorites(vec!["model-a"]);
        assert_eq!(one_favorite.len(), 1);

        let multiple_favorites = create_favorites(vec!["model-a", "model-b", "model-c"]);
        assert_eq!(multiple_favorites.len(), 3);

        let with_duplicates = create_favorites(vec!["model-a", "model-a", "model-b"]);
        assert_eq!(with_duplicates.len(), 2);
    }

    #[gpui::test]
    fn test_is_favorite_flag_set_correctly_in_entries(_cx: &mut TestAppContext) {
        let models = AgentModelList::Flat(vec![
            acp_thread::AgentModelInfo {
                id: AgentModelId::new("favorite-model"),
                name: "Favorite".into(),
                description: None,
                icon: None,
                is_latest: false,
                disabled: None,
                cost: None,
            },
            acp_thread::AgentModelInfo {
                id: AgentModelId::new("regular-model"),
                name: "Regular".into(),
                description: None,
                icon: None,
                is_latest: false,
                disabled: None,
                cost: None,
            },
        ]);
        let favorites = create_favorites(vec!["favorite-model"]);

        let entries = picker_entries(models, &favorites, None, &GroupCollapseState::default());

        for entry in &entries {
            if let PickerRow::Item { item, is_favorite } = entry {
                if item.id.as_ref() == "favorite-model" {
                    assert!(*is_favorite, "favorite-model should have is_favorite=true");
                } else if item.id.as_ref() == "regular-model" {
                    assert!(!*is_favorite, "regular-model should have is_favorite=false");
                }
            }
        }
    }

    #[gpui::test]
    fn the_selection_starts_on_a_model_rather_than_a_heading(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            ("Anthropic", vec!["claude-sonnet"]),
            ("OpenAI", vec!["gpt-5"]),
        ]);
        let mut searching = GroupCollapseState::default();
        searching.set_query("gpt");
        let entries = picker_entries(
            models,
            &HashSet::default(),
            Some("claude-sonnet"),
            &searching,
        );

        assert_eq!(
            entry_labels(&entries),
            vec!["▼ Anthropic (1)", "claude-sonnet", "▼ OpenAI (1)", "gpt-5"]
        );
        assert_eq!(
            initial_row_index(&entries, Some(&AgentModelId::new("gpt-5"))),
            3,
            "on the model in use"
        );
        assert_eq!(
            initial_row_index(&entries, Some(&AgentModelId::new("elsewhere"))),
            1,
            "else on the first model, so Enter picks the best match"
        );
    }

    #[gpui::test]
    async fn confirming_a_heading_unfolds_it_without_choosing_a_model(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let selector = Rc::new(TestModelSelector::grouped(
            vec![
                ("Anthropic", vec!["claude-sonnet", "claude-opus"]),
                ("OpenAI", vec!["gpt-5", "gpt-5-mini"]),
            ],
            vec!["claude-sonnet"],
            "claude-sonnet",
        ));
        let (picker, cx) = open_picker(selector.clone(), None, cx);
        let dismissed = Rc::new(Cell::new(false));
        cx.update({
            let dismissed = dismissed.clone();
            |_, cx| {
                cx.subscribe(&picker, move |_, _: &DismissEvent, _| dismissed.set(true))
                    .detach();
            }
        });

        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▶ Anthropic (2)",
                "▶ OpenAI (2)"
            ]
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.set_selected_index(3, window, cx);
            picker.delegate.confirm(false, window, cx);
            assert_eq!(
                entry_labels(&picker.delegate.filtered_entries),
                vec![
                    "# Favorites",
                    "claude-sonnet",
                    "▶ Anthropic (2)",
                    "▼ OpenAI (2)",
                    "gpt-5",
                    "gpt-5-mini",
                ],
                "rebuilt on the spot, without waiting for a search"
            );
        });
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            assert_eq!(
                entry_labels(&picker.delegate.filtered_entries),
                vec![
                    "# Favorites",
                    "claude-sonnet",
                    "▶ Anthropic (2)",
                    "▼ OpenAI (2)",
                    "gpt-5",
                    "gpt-5-mini",
                ]
            );
            assert_eq!(
                picker.delegate.selected_index, 3,
                "the selection stays on the heading"
            );
            assert!(
                picker.delegate.reselect_row.is_none(),
                "the list was updated for the new number of rows"
            );
        });
        assert!(selector.selected_models.borrow().is_empty());
        assert!(!dismissed.get(), "the popover stays open");

        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "# Favorites",
                "claude-sonnet",
                "▶ Anthropic (2)",
                "▶ OpenAI (2)"
            ]
        );
        assert!(!dismissed.get());
    }

    fn native_provider(id: &str, name: &str, authenticated: bool) -> NativeProviderInput {
        NativeProviderInput {
            id: id.to_string().into(),
            name: name.to_string().into(),
            icon: AiChoiceIcon::Icon(IconName::Sparkle),
            authenticated,
            has_models: true,
        }
    }

    fn registry_agent(id: &str, name: &str) -> ExternalAgentInput {
        ExternalAgentInput {
            id: AgentId::new(id.to_string()),
            display_name: name.to_string().into(),
            icon_path: None,
            from_registry: true,
            outside_by_choice: false,
            subscription_sign_in_allowed: true,
        }
    }

    /// What the `+` menu offers in these tests: Claude through the CLI, its
    /// ACP agent and an API key; OpenAI through Codex and an API key; Google
    /// still to be set up.
    fn ai_choices(runner: ThreadRunner) -> PickerAiChoices {
        PickerAiChoices {
            runner,
            sections: build_ai_choice_sections(&AiChoiceInputs {
                native_providers: vec![
                    native_provider("anthropic", "Anthropic", true),
                    native_provider("openai", "OpenAI", true),
                    native_provider("google", "Google AI", false),
                ],
                native_default_provider: Some("anthropic".into()),
                external_agents: vec![
                    registry_agent("claude-acp", "Claude Agent"),
                    registry_agent("codex-acp", "Codex"),
                ],
                claude_code: Some(AiPlacement::Sandbox),
                installable_agents: Vec::new(),
                selected_agent: Agent::NativeAgent,
            }),
        }
    }

    fn claude_agent_runner() -> ThreadRunner {
        ThreadRunner::ExternalAgent(AgentId::new("claude-acp"))
    }

    fn launch_source(
        runner: ThreadRunner,
        opened: Rc<RefCell<Vec<AiChoiceTarget>>>,
    ) -> AgentLaunchSource {
        AgentLaunchSource {
            choices: Rc::new(move |_| Some(ai_choices(runner.clone()))),
            open: Rc::new(move |target, _, _| opened.borrow_mut().push(target.clone())),
        }
    }

    fn transitions(entries: &[ModelPickerEntry]) -> Vec<(String, AiChoiceTransition)> {
        entries
            .iter()
            .filter_map(|entry| match entry {
                PickerRow::Agent(entry) => Some((entry.choice.label.to_string(), entry.transition)),
                _ => None,
            })
            .collect()
    }

    /// A thread of the built-in agent: its providers' models sit in their
    /// vendors' sections where the `+` menu lists the provider, next to the
    /// vendor's other ways in, which open a thread or a tab of their own.
    #[gpui::test]
    fn the_built_in_agent_s_models_sit_in_their_vendor_s_section(_cx: &mut TestAppContext) {
        let models = create_model_list(vec![
            (
                "Recommended",
                vec!["anthropic/claude-sonnet", "openai/gpt-5"],
            ),
            (
                "Anthropic",
                vec!["anthropic/claude-sonnet", "anthropic/claude-opus"],
            ),
            ("OpenAI", vec!["openai/gpt-5"]),
        ]);
        let entries = info_list_to_picker_entries(
            models,
            &HashSet::default(),
            None,
            Some(ai_choices(ThreadRunner::BuiltInAgent)),
            &GroupCollapseState::default(),
        );

        assert_eq!(
            entry_labels(&entries),
            vec![
                "▼ Recommended (2)",
                "anthropic/claude-sonnet",
                "openai/gpt-5",
                "# Claude (Anthropic)",
                "@ Claude Code",
                "@ Claude Agent",
                "▼ Anthropic (2)",
                "anthropic/claude-sonnet",
                "anthropic/claude-opus",
                "# OpenAI",
                "@ Codex",
                "▼ OpenAI (1)",
                "openai/gpt-5",
                "# Google",
                "@ Google AI",
                "# Local Models",
                "@ Local Model (Ollama, LM Studio, …)",
            ],
            "a group across providers stays above the sections"
        );
        assert_eq!(
            transitions(&entries),
            vec![
                ("Claude Code".to_string(), AiChoiceTransition::NewTab),
                ("Claude Agent".to_string(), AiChoiceTransition::NewThread),
                ("Codex".to_string(), AiChoiceTransition::NewThread),
                ("Google AI".to_string(), AiChoiceTransition::SetUp),
                (
                    "Local Model (Ollama, LM Studio, …)".to_string(),
                    AiChoiceTransition::SetUp
                ),
            ]
        );
    }

    /// Claude Code's row comes before the Anthropic models in their vendor's
    /// section, yet Enter after a query that finds both picks the model.
    #[gpui::test]
    fn the_selection_starts_on_a_model_rather_than_on_another_ai(_cx: &mut TestAppContext) {
        let mut searching = GroupCollapseState::default();
        searching.set_query("claude");
        let entries = info_list_to_picker_entries(
            create_model_list(vec![("Anthropic", vec!["anthropic/claude-sonnet"])]),
            &HashSet::default(),
            Some(&AgentModelId::new("openai/gpt-5")),
            Some(ai_choices(ThreadRunner::BuiltInAgent)),
            &searching,
        );
        assert_eq!(
            entry_labels(&entries).get(..5),
            Some(
                &[
                    "# Claude (Anthropic)".to_string(),
                    "@ Claude Code".to_string(),
                    "@ Claude Agent".to_string(),
                    "▼ Anthropic (1)".to_string(),
                    "anthropic/claude-sonnet".to_string(),
                ][..]
            )
        );
        assert_eq!(
            initial_row_index(&entries, Some(&AgentModelId::new("openai/gpt-5"))),
            4
        );
    }

    /// A thread of an external agent: its own models stand where the `+`
    /// menu lists it, and the built-in agent's providers become rows that
    /// open a new thread.
    #[gpui::test]
    fn an_external_agent_s_models_stand_in_its_own_row(_cx: &mut TestAppContext) {
        let entries = info_list_to_picker_entries(
            AgentModelList::Flat(vec![
                acp_thread::AgentModelInfo {
                    id: AgentModelId::new("default"),
                    name: "Default".into(),
                    description: None,
                    icon: None,
                    is_latest: false,
                    disabled: None,
                    cost: None,
                },
                acp_thread::AgentModelInfo {
                    id: AgentModelId::new("opus"),
                    name: "Opus".into(),
                    description: None,
                    icon: None,
                    is_latest: false,
                    disabled: None,
                    cost: None,
                },
            ]),
            &create_favorites(vec!["opus"]),
            Some(&AgentModelId::new("opus")),
            Some(ai_choices(claude_agent_runner())),
            &GroupCollapseState::default(),
        );

        assert_eq!(
            entry_labels(&entries),
            vec![
                "# Favorites",
                "opus",
                "# Claude (Anthropic)",
                "@ Claude Code",
                "default",
                "opus",
                "@ Anthropic",
                "# OpenAI",
                "@ Codex",
                "@ OpenAI",
                "# Google",
                "@ Google AI",
                "# Local Models",
                "@ Local Model (Ollama, LM Studio, …)",
            ],
            "no 「All」 heading inside the vendor's section"
        );
        let transitions = transitions(&entries);
        assert!(transitions.contains(&("Anthropic".to_string(), AiChoiceTransition::NewThread)));
        assert!(
            !transitions.iter().any(|(label, _)| label == "Claude Agent"),
            "the thread's own agent is not offered as another thread"
        );
    }

    #[gpui::test]
    fn without_other_ais_the_picker_lists_only_the_models(_cx: &mut TestAppContext) {
        let models = || {
            create_model_list(vec![
                ("Anthropic", vec!["anthropic/claude-opus"]),
                ("OpenAI", vec!["openai/gpt-5"]),
            ])
        };
        assert_eq!(
            entry_labels(&info_list_to_picker_entries(
                models(),
                &HashSet::default(),
                None,
                None,
                &GroupCollapseState::default(),
            )),
            vec![
                "▼ Anthropic (1)",
                "anthropic/claude-opus",
                "▼ OpenAI (1)",
                "openai/gpt-5",
            ]
        );
    }

    #[gpui::test]
    async fn searching_keeps_the_matches_and_the_thread_s_own_models(cx: &mut TestAppContext) {
        let found = filter_agent_launch_entries(
            Some(ai_choices(claude_agent_runner())),
            "claude",
            cx.executor(),
        )
        .await
        .expect("choices");
        let mut targets = found
            .sections
            .iter()
            .flat_map(|section| &section.choices)
            .map(|choice| choice.target.clone())
            .collect::<Vec<_>>();
        targets.sort_by_key(|target| format!("{target:?}"));
        assert_eq!(
            targets,
            vec![
                AiChoiceTarget::ExternalAgentThread {
                    agent_id: AgentId::new("claude-acp")
                },
                AiChoiceTarget::LaunchClaudeCode,
                AiChoiceTarget::NativeThread {
                    provider_id: "anthropic".into()
                },
            ],
            "the vendor's name finds its entries"
        );

        let found = filter_agent_launch_entries(
            Some(ai_choices(claude_agent_runner())),
            "codex",
            cx.executor(),
        )
        .await
        .expect("choices");
        let labels = found
            .sections
            .iter()
            .flat_map(|section| &section.choices)
            .map(|choice| choice.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            labels,
            vec!["Claude Agent", "Codex"],
            "the thread's own agent stays, to place its models"
        );
    }

    #[gpui::test]
    async fn another_ai_is_started_from_its_vendor_s_section(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let opened = Rc::new(RefCell::new(Vec::<AiChoiceTarget>::new()));
        let selector = Rc::new(TestModelSelector::new());
        let (picker, cx) = open_picker(
            selector.clone(),
            Some(launch_source(claude_agent_runner(), opened.clone())),
            cx,
        );
        let dismissed = Rc::new(Cell::new(0));
        cx.update({
            let dismissed = dismissed.clone();
            |_, cx| {
                cx.subscribe(&picker, move |_, _: &DismissEvent, _| {
                    dismissed.set(dismissed.get() + 1)
                })
                .detach();
            }
        });

        assert_eq!(
            picker_labels(&picker, cx).get(..4),
            Some(
                &[
                    "# Claude (Anthropic)".to_string(),
                    "@ Claude Code".to_string(),
                    "auto".to_string(),
                    "manual".to_string(),
                ][..]
            ),
        );
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.selected_index, 2,
                "the selection starts on the model in use"
            );
        });

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("codex", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(picker_labels(&picker, cx), vec!["# OpenAI", "@ Codex"]);
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        picker.update_in(cx, |picker, window, cx| {
            assert_eq!(
                picker.delegate.selected_index, 1,
                "on the agent, not its vendor's heading"
            );
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            opened.borrow().as_slice(),
            &[AiChoiceTarget::ExternalAgentThread {
                agent_id: AgentId::new("codex-acp")
            }]
        );
        assert_eq!(dismissed.get(), 1);

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("claude code", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&picker, cx),
            vec!["# Claude (Anthropic)", "@ Claude Code"]
        );
        picker.update_in(cx, |picker, window, cx| {
            picker.delegate.confirm(false, window, cx);
        });
        cx.run_until_parked();
        assert_eq!(
            opened.borrow().last(),
            Some(&AiChoiceTarget::LaunchClaudeCode)
        );
        assert_eq!(dismissed.get(), 2);
        assert!(
            selector.selected_models.borrow().is_empty(),
            "starting another AI does not change this thread's model"
        );
    }

    /// ADR-F94 step 4 in the picker: what still has to be set up or
    /// installed is listed under its vendor and, chosen, opens the same setup
    /// as the `+` menu's entry rather than a thread.
    #[gpui::test]
    async fn what_still_needs_setting_up_opens_its_setup_from_the_picker(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let opened = Rc::new(RefCell::new(Vec::<AiChoiceTarget>::new()));
        let source = AgentLaunchSource {
            choices: Rc::new(|_| {
                Some(PickerAiChoices {
                    runner: claude_agent_runner(),
                    sections: build_ai_choice_sections(&AiChoiceInputs {
                        native_providers: vec![native_provider("google", "Google AI", false)],
                        installable_agents: vec![crate::ai_choices::InstallableAgentInput {
                            id: AgentId::new("gemini"),
                            display_name: "Gemini CLI".into(),
                            icon_path: None,
                        }],
                        selected_agent: Agent::NativeAgent,
                        ..AiChoiceInputs::default()
                    }),
                })
            }),
            open: Rc::new({
                let opened = opened.clone();
                move |target, _, _| opened.borrow_mut().push(target.clone())
            }),
        };
        let (picker, cx) = open_picker(Rc::new(TestModelSelector::new()), Some(source), cx);

        let entries = picker.read_with(cx, |picker, _| picker.delegate.filtered_entries.clone());
        let found = transitions(&entries);
        for label in ["Gemini CLI", "Google AI"] {
            assert!(
                found.contains(&(label.to_string(), AiChoiceTransition::SetUp)),
                "{label} in {found:?}"
            );
        }

        for (query, target) in [
            (
                "gemini cli",
                AiChoiceTarget::InstallExternalAgent {
                    agent_id: AgentId::new("gemini"),
                },
            ),
            (
                "google ai",
                AiChoiceTarget::SetUpProvider {
                    provider_id: Some("google".into()),
                },
            ),
        ] {
            picker.update_in(cx, |picker, window, cx| picker.set_query(query, window, cx));
            cx.run_until_parked();
            cx.update(|window, _| window.refresh());
            cx.run_until_parked();
            picker.update_in(cx, |picker, window, cx| {
                picker.delegate.confirm(false, window, cx);
            });
            cx.run_until_parked();
            assert_eq!(opened.borrow().last(), Some(&target), "{query}");
        }
    }

    #[gpui::test]
    async fn the_other_ais_do_not_keep_the_project_alive(cx: &mut TestAppContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "file.txt": "" }))
            .await;
        let project = project::Project::test(fs, [std::path::Path::new("/project")], cx).await;
        cx.update(language_model::LanguageModelRegistry::test);
        let agent_launchers = agent_launch_source(
            project.downgrade(),
            WeakEntity::new_invalid(),
            agent::ZED_AGENT_ID.clone(),
        );
        let choices = cx
            .update(|cx| (agent_launchers.choices)(cx))
            .expect("a project with a folder offers other AIs");
        assert_eq!(choices.runner, ThreadRunner::BuiltInAgent);

        let released = project.downgrade();
        drop(project);
        cx.run_until_parked();

        released.assert_released();
        assert!(cx.update(|cx| (agent_launchers.choices)(cx)).is_none());
    }

    fn star(selector: &TestModelSelector, model: &str, cx: &mut VisualTestContext) {
        selector
            .favorites
            .borrow_mut()
            .insert(AgentModelId::new(model));
        cx.update(|_, cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
        cx.run_until_parked();
    }

    fn selected_label(picker: &Entity<ModelSelector>, cx: &mut VisualTestContext) -> String {
        picker.read_with(cx, |picker, _| {
            entry_labels(&picker.delegate.filtered_entries)
                .get(picker.delegate.selected_index)
                .cloned()
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    async fn the_first_star_leaves_the_open_list_unfolded_until_it_closes(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let selector = Rc::new(TestModelSelector::grouped(
            vec![
                ("Anthropic", vec!["claude-sonnet", "claude-opus"]),
                ("OpenAI", vec!["gpt-5", "gpt-5-mini"]),
            ],
            vec![],
            "gpt-5",
        ));
        let (picker, cx) = open_picker(selector.clone(), None, cx);
        picker.update_in(cx, |picker, window, cx| {
            window.focus(&picker.focus_handle(cx), cx);
            picker.set_selected_index(2, None, false, window, cx);
        });
        cx.update(|window, _| window.refresh());
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "▼ Anthropic (2)",
                "claude-sonnet",
                "claude-opus",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ]
        );
        assert_eq!(selected_label(&picker, cx), "claude-opus");

        star(&selector, "claude-opus", cx);
        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "# Favorites",
                "claude-opus",
                "▼ Anthropic (2)",
                "claude-sonnet",
                "claude-opus",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ],
            "the star shows up under Favorites right away, and nothing folds"
        );
        picker.read_with(cx, |picker, _| {
            assert_eq!(
                picker.delegate.selected_index, 4,
                "the selection stays on the row it was on (not the new Favorites copy) \
                 although rows came in above it"
            );
        });

        picker.update(cx, |_, cx| cx.emit(DismissEvent));
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "# Favorites",
                "claude-opus",
                "▶ Anthropic (2)",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ],
            "the next opening folds behind the favorites (the group in use stays open)"
        );
    }

    #[gpui::test]
    async fn a_star_made_while_the_popover_is_closed_folds_at_once(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let selector = Rc::new(TestModelSelector::grouped(
            vec![
                ("Anthropic", vec!["claude-sonnet", "claude-opus"]),
                ("OpenAI", vec!["gpt-5", "gpt-5-mini"]),
            ],
            vec![],
            "gpt-5",
        ));
        let (picker, cx) = open_picker(selector.clone(), None, cx);

        star(&selector, "claude-opus", cx);

        assert_eq!(
            picker_labels(&picker, cx),
            vec![
                "# Favorites",
                "claude-opus",
                "▶ Anthropic (2)",
                "▼ OpenAI (2)",
                "gpt-5",
                "gpt-5-mini",
            ]
        );
    }

    #[gpui::test]
    async fn without_a_folder_no_other_ai_is_offered(cx: &mut TestAppContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = project::Project::test(fs, [], cx).await;
        let agent_launchers = agent_launch_source(
            project.downgrade(),
            WeakEntity::new_invalid(),
            AgentId::new("claude-acp"),
        );

        assert!(
            cx.update(|cx| (agent_launchers.choices)(cx)).is_none(),
            "the panel refuses to start a thread or the CLI without a folder"
        );
    }

    #[gpui::test]
    async fn a_query_of_only_spaces_is_no_search(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let selector = Rc::new(TestModelSelector::grouped(
            vec![
                ("Anthropic", vec!["claude-sonnet", "claude-opus"]),
                ("OpenAI", vec!["gpt-5", "gpt-5-mini"]),
            ],
            vec!["claude-sonnet"],
            "claude-sonnet",
        ));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let (picker, cx) = open_picker(
            selector,
            Some(launch_source(claude_agent_runner(), opened)),
            cx,
        );
        let browsing = picker_labels(&picker, cx);
        assert_eq!(
            browsing.get(..5),
            Some(
                &[
                    "# Favorites".to_string(),
                    "claude-sonnet".to_string(),
                    "# Claude (Anthropic)".to_string(),
                    "@ Claude Code".to_string(),
                    "▶ Anthropic (2)".to_string(),
                ][..]
            )
        );

        picker.update_in(cx, |picker, window, cx| picker.set_query("   ", window, cx));
        cx.run_until_parked();

        assert_eq!(
            picker_labels(&picker, cx),
            browsing,
            "blanks alone neither unfold every group nor hide the models"
        );
    }

    #[gpui::test]
    async fn queries_name_the_recommended_group_and_a_vendor(cx: &mut TestAppContext) {
        init_picker_test(cx);
        let selector = Rc::new(TestModelSelector::grouped(
            vec![
                ("Recommended", vec!["claude-sonnet"]),
                ("OpenAI", vec!["gpt-5"]),
            ],
            vec![],
            "gpt-5",
        ));
        let opened = Rc::new(RefCell::new(Vec::new()));
        let (picker, cx) = open_picker(
            selector,
            Some(launch_source(claude_agent_runner(), opened)),
            cx,
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("Recommended", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(
            picker_labels(&picker, cx),
            vec!["# Claude (Anthropic)", "▼ Recommended (1)", "claude-sonnet"]
        );

        picker.update_in(cx, |picker, window, cx| {
            picker.set_query("Google", window, cx)
        });
        cx.run_until_parked();
        assert_eq!(picker_labels(&picker, cx), vec!["# Google", "@ Google AI"]);
    }

    #[gpui::test]
    async fn external_agents_are_listed_by_name_ignoring_case(cx: &mut TestAppContext) {
        crate::test_support::init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "file.txt": "" }))
            .await;
        let project = project::Project::test(fs, [std::path::Path::new("/project")], cx).await;
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let agent_servers = settings.agent_servers.get_or_insert_default();
                    for name in ["beta", "Zeta", "Alpha"] {
                        agent_servers.insert(
                            name.to_string(),
                            serde_json::from_value(serde_json::json!({
                                "type": "custom",
                                "command": name,
                            }))
                            .unwrap(),
                        );
                    }
                });
            });
        });
        cx.run_until_parked();

        let items = project.read_with(cx, |project, cx| {
            external_agent_menu_items(project.agent_server_store().read(cx), cx)
        });

        assert_eq!(
            items,
            vec![
                external_agent("Alpha", "Alpha"),
                external_agent("beta", "beta"),
                external_agent("Zeta", "Zeta"),
            ]
        );
    }
}
