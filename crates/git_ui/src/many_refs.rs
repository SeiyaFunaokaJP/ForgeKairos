//! What keeps the Git UI usable on a repository with tens of thousands of branches and tags: a
//! searchable tag picker for the commit graph, and the suggestion to pack loose refs.

use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, Instant},
};

use collections::HashMap;
use fuzzy::StringMatchCandidate;
use git::repository::{LooseRefCount, TagInfo};
use gpui::{
    App, Context, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable, Global,
    PromptLevel, Render, SharedString, Subscription, Task, WeakEntity, Window, rems,
};
use notifications::status_toast::StatusToast;
use picker::{Picker, PickerDelegate};
use project::git_store::Repository;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::git_graph::{GitGraph, format_timestamp};

/// Below this many loose refs, packing them saves too little to be worth suggesting.
pub(crate) const LOOSE_REFS_SUGGESTION_THRESHOLD: usize = 1_000;

/// The graph's suggestion to pack the loose refs, or `None` when there are too few of them, or
/// when their number is not known (a host too old to count them).
pub(crate) fn loose_refs_banner_text(loose_refs: Option<LooseRefCount>) -> Option<String> {
    let loose_refs = loose_refs?;
    if loose_refs.count < LOOSE_REFS_SUGGESTION_THRESHOLD {
        return None;
    }
    let count = format_count(loose_refs.count);
    // Past the threshold the count is always plural.
    Some(if loose_refs.complete {
        i18n::t!(
            "{count} branches and tags are stored as separate files. Packing them makes Git faster."
        )
    } else {
        i18n::t!(
            "{count} or more branches and tags are stored as separate files. Packing them makes Git faster."
        )
    })
}

fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

fn pack_refs_success_message(elapsed: Duration) -> String {
    let seconds = format!("{:.1}", elapsed.as_secs_f64());
    i18n::t!("Packed the branches and tags ({seconds} s)")
}

/// Why packing failed, in words: an SSH host from before `pack-refs` joined its list of allowed
/// git commands refuses it outright.
fn pack_refs_failure_message(reason: &str) -> String {
    if git::repository::is_unknown_git_subcommand_refusal(reason, "pack-refs") {
        i18n::t!("The Zed server on the remote host is too old to pack branches and tags.")
            .to_string()
    } else {
        i18n::t!("Could not pack the branches and tags\n{reason}")
    }
}

/// Where packing a repository's refs stands. Kept per repository in a global, so that the
/// banner's button and `git::PackRefs` from the palette cannot start a second `git pack-refs`
/// while the first one is still being confirmed or running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PackRefsStage {
    Confirming,
    Packing,
}

#[derive(Default)]
pub(crate) struct PackRefsInProgress {
    stages: HashMap<EntityId, PackRefsStage>,
    /// The loose ref count each repository had when its refs were packed successfully. Until a
    /// different count arrives (the ref scan queued behind the packing, or over SSH the host's
    /// next update, which can come after other updates still carrying the old count), the
    /// banner would offer to pack refs that are already packed.
    counts_before_packing: HashMap<EntityId, LooseRefCount>,
}

impl Global for PackRefsInProgress {}

pub(crate) fn pack_refs_stage(repository: &Entity<Repository>, cx: &App) -> Option<PackRefsStage> {
    cx.try_global::<PackRefsInProgress>()?
        .stages
        .get(&repository.entity_id())
        .copied()
}

/// Whether the repository's refs were just packed and no count taken since has arrived:
/// `Some(true)` while the count is still the one from before packing, `Some(false)` once a new
/// one has replaced it (and the record can go), `None` without a record.
pub(crate) fn awaiting_recount(repository: &Entity<Repository>, cx: &App) -> Option<bool> {
    let count_before_packing = cx
        .try_global::<PackRefsInProgress>()?
        .counts_before_packing
        .get(&repository.entity_id())?;
    Some(repository.read(cx).loose_refs.as_ref() == Some(count_before_packing))
}

/// The count the banner has to wait out after packing: the one from before, if packing worked
/// and no count taken since has arrived yet. After a failure the old count is still true.
fn count_to_hide_after_packing(
    packed: bool,
    count_before_packing: Option<LooseRefCount>,
    count_now: Option<LooseRefCount>,
) -> Option<LooseRefCount> {
    count_before_packing.filter(|before| packed && count_now == Some(*before))
}

