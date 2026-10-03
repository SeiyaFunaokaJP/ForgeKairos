use std::{path::Path, sync::atomic::Ordering, time::Duration};

use anyhow::{Context as _, Result};
use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EntityId, Font, Task, WeakEntity, Window,
};
use language::{Buffer, Capability};
use project::{Project, ProjectEntryId, ProjectPath};
use settings::Settings as _;
use ui::{Icon, IconName, Label, LabelCommon as _, SharedString, prelude::*};
use workspace::{
    ItemId, ItemNavHistory, ItemSettings, Pane, ToolbarItemLocation, Workspace, WorkspaceId,
    delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{
        HighlightedText, Item, ItemBufferKind, ItemEvent, ProjectItem, SaveOptions,
        SerializableItem, TabContentParams,
    },
    searchable::SearchableItemHandle,
};

use crate::{
    TabularDataPreviewPane, grid_styles::key_value_store, parser::TabularFormat,
    table_data_engine::StoredTableView, tabular_data_settings::TabularDataSettings,
};

/// `Editor` registers `language::Buffer` as its project item, and the registry keys its
/// builders by the `TypeId` of that item, so reusing `Buffer` here would replace the
/// editor's builder and open *every* buffer as a grid. Wrapping the buffer in a type of
/// our own keeps the two registrations independent.
pub struct TabularBuffer {
    buffer: Entity<Buffer>,
}

impl project::ProjectItem for TabularBuffer {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<Result<Entity<Self>>>> {
        // Called for every file the user opens, so bail out before touching the project.
        match path.path.extension() {
            Some(extension) => TabularFormat::from_extension(extension)?,
            // A file opened on its own is a worktree whose root is the file, so the path
            // within it is empty and the name to read the extension from is the worktree's.
            None if path.path.is_empty() => {
                let worktree = project.read(cx).worktree_for_id(path.worktree_id, cx)?;
                let extension = Path::new(worktree.read(cx).root_name_str())
                    .extension()?
                    .to_str()?;
                TabularFormat::from_extension(extension)?
            }
            None => return None,
        };
        if !TabularDataSettings::get_global(cx).open_as_grid {
            return None;
        }

        let buffer = project.update(cx, |project, cx| project.open_buffer(path.clone(), cx));
        Some(cx.spawn(async move |cx| {
            let buffer = buffer.await?;
            Ok(cx.new(|_| TabularBuffer { buffer }))
        }))
    }

    fn entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        project::ProjectItem::entry_id(self.buffer.read(cx), cx)
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        project::ProjectItem::project_path(self.buffer.read(cx), cx)
    }

    /// This wrapper only exists as a registry key; the grid reports its underlying
    /// [`Buffer`] from [`Item::for_each_project_item`], and that is what the pane's
    /// unsaved-changes prompts actually consult. Reading the buffer here is impossible
    /// anyway, because the trait method is handed no context.
    fn is_dirty(&self) -> bool {
        false
    }
}

impl ProjectItem for TabularDataPreviewPane {
    type Item = TabularBuffer;

    fn for_project_item(
        project: Entity<Project>,
        _pane: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        let buffer = item.read(cx).buffer.clone();
        // The editor is never rendered; it is the shared text model that the grid reads
        // from and writes through, so that saving and dirty tracking behave like a tab
        // opened in the text editor.
        let editor = cx.new(|cx| Editor::for_buffer(buffer, Some(project), window, cx));
        Self::build(editor, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        error: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, error, window, cx))
    }
}

impl TabularDataPreviewPane {
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor_state().editor
    }

    pub(crate) fn buffer(&self, cx: &App) -> Option<Entity<Buffer>> {
        self.editor().read(cx).buffer().read(cx).as_singleton()
    }

    pub(crate) fn entry_id(&self, cx: &App) -> Option<ProjectEntryId> {
        project::ProjectItem::entry_id(self.buffer(cx)?.read(cx), cx)
    }

    /// Follows the text editor's tab so that a grid tab and an editor tab of the
    /// same file show the same git color.
    fn tab_label_color(&self, params: &TabContentParams, cx: &App) -> Color {
        if ItemSettings::get_global(cx).git_status {
            editor::items::editor_tab_label_color(self.editor().read(cx), params, cx)
        } else {
            params.text_color()
        }
    }
}

///// The filters and sorting kept between sessions /////

/// How long a change waits before it is written back, so that clicking through
/// a column's values does not write the stored row on every click.
pub(crate) const FILTER_SAVE_DEBOUNCE: Duration = Duration::from_millis(500);

/// Prefix of the key value store row a file's filters and sorting live in.
const FILTER_KEY_PREFIX: &str = "tabular_data_filters:";

pub(crate) fn filters_key_for_editor(editor: &Entity<Editor>, cx: &App) -> Option<String> {
    let buffer = editor.read(cx).buffer().read(cx).as_singleton()?;
    let path = buffer.read(cx).file()?.as_local()?.abs_path(cx);
    Some(format!("{FILTER_KEY_PREFIX}{}", path.to_string_lossy()))
}

