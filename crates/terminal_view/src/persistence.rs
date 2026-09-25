use anyhow::Result;
use async_recursion::async_recursion;
use futures::future::join_all;
use gpui::{AppContext as _, AsyncWindowContext, Axis, Entity, Task, WeakEntity};
use project::Project;
use serde::{Deserialize, Serialize};
use std::{path::PathBuf, sync::Arc};
use ui::{App, Context, Window};
use util::ResultExt as _;

use db::{
    query,
    sqlez::{domain::Domain, statement::Statement, thread_safe_connection::ThreadSafeConnection},
    sqlez_macros::sql,
};
use workspace::{
    ItemHandle, ItemId, Member, Pane, PaneAxis, PaneGroup, SerializableItem as _, Workspace,
    WorkspaceDb, WorkspaceId,
};

use crate::{
    TerminalView, default_working_directory,
    terminal_panel::{TerminalPanel, new_terminal_pane},
};

pub(crate) fn serialize_pane_group(
    pane_group: &PaneGroup,
    active_pane: &Entity<Pane>,
    cx: &mut App,
) -> SerializedPaneGroup {
    build_serialized_pane_group(&pane_group.root, active_pane, cx)
}

fn build_serialized_pane_group(
    pane_group: &Member,
    active_pane: &Entity<Pane>,
    cx: &mut App,
) -> SerializedPaneGroup {
    match pane_group {
        Member::Axis(PaneAxis {
            axis,
            members,
            flexes,
            bounding_boxes: _,
        }) => SerializedPaneGroup::Group {
            axis: SerializedAxis(*axis),
            children: members
                .iter()
                .map(|member| build_serialized_pane_group(member, active_pane, cx))
                .collect::<Vec<_>>(),
            flexes: Some(flexes.lock().clone()),
        },
        Member::Pane(pane_handle) => {
            SerializedPaneGroup::Pane(serialize_pane(pane_handle, pane_handle == active_pane, cx))
        }
    }
}

/// 種類を問わず、復元できるタブをすべて書き出す (ADR-F37)。ドラッグでブラウザーや
/// エディターがターミナルパネルに載っていても、次の起動でこのペインに戻ってくる。
fn serialize_pane(pane: &Entity<Pane>, active: bool, cx: &mut App) -> SerializedPane {
    let preview_item_id = pane.read(cx).preview_item_id();
    let active_item_index = pane.read(cx).active_item_index();
    let pinned_count = pane.read(cx).pinned_count();
    let pane = pane.read(cx);
    let items = pane
        .items()
        .enumerate()
        .filter_map(|(index, item)| {
            // タスクのターミナルは走り終わった実行の残骸なので復元しない。
            if let Some(terminal_view) = item.act_as::<TerminalView>(cx)
                && terminal_view.read(cx).terminal().read(cx).task().is_some()
            {
                return None;
            }
            let handle = item.to_serializable_item_handle(cx)?;
            Some(SerializedPanelItem {
                kind: Arc::from(handle.serialized_item_kind()),
                item_id: item.item_id().as_u64(),
                active: index == active_item_index,
                preview: Some(item.item_id()) == preview_item_id,
            })
        })
        .collect::<Vec<_>>();

    SerializedPane {
        active,
        items,
        children: Vec::new(),
        active_item: None,
        pinned_count,
    }
}

