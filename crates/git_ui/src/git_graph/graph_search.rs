use super::{GitGraph, QueryState};
use editor::{Editor, EditorEvent};
use git::{
    Oid,
    commit_search::{
        CommitSearchCondition, CommitSearchConditionKind, CommitSearchEnvironment,
        CommitSearchPlan, MergeFilter, ParsedCommitSearchQuery, parse_commit_search_query,
        plan_commit_search, remove_commit_search_condition, set_commit_search_condition,
    },
    repository::{LogSource, SearchCommitArgs},
};
use gpui::{
    AnyElement, App, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, SharedString,
    Subscription, Task, TextStyleRefinement, WeakEntity, Window, prelude::*, px,
};
use project::git_store::Repository;
use time::{OffsetDateTime, UtcOffset};
use ui::{ButtonStyle, PopoverMenu, Tooltip, prelude::*};
use workspace::item::ItemEvent;

pub(super) const SEARCH_RESULT_LIMIT: usize = git::commit_search::COMMIT_SEARCH_RESULT_LIMIT;

/// Translate it with `i18n::tr` where it is shown.
pub(super) const SEARCH_PLACEHOLDER: &str =
    i18n::msg!("Search commits… (e.g. author:alice file:src/ since:2w)");

/// Examples of the search box's conditions and what they find, in English; the translations
/// show the same conditions with their words in that language, which the box also reads.
const SEARCH_SYNTAX_EXAMPLES: [(&str, &str); 8] = [
    (
        i18n::msg!("author:alice"),
        i18n::msg!("The author's name or email address contains \"alice\""),
    ),
    (
        i18n::msg!("tag:v1.2"),
        i18n::msg!("Commits that tags with \"v1.2\" in their name point to"),
    ),
    (
        i18n::msg!("branch:feature"),
        i18n::msg!("Commits on feature that the default branch does not have"),
    ),
    (
        i18n::msg!("file:src/"),
        i18n::msg!("Commits that changed that file or folder (* for patterns)"),
    ),
    (
        i18n::msg!("since:2w  until:2026-09-01"),
        i18n::msg!("A period (3d, 2w, 1m, 1y, a date, \"2026-09-01 12:00\")"),
    ),
    (
        i18n::msg!("change:foo"),
        i18n::msg!("Added or removed lines contain \"foo\" (slow)"),
    ),
    (
        i18n::msg!("merge:no"),
        i18n::msg!("Leaves out merge commits (yes, no, only)"),
    ),
    (
        "sha:1a2b3c",
        i18n::msg!("Commits whose SHA starts like this"),
    ),
];

/// What the last search was, for the line under the search box: its conditions as chips, how far
/// it got, and why it could not run.
#[derive(Default)]
pub(super) struct SearchOutcome {
    /// The query the rest is about; the chips' ranges point into it.
    pub(super) query: SharedString,
    pub(super) conditions: Vec<CommitSearchCondition>,
    /// The words that are not conditions, which the subjects are highlighted with.
    pub(super) message_text: SharedString,
    pub(super) problems: Vec<SharedString>,
    pub(super) finished: bool,
    pub(super) truncated: bool,
    pub(super) stopped: bool,
    /// A `タグ:` condition waits for the tag list; the search runs again once it is in.
    pub(super) waiting_for_tags: bool,
}

impl SearchOutcome {
    fn new(query: SharedString, parsed: &ParsedCommitSearchQuery) -> Self {
        Self {
            query,
            conditions: parsed.conditions.clone(),
            message_text: parsed.text.clone().into(),
            ..Self::default()
        }
    }
}

fn local_now() -> OffsetDateTime {
    OffsetDateTime::now_utc().to_offset(UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC))
}