impl TabularDataPreviewPane {
    /// Called from [`TabularDataPreviewPane::apply_filter_sort`], which every
    /// change to the filters or the sorting - and every reparse - goes through.
    ///
    /// Two grids open on the same file do not share their filters, unlike their
    /// colors: narrowing one of two views of a table to look something up is
    /// the point of having two, so the last one to change wins the stored row.
    pub(crate) fn on_view_changed(&mut self, cx: &mut Context<Self>) {
        self.announce_restored_view(cx);

        if !self.engine.stored_view.load_started {
            self.engine.stored_view.load_started = true;
            self.engine.stored_view.key = filters_key_for_editor(self.editor(), cx);
            self.load_stored_view(cx);
            return;
        }
        self.save_stored_view(cx);
    }

    /// "Save as" gave the buffer a new path, and the view is stored under the
    /// path rather than in the file.
    pub(crate) fn on_filters_path_changed(&mut self, cx: &mut Context<Self>) {
        let key = filters_key_for_editor(self.editor(), cx);
        if self.engine.stored_view.key == key {
            return;
        }
        let old_key = std::mem::replace(&mut self.engine.stored_view.key, key);
        // Cancels any write still waiting to go to the old row, which the
        // delete below is about to remove.
        self.engine
            .stored_view
            .save_generation
            .fetch_add(1, Ordering::SeqCst);

        if let Some(old_key) = old_key
            && let Some(store) = key_value_store(cx)
        {
            cx.background_spawn(async move {
                if let Err(err) = store.delete_kvp(old_key).await {
                    log::error!(
                        "tabular data preview: failed to drop the old path's filters: {err}"
                    );
                }
            })
            .detach();
        }

        // Nothing is known about the new row - it may even hold a view stored
        // for that path in an earlier session - so the write goes out whether
        // or not the view itself has changed.
        self.engine.forget_where_the_view_was_saved();
        self.save_stored_view(cx);
    }

    fn announce_restored_view(&mut self, cx: &mut Context<Self>) {
        if !self.engine.take_restored_notice() {
            return;
        }
        // Everything the stored view named may have been on columns the file no
        // longer has, in which case nothing was restored after all.
        let mut message = match (
            self.engine.has_any_filters(),
            self.engine.applied_sorting.is_some(),
        ) {
            (true, true) => i18n::t!("Restored the previous filters and sorting").to_owned(),
            (true, false) => i18n::t!("Restored the previous filters").to_owned(),
            (false, true) => i18n::t!("Restored the previous sorting").to_owned(),
            (false, false) => return,
        };
        // A table too big to count here says only that it was restored: the
        // number is worth a lot less than the frame it would cost.
        if let Some(hidden) = self.engine.hidden_row_count()
            && hidden > 0
        {
            message.push_str(&i18n::tn!(
                hidden,
                " ({count} row hidden)",
                " ({count} rows hidden)"
            ));
        }
        self.set_status_message(message, cx);
    }

