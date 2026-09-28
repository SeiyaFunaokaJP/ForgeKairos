mod persistence;

use std::{
    cmp::{self, Reverse},
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use client::parse_zed_link;
use command_palette_hooks::{
    CommandInterceptItem, CommandInterceptResult, CommandPaletteFilter,
    GlobalCommandPaletteInterceptor,
};

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    Action, App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable,
    ParentElement, Render, Styled, Task, TaskExt, WeakEntity, Window,
};
use persistence::CommandPaletteDB;
use picker::Direction;
use picker::{Picker, PickerDelegate};
use postage::{sink::Sink, stream::Stream};
use settings::Settings;
use ui::{HighlightedLabel, KeyBinding, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Workspace, WorkspaceSettings};
use zed_actions::{OpenZedUrl, command_palette::Toggle};

pub fn init(cx: &mut App) {
    command_palette_hooks::init(cx);
    cx.observe_new(CommandPalette::register).detach();
}

impl ModalView for CommandPalette {}

pub struct CommandPalette {
    picker: Entity<Picker<CommandPaletteDelegate>>,
}

/// Removes subsequent whitespace characters and double colons from the query, and converts
/// underscores to spaces.
///
/// This improves the likelihood of a match by either humanized name or keymap-style name.
/// Underscores are converted to spaces because `humanize_action_name` converts them to spaces
/// when building the search candidates (e.g. `terminal_panel::Toggle` -> `terminal panel: toggle`).
pub fn normalize_action_query(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut last_char = None;

    for char in input.trim().chars() {
        let normalized_char = if char == '_' { ' ' } else { char };
        match (last_char, normalized_char) {
            (Some(':'), ':') => continue,
            (Some(last_char), c) if last_char.is_whitespace() && c.is_whitespace() => {
                continue;
            }
            _ => {
                last_char = Some(normalized_char);
            }
        }
        result.push(normalized_char);
    }

    result
}

impl CommandPalette {
    fn register(
        workspace: &mut Workspace,
        _window: Option<&mut Window>,
        _: &mut Context<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            Self::toggle(workspace, "", window, cx)
        });
    }

    pub fn toggle(
        workspace: &mut Workspace,
        query: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if workspace.active_modal::<CommandPalette>(cx).is_some() {
            workspace.hide_modal(window, cx);
            return;
        }

        if workspace.has_active_modal(window, cx) && !workspace.hide_modal(window, cx) {
            return;
        }

        let Some(previous_focus_handle) = window.focused(cx) else {
            return;
        };

        let entity = cx.weak_entity();
        workspace.toggle_modal(window, cx, move |window, cx| {
            CommandPalette::new(previous_focus_handle, query, entity, window, cx)
        });
    }

    fn new(
        previous_focus_handle: FocusHandle,
        query: &str,
        entity: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = CommandPaletteFilter::try_global(cx);

        let commands = window
            .available_actions(cx)
            .into_iter()
            .filter_map(|action| {
                if filter.is_some_and(|filter| filter.is_hidden(&*action)) {
                    return None;
                }

                Some(Command::for_action(action))
            })
            .collect();

        let delegate = CommandPaletteDelegate::new(
            cx.entity().downgrade(),
            entity,
            commands,
            previous_focus_handle,
        );

        let picker = cx.new(|cx| {
            // One-shot action; there's nothing to reopen.
            let picker = Picker::uniform_list(delegate, window, cx)
                .reopenable(false, cx)
                .show_scrollbar(true);
            picker.set_query(query, window, cx);
            picker
        });
        Self { picker }
    }

    pub fn set_query(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.picker
            .update(cx, |picker, cx| picker.set_query(query, window, cx))
    }
}

impl EventEmitter<DismissEvent> for CommandPalette {}

impl Focusable for CommandPalette {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for CommandPalette {
    fn render(&mut self, _window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("CommandPalette")
            .child(self.picker.clone())
    }
}

pub struct CommandPaletteDelegate {
    latest_query: String,
    command_palette: WeakEntity<CommandPalette>,
    workspace: WeakEntity<Workspace>,
    all_commands: Vec<Command>,
    commands: Vec<Command>,
    matches: Vec<StringMatch>,
    selected_ix: usize,
    previous_focus_handle: FocusHandle,
    updating_matches: Option<(
        Task<()>,
        postage::dispatch::Receiver<(Vec<Command>, Vec<StringMatch>, CommandInterceptResult)>,
    )>,
    query_history: QueryHistory,
}

struct Command {
    name: String,
    /// Fuzzy-match haystack, see [`localized_action_name_and_search_text`].
    search_text: String,
    /// Key of the usage history in the database. `name` is translated, so keying by it would lose
    /// the history on every change of the display language; the English humanized name is also
    /// what upstream stores, so databases stay interchangeable.
    usage_key: String,
    action: Box<dyn Action>,
}

impl Command {
    fn for_action(action: Box<dyn Action>) -> Self {
        let (name, search_text) = localized_action_name_and_search_text(action.name());
        Self {
            name,
            search_text,
            usage_key: humanize_action_name(action.name()),
            action,
        }
    }