/// What a search result count line says.
pub(super) fn search_count_text(outcome: &SearchOutcome, match_count: usize) -> Option<String> {
    if !outcome.problems.is_empty() || outcome.waiting_for_tags {
        return None;
    }
    Some(if !outcome.finished {
        i18n::tn!(
            match_count,
            "Searching… {count} commit",
            "Searching… {count} commits"
        )
    } else if outcome.stopped {
        i18n::tn!(
            match_count,
            "Stopped at {count} commit.",
            "Stopped at {count} commits."
        )
    } else if outcome.truncated {
        i18n::tn!(
            match_count,
            "Cut off at {count} commit. Add conditions to narrow the search.",
            "Cut off at {count} commits. Add conditions to narrow the search."
        )
    } else if match_count == 0 {
        i18n::t!("No commits match. Try fewer conditions.").to_string()
    } else {
        i18n::tn!(match_count, "{count} commit", "{count} commits")
    })
}

impl GitGraph {
    pub(super) fn search(&mut self, query: SharedString, cx: &mut Context<Self>) {
        let Some(repository) = self.get_repository(cx) else {
            return;
        };

        self.search_state.matches.clear();
        self.search_state.selected_index = None;
        self.search_state.editor.update(cx, |editor, _cx| {
            editor.set_text_style_refinement(Default::default());
        });

        let parsed = parse_commit_search_query(query.as_str());
        let plan = self.plan_search(&parsed, &repository, cx);
        self.search_state.outcome = SearchOutcome::new(query.clone(), &parsed);
        let search_args = match plan {
            CommitSearchPlan::Empty => {
                self.search_state.outcome = SearchOutcome::default();
                self.search_state.state = QueryState::Empty;
                cx.notify();
                return;
            }
            CommitSearchPlan::NeedsTags => {
                self.search_state.outcome.waiting_for_tags = true;
                if self._tags_task.is_none() {
                    self.load_tags(cx);
                }
                self.search_state.state = QueryState::Empty;
                cx.notify();
                return;
            }
            CommitSearchPlan::Invalid(problems) => {
                self.search_state.outcome.problems =
                    problems.into_iter().map(SharedString::from).collect();
                self.search_state.outcome.finished = true;
                self.search_state.state = QueryState::Empty;
                self.show_search_found_nothing(cx);
                cx.notify();
                return;
            }
            CommitSearchPlan::Search(search_args) => search_args,
        };

        if let Some(commits) = self.tagged_commits_in_graph(&search_args, &repository, cx) {
            self.add_search_matches(commits, cx);
            self.search_state.outcome.finished = true;
            if self.search_state.matches.is_empty() {
                self.show_search_found_nothing(cx);
            }
            self.search_state.state = QueryState::Confirmed((query, Task::ready(())));
            cx.emit(ItemEvent::Edit);
            cx.notify();
            return;
        }

        let (request_tx, request_rx) = async_channel::unbounded::<Oid>();
        let backend_search = repository.update(cx, |repository, cx| {
            repository.search_commits(self.log_source.clone(), search_args, request_tx, cx)
        });

        let search_task = cx.spawn(async move |this, cx| {
            let mut truncated = false;
            while let Ok(first_oid) = request_rx.recv().await {
                let mut pending_oids = vec![first_oid];
                while let Ok(oid) = request_rx.try_recv() {
                    pending_oids.push(oid);
                }

                let reached_limit = this
                    .update(cx, |this, cx| {
                        this.add_search_matches(pending_oids, cx);
                        cx.notify();
                        this.search_state.matches.len() >= SEARCH_RESULT_LIMIT
                    })
                    .unwrap_or(true);
                if reached_limit {
                    truncated = true;
                    break;
                }
            }

            // Past the limit the search is dropped, which stops git, rather than waited for.
            let result = if truncated {
                drop(backend_search);
                Ok(())
            } else {
                backend_search.await
            };

            this.update(cx, |this, cx| {
                let outcome = &mut this.search_state.outcome;
                outcome.finished = true;
                outcome.truncated = truncated;
                if let Err(error) = result {
                    outcome.problems.push(format!("{error:#}").into());
                }
                if this.search_state.matches.is_empty() {
                    this.show_search_found_nothing(cx);
                }
                cx.notify();
            })
            .ok();
        });

        self.search_state.state = QueryState::Confirmed((query, search_task));
        cx.emit(ItemEvent::Edit);
        cx.notify();
    }