pub(crate) fn remember_count_before_packing(
    repository: EntityId,
    count_before_packing: LooseRefCount,
    cx: &mut App,
) {
    cx.default_global::<PackRefsInProgress>()
        .counts_before_packing
        .insert(repository, count_before_packing);
}

/// Drops the record [`awaiting_recount`] reads once a new count has arrived, so that the count
/// coming back to the same number later does not hide the banner again.
pub(crate) fn forget_count_before_packing(repository: &Entity<Repository>, cx: &mut App) {
    if awaiting_recount(repository, cx) == Some(false) {
        cx.default_global::<PackRefsInProgress>()
            .counts_before_packing
            .remove(&repository.entity_id());
    }
}

pub(crate) fn set_pack_refs_stage(
    repository: EntityId,
    stage: Option<PackRefsStage>,
    cx: &mut App,
) {
    let in_progress = &mut cx.default_global::<PackRefsInProgress>().stages;
    match stage {
        Some(stage) => {
            in_progress.insert(repository, stage);
        }
        None => {
            in_progress.remove(&repository);
        }
    }
}

pub(crate) fn pack_refs_button_label(stage: Option<PackRefsStage>) -> &'static str {
    match stage {
        Some(PackRefsStage::Packing) => i18n::t!("Packing…"),
        Some(PackRefsStage::Confirming) | None => i18n::tc!("pack refs", "Pack"),
    }
}

fn pack_refs_confirmation_message(loose_refs: Option<LooseRefCount>) -> String {
    match loose_refs {
        Some(LooseRefCount {
            count,
            complete: true,
        }) => i18n::tn!(
            count,
            "Packs the {formatted} ref (branch or tag) stored as a separate file into one file.",
            "Packs the {formatted} refs (branches and tags) stored as separate files into one file.",
            formatted = format_count(count)
        ),
        Some(LooseRefCount {
            count,
            complete: false,
        }) => {
            let count = format_count(count);
            i18n::t!(
                "Packs the {count} or more refs (branches and tags) stored as separate files into one file."
            )
        }
        // `git::PackRefs` from the palette, on a host too old to count them.
        None => {
            i18n::t!("Packs the refs (branches and tags) stored as separate files into one file.")
                .to_string()
        }
    }
}

fn pack_refs_confirmation_detail() -> &'static str {
    i18n::t!(
        "With tens of thousands of them this takes about a minute, and other Git operations wait meanwhile. No data changes."
    )
}

/// Asks before packing: it does not change any ref, but it holds up every other git operation of
/// the repository for as long as it runs, which is about a minute for tens of thousands of refs.
pub(crate) fn confirm_and_pack_refs(
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    if pack_refs_stage(&repository, cx).is_some() {
        return;
    }
    let repository_id = repository.entity_id();
    set_pack_refs_stage(repository_id, Some(PackRefsStage::Confirming), cx);
    cx.default_global::<PackRefsInProgress>()
        .counts_before_packing
        .remove(&repository_id);
    let message = pack_refs_confirmation_message(repository.read(cx).loose_refs);
    let answer = window.prompt(
        PromptLevel::Info,
        &message,
        Some(pack_refs_confirmation_detail()),
        &[i18n::tc!("pack refs", "Pack"), i18n::t!("Cancel")],
        cx,
    );
    cx.spawn(async move |cx| {
        if answer.await.ok() != Some(0) {
            cx.update(|cx| set_pack_refs_stage(repository_id, None, cx));
            return;
        }
        let (packing, count_before_packing) = cx.update(|cx| {
            set_pack_refs_stage(repository_id, Some(PackRefsStage::Packing), cx);
            let count_before_packing = repository.read(cx).loose_refs;
            (
                pack_refs(repository.clone(), workspace, cx),
                count_before_packing,
            )
        });
        let packed = packing.await;
        cx.update(|cx| {
            set_pack_refs_stage(repository_id, None, cx);
            if let Some(count) = count_to_hide_after_packing(
                packed,
                count_before_packing,
                repository.read(cx).loose_refs,
            ) {
                remember_count_before_packing(repository_id, count, cx);
            }
        });
    })
    .detach();
}