    fn load_stored_view(&mut self, cx: &mut Context<Self>) {
        let store = key_value_store(cx);
        let (Some(key), Some(store)) = (self.engine.stored_view.key.clone(), store) else {
            // Nothing to read from, so nothing is in flight either and the
            // first change may be written (or dropped) right away.
            self.engine
                .mark_stored_view_saved(&StoredTableView::default());
            return;
        };

        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    let stored = match store.read_kvp(&key) {
                        Ok(stored) => stored?,
                        Err(err) => {
                            log::error!(
                                "tabular data preview: failed to read the stored filters: {err}"
                            );
                            return None;
                        }
                    };
                    match serde_json::from_str::<StoredTableView>(&stored) {
                        Ok(view) => Some(view),
                        Err(err) => {
                            log::error!(
                                "tabular data preview: failed to parse the stored filters: {err}"
                            );
                            None
                        }
                    }
                })
                .await;

            this.update(cx, |this, cx| {
                let loaded = loaded.unwrap_or_default();
                this.engine.mark_stored_view_saved(&loaded);
                this.engine.restore_stored_view(loaded);
                this.apply_filter_sort(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Writes the view back after a pause, and only when it differs from what
    /// the store already holds: every reparse comes through here too, so a
    /// keystroke in an unfiltered table must not schedule a write.
    ///
    /// The task holds its own copy and is detached, so closing the tab inside
    /// the debounce still stores what the user narrowed the table to.
    fn save_stored_view(&mut self, cx: &mut Context<Self>) {
        let Some(current) = self.engine.take_unsaved_view() else {
            return;
        };

        let payload = if current.is_empty() {
            None
        } else {
            match serde_json::to_string(&current) {
                Ok(payload) => Some(payload),
                Err(err) => {
                    log::error!("tabular data preview: failed to serialize the filters: {err}");
                    return;
                }
            }
        };

        let store = key_value_store(cx);
        let (Some(key), Some(store)) = (self.engine.stored_view.key.clone(), store) else {
            return;
        };
        let generation = self
            .engine
            .stored_view
            .save_generation
            .fetch_add(1, Ordering::SeqCst)
            + 1;
        let save_generation = self.engine.stored_view.save_generation.clone();
        let executor = cx.background_executor().clone();

        cx.background_spawn(async move {
            executor.timer(FILTER_SAVE_DEBOUNCE).await;
            if save_generation.load(Ordering::SeqCst) != generation {
                return;
            }
            let written = match payload {
                Some(payload) => store.write_kvp(key, payload).await,
                None => store.delete_kvp(key).await,
            };
            if let Err(err) = written {
                log::error!("tabular data preview: failed to store the filters: {err}");
            }
        })
        .detach();
    }
}

/// A pane recognizes an already-open file by its project entry, so one pane can never hold
/// both the text tab and the grid tab of the same file: `Pane::add_item` would match the
/// entry and merely re-activate the tab that is already there. Opening "in this pane" is
/// therefore defined as a swap - the tab on that entry is replaced in place, keeping its
/// position - and unsaved edits survive it because the buffer belongs to the project rather
/// than to either view.
pub(crate) fn activate_or_replace_in_pane<T: Item>(
    pane: Entity<Pane>,
    entry_id: Option<ProjectEntryId>,
    focus: bool,
    build_item: impl FnOnce(&mut Window, &mut App) -> Entity<T>,
    window: &mut Window,
    cx: &mut App,
) {
    let occupant = entry_id.and_then(|entry_id| {
        pane.read(cx)
            .items()
            .enumerate()
            .find(|(_, item)| {
                item.buffer_kind(cx) == ItemBufferKind::Singleton
                    && item.project_entry_ids(cx).first() == Some(&entry_id)
            })
            .map(|(index, item)| (index, item.boxed_clone()))
    });

    let Some((index, occupant)) = occupant else {
        let item = build_item(window, cx);
        pane.update(cx, |pane, cx| {
            pane.add_item(Box::new(item), focus, focus, None, window, cx);
        });
        return;
    };

    if occupant.downcast::<T>().is_some() {
        pane.update(cx, |pane, cx| {
            pane.activate_item(index, focus, focus, window, cx);
        });
        return;
    }

    let item = build_item(window, cx);
    pane.update(cx, |pane, cx| {
        pane.remove_item(occupant.item_id(), false, false, window, cx);
        pane.add_item(Box::new(item), focus, focus, Some(index), window, cx);
    });
}

/// Brings a tab of type `T` that some pane already holds for `entry_id` to the
/// front, reporting whether it found one.
///
/// A pane's neighbour is only ever looked for to its right, so the grid and the
/// text tab of one file, sitting side by side, would otherwise split a third
/// pane the moment the button on the right hand one is pressed - and fill it
/// with a second copy of the tab standing to its left.
pub(crate) fn activate_existing_item<T: Item>(
    workspace: &mut Workspace,
    entry_id: Option<ProjectEntryId>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> bool {
    let Some(entry_id) = entry_id else {
        return false;
    };
    let existing = workspace.panes().iter().find_map(|pane| {
        pane.read(cx)
            .items()
            .find(|item| {
                item.downcast::<T>().is_some()
                    && item.buffer_kind(cx) == ItemBufferKind::Singleton
                    && item.project_entry_ids(cx).first() == Some(&entry_id)
            })
            .map(|item| item.boxed_clone())
    });
    let Some(item) = existing else {
        return false;
    };
    workspace.activate_item(item.as_ref(), true, true, window, cx)
}

impl Item for TabularDataPreviewPane {
    type Event = EditorEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            EditorEvent::Saved | EditorEvent::TitleChanged | EditorEvent::FileHandleChanged => {
                f(ItemEvent::UpdateTab);
                f(ItemEvent::UpdateBreadcrumbs);
            }
            EditorEvent::DirtyChanged => {
                f(ItemEvent::UpdateTab);
            }
            EditorEvent::BufferEdited => {
                f(ItemEvent::Edit);
            }
            _ => {}
        }
    }

    fn as_searchable(
        &self,
        handle: &Entity<Self>,
        _: &App,
    ) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(handle.clone()))
    }

    fn tab_icon(&self, _window: &Window, cx: &App) -> Option<Icon> {
        // A table rather than the file type's icon, so the tab reads as the grid view of
        // the file even when a text tab for it sits in the next pane.
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| Icon::new(IconName::Table))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(self.tab_label_color(&params, cx))
            .when(params.preview, |this| this.italic())
            .when(self.has_deleted_file(cx), |this| this.strikethrough())
            .into_any_element()
    }

    fn tab_content_text(&self, detail: usize, cx: &App) -> SharedString {
        Item::tab_content_text(self.editor().read(cx), detail, cx)
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        Item::tab_tooltip_text(self.editor().read(cx), cx)
    }

    /// The grid's toolbar row would otherwise have nothing on its left: the
    /// breadcrumbs are the workspace toolbar's only left-hand item, and without
    /// them the buttons on the right sit alone against an empty bar. Delegating
    /// to the hidden editor gives the same file path a text tab shows.
    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        Item::breadcrumb_location(self.editor().read(cx), cx)
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        Item::breadcrumbs(self.editor().read(cx), cx)
    }

    fn breadcrumb_prefix(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let editor = self.editor().clone();
        editor.update(cx, |editor, cx| Item::breadcrumb_prefix(editor, window, cx))
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(EntityId, &dyn project::ProjectItem),
    ) {
        if let Some(buffer) = self.buffer(cx) {
            f(buffer.entity_id(), buffer.read(cx));
        }
    }

    fn buffer_kind(&self, _cx: &App) -> ItemBufferKind {
        ItemBufferKind::Singleton
    }

    fn is_dirty(&self, cx: &App) -> bool {
        Item::is_dirty(self.editor().read(cx), cx)
    }

    fn has_conflict(&self, cx: &App) -> bool {
        Item::has_conflict(self.editor().read(cx), cx)
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        Item::has_deleted_file(self.editor().read(cx), cx)
    }

    fn capability(&self, cx: &App) -> Capability {
        Item::capability(self.editor().read(cx), cx)
    }

    fn toggle_read_only(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor().clone();
        editor.update(cx, |editor, cx| {
            Item::toggle_read_only(editor, window, cx);
        });
        cx.notify();
    }

    fn set_nav_history(
        &mut self,
        history: ItemNavHistory,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.nav_history = Some(history);
    }

    /// Leaving a grid tab has to leave a navigation entry behind, or "go back" and
    /// "reopen closed tab" skip over it as if it had never been open.
    fn deactivated(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // A tab that is no longer drawn has no mouse handlers left to see the
        // button come up, so a drag it was left holding would never end and the
        // task scrolling after it would never stop.
        self.cancel_drag(cx);
        if let Some(nav_history) = self.nav_history.as_mut() {
            nav_history.push::<()>(None, None, cx);
        }
    }

    fn can_save(&self, cx: &App) -> bool {
        Item::can_save(self.editor().read(cx), cx)
    }

    fn can_save_as(&self, cx: &App) -> bool {
        Item::can_save_as(self.editor().read(cx), cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let editor = self.editor().clone();
        editor.update(cx, |editor, cx| {
            Item::save(editor, options, project, window, cx)
        })
    }

    fn save_as(
        &mut self,
        project: Entity<Project>,
        path: ProjectPath,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let editor = self.editor().clone();
        editor.update(cx, |editor, cx| {
            Item::save_as(editor, project, path, window, cx)
        })
    }

    fn reload(
        &mut self,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let editor = self.editor().clone();
        editor.update(cx, |editor, cx| Item::reload(editor, project, window, cx))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        let editor = self.editor().clone();
        Task::ready(Some(
            cx.new(|cx| TabularDataPreviewPane::build(editor, window, cx)),
        ))
    }
}