pub(crate) fn deserialize_terminal_panel(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    database_id: WorkspaceId,
    serialized_panel: SerializedTerminalPanel,
    window: &mut Window,
    cx: &mut App,
) -> Task<anyhow::Result<Entity<TerminalPanel>>> {
    window.spawn(cx, async move |cx| {
        // 復元中のペインはまだ `terminal_panel.center` に載っていない。タブが 1 枚
        // 足されるたびに保存すると、その「まだ空のパネル」が保存済みのレイアウトを
        // 上書きしてしまう。
        let terminal_panel = workspace.update_in(cx, |workspace, window, cx| {
            cx.new(|cx| {
                let mut terminal_panel = TerminalPanel::new(workspace, window, cx);
                terminal_panel.restoring = true;
                terminal_panel
            })
        })?;
        match &serialized_panel.items {
            SerializedItems::NoSplits(item_ids) => {
                let items = deserialize_terminal_views(
                    database_id,
                    project,
                    workspace,
                    item_ids.as_slice(),
                    cx,
                )
                .await;
                let active_item = serialized_panel.active_item_id;
                terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.active_pane.update(cx, |pane, cx| {
                        populate_pane_items(pane, items, active_item, window, cx);
                    });
                })?;
            }
            SerializedItems::WithSplits(serialized_pane_group) => {
                let center_pane = deserialize_pane_group(
                    workspace,
                    project,
                    terminal_panel.clone(),
                    database_id,
                    serialized_pane_group,
                    cx,
                )
                .await;
                if let Some((center_group, active_pane)) = center_pane {
                    terminal_panel.update(cx, |terminal_panel, cx| {
                        terminal_panel.center = PaneGroup::with_root(center_group);
                        terminal_panel.active_pane =
                            active_pane.unwrap_or_else(|| terminal_panel.center.first_pane());
                        // `TerminalPanel::new` only dressed its own first pane; the
                        // restored panes were built by `new_terminal_pane` and would
                        // otherwise show the workspace's generic buttons until some
                        // later event (a split, or the inline assistant toggling)
                        // happened to refresh them.
                        terminal_panel.refresh_tab_bar_buttons(cx);
                    });
                }
            }
        }

        terminal_panel.update(cx, |terminal_panel, cx| {
            terminal_panel.restoring = false;
            // A pane that came back empty was given a fresh shell while saving
            // was held off. Unless the panel is saved now, it keeps its empty
            // record, and every later start opens yet another shell instead of
            // returning to the one this tab runs (a remote tab's session).
            terminal_panel.serialize(cx);
        });
        Ok(terminal_panel)
    })
}

fn populate_pane_items(
    pane: &mut Pane,
    items: Vec<Entity<TerminalView>>,
    active_item: Option<u64>,
    window: &mut Window,
    cx: &mut Context<Pane>,
) {
    let mut active_item_index = None;
    for (item_index, item) in (pane.items_len()..).zip(items) {
        if Some(item.item_id().as_u64()) == active_item {
            active_item_index = Some(item_index);
        }
        pane.add_item(Box::new(item), false, false, None, window, cx);
    }
    if let Some(index) = active_item_index {
        pane.activate_item(index, false, false, window, cx);
    }
}