    fn from_intercepted(name: String, action: Box<dyn Action>) -> Self {
        Self {
            search_text: name.clone(),
            usage_key: name.clone(),
            name,
            action,
        }
    }

    /// Highlight offsets restricted to the displayed label. Matches inside the English alias part
    /// of [`Self::search_text`] fall outside the label and are dropped, so such a query filters
    /// correctly but shows no underline.
    fn highlights_within_name(&self, positions: &[usize]) -> Vec<usize> {
        positions
            .iter()
            .copied()
            .filter(|position| *position < self.name.len())
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CommandUsage {
    last_invoked: i64,
    invocations: u16,
}

#[derive(Default)]
struct QueryHistory {
    history: Option<VecDeque<String>>,
    cursor: Option<usize>,
    prefix: Option<String>,
}

impl QueryHistory {
    fn history(&mut self, cx: &App) -> &mut VecDeque<String> {
        self.history.get_or_insert_with(|| {
            CommandPaletteDB::global(cx)
                .list_recent_queries()
                .unwrap_or_default()
                .into_iter()
                .collect()
        })
    }

    fn add(&mut self, query: String, cx: &App) {
        if let Some(pos) = self.history(cx).iter().position(|h| h == &query) {
            self.history(cx).remove(pos);
        }
        self.history(cx).push_back(query);
        self.cursor = None;
        self.prefix = None;
    }

    fn validate_cursor(&mut self, current_query: &str, cx: &App) -> Option<usize> {
        if let Some(pos) = self.cursor {
            if self.history(cx).get(pos).map(|s| s.as_str()) != Some(current_query) {
                self.cursor = None;
                self.prefix = None;
            }
        }
        self.cursor
    }

    fn previous(&mut self, current_query: &str, cx: &App) -> Option<&str> {
        if self.validate_cursor(current_query, cx).is_none() {
            self.prefix = Some(current_query.to_string());
        }

        let prefix = self.prefix.clone().unwrap_or_default();
        let start_index = self.cursor.unwrap_or(self.history(cx).len());

        for i in (0..start_index).rev() {
            if self
                .history(cx)
                .get(i)
                .is_some_and(|e| e.starts_with(&prefix))
            {
                self.cursor = Some(i);
                return self.history(cx).get(i).map(|s| s.as_str());
            }
        }
        None
    }

    fn next(&mut self, current_query: &str, cx: &App) -> Option<&str> {
        let selected = self.validate_cursor(current_query, cx)?;
        let prefix = self.prefix.clone().unwrap_or_default();

        for i in (selected + 1)..self.history(cx).len() {
            if self
                .history(cx)
                .get(i)
                .is_some_and(|e| e.starts_with(&prefix))
            {
                self.cursor = Some(i);
                return self.history(cx).get(i).map(|s| s.as_str());
            }
        }
        None
    }

    fn reset_cursor(&mut self) {
        self.cursor = None;
        self.prefix = None;
    }

    fn is_navigating(&self) -> bool {
        self.cursor.is_some()
    }
}

impl Clone for Command {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            search_text: self.search_text.clone(),
            usage_key: self.usage_key.clone(),
            action: self.action.boxed_clone(),
        }
    }
}

impl CommandPaletteDelegate {
    fn new(
        command_palette: WeakEntity<CommandPalette>,
        workspace: WeakEntity<Workspace>,
        commands: Vec<Command>,
        previous_focus_handle: FocusHandle,
    ) -> Self {
        Self {
            command_palette,
            workspace,
            all_commands: commands.clone(),
            matches: vec![],
            commands,
            selected_ix: 0,
            previous_focus_handle,
            latest_query: String::new(),
            updating_matches: None,
            query_history: Default::default(),
        }
    }

    fn matches_updated(
        &mut self,
        query: String,
        mut commands: Vec<Command>,
        mut matches: Vec<StringMatch>,
        intercept_result: CommandInterceptResult,
        _: &mut Context<Picker<Self>>,
    ) {
        self.updating_matches.take();
        self.latest_query = query;

        let mut new_matches = Vec::new();

        for CommandInterceptItem {
            action,
            string,
            positions,
        } in intercept_result.results
        {
            if let Some(idx) = matches
                .iter()
                .position(|m| commands[m.candidate_id].action.partial_eq(&*action))
            {
                matches.remove(idx);
            }
            commands.push(Command::from_intercepted(string.clone(), action));
            new_matches.push(StringMatch {
                candidate_id: commands.len() - 1,
                string: string.into(),
                positions,
                score: 0.0,
            })
        }
        if !intercept_result.exclusive {
            new_matches.append(&mut matches);
        }
        self.commands = commands;
        self.matches = new_matches;
        if self.matches.is_empty() {
            self.selected_ix = 0;
        } else {
            self.selected_ix = cmp::min(self.selected_ix, self.matches.len() - 1);
        }
    }