    /// The 中止 button: drops the running search, which stops git, and keeps what it found.
    pub(super) fn stop_search(&mut self, cx: &mut Context<Self>) {
        if let QueryState::Confirmed((_, task)) = &mut self.search_state.state
            && !task.is_ready()
        {
            *task = Task::ready(());
            self.search_state.outcome.finished = true;
            self.search_state.outcome.stopped = true;
            cx.notify();
        }
    }

    /// Has the search run again once the graph or the search options change, unless it was
    /// stopped: a stopped search, maybe a slow `変更:` one, runs again only when the box is
    /// confirmed again.
    pub(super) fn search_again_after_change(&mut self) {
        if !self.search_state.outcome.stopped {
            self.search_state.state.next_state();
        }
    }

    fn plan_search(
        &self,
        parsed: &ParsedCommitSearchQuery,
        repository: &Entity<Repository>,
        cx: &App,
    ) -> CommitSearchPlan {
        let snapshot = repository.read(cx).snapshot();
        plan_commit_search(
            parsed,
            &CommitSearchEnvironment {
                now: local_now(),
                case_sensitive: self.search_state.case_sensitive,
                branches: &snapshot.branch_list,
                tags: self.tags.as_deref(),
                log_source: &self.log_source,
            },
        )
    }

    /// The answer to a search that is nothing but tags, straight from the graph's rows, when the
    /// graph holds every commit. `None` sends the search to git instead.
    fn tagged_commits_in_graph(
        &self,
        search_args: &SearchCommitArgs,
        repository: &Entity<Repository>,
        cx: &mut App,
    ) -> Option<Vec<Oid>> {
        if !search_args.is_only_commit_set() || !matches!(self.log_source, LogSource::All) {
            return None;
        }
        let commits = search_args.commits.as_ref()?;
        let graph_is_loading = repository.update(cx, |repository, cx| {
            repository
                .graph_data(self.log_source.clone(), self.log_order, 0..0, cx)
                .is_loading
        });
        if graph_is_loading {
            return None;
        }
        let repository = repository.read(cx);
        let graph = repository.get_graph_data(self.log_source.clone(), self.log_order)?;
        let mut rows = commits
            .iter()
            .filter_map(|oid| {
                graph
                    .commit_oid_to_index
                    .get(oid)
                    .map(|index| (*index, *oid))
            })
            .collect::<Vec<_>>();
        rows.sort_by_key(|(index, _)| *index);
        Some(rows.into_iter().map(|(_, oid)| oid).collect())
    }

    fn add_search_matches(&mut self, oids: Vec<Oid>, cx: &mut Context<Self>) {
        let Some(&first_oid) = oids.first() else {
            return;
        };
        if self.search_state.selected_index.is_none() {
            self.search_state.selected_index = Some(0);
            self.select_commit_by_sha(first_oid, cx);
        }
        self.search_state.matches.extend(oids);
        self.search_state.matches.truncate(SEARCH_RESULT_LIMIT);
    }

    fn show_search_found_nothing(&self, cx: &mut Context<Self>) {
        self.search_state.editor.update(cx, |editor, cx| {
            editor.set_text_style_refinement(TextStyleRefinement {
                color: Some(Color::Error.color(cx)),
                ..Default::default()
            });
        });
    }

    /// Runs the search again once the tag list a `タグ:` condition waited for is in.
    pub(super) fn search_again_if_waiting_for_tags(&mut self, cx: &mut Context<Self>) {
        if self.search_state.outcome.waiting_for_tags {
            let query = self.search_state.outcome.query.clone();
            self.search(query, cx);
        }
    }

