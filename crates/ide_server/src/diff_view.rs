//! `openDiff` のためのタブ。
//!
//! 差分の描画そのものは `git_ui_core` の [`FileDiffView`] をそのまま使う (左が旧内容 =
//! `BufferDiff` の base text、右が新内容の作業用バッファー)。このタブはその上に
//! 「受け入れる / 拒否」の 1 行を足し、押されるまで待つ `oneshot` を持つだけ。
//!
//! **新内容はユーザーが受け入れるまで実ファイルに書かない**。編集はスクラッチ
//! バッファーの上で行われ、受け入れたときにその時点の本文を実ファイルのバッファーへ
//! 移して保存する (途中でユーザーが手直しした内容がそのまま採用される)。
//!
//! リモートプロジェクトでも動く。対象ファイルはワークツリー配下に解決できたものだけ。

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use futures::channel::oneshot;
use git_ui_core::file_diff_view::{FileDiffView, build_buffer_diff};
use gpui::{
    App, AppContext as _, AsyncApp, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window,
};
use language::Buffer;
use project::Project;
use ui::prelude::*;
use workspace::item::ItemEvent;
use workspace::{Item, Workspace};

/// CLI が待っている結果。`FILE_SAVED` のときは保存後の本文も返す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IdeServerDiffOutcome {
    Accepted { contents: String },
    Rejected,
}

pub enum IdeServerDiffEvent {
    Close,
}

pub struct IdeServerDiff {
    tab_name: SharedString,
    new_file_path: PathBuf,
    diff_view: Entity<FileDiffView>,
    new_buffer: Entity<Buffer>,
    project: Entity<Project>,
    outcome: Option<oneshot::Sender<IdeServerDiffOutcome>>,
    saving: bool,
}

impl IdeServerDiff {
    /// 差分タブを開き、ユーザーの決定を待つ受信端を返す。
    pub fn open(
        workspace: WeakEntity<Workspace>,
        tab_name: String,
        old_file_path: PathBuf,
        new_file_path: PathBuf,
        new_contents: String,
        cx: &mut AsyncApp,
    ) -> Task<Result<(Entity<Self>, oneshot::Receiver<IdeServerDiffOutcome>)>> {
        cx.spawn(async move |cx| {
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let old_buffer = open_buffer(&project, &old_file_path, cx)
                .await
                .with_context(|| format!("opening {} for the diff", old_file_path.display()))?;
            let language = old_buffer.read_with(cx, |buffer, _| buffer.language().cloned());
            // `create_local_buffer` はリモートプロジェクトで panic するので、
            // ローカル・リモート双方で動く `create_buffer` に本文を流し込む。
            let new_buffer = project
                .update(cx, |project, cx| project.create_buffer(language, false, cx))
                .await
                .context("creating the scratch buffer for the proposed contents")?;
            new_buffer.update(cx, |buffer, cx| {
                buffer.set_text(new_contents, cx);
            });
            let diff = build_buffer_diff(&old_buffer, &new_buffer, cx).await?;

            let (outcome_tx, outcome_rx) = oneshot::channel();
            let diff_tab = workspace.update_in(cx, |workspace, window, cx| {
                let workspace_entity = cx.entity();
                let diff_view = cx.new(|cx| {
                    FileDiffView::new(
                        old_buffer,
                        new_buffer.clone(),
                        diff,
                        project.clone(),
                        workspace_entity,
                        window,
                        cx,
                    )
                });
                let diff_tab = cx.new(|_| Self {
                    tab_name: tab_name.into(),
                    new_file_path,
                    diff_view,
                    new_buffer,
                    project,
                    outcome: Some(outcome_tx),
                    saving: false,
                });
                workspace.add_item_to_active_pane(
                    Box::new(diff_tab.clone()),
                    None,
                    true,
                    window,
                    cx,
                );
                diff_tab
            })?;
            Ok((diff_tab, outcome_rx))
        })
    }

    pub fn tab_name(&self) -> &SharedString {
        &self.tab_name
    }

