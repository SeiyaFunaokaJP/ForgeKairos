//! `openDiff` のためのタブ。
//!
//! 差分の描画そのものは `git_ui_core` の [`FileDiffView`] をそのまま使う (左が旧内容 =
//! `BufferDiff` の base text、右が新内容の作業用バッファー)。このタブはその上に
//! 「受け入れる / 拒否」の 1 行を足し、押されるまで待つ `oneshot` を持つだけ。
//!
//! **このタブは実ファイルをいっさい書かない**。編集はスクラッチバッファーの上で
//! 行われ、「採用」はその時点の本文を `FILE_SAVED` の 2 番目のテキストとして CLI に
//! 返すだけ (途中でユーザーが手直しした内容がそのまま採用される)。
//!
//! 名前に反して `FILE_SAVED` は「保存した」ではなく「この本文で承認した」という意味で、
//! ディスクに書くのは CLI 側の Edit / Write ツールである。CLI 2.1.261 は `openDiff` の
//! 結果を「保留中のツール呼び出しの入力を差し替える許可応答」に変換し (`Ho`)、その後
//! ツール本体が改めてファイルを読み、`readFileState` の mtime と内容を照合してから
//! 書き込む。IDE が先にディスクへ書くと、この照合が
//! 「File content has changed since it was last read」で落ちる。公式の VSCode 拡張も
//! 右側を独自スキームの仮想ドキュメントで開くだけで、実ファイルには触れていない。
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

/// CLI が待っている結果。`FILE_SAVED` のときは承認された本文も返す (ディスクへ書くのは
/// CLI 側)。
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
    outcome: Option<oneshot::Sender<IdeServerDiffOutcome>>,
}

/// `openDiff` で開く差分の中身。
pub(crate) struct ProposedDiff {
    pub tab_name: String,
    /// 旧内容 (左)。
    pub old_buffer: Entity<Buffer>,
    /// 提案された新内容 (右)。ユーザーが手直ししてから採用できる。
    pub new_buffer: Entity<Buffer>,
    pub new_file_path: PathBuf,
    pub take_focus: bool,
}

impl IdeServerDiff {
    /// 差分タブを開き、ユーザーの決定を待つ受信端を返す。
    ///
    /// `register` はタブをペインに出す直前に呼ぶ。出してから登録すると、そのあいだに
    /// 呼び出し元 (セッション) が終わったとき、誰にも閉じられないタブが残る。登録を
    /// 断られたらタブは出さない。
    pub(crate) fn open(
        workspace: WeakEntity<Workspace>,
        proposal: ProposedDiff,
        register: impl FnOnce(&Entity<Self>, &mut App) -> Result<()> + 'static,
        cx: &mut AsyncApp,
    ) -> Task<Result<oneshot::Receiver<IdeServerDiffOutcome>>> {
        cx.spawn(async move |cx| {
            let ProposedDiff {
                tab_name,
                old_buffer,
                new_buffer,
                new_file_path,
                take_focus,
            } = proposal;
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let diff = build_buffer_diff(&old_buffer, &new_buffer, cx).await?;

            let (outcome_tx, outcome_rx) = oneshot::channel();
            workspace.update_in(cx, |workspace, window, cx| {
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
                    outcome: Some(outcome_tx),
                });
                register(&diff_tab, cx)?;
                if take_focus {
                    workspace.add_item_to_active_pane(Box::new(diff_tab), None, true, window, cx);
                } else {
                    // フォーカスのあるタブと同じペインで差分を前面に出すと、隠れたタブから
                    // ペインへ戻ったフォーカスを、ペインが新しい前面のタブ (差分) へ渡して
                    // しまう。その場合はタブを足すだけにして、ユーザーのタブを前面に残す。
                    let pane = workspace.active_pane().clone();
                    let activate = !pane.read(cx).focus_handle(cx).contains_focused(window, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item_inner(
                            Box::new(diff_tab),
                            false,
                            false,
                            activate,
                            None,
                            window,
                            cx,
                        )
                    });
                }
                anyhow::Ok(())
            })??;
            Ok(outcome_rx)
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

    pub(crate) fn accept(&mut self, cx: &mut Context<Self>) {
        let contents = self.new_buffer.read(cx).text();
        self.resolve(IdeServerDiffOutcome::Accepted { contents }, cx);
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
pub(crate) async fn open_buffer(
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
                                    .disabled(decided)
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

    fn added_to_workspace(
        &mut self,
        workspace: &mut workspace::Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The diff this tab wraps owns the editors, and their focus listeners
        // belong to the window they were registered against.
        self.diff_view.update(cx, |diff_view, cx| {
            diff_view.added_to_workspace(workspace, window, cx);
        });
    }

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