    /// Hit count for each command in the palette.
    /// We only account for commands triggered directly via command palette and not by e.g. keystrokes because
    /// if a user already knows a keystroke for a command, they are unlikely to use a command palette to look for it.
    fn command_usage(&self, cx: &App) -> HashMap<String, CommandUsage> {
        if let Ok(commands) = CommandPaletteDB::global(cx).list_commands_used() {
            commands
                .into_iter()
                .map(|command| {
                    (
                        command.command_name,
                        CommandUsage {
                            last_invoked: command.last_invoked.unix_timestamp(),
                            invocations: command.invocations,
                        },
                    )
                })
                .collect()
        } else {
            HashMap::new()
        }
    }

    fn selected_command(&self) -> Option<&Command> {
        if self.matches.is_empty() {
            return None;
        }
        let action_ix = self
            .matches
            .get(self.selected_ix)
            .map(|m| m.candidate_id)
            .unwrap_or(self.selected_ix);
        // this gets called in headless tests where there are no commands loaded
        // so we need to return an Option here
        self.commands.get(action_ix)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn seed_history(&mut self, queries: &[&str]) {
        self.query_history.history = Some(queries.iter().map(|s| s.to_string()).collect());
    }
}

impl PickerDelegate for CommandPaletteDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "command palette"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "コマンドを実行...".into()
    }

    fn select_history(
        &mut self,
        direction: Direction,
        query: &str,
        _window: &mut Window,
        cx: &mut App,
    ) -> Option<String> {
        match direction {
            Direction::Up => {
                let should_use_history =
                    self.selected_ix == 0 || self.query_history.is_navigating();
                if should_use_history {
                    if let Some(query) = self
                        .query_history
                        .previous(query, cx)
                        .map(|s| s.to_string())
                    {
                        return Some(query);
                    }
                }
            }
            Direction::Down => {
                if self.query_history.is_navigating() {
                    if let Some(query) = self.query_history.next(query, cx).map(|s| s.to_string()) {
                        return Some(query);
                    } else {
                        let prefix = self.query_history.prefix.take().unwrap_or_default();
                        self.query_history.reset_cursor();
                        return Some(prefix);
                    }
                }
            }
        }
        None
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_ix
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) {
        self.selected_ix = ix;
    }