#[async_recursion(?Send)]
async fn deserialize_pane_group(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    panel: Entity<TerminalPanel>,
    workspace_id: WorkspaceId,
    serialized: &SerializedPaneGroup,
    cx: &mut AsyncWindowContext,
) -> Option<(Member, Option<Entity<Pane>>)> {
    match serialized {
        SerializedPaneGroup::Group {
            axis,
            flexes,
            children,
        } => {
            let mut current_active_pane = None;
            let mut members = Vec::new();
            for child in children {
                if let Some((new_member, active_pane)) = deserialize_pane_group(
                    workspace.clone(),
                    project.clone(),
                    panel.clone(),
                    workspace_id,
                    child,
                    cx,
                )
                .await
                {
                    members.push(new_member);
                    current_active_pane = current_active_pane.or(active_pane);
                }
            }

            if members.is_empty() {
                return None;
            }

            if members.len() == 1 {
                return Some((members.remove(0), current_active_pane));
            }

            Some((
                Member::Axis(PaneAxis::load(axis.0, members, flexes.clone())),
                current_active_pane,
            ))
        }
        SerializedPaneGroup::Pane(serialized_pane) => {
            let active = serialized_pane.active;

            let pane = panel
                .update_in(cx, |terminal_panel, window, cx| {
                    new_terminal_pane(
                        workspace.clone(),
                        project.clone(),
                        terminal_panel.active_pane.read(cx).is_zoomed(),
                        window,
                        cx,
                    )
                })
                .log_err()?;

            // await をまたぐ更新は弱参照で行う (強い方はこの関数の戻り値のために
            // 持っているだけで、更新のたびに「まだ生きているか」を確かめられる方が安全)。
            let weak_pane = pane.downgrade();
            if serialized_pane.items.is_empty() {
                // 第 12 便までの形式: ターミナルの item_id だけが並んでいた。
                let active_item = serialized_pane.active_item;
                let new_items = deserialize_terminal_views(
                    workspace_id,
                    project.clone(),
                    workspace.clone(),
                    serialized_pane.children.as_slice(),
                    cx,
                )
                .await;
                weak_pane
                    .update_in(cx, |pane, window, cx| {
                        populate_pane_items(pane, new_items, active_item, window, cx);
                    })
                    .log_err();
            } else {
                deserialize_pane_items(
                    workspace.clone(),
                    project.clone(),
                    workspace_id,
                    &pane,
                    &serialized_pane.items,
                    cx,
                )
                .await;
            }

            let pinned_count = serialized_pane.pinned_count;
            let items = weak_pane
                .update(cx, |pane, _| {
                    pane.set_pinned_count(pinned_count.min(pane.items_len()));
                    pane.items_len()
                })
                .log_err();
            // Avoid blank panes in splits
            if items == Some(0) {
                let working_directory = workspace
                    .update(cx, |workspace, cx| default_working_directory(workspace, cx))
                    .ok()
                    .flatten();
                let terminal = project
                    .update(cx, |project, cx| {
                        project.create_persistent_terminal_shell(working_directory, None, cx)
                    })
                    .await
                    .log_err();
                if let Some(terminal) = terminal {
                    weak_pane
                        .update_in(cx, |pane, window, cx| {
                            let terminal_view = Box::new(cx.new(|cx| {
                                TerminalView::new(
                                    terminal,
                                    workspace.clone(),
                                    Some(workspace_id),
                                    project.downgrade(),
                                    window,
                                    cx,
                                )
                            }));
                            pane.add_item(terminal_view, true, false, None, window, cx);
                        })
                        .log_err();
                }
            }
            Some((Member::Pane(pane.clone()), active.then_some(pane)))
        }
    }
}

/// 種類ごとの復元 (ADR-F37)。`workspace` に登録の無い種類は飛ばす (それを知っていた
/// 版で保存された可能性があるので、失敗ではなく読み飛ばしとして扱う)。
async fn deserialize_pane_items(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    workspace_id: WorkspaceId,
    pane: &Entity<Pane>,
    items: &[SerializedPanelItem],
    cx: &mut AsyncWindowContext,
) {
    let mut tasks = Vec::new();
    for item in items {
        let task = pane.update_in(cx, |_, window, cx| {
            workspace::deserialize_serializable_item(
                &item.kind,
                project.clone(),
                workspace.clone(),
                workspace_id,
                item.item_id,
                window,
                cx,
            )
        });
        match task {
            Ok(Some(task)) => tasks.push((item.kind.clone(), item.active, task)),
            Ok(None) => log::info!(
                "terminal panel: no restore handler for a {:?} tab; skipping it",
                item.kind
            ),
            Err(error) => {
                log::warn!(
                    "terminal panel: the window went away while restoring its tabs: {error:#}"
                );
                return;
            }
        }
    }

    let mut active_index = None;
    let mut next_index = 0;
    for (kind, active, task) in tasks {
        // 復元できないタブは飛ばすだけ。中身の記録が (終了間際に足したタブの
        // ように) 書かれていなかった、というのが主な理由なので、警告ではなく
        // 記録として残す。
        let item = match task.await {
            Ok(item) => item,
            Err(error) => {
                log::info!(
                    "terminal panel: skipping a {kind:?} tab that could not be restored: {error:#}"
                );
                continue;
            }
        };
        // 表に出すのは最後に選ぶ 1 枚だけ (裏に回ったブラウザータブは WebView2 を
        // 作らない)。裏で足すときの挿し先は `Pane` に任せると「表のタブの次」に
        // なって並びが崩れるので、末尾を明示する。
        let added = pane
            .update_in(cx, |pane, window, cx| {
                pane.add_item_inner(item, false, false, false, Some(next_index), window, cx);
            })
            .log_err()
            .is_some();
        if added {
            if active {
                active_index = Some(next_index);
            }
            next_index += 1;
        }
    }
    pane.update_in(cx, |pane, window, cx| {
        pane.activate_item(active_index.unwrap_or(0), false, false, window, cx);
    })
    .log_err();
}