impl SerializableItem for TabularDataPreviewPane {
    fn serialized_item_kind() -> &'static str {
        "TabularDataGrid"
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        let db = persistence::TabularDataGridDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "tabular_data_grids", &db, cx)
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Entity<Self>>> {
        let db = persistence::TabularDataGridDb::global(cx);
        window.spawn(cx, async move |cx| {
            let abs_path = db
                .get_grid_path(item_id, workspace_id)?
                .context("No tabular data grid entry found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(abs_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.read_with(cx, |worktree, _| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))
                .await?;

            cx.update(|window, cx| {
                cx.new(|cx| {
                    let editor =
                        cx.new(|cx| Editor::for_buffer(buffer, Some(project.clone()), window, cx));
                    TabularDataPreviewPane::build(editor, window, cx)
                })
            })
        })
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let buffer = self.buffer(cx)?;
        let file = buffer.read(cx).file()?;
        let worktree_id = file.worktree_id(cx);
        let abs_path = workspace
            .project()
            .read(cx)
            .worktree_for_id(worktree_id, cx)?
            .read(cx)
            .absolutize(file.path());

        let db = persistence::TabularDataGridDb::global(cx);
        Some(cx.background_spawn(async move {
            db.save_grid_path(item_id, workspace_id, abs_path).await
        }))
    }

    fn should_serialize(&self, event: &Self::Event) -> bool {
        matches!(
            event,
            EditorEvent::TitleChanged | EditorEvent::FileHandleChanged
        )
    }
}