    fn update_matches(
        &mut self,
        mut query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> gpui::Task<()> {
        let settings = WorkspaceSettings::get_global(cx);
        if let Some(alias) = settings.command_aliases.get(&query) {
            query = alias.as_ref().to_owned();
        }

        let workspace = self.workspace.clone();

        let intercept_task = GlobalCommandPaletteInterceptor::intercept(&query, workspace, cx);

        let (mut tx, mut rx) = postage::dispatch::channel(1);

        let query_str = query.as_str();
        let is_zed_link = parse_zed_link(query_str, cx).is_some();

        let task = cx.background_spawn({
            let mut commands = self.all_commands.clone();
            let command_usage = self.command_usage(cx);
            let executor = cx.background_executor().clone();
            let query = normalize_action_query(query_str);
            let query_for_link = query_str.to_string();
            async move {
                commands.sort_by_key(|action| {
                    (
                        Reverse(command_usage.get(&action.usage_key).copied()),
                        action.name.clone(),
                    )
                });

                let candidates = commands
                    .iter()
                    .enumerate()
                    .map(|(ix, command)| StringMatchCandidate::new(ix, &command.search_text))
                    .collect::<Vec<_>>();

                let mut matches = fuzzy_nucleo::match_strings_async(
                    &candidates,
                    &query,
                    fuzzy_nucleo::Case::Smart,
                    fuzzy_nucleo::LengthPenalty::On,
                    10000,
                    &Default::default(),
                    executor,
                )
                .await;

                let used_commands = commands
                    .iter()
                    .take_while(|command| command_usage.contains_key(&command.usage_key))
                    .count();
                matches.sort_by_key(|string_match| {
                    if string_match.candidate_id < used_commands {
                        string_match.candidate_id
                    } else {
                        usize::MAX
                    }
                });

                let intercept_result = if is_zed_link {
                    CommandInterceptResult {
                        results: vec![CommandInterceptItem {
                            action: OpenZedUrl {
                                url: query_for_link.clone().into(),
                            }
                            .boxed_clone(),
                            string: query_for_link,
                            positions: vec![],
                        }],
                        exclusive: false,
                    }
                } else if let Some(task) = intercept_task {
                    task.await
                } else {
                    CommandInterceptResult::default()
                };

                tx.send((commands, matches, intercept_result))
                    .await
                    .log_err();
            }
        });

        self.updating_matches = Some((task, rx.clone()));

        cx.spawn_in(window, async move |picker, cx| {
            let Some((commands, matches, intercept_result)) = rx.recv().await else {
                return;
            };

            picker
                .update(cx, |picker, cx| {
                    picker
                        .delegate
                        .matches_updated(query, commands, matches, intercept_result, cx)
                })
                .ok();
        })
    }

    fn finalize_update_matches(
        &mut self,
        query: String,
        duration: Duration,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> bool {
        let Some((task, rx)) = self.updating_matches.take() else {
            return true;
        };

        match cx
            .foreground_executor()
            .block_with_timeout(duration, rx.clone().recv())
        {
            Ok(Some((commands, matches, interceptor_result))) => {
                self.matches_updated(query, commands, matches, interceptor_result, cx);
                true
            }
            _ => {
                self.updating_matches = Some((task, rx));
                false
            }
        }
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.command_palette
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .ok();
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if secondary {
            if self.matches.is_empty() {
                return;
            }
            let Some(selected_command) = self.selected_command() else {
                return;
            };
            let action_name = selected_command.action.name();
            let open_keymap = Box::new(zed_actions::ChangeKeybinding {
                action: action_name.to_string(),
            });
            window.dispatch_action(open_keymap, cx);
            self.dismissed(window, cx);
            return;
        }

        if self.matches.is_empty() {
            self.dismissed(window, cx);
            return;
        }

        if !self.latest_query.is_empty() {
            self.query_history.add(self.latest_query.clone(), cx);
            self.query_history.reset_cursor();
        }

        let action_ix = self.matches[self.selected_ix].candidate_id;
        let command = self.commands.swap_remove(action_ix);
        telemetry::event!(
            "Action Invoked",
            source = "command palette",
            action = command.usage_key
        );
        self.matches.clear();
        self.commands.clear();
        let command_name = command.usage_key.clone();
        let latest_query = self.latest_query.clone();
        let db = CommandPaletteDB::global(cx);
        cx.background_spawn(async move {
            db.write_command_invocation(command_name, latest_query)
                .await
        })
        .detach_and_log_err(cx);
        let action = command.action;
        window.focus(&self.previous_focus_handle, cx);
        self.dismissed(window, cx);
        window.dispatch_action(action, cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let matching_command = self.matches.get(ix)?;
        let command = self.commands.get(matching_command.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    h_flex()
                        .w_full()
                        .py_px()
                        .justify_between()
                        .child(HighlightedLabel::new(
                            command.name.clone(),
                            command.highlights_within_name(&matching_command.positions),
                        ))
                        .child(KeyBinding::for_action_in(
                            &*command.action,
                            &self.previous_focus_handle,
                            cx,
                        )),
                ),
        )
    }

    fn render_footer(
        &self,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        let selected_command = self.selected_command()?;
        let keybind =
            KeyBinding::for_action_in(&*selected_command.action, &self.previous_focus_handle, cx);

        let focus_handle = &self.previous_focus_handle;
        let keybinding_buttons = if keybind.has_binding(window) {
            Button::new("change", "キーバインドを変更…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        } else {
            Button::new("add", "キーバインドを追加…")
                .key_binding(
                    KeyBinding::for_action_in(&menu::SecondaryConfirm, focus_handle, cx)
                        .map(|kb| kb.size(rems_from_px(12_f32))),
                )
                .on_click(move |_, window, cx| {
                    window.dispatch_action(menu::SecondaryConfirm.boxed_clone(), cx);
                })
        };

        Some(
            h_flex()
                .w_full()
                .p_1p5()
                .gap_1()
                .justify_end()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(keybinding_buttons)
                .child(
                    Button::new("run-action", "実行")
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(|_, window, cx| {
                            window.dispatch_action(menu::Confirm.boxed_clone(), cx)
                        }),
                )
                .into_any(),
        )
    }
}

/// Japanese label for an action name, falling back to [`humanize_action_name`].
///
/// DESIGN.md §3 方式 2: アクション名から表示名を組み立てる箇所は
/// `humanize_action_name` を直接呼ばずにこちらを使うと日本語ラベルが効く。
/// **表示専用**。fuzzy 検索の候補文字列も要る場合は
/// [`localized_action_name_and_search_text`] を使うこと。
pub fn localized_action_name(action_name: &str) -> String {
    match i18n::tr_ctx_opt(i18n::context::ACTION, action_name) {
        Some(label) => label.to_string(),
        None => humanize_action_name(action_name),
    }
}

/// 表示ラベルと fuzzy 検索用の haystack の組。
///
/// haystack は日本語ラベルがあるときだけ「ラベル + 空白 + 英語 `humanize_action_name` 名」に
/// なり、日英どちらの綴りでもアクションが見つかる (DESIGN.md §3)。必ずラベルで始まるので、
/// haystack に対して得たマッチ位置のうちラベルの byte 長未満のものだけを残せば、そのまま表示
/// ラベルのハイライトに使える (英語エイリアス部分に当たったマッチはハイライトされない)。
pub fn localized_action_name_and_search_text(action_name: &str) -> (String, String) {
    let humanized = humanize_action_name(action_name);
    match i18n::tr_ctx_opt(i18n::context::ACTION, action_name) {
        Some(label) => (label.to_string(), format!("{label} {humanized}")),
        None => (humanized.clone(), humanized),
    }
}

pub fn humanize_action_name(name: &str) -> String {
    let chars = name.chars().collect::<Vec<_>>();
    let capacity = name.len() + chars.iter().filter(|c| c.is_uppercase()).count();
    let mut result = String::with_capacity(capacity);
    let mut index = 0;

    while index < chars.len() {
        let char = chars[index];
        if char == ':' {
            if result.ends_with(':') {
                result.push(' ');
            } else {
                result.push(':');
            }
            index += 1;
        } else if char == '_' {
            result.push(' ');
            index += 1;
        } else if char.is_uppercase() {
            let start = index;
            index += 1;
            while chars
                .get(index)
                .is_some_and(|next_char| next_char.is_uppercase())
            {
                index += 1;
            }

            let uppercase_run = &chars[start..index];
            if uppercase_run.len() > 1 {
                let split_before_last = chars
                    .get(index)
                    .is_some_and(|next_char| next_char.is_lowercase());
                let acronym_end = if split_before_last {
                    uppercase_run.len() - 1
                } else {
                    uppercase_run.len()
                };

                if acronym_end > 0 {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(&uppercase_run[..acronym_end]);
                }

                if split_before_last {
                    if !result.ends_with(' ') {
                        result.push(' ');
                    }
                    result.extend(uppercase_run[acronym_end].to_lowercase());
                }
            } else {
                if !result.ends_with(' ') {
                    result.push(' ');
                }
                result.extend(char.to_lowercase());
            }
        } else {
            result.push(char);
            index += 1;
        }
    }

    result
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use editor::Editor;
    use go_to_line::GoToLine;
    use gpui::{TestAppContext, VisualTestContext};
    use language::Point;
    use project::Project;
    use settings::KeymapFile;
    use workspace::{AppState, MultiWorkspace, Workspace};

    fn actions_catalog() -> &'static i18n_catalog::PoCatalog {
        i18n_catalog::test_support::bundled_catalog("ja")
    }

    /// Installs the bundled Japanese catalog for the current thread until the
    /// returned guard is dropped. Tests that want the Japanese labels call this;
    /// tests that want the upstream English behavior simply do not.
    #[must_use]
    fn install_actions_catalog() -> i18n::test_support::LocaleOverride {
        i18n::test_support::LocaleOverride::new(Some(actions_catalog()))
    }

    /// Every `(msgctxt, msgid, msgstr)` of `locales/ja/_actions.po`.
    fn actions_po_entries() -> Vec<(Option<String>, String, String)> {
        let (file, errors) = i18n_catalog::parse_po(
            "_actions.po",
            include_str!("../../../locales/ja/_actions.po"),
        );
        assert!(errors.is_empty(), "{errors:?}");
        file.entries
            .into_iter()
            .map(|entry| {
                let msgstr = entry.msgstr.into_iter().next().unwrap_or_default();
                (entry.context, entry.msgid, msgstr)
            })
            .collect()
    }

    #[test]
    fn test_humanize_action_name() {
        assert_eq!(
            humanize_action_name("editor::GoToDefinition"),
            "editor: go to definition"
        );
        assert_eq!(
            humanize_action_name("editor::Backspace"),
            "editor: backspace"
        );
        assert_eq!(
            humanize_action_name("go_to_line::Deploy"),
            "go to line: deploy"
        );
        assert_eq!(
            humanize_action_name("agent::OpenGlobalAGENTS.mdRules"),
            "agent: open global AGENTS.md rules"
        );
        assert_eq!(
            humanize_action_name("agent::OpenProjectAGENTS.mdRules"),
            "agent: open project AGENTS.md rules"
        );
        assert_eq!(humanize_action_name("editor::OpenURL"), "editor: open URL");
        assert_eq!(
            humanize_action_name("editor::OpenURLParser"),
            "editor: open URL parser"
        );
    }

    #[test]
    fn test_japanese_label_with_english_alias() {
        let _locale = install_actions_catalog();
        let command = Command::for_action(Box::new(editor::actions::GoToDefinition::default()));
        assert_eq!(command.name, "定義へ移動");
        assert_eq!(command.search_text, "定義へ移動 editor: go to definition");

        let alias_offset = command.name.len() + 1;
        assert_eq!(command.highlights_within_name(&[0, alias_offset]), vec![0]);
    }

    #[test]
    fn test_no_catalog_keeps_upstream_english_name() {
        // With no catalog installed (the default the test suite runs under), the
        // display name and search text must match plain upstream (no translation).
        let command = Command::for_action(Box::new(editor::actions::GoToDefinition::default()));
        assert_eq!(command.name, "editor: go to definition");
        assert_eq!(command.search_text, "editor: go to definition");
    }

    #[test]
    fn test_unmapped_action_keeps_humanized_name() {
        let _locale = install_actions_catalog();
        // `menu` 名前空間は _actions.po の対象外なので、対応表に載らないアクションの代表として使う
        let command = Command::for_action(Box::new(menu::SelectNext));
        assert_eq!(command.name, "menu: select next");
        assert_eq!(command.search_text, command.name);
        assert_eq!(i18n::tr_ctx_opt("action", "menu::SelectNext"), None);
    }

    /// `ja_labels::LABEL_TABLE` as it was at a97e20b, before the labels moved to
    /// `locales/ja/_actions.po`: one `action name<TAB>label` per line.
    const JA_LABELS_BEFORE: &str = include_str!("../test_data/ja_labels_before.tsv");

    #[test]
    fn test_actions_po_matches_pre_migration_ja_labels_table() {
        let _locale = install_actions_catalog();
        let before = JA_LABELS_BEFORE
            .lines()
            .map(|line| {
                line.split_once('\t')
                    .unwrap_or_else(|| panic!("fixture line without a tab: {line:?}"))
            })
            .collect::<Vec<_>>();
        let mismatches = before
            .iter()
            .filter(|(action_name, label)| localized_action_name(action_name) != *label)
            .map(|(action_name, label)| {
                format!(
                    "{action_name} shows {:?}, expected {label:?}",
                    localized_action_name(action_name)
                )
            })
            .collect::<Vec<_>>();
        assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));

        assert!(
            actions_po_entries()
                .iter()
                .all(|(context, ..)| context.as_deref() == Some(i18n::context::ACTION))
        );
    }