    fn set_search_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search_state.editor.update(cx, |editor, cx| {
            editor.set_text(text, window, cx);
        });
    }

    /// The chip's ×: searches again without that condition.
    pub(super) fn remove_search_condition(
        &mut self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(condition) = self.search_state.outcome.conditions.get(index) else {
            return;
        };
        // The box may have been edited since this search ran: the condition comes out of what
        // is typed there now, rather than the search's query coming back over it.
        let current = self.search_state.editor.read(cx).text(cx);
        let query = parse_commit_search_query(&current)
            .conditions
            .iter()
            .find(|typed| typed.kind == condition.kind && typed.value == condition.value)
            .map_or_else(
                || current.clone(),
                |typed| remove_commit_search_condition(&current, typed.range.clone()),
            );
        self.set_search_text(query.clone(), window, cx);
        self.search(query.into(), cx);
    }

    /// Writes one condition of the advanced search form into the search box, replacing the ones
    /// of the same kind. `false` for a value the box cannot hold.
    pub(super) fn write_search_condition(
        &mut self,
        kind: CommitSearchConditionKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = self.search_state.editor.read(cx).text(cx);
        let existing = parse_commit_search_query(&current)
            .conditions
            .into_iter()
            .filter(|condition| condition.kind == kind)
            .map(|condition| condition.value)
            .collect::<Vec<_>>();
        let value = value.trim();
        let unchanged = if value.is_empty() {
            existing.is_empty()
        } else {
            existing == [value]
        };
        if unchanged {
            return true;
        }
        let Some(query) = set_commit_search_condition(&current, kind, value) else {
            return false;
        };
        if query != current {
            self.set_search_text(query, window, cx);
        }
        true
    }

    pub(super) fn run_search_from_box(&mut self, cx: &mut Context<Self>) {
        let query = self.search_state.editor.read(cx).text(cx).into();
        self.search(query, cx);
    }

    pub(super) fn render_search_tools(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let graph = cx.entity().downgrade();
        h_flex()
            .gap_0p5()
            .child(
                PopoverMenu::new("git-graph-advanced-search")
                    .menu(move |window, cx| {
                        let graph = graph.upgrade()?;
                        Some(cx.new(|cx| GraphSearchForm::new(graph, window, cx)))
                    })
                    .anchor(gpui::Anchor::TopRight)
                    .trigger_with_tooltip(
                        Button::new(
                            "git-graph-advanced-search-trigger",
                            i18n::tc!("commit search", "Advanced"),
                        )
                        .label_size(LabelSize::Small),
                        Tooltip::text(i18n::t!(
                            "Search by conditions such as the author, a tag or a period"
                        )),
                    ),
            )
            .child(
                IconButton::new("git-graph-search-help", IconName::Info)
                    .shape(ui::IconButtonShape::Square)
                    .icon_size(IconSize::Small)
                    .icon_color(Color::Muted)
                    .tooltip(Tooltip::element(|_window, cx| {
                        render_search_syntax_help(cx)
                    })),
            )
    }

    /// The line under the search box: the conditions in effect as chips, the count, and the
    /// reasons a search could not run.
    pub(super) fn render_search_details(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let outcome = &self.search_state.outcome;
        if outcome.query.is_empty() && outcome.problems.is_empty() {
            return None;
        }
        let count_text = search_count_text(outcome, self.search_state.matches.len());
        let found_nothing = outcome.finished && self.search_state.matches.is_empty();

        Some(
            v_flex()
                .debug_selector(|| "git-graph-search-details".into())
                .w_full()
                .px_1p5()
                .py_1()
                .gap_1()
                .border_b_1()
                .border_color(cx.theme().colors().border_variant)
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1()
                        .children(outcome.conditions.iter().enumerate().map(
                            |(index, condition)| {
                                h_flex()
                                    .id(("git-graph-search-condition", index))
                                    .pl_1p5()
                                    .gap_0p5()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(cx.theme().colors().border)
                                    .bg(cx.theme().colors().element_background)
                                    .child(
                                        Label::new(format!(
                                            "{}: {}",
                                            condition.kind.label(),
                                            condition.value
                                        ))
                                        .size(LabelSize::XSmall),
                                    )
                                    .child(
                                        IconButton::new(
                                            ("git-graph-search-condition-remove", index),
                                            IconName::Close,
                                        )
                                        .icon_size(IconSize::XSmall)
                                        .tooltip(Tooltip::text(i18n::t!(
                                            "Remove this condition and search again"
                                        )))
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                this.remove_search_condition(index, window, cx);
                                            }),
                                        ),
                                    )
                            },
                        ))
                        .when_some(count_text, |this, text| {
                            this.child(Label::new(text).size(LabelSize::Small).color(
                                if found_nothing {
                                    Color::Warning
                                } else {
                                    Color::Muted
                                },
                            ))
                        })
                        .when(!outcome.finished && outcome.problems.is_empty(), |this| {
                            this.child(
                                Button::new(
                                    "git-graph-search-stop",
                                    i18n::tc!("commit search", "Stop"),
                                )
                                .label_size(LabelSize::Small)
                                .tooltip(Tooltip::text(i18n::t!(
                                    "Stops the search. The commits found so far stay"
                                )))
                                .on_click(cx.listener(
                                    |this, _, _window, cx| {
                                        this.stop_search(cx);
                                    },
                                )),
                            )
                        })
                        .when(outcome.waiting_for_tags, |this| {
                            this.child(
                                Label::new(i18n::t!("Loading the tag list…"))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
                .children(outcome.problems.iter().map(|problem| {
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(IconName::Warning)
                                .size(IconSize::XSmall)
                                .color(Color::Warning),
                        )
                        .child(Label::new(problem.clone()).size(LabelSize::Small))
                }))
                .into_any_element(),
        )
    }
}