pub(crate) mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct TabularDataGridDb(ThreadSafeConnection);

    impl Domain for TabularDataGridDb {
        const NAME: &str = stringify!(TabularDataGridDb);

        const MIGRATIONS: &[&str] = &[sql!(
            CREATE TABLE tabular_data_grids (
                workspace_id INTEGER,
                item_id INTEGER,
                abs_path BLOB,

                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        )];
    }

    db::static_connection!(TabularDataGridDb, [WorkspaceDb]);

    impl TabularDataGridDb {
        query! {
            pub async fn save_grid_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                abs_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO tabular_data_grids(item_id, workspace_id, abs_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_grid_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT abs_path
                FROM tabular_data_grids
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{any::TypeId, cell::RefCell, path::PathBuf, rc::Rc, sync::Arc};

    use gpui::{Focusable as _, TestAppContext, VisualTestContext};
    use project::WorktreeId;
    use serde_json::json;
    use settings::SettingsStore;
    use util::{path, rel_path::rel_path};
    use workspace::{AppState, SaveIntent, item::ItemHandle};

    use super::*;

    fn init_test(cx: &mut TestAppContext) -> Arc<AppState> {
        cx.update(|cx| {
            let state = AppState::test(cx);
            // Without a per-`App` database these tests fall back to one shared connection,
            // where item ids restart at 1 in every `TestAppContext`, so concurrently
            // running tests would read and overwrite each other's rows.
            cx.set_global(db::AppDatabase::test_new());
            editor::init(cx);
            crate::init(cx);
            state
        })
    }

    async fn open_test_workspace(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Workspace>,
        Entity<Project>,
        WorktreeId,
        &mut VisualTestContext,
    ) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/root"),
                json!({
                    "data.csv": "name,count\nalpha,1\n",
                    "empty.csv": "",
                    "notes.txt": "plain text\n",
                }),
            )
            .await;

        let project = Project::test(app_state.fs.clone(), [path!("/root").as_ref()], cx).await;
        let worktree_id = project.update(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));

        (workspace, project, worktree_id, cx)
    }

    async fn open_file(
        workspace: &Entity<Workspace>,
        worktree_id: WorktreeId,
        path: &str,
        cx: &mut VisualTestContext,
    ) -> Box<dyn ItemHandle> {
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_path((worktree_id, rel_path(path)), None, true, window, cx)
            })
            .await
            .unwrap()
    }

    /// `tabular_data_grids` has a foreign key onto `workspaces`, and only the workspace
    /// crate can give a `Workspace` an id that came out of that table, so the row for the
    /// test workspace's id is registered by hand.
    async fn register_workspace_row(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> WorkspaceId {
        let workspace_id = workspace.update(cx, |workspace, _| {
            workspace.set_random_database_id();
            workspace.database_id().unwrap()
        });

        cx.update(|_, cx| workspace::WorkspaceDb::global(cx))
            .write(move |connection| {
                let mut statement = db::sqlez::statement::Statement::prepare(
                    connection,
                    "INSERT OR IGNORE INTO workspaces(workspace_id) VALUES (?)",
                )?;
                statement.bind(&workspace_id, 1)?;
                statement.exec()
            })
            .await
            .unwrap();

        workspace_id
    }

    fn set_open_as_grid(open_as_grid: bool, cx: &mut VisualTestContext) {
        cx.update(|_, cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.tabular_data.get_or_insert_default().open_as_grid = Some(open_as_grid);
                });
            });
        });
    }

    fn buffer_text(buffer: &Entity<Buffer>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| buffer.read(cx).text())
    }

    fn active_item(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Box<dyn ItemHandle> {
        workspace.update(cx, |workspace, cx| workspace.active_item(cx).unwrap())
    }

    #[gpui::test]
    async fn tabular_files_open_as_a_grid_and_other_files_stay_in_the_editor(
        cx: &mut TestAppContext,
    ) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;

        let grid = open_file(&workspace, worktree_id, "data.csv", cx).await;
        assert_eq!(
            grid.to_any_view().entity_type(),
            TypeId::of::<TabularDataPreviewPane>(),
        );

        let text = open_file(&workspace, worktree_id, "notes.txt", cx).await;
        assert_eq!(text.to_any_view().entity_type(), TypeId::of::<Editor>());
    }

    #[gpui::test]
    async fn grid_tabs_take_the_git_colors_of_editor_tabs(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        let fs = app_state.fs.as_fake();
        fs.insert_tree(
            path!("/repo"),
            json!({
                ".git": {},
                ".gitignore": "ignored.csv\n",
                "clean.csv": "a,b\n",
                "ignored.csv": "a,b\n",
            }),
        )
        .await;
        fs.set_head_and_index_for_repo(
            path!("/repo/.git").as_ref(),
            &[
                (".gitignore", "ignored.csv\n".into()),
                ("clean.csv", "a,b\n".into()),
            ],
        );
        let project = Project::test(app_state.fs.clone(), [path!("/repo").as_ref()], cx).await;
        let worktree_id = project.update(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        cx.run_until_parked();

        let mut label_colors = Vec::new();
        for file_name in ["clean.csv", "ignored.csv"] {
            let item = open_file(&workspace, worktree_id, file_name, cx).await;
            let grid = item.downcast::<TabularDataPreviewPane>().unwrap();
            cx.run_until_parked();
            let params = TabContentParams {
                selected: false,
                ..TabContentParams::default()
            };
            let color = cx.update(|_, cx| grid.read(cx).tab_label_color(&params, cx));
            label_colors.push((file_name, color));
        }

        assert_eq!(
            label_colors,
            [
                ("clean.csv", Color::Default),
                ("ignored.csv", Color::Ignored)
            ],
            "an inactive grid tab of a clean file keeps the normal color, unlike an ignored file",
        );
    }

    /// A file opened on its own is a worktree whose root is the file, so its path within
    /// the worktree is empty and the extension has to come from the worktree's name.
    #[gpui::test]
    async fn a_tabular_file_opened_on_its_own_opens_as_a_grid(cx: &mut TestAppContext) {
        let app_state = init_test(cx);
        app_state
            .fs
            .as_fake()
            .insert_tree(
                path!("/elsewhere"),
                json!({ "alone.CSV": "name,count\nalpha,1\n", "alone.txt": "plain\n" }),
            )
            .await;

        for (file, expected) in [
            (
                path!("/elsewhere/alone.CSV"),
                TypeId::of::<TabularDataPreviewPane>(),
            ),
            (path!("/elsewhere/alone.txt"), TypeId::of::<Editor>()),
        ] {
            let project = Project::test(app_state.fs.clone(), [file.as_ref()], cx).await;
            let worktree_id = project.update(cx, |project, cx| {
                let worktree = project.worktrees(cx).next().unwrap();
                assert!(worktree.read(cx).is_single_file(), "{file}");
                worktree.read(cx).id()
            });
            let (workspace, cx) =
                cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
            let item = open_file(&workspace, worktree_id, "", cx).await;
            assert_eq!(item.to_any_view().entity_type(), expected, "{file}");
        }
    }

    #[gpui::test]
    async fn tabular_files_open_in_the_editor_when_the_grid_is_disabled(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;

        set_open_as_grid(false, cx);

        let item = open_file(&workspace, worktree_id, "data.csv", cx).await;
        assert_eq!(item.to_any_view().entity_type(), TypeId::of::<Editor>());
    }

    #[gpui::test]
    async fn editing_the_grid_buffer_marks_the_tab_dirty_until_it_is_saved(
        cx: &mut TestAppContext,
    ) {
        let (workspace, project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();

        let tab_updates = Rc::new(RefCell::new(0usize));
        let subscription = cx.update(|_, cx| {
            cx.subscribe(&grid, {
                let tab_updates = tab_updates.clone();
                move |_, event: &EditorEvent, _| {
                    TabularDataPreviewPane::to_item_events(event, &mut |item_event| {
                        if item_event == ItemEvent::UpdateTab {
                            *tab_updates.borrow_mut() += 1;
                        }
                    });
                }
            })
        });

        let is_dirty =
            |cx: &mut VisualTestContext| cx.update(|_, cx| Item::is_dirty(grid.read(cx), cx));
        assert!(!is_dirty(cx));

        let buffer = cx.update(|_, cx| grid.read(cx).buffer(cx)).unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "id,")], None, cx));
        cx.run_until_parked();

        assert!(is_dirty(cx));
        assert!(*tab_updates.borrow() > 0);

        grid.update_in(cx, |grid, window, cx| {
            Item::save(
                grid,
                SaveOptions {
                    format: false,
                    ..Default::default()
                },
                project.clone(),
                window,
                cx,
            )
        })
        .await
        .unwrap();
        cx.run_until_parked();

        assert!(!is_dirty(cx));
        drop(subscription);
    }

    #[gpui::test]
    async fn open_as_text_shows_the_same_buffer_beside_the_grid(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let grid_buffer = cx.update(|_, cx| grid.read(cx).buffer(cx)).unwrap();

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            TabularDataPreviewPane::open_as_text(workspace, grid.clone(), pane, false, window, cx);
        });
        cx.run_until_parked();

        let (pane_count, editor_buffer) = workspace.update(cx, |workspace, cx| {
            let editor = workspace
                .active_item(cx)
                .unwrap()
                .downcast::<Editor>()
                .unwrap();
            let buffer = editor.read(cx).buffer().read(cx).as_singleton().unwrap();
            (workspace.panes().len(), buffer)
        });

        assert_eq!(pane_count, 2);
        assert_eq!(editor_buffer, grid_buffer);
    }

    /// The pane a neighbour is looked for in is always the one to the right, so
    /// the button on the right hand tab of a pair used to split a third pane and
    /// fill it with a copy of the tab already standing on the left.
    #[gpui::test]
    async fn the_side_by_side_buttons_walk_between_the_two_panes(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let grid_pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            TabularDataPreviewPane::open_as_text(workspace, grid.clone(), pane, false, window, cx);
        });
        cx.run_until_parked();

        let (text_pane, editor) = workspace.update(cx, |workspace, cx| {
            (
                workspace.active_pane().clone(),
                workspace
                    .active_item(cx)
                    .unwrap()
                    .downcast::<Editor>()
                    .unwrap(),
            )
        });
        assert_ne!(text_pane, grid_pane);

        // The grid is now to the left of the text, where no neighbour is ever
        // looked for: the button has to find the pane rather than make one.
        workspace.update_in(cx, |workspace, window, cx| {
            TabularDataPreviewPane::open_preview_to_the_side_of_pane(
                workspace,
                editor.clone(),
                text_pane.clone(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2, "no third pane may be split off");
            assert_eq!(workspace.active_pane(), &grid_pane);
            assert_eq!(
                workspace.active_item(cx).unwrap().item_id(),
                grid.item_id(),
                "the grid that was already open should have come to the front"
            );
        });

        // And back again, onto the text tab that is already open.
        workspace.update_in(cx, |workspace, window, cx| {
            TabularDataPreviewPane::open_as_text(workspace, grid, grid_pane, false, window, cx);
        });
        cx.run_until_parked();

        workspace.update(cx, |workspace, cx| {
            assert_eq!(workspace.panes().len(), 2);
            assert_eq!(workspace.active_pane(), &text_pane);
            assert_eq!(
                workspace.active_item(cx).unwrap().item_id(),
                editor.item_id()
            );
        });
    }

    /// The workspace toolbar's only left hand item is the breadcrumbs, so a grid
    /// that reports none leaves the buttons on the right standing alone against
    /// an empty bar.
    #[gpui::test]
    async fn the_grid_shows_its_file_path_in_the_breadcrumbs(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        cx.run_until_parked();

        grid.read_with(cx, |grid, cx| {
            assert_eq!(
                Item::breadcrumb_location(grid, cx),
                ToolbarItemLocation::PrimaryLeft,
                "the grid should fill the left of the toolbar the way a text tab does"
            );
            let (segments, font) = Item::breadcrumbs(grid, cx)
                .expect("a grid on a singleton buffer should have breadcrumbs");
            assert!(
                font.is_some(),
                "the path is drawn in the buffer font, as it is for a text tab"
            );
            let path = segments
                .first()
                .map(|segment| segment.text.to_string())
                .unwrap_or_default();
            assert!(
                path.contains("data.csv"),
                "the first segment should be the file's path, got {path:?}"
            );
        });
    }

    /// An empty file draws a message instead of a table, and the grid still has
    /// to be able to take what the user types - and show it - without being
    /// clicked first.
    #[gpui::test]
    async fn an_empty_file_can_be_typed_into_right_away(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "empty.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        // Keys only reach the element that holds the focus once a frame has
        // been drawn with it in the tree.
        cx.run_until_parked();

        assert!(
            cx.update(|window, cx| grid.focus_handle(cx).is_focused(window)),
            "opening the grid should put the caret in it"
        );

        cx.simulate_input("x");
        cx.run_until_parked();
        grid.read_with(cx, |grid, cx| {
            assert_eq!(
                grid.grid.cell_editor.read(cx).text(cx),
                "x",
                "the key should have reached the cell editor"
            );
            assert!(
                grid.cell_editor_is_visible(),
                "a file with no columns has no cell for the editor to sit over, \
                 and typing into a one pixel box shows the user nothing"
            );
        });

        grid.update_in(cx, |grid, window, cx| grid.commit_editing(window, cx));
        cx.run_until_parked();

        let buffer = cx.update(|_, cx| grid.read(cx).buffer(cx)).unwrap();
        assert_eq!(buffer_text(&buffer, cx), "x");
    }

    #[gpui::test]
    async fn a_serialized_grid_is_restored_for_the_same_file(cx: &mut TestAppContext) {
        let (workspace, project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let item_id = grid.entity_id().as_u64();
        let workspace_id = register_workspace_row(&workspace, cx).await;

        workspace
            .update_in(cx, |workspace, _window, cx| {
                grid.update(cx, |grid, cx| grid.serialize(workspace, item_id, false, cx))
                    .unwrap()
            })
            .await
            .unwrap();

        assert_eq!(
            cx.update(|_, cx| persistence::TabularDataGridDb::global(cx))
                .get_grid_path(item_id, workspace_id)
                .unwrap(),
            Some(PathBuf::from(path!("/root/data.csv"))),
        );

        let restored = workspace
            .update_in(cx, |_workspace, window, cx| {
                TabularDataPreviewPane::deserialize(
                    project.clone(),
                    cx.entity().downgrade(),
                    workspace_id,
                    item_id,
                    window,
                    cx,
                )
            })
            .await
            .unwrap();

        let (restored_path, restored_text) = cx.update(|_, cx| {
            let buffer = restored.read(cx).buffer(cx).unwrap();
            let buffer = buffer.read(cx);
            (buffer.file().unwrap().path().clone(), buffer.text())
        });
        assert_eq!(restored_path.as_ref(), rel_path("data.csv"));
        assert_eq!(restored_text, "name,count\nalpha,1\n");
    }

    #[gpui::test]
    async fn opening_the_grid_in_the_same_pane_swaps_the_text_tab(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        set_open_as_grid(false, cx);
        let editor = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<Editor>()
            .unwrap();
        let buffer = cx
            .update(|_, cx| editor.read(cx).buffer().read(cx).as_singleton())
            .unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "id,")], None, cx));
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            TabularDataPreviewPane::open_preview_in_pane(editor.clone(), pane, window, cx);
        });
        cx.run_until_parked();

        let pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());
        assert_eq!(pane.update(cx, |pane, _| pane.items_len()), 1);

        let grid = active_item(&workspace, cx)
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        assert_eq!(cx.update(|_, cx| grid.read(cx).buffer(cx)), Some(buffer));
        assert!(cx.update(|_, cx| Item::is_dirty(grid.read(cx), cx)));
        assert_eq!(
            cx.update(|_, cx| Item::tab_content_text(grid.read(cx), 0, cx)),
            "data.csv"
        );
    }

    /// Alt clicking the grid's "テキストを横に開く" button asks for the swap
    /// rather than for a second pane, and it does so by putting the modifier on
    /// the action; dispatching the action is the whole of what that click does.
    #[gpui::test]
    async fn the_open_as_text_action_can_ask_for_the_same_pane(cx: &mut TestAppContext) {
        let (workspace, grid, _editor, mut cx) =
            crate::grid_editing::tests::open_grid_in_workspace(
                "name,count
alpha,1
",
                cx,
            )
            .await;
        // `crate::init` does this for every workspace the app opens; this
        // fixture builds one without going through it.
        workspace.update(&mut cx, |workspace, cx| {
            TabularDataPreviewPane::register(workspace);
            cx.notify();
        });
        let buffer = cx
            .update(|_, cx| grid.read(cx).buffer(cx))
            .expect("the fixture should have opened on a singleton buffer");
        cx.run_until_parked();

        cx.dispatch_action(crate::OpenAsText { in_same_pane: true });
        cx.run_until_parked();

        // Opening beside the grid - what the same action does without the
        // modifier - would have split the workspace instead.
        assert_eq!(
            workspace.update(&mut cx, |workspace, _| workspace.panes().len()),
            1,
            "the swap should not open a second pane"
        );
        let pane = workspace.update(&mut cx, |workspace, _| workspace.active_pane().clone());
        assert_eq!(
            pane.update(&mut cx, |pane, _| pane.items_len()),
            1,
            "the grid tab should have been replaced, not added to"
        );
        let text_tab = pane
            .update(&mut cx, |pane, _| pane.active_item())
            .and_then(|item| item.downcast::<Editor>())
            .expect("the pane should now hold the file's text");
        assert_eq!(
            cx.update(|_, cx| text_tab.read(cx).buffer().read(cx).as_singleton()),
            Some(buffer),
        );
    }

    #[gpui::test]
    async fn opening_text_in_the_same_pane_swaps_the_grid_tab(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let buffer = cx.update(|_, cx| grid.read(cx).buffer(cx)).unwrap();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, "id,")], None, cx));
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            let pane = workspace.active_pane().clone();
            TabularDataPreviewPane::open_as_text(workspace, grid.clone(), pane, true, window, cx);
        });
        cx.run_until_parked();

        let pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());
        assert_eq!(
            workspace.update(cx, |workspace, _| workspace.panes().len()),
            1
        );
        assert_eq!(pane.update(cx, |pane, _| pane.items_len()), 1);

        let editor = active_item(&workspace, cx).downcast::<Editor>().unwrap();
        assert_eq!(
            cx.update(|_, cx| editor.read(cx).buffer().read(cx).as_singleton()),
            Some(buffer.clone()),
        );
        // The buffer outlives the tab it was shown in, so the swap keeps unsaved work.
        assert_eq!(buffer_text(&buffer, cx), "id,name,count\nalpha,1\n");
        assert!(cx.update(|_, cx| Item::is_dirty(editor.read(cx), cx)));
    }

    #[gpui::test]
    async fn opening_to_the_side_reuses_or_swaps_the_neighbouring_tab(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let origin_pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            TabularDataPreviewPane::open_as_text(
                workspace,
                grid.clone(),
                origin_pane.clone(),
                false,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        let side_pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());
        assert_ne!(side_pane.entity_id(), origin_pane.entity_id());
        let editor = active_item(&workspace, cx).downcast::<Editor>().unwrap();

        // Asking again finds the same kind of tab on that entry and only activates it.
        workspace.update_in(cx, |workspace, window, cx| {
            TabularDataPreviewPane::open_as_text(
                workspace,
                grid.clone(),
                origin_pane.clone(),
                false,
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            workspace.update(cx, |workspace, _| workspace.panes().len()),
            2
        );
        assert_eq!(side_pane.update(cx, |pane, _| pane.items_len()), 1);
        assert_eq!(
            active_item(&workspace, cx).item_id(),
            editor.entity_id(),
            "the text tab already beside the grid should be reused"
        );

        // With no grid left to come back to, a grid asked for on that same entry
        // replaces the text tab sitting there.
        origin_pane.update_in(cx, |pane, window, cx| {
            pane.remove_item(grid.item_id(), false, false, window, cx);
        });
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            TabularDataPreviewPane::open_preview_to_the_side_of_pane(
                workspace,
                editor.clone(),
                origin_pane.clone(),
                window,
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            workspace.update(cx, |workspace, _| workspace.panes().len()),
            2
        );
        assert_eq!(side_pane.update(cx, |pane, _| pane.items_len()), 1);
        let swapped = side_pane
            .update(cx, |pane, _| pane.active_item())
            .unwrap()
            .downcast::<TabularDataPreviewPane>();
        assert!(
            swapped.is_some(),
            "the neighbouring text tab should be swapped for a grid"
        );
    }

    #[gpui::test]
    async fn repeated_edits_from_the_text_side_keep_reparsing(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        let buffer = cx.update(|_, cx| grid.read(cx).buffer(cx)).unwrap();

        let data_rows = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| grid.read(cx).engine.contents.rows.len())
        };
        let settle = |cx: &mut VisualTestContext| {
            cx.executor()
                .advance_clock(crate::parser::REPARSE_DEBOUNCE * 2);
            cx.run_until_parked();
        };
        let append = |text: &str, cx: &mut VisualTestContext| {
            let end = cx.update(|_, cx| buffer.read(cx).len());
            buffer.update(cx, |buffer, cx| buffer.edit([(end..end, text)], None, cx));
            settle(cx);
        };

        settle(cx);
        assert_eq!(data_rows(cx), 1);
        append("beta,2\n", cx);
        assert_eq!(data_rows(cx), 2);
        // The second edit is the one `EditorEvent::Edited` never reports, because it is the
        // buffer - not the grid's own editor - that performed it.
        append("gamma,3\n", cx);
        assert_eq!(data_rows(cx), 3);
    }

    #[gpui::test]
    async fn going_back_and_reopening_reach_the_grid_tab(cx: &mut TestAppContext) {
        let (workspace, _project, worktree_id, cx) = open_test_workspace(cx).await;
        let grid = open_file(&workspace, worktree_id, "data.csv", cx)
            .await
            .downcast::<TabularDataPreviewPane>()
            .unwrap();
        open_file(&workspace, worktree_id, "notes.txt", cx).await;
        cx.run_until_parked();

        let pane = workspace.update(cx, |workspace, _| workspace.active_pane().clone());
        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.go_back(pane.downgrade(), window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert_eq!(active_item(&workspace, cx).item_id(), grid.entity_id());

        pane.update_in(cx, |pane, window, cx| {
            pane.close_item_by_id(grid.entity_id(), SaveIntent::Skip, window, cx)
        })
        .await
        .unwrap();
        cx.run_until_parked();
        assert!(
            active_item(&workspace, cx)
                .downcast::<TabularDataPreviewPane>()
                .is_none()
        );

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.reopen_closed_item(window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        assert!(
            active_item(&workspace, cx)
                .downcast::<TabularDataPreviewPane>()
                .is_some(),
            "reopening the closed tab should bring the grid back"
        );
    }

    #[gpui::test]
    async fn a_file_that_fails_to_open_falls_back_to_the_invalid_item_view(
        cx: &mut TestAppContext,
    ) {
        let (_workspace, _project, _worktree_id, cx) = open_test_workspace(cx).await;

        let view = cx.update(|window, cx| {
            <TabularDataPreviewPane as ProjectItem>::for_broken_project_item(
                Path::new(path!("/root/data.csv")),
                true,
                &anyhow::anyhow!("failed to open"),
                window,
                cx,
            )
        });
        assert!(view.is_some());
    }
}