    /// 外から (`close_tab` / `closeAllDiffTabs`) 閉じるときの入口。
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        self.resolve(IdeServerDiffOutcome::Rejected, cx);
    }

    fn resolve(&mut self, outcome: IdeServerDiffOutcome, cx: &mut Context<Self>) {
        if let Some(sender) = self.outcome.take() {
            sender.send(outcome).ok();
        }
        cx.emit(IdeServerDiffEvent::Close);
    }

    fn accept(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.saving = true;
        let contents = self.new_buffer.read(cx).text();
        let path = self.new_file_path.clone();
        let project = self.project.clone();
        cx.spawn(async move |this, cx| {
            let target = open_buffer(&project, &path, cx)
                .await
                .with_context(|| format!("opening {} to apply the diff", path.display()))?;
            target.update(cx, |buffer, cx| {
                buffer.set_text(contents.clone(), cx);
            });
            project
                .update(cx, |project, cx| project.save_buffer(target, cx))
                .await
                .with_context(|| format!("saving {}", path.display()))?;
            this.update(cx, |this, cx| {
                this.resolve(IdeServerDiffOutcome::Accepted { contents }, cx);
            })
        })
        .detach_and_log_err(cx);
    }

    fn reject(&mut self, cx: &mut Context<Self>) {
        self.resolve(IdeServerDiffOutcome::Rejected, cx);
    }
}

/// 差分の対象ファイルをバッファーとして開く。
///
/// `open_local_buffer` はワークツリー外のパスにも隠しワークツリーを作って対応するが、
/// リモートプロジェクトではその作成がホスト側に行ってしまう。リモートのときは
/// 既存のワークツリー配下に解決できたものだけを開き、外れていればエラーにする。
async fn open_buffer(
    project: &Entity<Project>,
    path: &Path,
    cx: &mut AsyncApp,
) -> Result<Entity<Buffer>> {
    if project.read_with(cx, |project, _| project.is_local()) {
        return project
            .update(cx, |project, cx| project.open_local_buffer(path, cx))
            .await;
    }
    let project_path = project
        .read_with(cx, |project, cx| project.find_project_path(path, cx))
        .with_context(|| {
            format!(
                "{} is not inside a worktree of this remote window",
                path.display()
            )
        })?;
    project
        .update(cx, |project, cx| project.open_buffer(project_path, cx))
        .await
}

impl Drop for IdeServerDiff {
    fn drop(&mut self) {
        // タブがボタンを経ずに閉じられたときは「拒否」として返す。CLI 側の
        // `TAB_CLOSED` は「提案どおり適用された」とみなされてしまうので使わない。
        if let Some(sender) = self.outcome.take() {
            sender.send(IdeServerDiffOutcome::Rejected).ok();
        }
    }
}

impl EventEmitter<IdeServerDiffEvent> for IdeServerDiff {}

impl Focusable for IdeServerDiff {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.diff_view.focus_handle(cx)
    }
}

impl Render for IdeServerDiff {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let decided = self.outcome.is_none();
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .w_full()
                    .p_1p5()
                    .gap_2()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        Label::new(if decided {
                            "この差分は処理済みです".to_string()
                        } else {
                            format!("エージェントの提案: {}", self.tab_name)
                        })
                        .color(Color::Muted),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("ide-server-diff-reject", "拒否")
                                    .disabled(decided)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.reject(cx);
                                    })),
                            )
                            .child(
                                Button::new("ide-server-diff-accept", "採用")
                                    .style(ButtonStyle::Tinted(ui::TintColor::Accent))
                                    .disabled(decided || self.saving)
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.accept(cx);
                                    })),
                            ),
                    ),
            )
            .child(div().flex_1().child(self.diff_view.clone()))
    }
}

impl Item for IdeServerDiff {
    type Event = IdeServerDiffEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.tab_name.clone()
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some(self.new_file_path.to_string_lossy().into_owned().into())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            IdeServerDiffEvent::Close => f(ItemEvent::CloseItem),
        }
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        None
    }
}