fn render_search_syntax_help(cx: &App) -> AnyElement {
    v_flex()
        .gap_1()
        .child(Label::new(i18n::t!("How to Write a Search")).size(LabelSize::Small))
        .children(SEARCH_SYNTAX_EXAMPLES.iter().map(|(example, meaning)| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .w(px(180.))
                        .child(
                            Label::new(i18n::tr(example))
                                .size(LabelSize::Small)
                                .buffer_font(cx),
                        ),
                )
                .child(
                    Label::new(i18n::tr(meaning))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        }))
        .child(
            Label::new(i18n::t!(
                "Separate conditions with spaces to find the commits that match all of them. Put a value that contains spaces in \"…\"."
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .into_any_element()
}

struct SearchFormField {
    kind: CommitSearchConditionKind,
    label: &'static str,
    editor: Entity<Editor>,
    /// What the field held when it last wrote to the box. Filling a field in (the form opening
    /// on the box's conditions) is not the user changing it, and must not rewrite the box: with
    /// two authors there, the field shows only the first.
    last_value: String,
}

/// How many tags or branches the form suggests under their field at a time.
const SUGGESTION_LIMIT: usize = 6;

/// The 詳細 popover: one field per condition, each writing its condition into the search box as
/// it is typed, so that the box shows how to write it.
pub(super) struct GraphSearchForm {
    graph: WeakEntity<GitGraph>,
    focus_handle: FocusHandle,
    fields: Vec<SearchFormField>,
    merges: MergeFilter,
    tag_names: Vec<SharedString>,
    branch_names: Vec<SharedString>,
    suggesting_for: Option<CommitSearchConditionKind>,
    note: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl GraphSearchForm {
    pub(super) fn new(
        graph: Entity<GitGraph>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        use CommitSearchConditionKind as Kind;

        let graph_state = graph.read(cx);
        let query = graph_state.search_state.editor.read(cx).text(cx);
        let parsed = parse_commit_search_query(&query);
        let value_of = |kind: Kind| {
            parsed
                .conditions
                .iter()
                .find(|condition| condition.kind == kind)
                .map(|condition| condition.value.clone())
                .unwrap_or_default()
        };
        let merges = parsed
            .conditions
            .iter()
            .rev()
            .find(|condition| condition.kind == Kind::Merge)
            .and_then(|condition| MergeFilter::parse(&condition.value))
            .unwrap_or_default();
        let tag_names = graph_state
            .tags
            .iter()
            .flat_map(|tags| tags.iter())
            .map(|tag| SharedString::from(tag.name.clone()))
            .collect();
        let branch_names = graph_state
            .get_repository(cx)
            .map(|repository| {
                repository
                    .read(cx)
                    .snapshot()
                    .branch_list
                    .iter()
                    .map(|branch| SharedString::from(branch.name().to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let mut fields = Vec::new();
        let mut subscriptions = Vec::new();
        for (kind, label, placeholder) in [
            (
                Kind::Author,
                i18n::tc!("commit search", "Author"),
                i18n::t!("Part of a name or email address"),
            ),
            (
                Kind::Tag,
                i18n::tc!("commit search", "Tag"),
                i18n::t!("e.g. v1.2"),
            ),
            (
                Kind::Branch,
                i18n::tc!("commit search", "Branch"),
                i18n::t!("e.g. feature/login"),
            ),
            (
                Kind::Path,
                i18n::tc!("commit search", "File"),
                i18n::t!("e.g. src/ or *.rs"),
            ),
            (
                Kind::Since,
                i18n::tc!("commit search", "Since"),
                i18n::t!("e.g. 2w, 2026-09-01"),
            ),
            (
                Kind::Until,
                i18n::tc!("commit search", "Until"),
                i18n::t!("e.g. 2026-09-30"),
            ),
            (
                Kind::Change,
                i18n::tc!("commit search", "Changed Text"),
                i18n::t!("Text of added or removed lines (slow)"),
            ),
        ] {
            let initial_value = value_of(kind);
            let editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text(placeholder, window, cx);
                editor.set_text(initial_value.clone(), window, cx);
                editor
            });
            subscriptions.push(cx.subscribe_in(
                &editor,
                window,
                move |this: &mut Self, editor, event: &EditorEvent, window, cx| match event {
                    EditorEvent::BufferEdited => {
                        let value = editor.read(cx).text(cx);
                        let Some(field) = this.fields.iter_mut().find(|field| field.kind == kind)
                        else {
                            return;
                        };
                        if field.last_value == value {
                            return;
                        }
                        field.last_value = value.clone();
                        this.write(kind, &value, window, cx);
                    }
                    EditorEvent::Focused => {
                        this.suggesting_for = Some(kind);
                        cx.notify();
                    }
                    _ => {}
                },
            ));
            fields.push(SearchFormField {
                kind,
                label,
                editor,
                last_value: initial_value,
            });
        }

        let focus_handle = cx.focus_handle();
        // The popover focuses the form itself; typing starts in the first field.
        subscriptions.push(cx.on_focus(&focus_handle, window, |this, window, cx| {
            if let Some(field) = this.fields.first() {
                window.focus(&field.editor.focus_handle(cx), cx);
            }
        }));

        Self {
            graph: graph.downgrade(),
            focus_handle,
            fields,
            merges,
            tag_names,
            branch_names,
            suggesting_for: None,
            note: None,
            _subscriptions: subscriptions,
        }
    }

    fn write(
        &mut self,
        kind: CommitSearchConditionKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let written = self
            .graph
            .update(cx, |graph, cx| {
                graph.write_search_condition(kind, value, window, cx)
            })
            .unwrap_or(false);
        self.note = (!written).then(|| {
            let label = kind.label();
            SharedString::from(i18n::t!(
                "A value for \"{label}\" cannot contain both spaces and \"."
            ))
        });
        cx.notify();
    }

    fn set_merges(&mut self, merges: MergeFilter, window: &mut Window, cx: &mut Context<Self>) {
        self.merges = merges;
        let value = match merges {
            MergeFilter::Include => "",
            other => other.query_value(),
        };
        self.write(CommitSearchConditionKind::Merge, value, window, cx);
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        self.graph
            .update(cx, |graph, cx| graph.run_search_from_box(cx))
            .ok();
        cx.emit(DismissEvent);
    }

    fn suggestions(&self, kind: CommitSearchConditionKind, cx: &App) -> (Vec<SharedString>, usize) {
        let names = match kind {
            CommitSearchConditionKind::Tag => &self.tag_names,
            CommitSearchConditionKind::Branch => &self.branch_names,
            _ => return (Vec::new(), 0),
        };
        let typed = self
            .fields
            .iter()
            .find(|field| field.kind == kind)
            .map(|field| field.editor.read(cx).text(cx).to_lowercase())
            .unwrap_or_default();
        let matching = names
            .iter()
            .filter(|name| {
                let name = name.to_lowercase();
                name != typed && name.contains(&typed)
            })
            .collect::<Vec<_>>();
        let hidden = matching.len().saturating_sub(SUGGESTION_LIMIT);
        (
            matching
                .into_iter()
                .take(SUGGESTION_LIMIT)
                .cloned()
                .collect(),
            hidden,
        )
    }

    fn pick_suggestion(
        &mut self,
        kind: CommitSearchConditionKind,
        name: SharedString,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.fields.iter().find(|field| field.kind == kind) {
            field.editor.update(cx, |editor, cx| {
                editor.set_text(name.to_string(), window, cx);
            });
        }
    }

    fn render_suggestions(
        &self,
        kind: CommitSearchConditionKind,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.suggesting_for != Some(kind) {
            return None;
        }
        let (names, hidden) = self.suggestions(kind, cx);
        if names.is_empty() {
            return None;
        }
        Some(
            v_flex()
                .ml(px(76.))
                .children(names.into_iter().enumerate().map(|(index, name)| {
                    h_flex()
                        .id(("git-graph-search-suggestion", index))
                        .px_1()
                        .rounded_sm()
                        .hover(|style| style.bg(cx.theme().colors().element_hover))
                        .cursor_pointer()
                        .child(Label::new(name.clone()).size(LabelSize::Small))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.pick_suggestion(kind, name.clone(), window, cx);
                        }))
                }))
                .when(hidden > 0, |this| {
                    this.child(
                        Label::new(i18n::t!("{hidden} more (keep typing to narrow them down)"))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                })
                .into_any_element(),
        )
    }

    #[cfg(test)]
    pub(super) fn set_field_for_test(
        &mut self,
        kind: CommitSearchConditionKind,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(field) = self.fields.iter().find(|field| field.kind == kind) {
            field
                .editor
                .update(cx, |editor, cx| editor.set_text(value, window, cx));
        }
    }

    #[cfg(test)]
    pub(super) fn set_merges_for_test(
        &mut self,
        merges: MergeFilter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_merges(merges, window, cx);
    }

    #[cfg(test)]
    pub(super) fn field_text_for_test(&self, kind: CommitSearchConditionKind, cx: &App) -> String {
        self.fields
            .iter()
            .find(|field| field.kind == kind)
            .map(|field| field.editor.read(cx).text(cx))
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(super) fn suggestions_for_test(
        &self,
        kind: CommitSearchConditionKind,
        cx: &App,
    ) -> Vec<SharedString> {
        self.suggestions(kind, cx).0
    }
}

impl EventEmitter<DismissEvent> for GraphSearchForm {}

impl Focusable for GraphSearchForm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for GraphSearchForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border_variant;
        let field_background = cx.theme().colors().editor_background;
        let mut field_rows = Vec::new();
        for field in &self.fields {
            let suggestions = self.render_suggestions(field.kind, cx);
            field_rows.push(
                v_flex()
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                div()
                                    .w(px(72.))
                                    .child(Label::new(field.label).size(LabelSize::Small)),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .h_7()
                                    .px_1()
                                    .flex()
                                    .items_center()
                                    .border_1()
                                    .border_color(border_color)
                                    .rounded_sm()
                                    .bg(field_background)
                                    .child(field.editor.clone()),
                            ),
                    )
                    .children(suggestions),
            );
        }
        let merge_buttons = [
            (
                "git-graph-search-merges-include",
                i18n::tc!("merge commits", "Include"),
                MergeFilter::Include,
            ),
            (
                "git-graph-search-merges-exclude",
                i18n::tc!("merge commits", "Exclude"),
                MergeFilter::Exclude,
            ),
            (
                "git-graph-search-merges-only",
                i18n::tc!("merge commits", "Only"),
                MergeFilter::Only,
            ),
        ]
        .map(|(id, label, merges)| {
            Button::new(id, label)
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(self.merges == merges)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.set_merges(merges, window, cx);
                }))
        });

        v_flex()
            .key_context("GitGraphSearchForm")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                this.run_search(cx);
            }))
            .on_action(cx.listener(|_, _: &menu::Cancel, _window, cx| {
                cx.emit(DismissEvent);
            }))
            .on_mouse_down_out(cx.listener(|_, _, _window, cx| cx.emit(DismissEvent)))
            .w(px(420.))
            .p_2()
            .gap_1()
            .elevation_2(cx)
            .child(Label::new(i18n::t!("Search by Conditions")).size(LabelSize::Small))
            .child(
                Label::new(i18n::t!(
                    "The conditions you enter are written into the search box."
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .children(field_rows)
            .child(
                h_flex()
                    .gap_1()
                    .child(div().w(px(72.)).child(
                        Label::new(i18n::tc!("commit search", "Merges")).size(LabelSize::Small),
                    ))
                    .children(merge_buttons),
            )
            .when_some(self.note.clone(), |this, note| {
                this.child(
                    Label::new(note)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .child(
                        Button::new("git-graph-search-form-close", i18n::t!("Close"))
                            .label_size(LabelSize::Small)
                            .on_click(cx.listener(|_, _, _window, cx| cx.emit(DismissEvent))),
                    )
                    .child(
                        Button::new("git-graph-search-form-run", i18n::t!("Search"))
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, _window, cx| this.run_search(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use git::commit_search::{DateBound, parse_commit_search_date};

    /// The examples the search box shows, as the box itself would read them.
    fn shown_examples() -> Vec<(String, Vec<CommitSearchConditionKind>)> {
        use CommitSearchConditionKind as Kind;
        let expected_kinds = [
            vec![Kind::Author],
            vec![Kind::Tag],
            vec![Kind::Branch],
            vec![Kind::Path],
            vec![Kind::Since, Kind::Until],
            vec![Kind::Change],
            vec![Kind::Merge],
            vec![Kind::Sha],
        ];
        let placeholder = i18n::tr(SEARCH_PLACEHOLDER);
        let placeholder_example = placeholder
            .split_once('(')
            .and_then(|(_, rest)| rest.rsplit_once(')'))
            .and_then(|(inside, _)| inside.split_once(char::is_whitespace))
            .map(|(_, conditions)| conditions.to_string())
            .expect("the placeholder carries an example in brackets");
        SEARCH_SYNTAX_EXAMPLES
            .iter()
            .map(|(example, _)| i18n::tr(example).to_string())
            .zip(expected_kinds)
            .chain([(
                placeholder_example,
                vec![Kind::Author, Kind::Path, Kind::Since],
            )])
            .collect()
    }

    fn assert_examples_parse() {
        let now = OffsetDateTime::now_utc();
        for (example, expected_kinds) in shown_examples() {
            let parsed = parse_commit_search_query(&example);
            assert_eq!(parsed.text, "", "{example:?} is all conditions");
            assert_eq!(
                parsed
                    .conditions
                    .iter()
                    .map(|condition| condition.kind)
                    .collect::<Vec<_>>(),
                expected_kinds,
                "{example:?}"
            );
            for condition in &parsed.conditions {
                let value = condition.value.as_str();
                let readable = match condition.kind {
                    CommitSearchConditionKind::Since => {
                        parse_commit_search_date(value, DateBound::Since, now).is_some()
                    }
                    CommitSearchConditionKind::Until => {
                        parse_commit_search_date(value, DateBound::Until, now).is_some()
                    }
                    CommitSearchConditionKind::Merge => MergeFilter::parse(value).is_some(),
                    CommitSearchConditionKind::Sha => {
                        value.bytes().all(|byte| byte.is_ascii_hexdigit())
                    }
                    _ => true,
                };
                assert!(readable, "{example:?}: {value:?} cannot be read");
            }
        }
    }

    /// The syntax the box shows is syntax the box reads: in English with the English keywords, and
    /// in each translation with the keywords the translation shows.
    #[test]
    fn test_shown_search_examples_are_readable() {
        assert_examples_parse();
        assert!(
            shown_examples()
                .iter()
                .any(|(example, _)| example.starts_with("author:")),
            "the English examples use the English keywords"
        );

        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert!(
            shown_examples()
                .iter()
                .any(|(example, _)| example.starts_with("著者:")),
            "the Japanese examples use the Japanese keywords"
        );
        assert_examples_parse();
    }
}