/// Runs `git pack-refs --all` and says how it went in a toast; resolves to whether it succeeded.
fn pack_refs(
    repository: Entity<Repository>,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Task<bool> {
    let started_at = Instant::now();
    let receiver = repository.update(cx, |repository, cx| repository.pack_refs(cx));
    cx.spawn(async move |cx| {
        let result = receiver.await;
        let (message, failed) = match result {
            Ok(Ok(output)) if output.succeeded() => {
                (pack_refs_success_message(started_at.elapsed()), false)
            }
            Ok(Ok(output)) => (pack_refs_failure_message(&output.failure_message()), true),
            Ok(Err(error)) => (pack_refs_failure_message(&format!("{error:#}")), true),
            Err(_) => return false,
        };
        workspace
            .update(cx, |workspace, cx| {
                let toast = StatusToast::new(message, cx, move |this, _cx| {
                    this.icon(
                        Icon::new(if failed {
                            IconName::XCircle
                        } else {
                            IconName::Check
                        })
                        .size(IconSize::Small)
                        .color(if failed {
                            Color::Error
                        } else {
                            Color::Success
                        }),
                    )
                    .dismiss_button(true)
                });
                workspace.toggle_status_toast(toast, cx);
            })
            .log_err();
        !failed
    })
}

/// `git::PackRefs` from the command palette: packs the active repository's refs.
pub(crate) fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &git::PackRefs, window, cx| {
        let Some(repository) = workspace.project().read(cx).active_repository(cx) else {
            let toast = StatusToast::new(
                i18n::t!("There is no active repository."),
                cx,
                |this, _cx| {
                    this.icon(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .dismiss_button(true)
                },
            );
            workspace.toggle_status_toast(toast, cx);
            return;
        };
        let workspace = cx.weak_entity();
        confirm_and_pack_refs(repository, workspace, window, cx);
    });
}

/// The graph toolbar's tag list: every tag, newest first, searchable by name. Picking one moves
/// the graph to the commit it names.
pub(crate) struct GraphTagPicker {
    pub(crate) picker: Entity<Picker<GraphTagPickerDelegate>>,
    _subscriptions: Vec<Subscription>,
}

impl GraphTagPicker {
    /// `tags` is what the graph has in hand, since the graph itself cannot be read while it is
    /// the one opening the picker. A list still loading, or reloaded while the picker is open,
    /// arrives through the observation of `graph`.
    pub(crate) fn new(
        graph: Entity<GitGraph>,
        tags: Option<Arc<[TagInfo]>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let delegate = GraphTagPickerDelegate::new(graph.downgrade(), tags);
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(40.)));
        let subscriptions = vec![
            cx.subscribe(&picker, |_, _, _: &DismissEvent, cx| cx.emit(DismissEvent)),
            cx.observe_in(&graph, window, |this, graph, window, cx| {
                let tags = graph.read(cx).tags();
                this.picker.update(cx, |picker, cx| {
                    if picker.delegate.set_tags(tags) {
                        picker.refresh(window, cx);
                    }
                });
            }),
        ];
        Self {
            picker,
            _subscriptions: subscriptions,
        }
    }
}

impl ModalView for GraphTagPicker {}
impl EventEmitter<DismissEvent> for GraphTagPicker {}

impl Focusable for GraphTagPicker {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for GraphTagPicker {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
struct TagMatch {
    tag_index: usize,
    positions: Vec<usize>,
}

pub(crate) struct GraphTagPickerDelegate {
    graph: WeakEntity<GitGraph>,
    tags: Option<Arc<[TagInfo]>>,
    /// Built once per tag list, on the first search, rather than on every keystroke.
    candidates: Option<Arc<[StringMatchCandidate]>>,
    matches: Vec<TagMatch>,
    selected_index: usize,
}

impl GraphTagPickerDelegate {
    fn new(graph: WeakEntity<GitGraph>, tags: Option<Arc<[TagInfo]>>) -> Self {
        Self {
            graph,
            tags,
            candidates: None,
            matches: Vec::new(),
            selected_index: 0,
        }
    }

    /// Returns whether the list changed and the matches have to be recomputed.
    fn set_tags(&mut self, tags: Option<Arc<[TagInfo]>>) -> bool {
        let unchanged = match (&self.tags, &tags) {
            (Some(current), Some(new)) => Arc::ptr_eq(current, new),
            (None, None) => true,
            _ => false,
        };
        if unchanged {
            return false;
        }
        self.tags = tags;
        self.candidates = None;
        true
    }