fn deserialize_terminal_views(
    workspace_id: WorkspaceId,
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    item_ids: &[u64],
    cx: &mut AsyncWindowContext,
) -> impl Future<Output = Vec<Entity<TerminalView>>> + use<> {
    let deserialized_items = join_all(item_ids.iter().filter_map(|item_id| {
        cx.update(|window, cx| {
            TerminalView::deserialize(
                project.clone(),
                workspace.clone(),
                workspace_id,
                *item_id,
                window,
                cx,
            )
        })
        .ok()
    }));
    async move {
        deserialized_items
            .await
            .into_iter()
            .filter_map(|item| item.log_err())
            .collect()
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedTerminalPanel {
    pub items: SerializedItems,
    // A deprecated field, kept for backwards compatibility for the code before terminal splits were introduced.
    pub active_item_id: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum SerializedItems {
    // The data stored before terminal splits were introduced.
    NoSplits(Vec<u64>),
    WithSplits(SerializedPaneGroup),
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) enum SerializedPaneGroup {
    Pane(SerializedPane),
    Group {
        axis: SerializedAxis,
        flexes: Option<Vec<f32>>,
        children: Vec<SerializedPaneGroup>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct SerializedPane {
    pub active: bool,
    /// 種類を問わないタブの並び (ADR-F37)。
    #[serde(default)]
    pub items: Vec<SerializedPanelItem>,
    /// 第 12 便までの形式。ターミナルの item_id だけを並べていた。移行のため
    /// 読むだけ残してある (次の保存で `items` に書き換わる)。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<u64>,
    /// 同上。表に出ていたターミナルの item_id。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_item: Option<u64>,
    #[serde(default)]
    pub pinned_count: usize,
}

/// パネルのタブ 1 枚。中央の `SerializedItem` と同じ 4 つ組で、種類ごとの復元は
/// `workspace::deserialize_serializable_item` に投げる。
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SerializedPanelItem {
    pub kind: Arc<str>,
    pub item_id: ItemId,
    pub active: bool,
    /// 中央の形式に合わせて書き出すが、戻すのは今のところ中央だけ
    /// (`Pane::set_preview_item_id` が workspace の外に出ていない)。
    #[serde(default)]
    pub preview: bool,
}

#[derive(Debug)]
pub(crate) struct SerializedAxis(pub Axis);

impl Serialize for SerializedAxis {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self.0 {
            Axis::Horizontal => serializer.serialize_str("horizontal"),
            Axis::Vertical => serializer.serialize_str("vertical"),
        }
    }
}

impl<'de> Deserialize<'de> for SerializedAxis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "horizontal" => Ok(SerializedAxis(Axis::Horizontal)),
            "vertical" => Ok(SerializedAxis(Axis::Vertical)),
            invalid => Err(serde::de::Error::custom(format!(
                "Invalid axis value: '{invalid}'"
            ))),
        }
    }
}

pub struct TerminalDb(ThreadSafeConnection);

impl Domain for TerminalDb {
    const NAME: &str = stringify!(TerminalDb);

    const MIGRATIONS: &[&str] = &[
        sql!(
            CREATE TABLE terminals (
                workspace_id INTEGER,
                item_id INTEGER UNIQUE,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;
        ),
        // Remove the unique constraint on the item_id table
        // SQLite doesn't have a way of doing this automatically, so
        // we have to do this silly copying.
        sql!(
            CREATE TABLE terminals2 (
                workspace_id INTEGER,
                item_id INTEGER,
                working_directory BLOB,
                PRIMARY KEY(workspace_id, item_id),
                FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                ON DELETE CASCADE
            ) STRICT;

            INSERT INTO terminals2 (workspace_id, item_id, working_directory)
            SELECT workspace_id, item_id, working_directory FROM terminals;

            DROP TABLE terminals;

            ALTER TABLE terminals2 RENAME TO terminals;
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN working_directory_path TEXT;
            UPDATE terminals SET working_directory_path = CAST(working_directory AS TEXT);
        ),
        sql! (
            ALTER TABLE terminals ADD COLUMN custom_title TEXT;
        ),
        // A remote shell's directory is a path on the host, so it cannot share
        // the `working_directory` column, which the client resolves against its
        // own filesystem.
        sql! (
            ALTER TABLE terminals ADD COLUMN remote_working_directory TEXT;
        ),
        // The persistent session a remote shell runs in on its host, which
        // outlives Zed and is attached to again when the tab is restored.
        sql! (
            ALTER TABLE terminals ADD COLUMN remote_session_id TEXT;
        ),
    ];
}

db::static_connection!(TerminalDb, [WorkspaceDb]);

impl TerminalDb {
    query! {
       pub async fn update_workspace_id(
            new_id: WorkspaceId,
            old_id: WorkspaceId,
            item_id: ItemId
        ) -> Result<()> {
            UPDATE terminals
            SET workspace_id = ?
            WHERE workspace_id = ? AND item_id = ?
        }
    }

    pub async fn save_working_directory(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        working_directory: PathBuf,
    ) -> Result<()> {
        log::debug!(
            "Saving working directory {working_directory:?} for item {item_id} in workspace {workspace_id:?}"
        );
        let query =
            "INSERT INTO terminals(item_id, workspace_id, working_directory, working_directory_path)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT DO UPDATE SET
                item_id = ?1,
                workspace_id = ?2,
                working_directory = ?3,
                working_directory_path = ?4"
        ;
        self.write(move |conn| {
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            next_index = statement.bind(&working_directory, next_index)?;
            statement.bind(
                &working_directory.to_string_lossy().into_owned(),
                next_index,
            )?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_working_directory(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
            SELECT working_directory
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_remote_working_directory(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        remote_working_directory: String,
    ) -> Result<()> {
        log::debug!(
            "Saving remote working directory {remote_working_directory:?} for item {item_id} in workspace {workspace_id:?}"
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, remote_working_directory)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    remote_working_directory = excluded.remote_working_directory";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&remote_working_directory, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_remote_working_directory(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT remote_working_directory
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_remote_session_id(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        remote_session_id: Option<String>,
    ) -> Result<()> {
        log::debug!(
            "Saving remote session id {remote_session_id:?} for item {item_id} in workspace {workspace_id:?}"
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, remote_session_id)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    remote_session_id = excluded.remote_session_id";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&remote_session_id, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_remote_session_id(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT remote_session_id
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }

    pub async fn save_custom_title(
        &self,
        item_id: ItemId,
        workspace_id: WorkspaceId,
        custom_title: Option<String>,
    ) -> Result<()> {
        log::debug!(
            "Saving custom title {:?} for item {} in workspace {:?}",
            custom_title,
            item_id,
            workspace_id
        );
        self.write(move |conn| {
            let query = "INSERT INTO terminals (item_id, workspace_id, custom_title)
                VALUES (?1, ?2, ?3)
                ON CONFLICT (workspace_id, item_id) DO UPDATE SET
                    custom_title = excluded.custom_title";
            let mut statement = Statement::prepare(conn, query)?;
            let mut next_index = statement.bind(&item_id, 1)?;
            next_index = statement.bind(&workspace_id, next_index)?;
            statement.bind(&custom_title, next_index)?;
            statement.exec()
        })
        .await
    }

    query! {
        pub fn get_custom_title(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<String>> {
            SELECT custom_title
            FROM terminals
            WHERE item_id = ? AND workspace_id = ?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 第 12 便までに保存されたレイアウトは、そのまま読めなければならない
    /// (読めたものは次の保存で新形式に移る)。
    #[test]
    fn the_terminal_only_layout_of_the_previous_build_still_parses() {
        let stored = r#"{
            "items": { "Pane": { "active": true, "children": [7, 9], "active_item": 9, "pinned_count": 1 } },
            "active_item_id": null
        }"#;
        let panel: SerializedTerminalPanel =
            serde_json::from_str(stored).expect("the old layout should still deserialize");
        let SerializedItems::WithSplits(SerializedPaneGroup::Pane(pane)) = &panel.items else {
            panic!("a stored split layout should come back as one");
        };
        assert_eq!(pane.children, vec![7, 9]);
        assert_eq!(pane.active_item, Some(9));
        assert_eq!(pane.pinned_count, 1);
        assert!(
            pane.items.is_empty(),
            "an old pane has no per-kind items, which is what makes the restore take the old path"
        );
    }

    /// 分割が入る前の、さらに古い形式 (item_id の平坦な並び)。
    #[test]
    fn the_layout_from_before_splits_still_parses() {
        let panel: SerializedTerminalPanel =
            serde_json::from_str(r#"{"items": [1, 2, 3], "active_item_id": 2}"#)
                .expect("the pre-splits layout should still deserialize");
        let SerializedItems::NoSplits(item_ids) = &panel.items else {
            panic!("a flat list of item ids should stay flat");
        };
        assert_eq!(item_ids, &vec![1, 2, 3]);
    }

    #[test]
    fn the_per_kind_layout_round_trips_without_the_old_fields() {
        let panel = SerializedTerminalPanel {
            items: SerializedItems::WithSplits(SerializedPaneGroup::Pane(SerializedPane {
                active: true,
                items: vec![SerializedPanelItem {
                    kind: Arc::from("Browser"),
                    item_id: 12,
                    active: true,
                    preview: false,
                }],
                children: Vec::new(),
                active_item: None,
                pinned_count: 0,
            })),
            active_item_id: None,
        };
        let text = serde_json::to_string(&panel).expect("the layout should serialize");
        assert!(
            !text.contains("children") && !text.contains("active_item\""),
            "the migrated-away fields should not be written back: {text}"
        );

        let round_tripped: SerializedTerminalPanel =
            serde_json::from_str(&text).expect("the layout should deserialize");
        let SerializedItems::WithSplits(SerializedPaneGroup::Pane(pane)) = &round_tripped.items
        else {
            panic!("a split layout should come back as one");
        };
        assert_eq!(
            pane.items,
            vec![SerializedPanelItem {
                kind: Arc::from("Browser"),
                item_id: 12,
                active: true,
                preview: false,
            }]
        );
    }

    #[gpui::test]
    async fn test_remote_working_directory_is_stored_beside_the_other_columns() {
        // Opening a test database runs the migrations of that domain alone, and
        // the `terminals` rows reference `workspaces`; the two names match so
        // that both domains land in the same shared in-memory database, and the
        // first handle is held so it stays alive.
        let _workspaces = WorkspaceDb::open_test_db("test_remote_working_directory").await;
        let db = TerminalDb::open_test_db("test_remote_working_directory").await;
        let workspace_id = WorkspaceId::from_i64(1);
        db.write(|connection| {
            connection.exec("INSERT INTO workspaces (workspace_id) VALUES (1)")?()
        })
        .await
        .expect("the terminals row needs a workspace to reference");

        db.save_custom_title(1, workspace_id, Some("frontend".to_string()))
            .await
            .expect("saving a title");
        db.save_remote_working_directory(1, workspace_id, "/srv/app".to_string())
            .await
            .expect("saving a remote directory");

        assert_eq!(
            db.get_remote_working_directory(1, workspace_id)
                .expect("reading back the remote directory"),
            Some("/srv/app".to_string())
        );
        // The two writes upsert the same row, so neither may drop what the
        // other stored.
        assert_eq!(
            db.get_custom_title(1, workspace_id)
                .expect("reading back the title"),
            Some("frontend".to_string())
        );

        db.save_remote_working_directory(1, workspace_id, "/srv/app/logs".to_string())
            .await
            .expect("saving a later remote directory");
        assert_eq!(
            db.get_remote_working_directory(1, workspace_id)
                .expect("reading back the later remote directory"),
            Some("/srv/app/logs".to_string())
        );
    }

    #[gpui::test]
    async fn test_remote_session_id_is_stored_and_can_be_cleared() {
        let _workspaces = WorkspaceDb::open_test_db("test_remote_session_id").await;
        let db = TerminalDb::open_test_db("test_remote_session_id").await;
        let workspace_id = WorkspaceId::from_i64(1);
        db.write(|connection| {
            connection.exec("INSERT INTO workspaces (workspace_id) VALUES (1)")?()
        })
        .await
        .expect("the terminals row needs a workspace to reference");

        db.save_remote_working_directory(1, workspace_id, "/srv/app".to_string())
            .await
            .expect("saving a remote directory");
        db.save_remote_session_id(
            1,
            workspace_id,
            Some("0f8e5d7c-2a4b-4c6d-8e9f-001122334455".to_string()),
        )
        .await
        .expect("saving a session id");
        assert_eq!(
            db.get_remote_session_id(1, workspace_id)
                .expect("reading back the session id"),
            Some("0f8e5d7c-2a4b-4c6d-8e9f-001122334455".to_string())
        );
        assert_eq!(
            db.get_remote_working_directory(1, workspace_id)
                .expect("reading back the remote directory"),
            Some("/srv/app".to_string()),
            "the session id shares the row with the directory"
        );

        db.save_remote_session_id(1, workspace_id, None)
            .await
            .expect("clearing the session id");
        // A cleared column reads back empty, which restoring treats as none.
        assert_eq!(
            db.get_remote_session_id(1, workspace_id)
                .expect("reading back the cleared session id")
                .filter(|id| !id.is_empty()),
            None
        );
    }

    /// A database written by a build from before the column existed gets it
    /// on the next start, with every row it already had intact.
    #[test]
    fn test_remote_session_id_migration_runs_on_an_existing_database() {
        let connection = db::sqlez::connection::Connection::open_memory(Some(
            "test_remote_session_id_migration_runs_on_an_existing_database",
        ));
        connection
            .exec("CREATE TABLE workspaces (workspace_id INTEGER PRIMARY KEY)")
            .expect("preparing the workspaces table")()
        .expect("creating the workspaces table");
        let migrations = TerminalDb::MIGRATIONS;
        let previous_migrations = migrations
            .split_last()
            .map(|(_, previous)| previous)
            .expect("the terminal domain has migrations");
        connection
            .migrate(TerminalDb::NAME, previous_migrations, &mut |_, _, _| false)
            .expect("the previous build's migrations");
        connection
            .exec("INSERT INTO workspaces (workspace_id) VALUES (1)")
            .expect("preparing the workspace row")()
        .expect("inserting the workspace row");
        connection
            .exec(
                "INSERT INTO terminals (workspace_id, item_id, remote_working_directory, custom_title)
                 VALUES (1, 7, '/srv/app', 'frontend')",
            )
            .expect("preparing an old row")()
        .expect("inserting an old row");

        connection
            .migrate(TerminalDb::NAME, migrations, &mut |_, _, _| false)
            .expect("the current migrations run over the old database");

        let row = connection
            .select_row::<(Option<String>, Option<String>, Option<String>)>(
                "SELECT remote_working_directory, custom_title, remote_session_id
                 FROM terminals WHERE item_id = 7",
            )
            .expect("preparing the read")()
        .expect("reading the old row");
        assert_eq!(
            row,
            Some((
                Some("/srv/app".to_string()),
                Some("frontend".to_string()),
                None
            ))
        );
    }
}