    #[test]
    fn test_normalize_query() {
        assert_eq!(
            normalize_action_query("editor: backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor:  backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor:    backspace"),
            "editor: backspace"
        );
        assert_eq!(
            normalize_action_query("editor::GoToDefinition"),
            "editor:GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("editor::::GoToDefinition"),
            "editor:GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("editor: :GoToDefinition"),
            "editor: :GoToDefinition"
        );
        assert_eq!(
            normalize_action_query("terminal_panel::Toggle"),
            "terminal panel:Toggle"
        );
        assert_eq!(
            normalize_action_query("project_panel::ToggleFocus"),
            "project panel:ToggleFocus"
        );
    }

    #[gpui::test]
    async fn test_command_palette(cx: &mut TestAppContext) {
        let _locale = install_actions_catalog();
        let app_state = init_test(cx);
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));
        db.clear_all().await.unwrap();
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.commands.len() > 5);
            let is_sorted =
                |actions: &[Command]| actions.windows(2).all(|pair| pair[0].name <= pair[1].name);
            assert!(is_sorted(&palette.delegate.commands));
        });

        cx.simulate_input("bcksp");

        palette.read_with(cx, |palette, _| {
            assert_eq!(
                palette.delegate.matches[0].string,
                "編集: 前の文字を削除 editor: backspace"
            );
        });

        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<CommandPalette>(cx).is_none());
            assert_eq!(editor.read(cx).text(cx), "ab")
        });

        // Add namespace filter, and redeploy the palette
        cx.update(|_window, cx| {
            CommandPaletteFilter::update_global(cx, |filter, _| {
                filter.hide_namespace("editor");
            });
        });

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("bcksp");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.matches.is_empty())
        });
    }

    /// Upstream (`de178dd`) behavior: with no catalog installed the palette must show
    /// and match on the plain English `humanize_action_name` text, unchanged.
    #[gpui::test]
    async fn test_command_palette_without_catalog_matches_upstream(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));
        db.clear_all().await.unwrap();
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("bcksp");

        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.matches[0].string, "editor: backspace");
        });
    }

    #[gpui::test]
    async fn test_commands_sorted_by_recency(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));

        db.write_command_invocation("editor: backspace", "")
            .await
            .unwrap();
        db.write_command_invocation("editor: backspace", "")
            .await
            .unwrap();
        db.write_command_invocation("go to line: toggle", "")
            .await
            .unwrap();
        db.set_last_invoked(100, "editor: backspace".to_string())
            .await
            .unwrap();
        db.set_last_invoked(200, "go to line: toggle".to_string())
            .await
            .unwrap();

        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-n");
        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.read_with(cx, |palette, _| {
            let names = palette
                .delegate
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>();
            assert_eq!(names[0], "go to line: toggle");
            assert_eq!(names[1], "editor: backspace");
            assert!(
                names[2..].windows(2).all(|pair| pair[0] <= pair[1]),
                "unused commands should stay alphabetical"
            );
        });
    }

    #[gpui::test]
    async fn test_usage_history_does_not_depend_on_the_display_language(cx: &mut TestAppContext) {
        let _locale = install_actions_catalog();
        let app_state = init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));
        // Recorded while the display language was English, or by upstream Zed.
        db.write_command_invocation("editor: backspace", "")
            .await
            .unwrap();

        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-n");
        cx.simulate_keystrokes("cmd-shift-p");
        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });
        palette.read_with(cx, |palette, _| {
            let first = &palette.delegate.commands[0];
            assert_eq!(first.action.name(), "editor::Backspace");
            assert_eq!(first.name, "編集: 前の文字を削除");
        });

        cx.simulate_input("bcksp");
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();

        let usage = db
            .list_commands_used()
            .unwrap()
            .into_iter()
            .map(|command| (command.command_name, command.invocations))
            .collect::<Vec<_>>();
        assert_eq!(usage, vec![("editor: backspace".to_string(), 2)]);
    }

    #[gpui::test]
    async fn test_used_commands_rank_above_unused_when_filtering(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let db = cx.update(|cx| persistence::CommandPaletteDB::global(cx));

        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-n");
        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace.active_modal::<CommandPalette>(cx).unwrap()
        });
        let picker = palette.read_with(cx, |palette, _| palette.picker.clone());

        palette.update_in(cx, |palette, window, cx| {
            palette.set_query("toggle", window, cx)
        });
        cx.run_until_parked();

        let (worst_match, second_worst_match, match_count) = picker.read_with(cx, |picker, _| {
            let matches = &picker.delegate.matches;
            (
                matches[matches.len() - 1].string.to_string(),
                matches[matches.len() - 2].string.to_string(),
                matches.len(),
            )
        });
        assert!(match_count > 2);

        db.write_command_invocation(second_worst_match.clone(), "")
            .await
            .unwrap();
        db.write_command_invocation(worst_match.clone(), "")
            .await
            .unwrap();
        db.set_last_invoked(100, second_worst_match.clone())
            .await
            .unwrap();
        db.set_last_invoked(200, worst_match.clone()).await.unwrap();

        palette.update_in(cx, |palette, window, cx| palette.set_query("", window, cx));
        cx.run_until_parked();
        palette.update_in(cx, |palette, window, cx| {
            palette.set_query("toggle", window, cx)
        });
        cx.run_until_parked();

        picker.read_with(cx, |picker, _| {
            let names = picker
                .delegate
                .matches
                .iter()
                .map(|string_match| string_match.string.to_string())
                .collect::<Vec<_>>();
            assert_eq!(names[0], worst_match);
            assert_eq!(names[1], second_worst_match);
            assert_eq!(names.len(), match_count);
        });
    }

    #[gpui::test]
    async fn test_selected_command_none_when_no_matches(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-shift-p");
        let picker = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("definitely-no-command-should-match-this");
        cx.background_executor.run_until_parked();

        picker.read_with(cx, |picker, _cx| {
            assert!(picker.delegate.matches.is_empty());
            assert!(picker.delegate.selected_command().is_none());
        });
    }
    #[gpui::test]
    async fn test_normalized_matches(cx: &mut TestAppContext) {
        let _locale = install_actions_catalog();
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        // Test normalize (trimming whitespace and double colons)
        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("Editor::    Backspace");
        palette.read_with(cx, |palette, _| {
            assert_eq!(
                palette.delegate.matches[0].string,
                "編集: 前の文字を削除 editor: backspace"
            );
        });
    }

    /// Upstream (`de178dd`) behavior: without a catalog, normalization still matches on
    /// the plain English name.
    #[gpui::test]
    async fn test_normalized_matches_without_catalog_matches_upstream(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let editor = cx.new_window_entity(|window, cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_text("abc", window, cx);
            editor
        });

        workspace.update_in(cx, |workspace, window, cx| {
            workspace.add_item_to_active_pane(Box::new(editor.clone()), None, true, window, cx);
            editor.update(cx, |editor, cx| window.focus(&editor.focus_handle(cx), cx))
        });

        cx.simulate_keystrokes("cmd-shift-p");

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        cx.simulate_input("Editor::    Backspace");
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.matches[0].string, "editor: backspace");
        });
    }

    #[gpui::test]
    async fn test_go_to_line(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        cx.simulate_keystrokes("cmd-n");

        let editor = workspace.update(cx, |workspace, cx| {
            workspace.active_item_as::<Editor>(cx).unwrap()
        });
        editor.update_in(cx, |editor, window, cx| {
            editor.set_text("1\n2\n3\n4\n5\n6\n", window, cx)
        });

        cx.simulate_keystrokes("cmd-shift-p");
        cx.simulate_input("go to line: Toggle");
        cx.simulate_keystrokes("enter");

        workspace.update(cx, |workspace, cx| {
            assert!(workspace.active_modal::<GoToLine>(cx).is_some())
        });

        cx.simulate_keystrokes("3 enter");

        editor.update_in(cx, |editor, window, cx| {
            assert!(editor.focus_handle(cx).is_focused(window));
            assert_eq!(
                editor
                    .selections
                    .last::<Point>(&editor.display_snapshot(cx))
                    .range()
                    .start,
                Point::new(2, 0)
            );
        });
    }

    #[gpui::test]
    async fn test_reopen_command_palette_over_another_modal(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace =
            multi_workspace.read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone());

        cx.simulate_keystrokes("cmd-n");

        for _ in 0..2 {
            cx.simulate_keystrokes("cmd-shift-p");
            cx.simulate_input("go to line: Toggle");
            cx.simulate_keystrokes("enter");

            workspace.update(cx, |workspace, cx| {
                assert!(workspace.active_modal::<GoToLine>(cx).is_some());
            });
        }
    }

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let app_state = AppState::test(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            go_to_line::init(cx);
            workspace::init(app_state.clone(), cx);
            init(cx);
            cx.bind_keys(KeymapFile::load_panic_on_failure(
                r#"[
                    {
                        "bindings": {
                            "cmd-n": "workspace::NewFile",
                            "enter": "menu::Confirm",
                            "cmd-shift-p": "command_palette::Toggle",
                            "up": "menu::SelectPrevious",
                            "down": "menu::SelectNext"
                        }
                    }
                ]"#,
                cx,
            ));
            app_state
        })
    }

    fn open_palette_with_history(
        workspace: &Entity<Workspace>,
        history: &[&str],
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<CommandPaletteDelegate>> {
        cx.simulate_keystrokes("cmd-shift-p");
        cx.run_until_parked();

        let palette = workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<CommandPalette>(cx)
                .unwrap()
                .read(cx)
                .picker
                .clone()
        });

        palette.update(cx, |palette, _cx| {
            palette.delegate.seed_history(history);
        });

        palette
    }

    #[gpui::test]
    async fn test_history_navigation_basic(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["backspace", "select all"], cx);

        // Query should be empty initially
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });

        // Press up - should load most recent query "select all"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press up again - should load "backspace"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspace");
        });

        // Press down - should go back to "select all"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press down again - should clear query (exit history mode)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });
    }

    #[gpui::test]
    async fn test_history_mode_exit_on_typing(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["backspace"], cx);

        // Press up to enter history mode
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspace");
        });

        // Type something - should append to the history query
        cx.simulate_input("x");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "backspacex");
        });
    }

    #[gpui::test]
    async fn test_history_navigation_with_suggestions(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["editor: close", "editor: open"], cx);

        // Open palette with a query that has multiple matches
        cx.simulate_input("editor");
        cx.background_executor.run_until_parked();

        // Should have multiple matches, selected_ix should be 0
        palette.read_with(cx, |palette, _| {
            assert!(palette.delegate.matches.len() > 1);
            assert_eq!(palette.delegate.selected_ix, 0);
        });

        // Press down - should navigate to next suggestion (not history)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.selected_ix, 1);
        });

        // Press up - should go back to first suggestion
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, _| {
            assert_eq!(palette.delegate.selected_ix, 0);
        });

        // Press up again at top - should enter history mode and show previous query
        // that matches the "editor" prefix
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "editor: open");
        });
    }

    #[gpui::test]
    async fn test_history_prefix_search(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(
            &workspace,
            &["open file", "select all", "select line", "backspace"],
            cx,
        );

        // Type "sel" as a prefix
        cx.simulate_input("sel");
        cx.background_executor.run_until_parked();

        // Press up - should get "select line" (most recent matching "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select line");
        });

        // Press up again - should get "select all" (next matching "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press up again - should stay at "select all" (no more matches for "sel")
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select all");
        });

        // Press down - should go back to "select line"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "select line");
        });

        // Press down again - should return to original prefix "sel"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "sel");
        });
    }

    #[gpui::test]
    async fn test_history_prefix_search_no_matches(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette =
            open_palette_with_history(&workspace, &["open file", "backspace", "select all"], cx);

        // Type "xyz" as a prefix that doesn't match anything
        cx.simulate_input("xyz");
        cx.background_executor.run_until_parked();

        // Press up - should stay at "xyz" (no matches)
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "xyz");
        });
    }

    #[gpui::test]
    async fn test_history_empty_prefix_searches_all(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let project = Project::test(app_state.fs.clone(), [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        let palette = open_palette_with_history(&workspace, &["alpha", "beta", "gamma"], cx);

        // With empty query, press up - should get "gamma" (most recent)
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "gamma");
        });

        // Press up - should get "beta"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "beta");
        });

        // Press up - should get "alpha"
        cx.simulate_keystrokes("up");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "alpha");
        });

        // Press down - should get "beta"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "beta");
        });

        // Press down - should get "gamma"
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "gamma");
        });

        // Press down - should return to empty string (exit history mode)
        cx.simulate_keystrokes("down");
        cx.background_executor.run_until_parked();
        palette.read_with(cx, |palette, cx| {
            assert_eq!(palette.query(cx), "");
        });
    }
}