    fn tag_at(&self, match_index: usize) -> Option<&TagInfo> {
        let tag_match = self.matches.get(match_index)?;
        self.tags.as_ref()?.get(tag_match.tag_index)
    }
}

/// Finds the tags whose names match `query`; an empty query keeps every tag, newest first.
async fn match_tags(
    tags: &[TagInfo],
    candidates: &[StringMatchCandidate],
    query: &str,
    executor: gpui::BackgroundExecutor,
) -> Vec<TagMatch> {
    if query.is_empty() {
        return (0..tags.len())
            .map(|tag_index| TagMatch {
                tag_index,
                positions: Vec::new(),
            })
            .collect();
    }
    fuzzy::match_strings(
        candidates,
        query,
        true,
        true,
        candidates.len(),
        &AtomicBool::new(false),
        executor,
    )
    .await
    .into_iter()
    .map(|string_match| TagMatch {
        tag_index: string_match.candidate_id,
        positions: string_match.positions,
    })
    .collect()
}

fn tag_candidates(tags: &[TagInfo]) -> Arc<[StringMatchCandidate]> {
    tags.iter()
        .enumerate()
        .map(|(index, tag)| StringMatchCandidate::new(index, &tag.name))
        .collect()
}

impl PickerDelegate for GraphTagPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "graph-tag-picker"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        i18n::t!("Search tags… (newest first)").into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some(match &self.tags {
            None => i18n::t!("Loading tags…").into(),
            Some(tags) if tags.is_empty() => i18n::t!("No tags").into(),
            Some(_) => i18n::t!("No matching tags").into(),
        })
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let Some(tags) = self.tags.clone() else {
            self.matches.clear();
            self.selected_index = 0;
            return Task::ready(());
        };
        let candidates = self.candidates.clone();
        cx.spawn_in(window, async move |picker, cx| {
            let executor = cx.background_executor().clone();
            let (candidates, matches) = cx
                .background_spawn({
                    let tags = tags.clone();
                    async move {
                        let candidates = candidates.unwrap_or_else(|| tag_candidates(&tags));
                        let matches = match_tags(&tags, &candidates, &query, executor).await;
                        (candidates, matches)
                    }
                })
                .await;
            picker
                .update(cx, |picker, _| {
                    let delegate = &mut picker.delegate;
                    // The list may have been reloaded while this search ran; its results would
                    // point into the old one.
                    if !delegate
                        .tags
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &tags))
                    {
                        return;
                    }
                    delegate.candidates = Some(candidates);
                    delegate.matches = matches;
                    delegate.selected_index = delegate
                        .selected_index
                        .min(delegate.matches.len().saturating_sub(1));
                })
                .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(commit_sha) = self
            .tag_at(self.selected_index)
            .map(|tag| tag.commit_sha.clone())
        else {
            return;
        };
        self.graph
            .update(cx, |graph, cx| {
                graph.select_commit_by_sha(commit_sha.as_str(), cx);
            })
            .log_err();
        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        cx.emit(DismissEvent);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let tag_match = self.matches.get(ix)?;
        let tag = self.tags.as_ref()?.get(tag_match.tag_index)?;
        let date = tag.created_at_unix.map(format_timestamp);
        let subject = tag
            .subject
            .as_ref()
            .or(tag.commit_subject.as_ref())
            .cloned();
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .start_slot(
                    Icon::new(IconName::Bookmark)
                        .size(IconSize::Small)
                        .color(Color::Created),
                )
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(div().flex_none().child(HighlightedLabel::new(
                            tag.name.clone(),
                            tag_match.positions.clone(),
                        )))
                        .children(date.map(|date| {
                            Label::new(date)
                                .size(LabelSize::Small)
                                .color(Color::Muted)
                                .into_any_element()
                        }))
                        .children(subject.map(|subject| {
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    Label::new(subject)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted)
                                        .truncate(),
                                )
                                .into_any_element()
                        })),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loose_refs_banner_text() {
        assert_eq!(
            loose_refs_banner_text(None),
            None,
            "unknown counts say nothing"
        );
        assert_eq!(
            loose_refs_banner_text(Some(LooseRefCount {
                count: LOOSE_REFS_SUGGESTION_THRESHOLD - 1,
                complete: true,
            })),
            None
        );
        assert_eq!(
            loose_refs_banner_text(Some(LooseRefCount {
                count: LOOSE_REFS_SUGGESTION_THRESHOLD,
                complete: true,
            }))
            .as_deref(),
            Some(
                "1,000 branches and tags are stored as separate files. Packing them makes Git faster."
            )
        );
        assert_eq!(
            loose_refs_banner_text(Some(LooseRefCount {
                count: 10_000,
                complete: false,
            }))
            .as_deref(),
            Some(
                "10,000 or more branches and tags are stored as separate files. Packing them makes Git faster."
            )
        );
    }

    #[test]
    fn test_format_count() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(52_001), "52,001");
        assert_eq!(format_count(1_234_567), "1,234,567");
    }

    #[test]
    fn test_pack_refs_failure_message() {
        assert_eq!(
            pack_refs_failure_message("unsupported git command: pack-refs"),
            "The Zed server on the remote host is too old to pack branches and tags."
        );
        assert_eq!(
            pack_refs_failure_message("fatal: Unable to create '.git/packed-refs.lock'"),
            "Could not pack the branches and tags\nfatal: Unable to create '.git/packed-refs.lock'"
        );
    }

    #[test]
    fn test_pack_refs_confirmation_message() {
        assert_eq!(
            pack_refs_confirmation_message(Some(LooseRefCount {
                count: 52_001,
                complete: true,
            })),
            "Packs the 52,001 refs (branches and tags) stored as separate files into one file."
        );
        assert_eq!(
            pack_refs_confirmation_message(Some(LooseRefCount {
                count: 10_000,
                complete: false,
            })),
            "Packs the 10,000 or more refs (branches and tags) stored as separate files into one file."
        );
        assert_eq!(
            pack_refs_confirmation_message(None),
            "Packs the refs (branches and tags) stored as separate files into one file."
        );
    }

    #[test]
    fn test_count_to_hide_after_packing() {
        let before = LooseRefCount {
            count: 52_001,
            complete: true,
        };
        let after = LooseRefCount {
            count: 2,
            complete: true,
        };
        assert_eq!(
            count_to_hide_after_packing(true, Some(before), Some(before)),
            Some(before),
            "packed, and the new count is still on its way"
        );
        assert_eq!(
            count_to_hide_after_packing(false, Some(before), Some(before)),
            None,
            "a failed packing left the refs loose"
        );
        assert_eq!(
            count_to_hide_after_packing(true, Some(before), Some(after)),
            None,
            "the new count is already in"
        );
        assert_eq!(count_to_hide_after_packing(true, None, None), None);
    }

    #[test]
    fn test_pack_refs_button_label() {
        assert_eq!(pack_refs_button_label(None), "Pack");
        assert_eq!(
            pack_refs_button_label(Some(PackRefsStage::Confirming)),
            "Pack"
        );
        assert_eq!(
            pack_refs_button_label(Some(PackRefsStage::Packing)),
            "Packing…"
        );
    }

    #[test]
    fn test_pack_refs_success_message() {
        assert_eq!(
            pack_refs_success_message(Duration::from_millis(1_340)),
            "Packed the branches and tags (1.3 s)"
        );
    }

    fn tag(name: &str) -> TagInfo {
        TagInfo {
            name: name.into(),
            commit_sha: "0".repeat(40),
            created_at_unix: None,
            subject: None,
            commit_subject: None,
            annotated: false,
        }
    }

    #[gpui::test]
    async fn test_match_tags_searches_every_tag(cx: &mut gpui::TestAppContext) {
        // Far more than the 50 the old menu listed, so that an old tag is still findable.
        let tags: Vec<TagInfo> = (0..5_000)
            .map(|number| tag(&format!("build-{number:05}")))
            .chain([tag("v1.0.0")])
            .collect();
        let candidates = tag_candidates(&tags);

        let everything = match_tags(&tags, &candidates, "", cx.background_executor.clone()).await;
        assert_eq!(everything.len(), tags.len());
        assert_eq!(
            everything
                .iter()
                .map(|m| m.tag_index)
                .take(3)
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
            "an empty query keeps git's newest-first order"
        );

        let found = match_tags(&tags, &candidates, "v1.0", cx.background_executor.clone()).await;
        assert_eq!(
            found.first().map(|m| tags[m.tag_index].name.as_str()),
            Some("v1.0.0")
        );
    }
}
