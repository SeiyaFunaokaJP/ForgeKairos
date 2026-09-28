//! What the graph's buttons, its toolbar and the commit menus say about each git operation, in
//! plain words: the tooltips, the line under the graph's buttons and the menus' side notes all
//! read from here so that they cannot drift apart.

use crate::commit_context_menu::CommitMenuOperation;
use ui::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationHelp {
    /// The operation's short name, with the git term in brackets where there is one.
    pub(crate) title: &'static str,
    pub(crate) explanation: &'static str,
    /// What the operation does that cannot be seen from the explanation and is easy to regret.
    pub(crate) caution: Option<&'static str>,
}

impl OperationHelp {
    /// The explanation and the caution as one sentence, for a tooltip or a side note.
    pub(crate) fn sentence(&self) -> String {
        match self.caution {
            Some(caution) => format!("{}。注意: {caution}", self.explanation),
            None => format!("{}。", self.explanation),
        }
    }

    /// The sentence led by the operation's name, for a line that is not next to the button.
    pub(crate) fn line(&self) -> String {
        format!("{}: {}", self.title, self.sentence())
    }
}

/// What each reset leaves of the changes after the commit, short enough for a menu item's label.
pub(crate) const RESET_SOFT_SUMMARY: &str = "変更をステージに残す";
pub(crate) const RESET_MIXED_SUMMARY: &str = "ステージだけ解除、ファイルは残す";
pub(crate) const RESET_HARD_SUMMARY: &str = "変更ごと捨てる";

const HISTORY_REWRITE_CAUTION: &str = "後のコミットの SHA が変わります";

pub(crate) const AMEND_HELP: OperationHelp = OperationHelp {
    title: "直前のコミットを修正 (amend)",
    explanation: "今の変更とメッセージで直前のコミットを作り直します (Git パネルで)",
    caution: Some("このコミットの SHA が変わります"),
};

pub(crate) const RESET_MENU_HELP: OperationHelp = OperationHelp {
    title: "このコミットまで戻す (reset)",
    explanation: "今のブランチの先頭をここまで戻します。後の変更の扱いを soft / mixed / hard から選びます",
    caution: None,
};

pub(crate) const MERGE_REQUEST_BOX_HELP: OperationHelp = OperationHelp {
    title: "MR の箱",
    explanation: "コミットを MR の箱に集めます。箱の中身から、MR 用に 1 コミットにまとめたブランチを作れます",
    caution: None,
};

pub(crate) const MERGE_REQUEST_EXCLUDE_HELP: OperationHelp = OperationHelp {
    title: "MR から除外する",
    explanation: "MR に入れないと決めたコミットとして印を付けます。入れ忘れの確認で「未配置」に数えなくなります。箱に入っていれば箱からも出します",
    caution: None,
};

pub(crate) const MERGE_REQUEST_RESTORE_HELP: OperationHelp = OperationHelp {
    title: "MR の除外をやめる",
    explanation: "除外の印を外して「未配置」に戻します",
    caution: None,
};

pub(crate) const CHERRY_PICK_SELECTION_HELP: OperationHelp = OperationHelp {
    title: "まとめて取り込む (cherry-pick)",
    explanation: "選んだコミットの変更を、古い順に今のブランチの上へコピーします。元のコミットは残ります",
    caution: None,
};

pub(crate) const REWRITE_SELECTION_HELP: OperationHelp = OperationHelp {
    title: "並べ替え / まとめる / 捨てる",
    explanation: "選んだ範囲を並べ替え・合体 (squash)・削除 (drop) します",
    caution: Some(HISTORY_REWRITE_CAUTION),
};

pub(crate) const SQUASH_SELECTION_HELP: OperationHelp = OperationHelp {
    title: "1 つにまとめる (squash)",
    explanation: "選んだ連続したコミットを 1 つのコミットに合体させます",
    caution: Some(HISTORY_REWRITE_CAUTION),
};

pub(crate) const MORE_MENU_HELP: OperationHelp = OperationHelp {
    title: "その他",
    explanation: "幅が足りずに並ばなかった操作です。開くと各操作の説明が横に出ます",
    caution: None,
};

pub(crate) const LOG_SOURCE_HELP: OperationHelp = OperationHelp {
    title: "表示",
    explanation: "グラフに出すコミットを、すべてのブランチ / 今のブランチ / 選んだブランチから選びます",
    caution: None,
};

pub(crate) const TAG_LIST_HELP: OperationHelp = OperationHelp {
    title: "タグ一覧",
    explanation: "タグ (コミットに付けた目印の名前) を検索し、選ぶとそのコミットまで移動します",
    caution: None,
};

pub(crate) const COMPARE_REFS_HELP: OperationHelp = OperationHelp {
    title: "比較",
    explanation: "2 つのブランチやタグの中身を比べます。移し忘れたコミットが無いかの確認に使えます",
    caution: None,
};

pub(crate) const MERGE_REQUEST_TOOLBAR_HELP: OperationHelp = OperationHelp {
    title: "MR を作る",
    explanation: "コミットを箱に集めて、MR (マージリクエスト) 用に 1 コミットにまとめたブランチを作ります",
    caution: None,
};

pub(crate) fn commit_operation_help(operation: CommitMenuOperation) -> OperationHelp {
    match operation {
        CommitMenuOperation::CreateBranch => OperationHelp {
            title: "ここからブランチ",
            explanation: "このコミットを出発点に、新しいブランチ (作業の枝) を作ります。今の作業はそのままです",
            caution: None,
        },
        CommitMenuOperation::Checkout => OperationHelp {
            title: "チェックアウト",
            explanation: "この時点に切り替えます (detached HEAD: どのブランチにも乗らない状態)",
            caution: Some("作業するなら先にブランチを作ります"),
        },
        CommitMenuOperation::CreateTag => OperationHelp {
            title: "タグ (tag)",
            explanation: "このコミットに目印の名前 (タグ) を付けます。リリースした版の記録などに使います",
            caution: None,
        },
        CommitMenuOperation::CherryPick => OperationHelp {
            title: "取り込む (cherry-pick)",
            explanation: "このコミットの変更だけを、今のブランチに新しいコミットとしてコピーします。元は残ります",
            caution: None,
        },
        CommitMenuOperation::Revert => OperationHelp {
            title: "打ち消す (revert)",
            explanation: "このコミットの変更を逆向きに当てる新しいコミットを作ります。履歴を消さないので共有済みでも安全です",
            caution: None,
        },
        CommitMenuOperation::Reword => OperationHelp {
            title: "メッセージの書き換え (reword)",
            explanation: "このコミットの説明文だけを直します",
            caution: Some(HISTORY_REWRITE_CAUTION),
        },
        CommitMenuOperation::Fixup => OperationHelp {
            title: "今の変更を入れる (fixup)",
            explanation: "未コミットの今の変更を、このコミットに後から足します",
            caution: Some(HISTORY_REWRITE_CAUTION),
        },
        CommitMenuOperation::Drop => OperationHelp {
            title: "捨てる (drop)",
            explanation: "このコミットとその変更を履歴から取り除きます",
            caution: Some(HISTORY_REWRITE_CAUTION),
        },
        CommitMenuOperation::ResetSoft => OperationHelp {
            title: "戻す: soft",
            explanation: "ブランチだけを戻します。後の変更はステージにも作業ツリーにもそのまま残ります",
            caution: None,
        },
        CommitMenuOperation::ResetMixed => OperationHelp {
            title: "戻す: mixed",
            explanation: "ブランチを戻し、ステージだけ解除します。後の変更は作業ツリーのファイルに残ります",
            caution: None,
        },
        CommitMenuOperation::ResetHard => OperationHelp {
            title: "戻す: hard",
            explanation: "ブランチを戻し、後の変更も未コミットの変更もまとめて捨てます",
            caution: Some("未コミットの変更は元に戻せません"),
        },
        CommitMenuOperation::CopyTagDistance => OperationHelp {
            title: "タグからの距離",
            explanation: "一番近いタグから何コミット先か (git describe の形) をコピーします",
            caution: None,
        },
    }
}

/// The side note of a menu item: why it is greyed out, if it is, then what it does.
pub(crate) fn help_aside(
    help: OperationHelp,
    refusal: Option<SharedString>,
) -> impl Fn(&mut App) -> AnyElement + 'static {
    move |_cx: &mut App| {
        v_flex()
            .gap_1()
            .when_some(refusal.clone(), |this, refusal| {
                this.child(Label::new(refusal))
            })
            .child(
                Label::new(help.explanation)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when_some(help.caution, |this, caution| {
                this.child(
                    Label::new(format!("注意: {caution}"))
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .into_any_element()
    }
}

// What the Git panel, its menus, the diff views and the pickers it opens say about each operation.

use crate::git_panel_settings::GitPanelSettings;
use gpui::{Action, AnyView, FocusHandle};
use settings::Settings as _;
use ui::{ContextMenuEntry, DocumentationSide, Tooltip};
use workspace::dock::DockPosition;

impl OperationHelp {
    /// The sentence followed by the git command it runs, for readers who know git already.
    pub(crate) fn sentence_with_command(&self, command: &str) -> String {
        format!("{} ({command})", self.sentence())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::EnumIter)]
pub(crate) enum PanelOperation {
    StageAll,
    UnstageAll,
    DiscardTrackedChanges,
    StashAll,
    StashTracked,
    StashStaged,
    StashPop,
    ViewStash,
    CopyPath,
    CopyRelativePath,
    TrashUntrackedFiles,
    CreatePatch,
    ApplyPatch,
    InitRepository,
    ShowGuide,
    ViewOptions,
    ListView,
    TreeView,
    SortByPath,
    SortByName,
    GroupByNone,
    GroupByStatus,
    GroupByStaging,
    Commit,
    Amend,
    Signoff,
    SkipHooks,
    Uncommit,
    CoAuthors,
    Fetch,
    FetchFrom,
    Pull,
    PullRebase,
    Push,
    PushTo,
    ForcePush,
    ForcePushWithoutLease,
    Publish,
    Republish,
    Merge,
    ResetToRemote,
    AddRemote,
    RemoveRemote,
    StageFile,
    UnstageFile,
    DiscardFileChanges,
    TrashNewFile,
    RestoreDeletedFile,
    ViewUnstagedChanges,
    ViewStagedChanges,
    AddToGitignore,
    AddToGitInfoExclude,
    OpenDiff,
    OpenFileDiff,
    ViewFile,
    FileHistory,
    StageFolder,
    UnstageFolder,
    StageSection,
    UnstageSection,
    ShowDiff,
    TrustDirectory,
    SwitchBranch,
    SequencerContinue,
    SequencerSkip,
    SequencerAbort,
    ToggleHunkStaged,
    StageHunk,
    UnstageHunk,
    RestoreHunk,
    RestoreAllUnstaged,
    ViewStashEntry,
    PopStashEntry,
    DropStashEntry,
    FilterBranches,
    ShowAllBranches,
    ShowLocalBranches,
    ShowRemoteBranches,
    DeleteBranch,
    ForceDeleteBranch,
    CreateBranchFromDefault,
    UseOurs,
    UseTheirs,
    UseBoth,
}

const UNDO_UNAVAILABLE_CAUTION: &str = "元に戻せません";
const PUSHED_COMMIT_CAUTION: &str = "送信 (push) 済みのコミットなら、強制プッシュが必要になります";
const CONFLICT_CAUTION: &str = "同じ箇所を変えているとコンフリクト (競合) になります";
const STASH_POP_CAUTION: &str = "今の変更と同じファイルを変えていると、止まるかコンフリクト (競合) になります。コンフリクトしたときは棚にも残ります";
const DISCARD_UNSTAGED_CAUTION: &str = "ステージしていない編集は消えます";

pub(crate) fn panel_operation_help(operation: PanelOperation) -> OperationHelp {
    match operation {
        PanelOperation::StageAll => OperationHelp {
            title: "すべてステージ (stage)",
            explanation: "変更したファイルを全部、次のコミットに入れる印 (ステージ) を付けます。ファイルの中身は変わりません",
            caution: None,
        },
        PanelOperation::UnstageAll => OperationHelp {
            title: "すべてアンステージ (unstage)",
            explanation: "ステージの印を全部外します。変更はファイルに残り、次のコミットに入らなくなるだけです",
            caution: None,
        },
        PanelOperation::DiscardTrackedChanges => OperationHelp {
            title: "追跡中の変更を破棄 (restore)",
            explanation: "ステージした変更がある追跡中のファイルを、ステージしていない変更ごと最後のコミットの内容に戻します。ステージしていない変更しか無いファイルと、新しく作ったファイル (未追跡) は残ります",
            caution: Some(UNDO_UNAVAILABLE_CAUTION),
        },
        PanelOperation::StashAll => OperationHelp {
            title: "すべて退避 (stash)",
            explanation: "未コミットの変更を新しく作ったファイル (未追跡) も含めて全部、一時的な棚 (stash) に退避して、ファイルを最後のコミットの状態にします。後で「スタッシュをポップ」で戻せます",
            caution: None,
        },
        PanelOperation::StashTracked => OperationHelp {
            title: "追跡中を退避 (stash)",
            explanation: "Git が管理しているファイル (追跡中) の変更だけを棚 (stash) に退避します。新しく作ったファイルは残ります",
            caution: None,
        },
        PanelOperation::StashStaged => OperationHelp {
            title: "ステージ済みを退避 (stash --staged)",
            explanation: "ステージした変更だけを棚 (stash) に退避します。ステージしていない変更はファイルに残ります",
            caution: None,
        },
        PanelOperation::StashPop => OperationHelp {
            title: "スタッシュをポップ (stash pop)",
            explanation: "棚 (stash) に最後に退避した変更をファイルに戻し、棚からは消します",
            caution: Some(STASH_POP_CAUTION),
        },
        PanelOperation::ViewStash => OperationHelp {
            title: "スタッシュを表示 (stash list)",
            explanation: "棚 (stash) に退避した変更の一覧を開きます。中身を見る・取り出す・捨てるができます",
            caution: None,
        },
        PanelOperation::CopyPath => OperationHelp {
            title: "パスをコピー",
            explanation: "ファイルの場所をフルパス (絶対パス) でクリップボードにコピーします",
            caution: None,
        },
        PanelOperation::CopyRelativePath => OperationHelp {
            title: "相対パスをコピー",
            explanation: "ファイルの場所をプロジェクトのフォルダーから見た相対パスでクリップボードにコピーします",
            caution: None,
        },
        PanelOperation::TrashUntrackedFiles => OperationHelp {
            title: "未追跡ファイルをゴミ箱へ",
            explanation: "Git がまだ管理していない新しいファイル (未追跡) を、OS のゴミ箱へ移します。ゴミ箱からは戻せます",
            caution: Some("Git の履歴には無いので、ゴミ箱を空にすると戻せません"),
        },
        PanelOperation::CreatePatch => OperationHelp {
            title: "パッチファイルを作成 (diff)",
            explanation: "変更点だけを書いたテキスト (パッチ) をファイルに書き出します。作業ツリー / ステージ済み / 直近のコミットから選べます。ファイル自体は変わりません",
            caution: None,
        },
        PanelOperation::ApplyPatch => OperationHelp {
            title: "パッチファイルを適用 (apply)",
            explanation: "パッチファイルに書かれた変更を今のファイルに当てます。コミットはされないので、確かめてからコミットします",
            caution: None,
        },
        PanelOperation::InitRepository => OperationHelp {
            title: "リポジトリを初期化 (init)",
            explanation: "このフォルダーで Git を使い始めます (.git フォルダーを作ります)。今あるファイルは変わりません",
            caution: None,
        },
        PanelOperation::ShowGuide => OperationHelp {
            title: "Git の操作ガイド",
            explanation: "Git の用語と、よく使う操作の手順を説明するガイドを開きます",
            caution: None,
        },
        PanelOperation::ViewOptions => OperationHelp {
            title: "表示オプション",
            explanation: "変更の一覧の見せ方 (リスト / ツリー、並び順、グループ分け) を変えます。ファイルには何もしません",
            caution: None,
        },
        PanelOperation::ListView => OperationHelp {
            title: "リスト表示",
            explanation: "変更したファイルをフォルダーに分けず 1 列に並べます",
            caution: None,
        },
        PanelOperation::TreeView => OperationHelp {
            title: "ツリー表示",
            explanation: "変更したファイルをフォルダーの階層ごとにまとめて、開いたり閉じたりできるように表示します",
            caution: None,
        },
        PanelOperation::SortByPath => OperationHelp {
            title: "パス順",
            explanation: "フォルダーを含めた場所 (パス) の順に並べます。同じフォルダーのファイルが隣り合います",
            caution: None,
        },
        PanelOperation::SortByName => OperationHelp {
            title: "名前順",
            explanation: "フォルダーは気にせず、ファイル名の順に並べます",
            caution: None,
        },
        PanelOperation::GroupByNone => OperationHelp {
            title: "グループなし",
            explanation: "すべての変更を分けずに 1 つの一覧にまとめます",
            caution: None,
        },
        PanelOperation::GroupByStatus => OperationHelp {
            title: "追跡中と未追跡で分ける",
            explanation: "Git が管理しているファイル (追跡中) と、まだ管理していない新しいファイル (未追跡) に分けて並べます",
            caution: None,
        },
        PanelOperation::GroupByStaging => OperationHelp {
            title: "ステージ済みと未ステージで分ける",
            explanation: "次のコミットに入れる印を付けた変更 (ステージ済み) と、まだ付けていない変更 (未ステージ) に分けて並べます",
            caution: None,
        },
        PanelOperation::Commit => OperationHelp {
            title: "コミット (commit)",
            explanation: "ステージした変更を、メッセージ付きの 1 つの記録 (コミット) として保存します。ステージが無いときは追跡中の変更を全部入れます",
            caution: None,
        },
        PanelOperation::Amend => OperationHelp {
            title: "直前のコミットを修正 (amend)",
            explanation: "次のコミットで新しいコミットを足す代わりに、直前のコミットを今の変更とメッセージで作り直します",
            caution: Some(PUSHED_COMMIT_CAUTION),
        },
        PanelOperation::Signoff => OperationHelp {
            title: "サインオフ (signoff)",
            explanation: "コミットメッセージの最後に「Signed-off-by: 名前 <メール>」の行を足します。プロジェクトの決まりで求められたときに使います",
            caution: None,
        },
        PanelOperation::SkipHooks => OperationHelp {
            title: "フックをスキップ (--no-verify)",
            explanation: "コミットの前に自動で走るチェック (フック) を飛ばしてコミットします",
            caution: Some("チェックで止まるはずの問題がそのまま入ることがあります"),
        },
        PanelOperation::Uncommit => OperationHelp {
            title: "コミットを取り消す (reset --soft)",
            explanation: "直前のコミットを取り消して、その変更をステージされた未コミットの状態に戻します。ファイルの中身は変わりません",
            caution: Some(PUSHED_COMMIT_CAUTION),
        },
        PanelOperation::CoAuthors => OperationHelp {
            title: "共同作成者 (Co-authored-by)",
            explanation: "一緒に作業した人の名前を、コミットメッセージの「Co-authored-by」の行で残します",
            caution: None,
        },
        PanelOperation::Fetch => OperationHelp {
            title: "フェッチ (fetch)",
            explanation: "リモート (共有先のサーバー) の最新を取ってくるだけです。自分のファイルやブランチは変わりません",
            caution: None,
        },
        PanelOperation::FetchFrom => OperationHelp {
            title: "取得元を指定してフェッチ (fetch)",
            explanation: "登録しているリモートから 1 つを選んで、その最新を取ってくるだけです。ファイルは変わりません",
            caution: None,
        },
        PanelOperation::Pull => OperationHelp {
            title: "プル (pull)",
            explanation: "リモートの最新を取ってきて、今のブランチに取り込みます (フェッチ + マージ。git の設定 pull.rebase があれば、それに従ってリベースします)",
            caution: Some(CONFLICT_CAUTION),
        },
        PanelOperation::PullRebase => OperationHelp {
            title: "リベースしてプル (pull --rebase)",
            explanation: "リモートの最新を取り込み、まだ送っていない自分のコミットをその後ろに付け直します。履歴が一直線になります",
            caution: Some("付け直した自分のコミットの SHA が変わります"),
        },
        PanelOperation::Push => OperationHelp {
            title: "プッシュ (push)",
            explanation: "自分のコミットをリモートへ送ります。コミットしていない変更は送られません",
            caution: None,
        },
        PanelOperation::PushTo => OperationHelp {
            title: "プッシュ先を指定してプッシュ (push)",
            explanation: "送り先のリモートを選んでから、自分のコミットを送ります",
            caution: None,
        },
        PanelOperation::ForcePush => OperationHelp {
            title: "強制プッシュ (push --force-with-lease)",
            explanation: "リモートのブランチを自分の履歴で上書きします。最後にフェッチした後に他の人が送ったコミットがあれば止まります",
            caution: Some("上書きされた側のコミットはリモートのブランチから消えます"),
        },
        PanelOperation::ForcePushWithoutLease => OperationHelp {
            title: "強制プッシュ (lease なし, push --force)",
            explanation: "確かめずに、リモートのブランチを自分の履歴で上書きします",
            caution: Some(
                "強制プッシュは相手の変更を消すことがあります。他の人が送ったコミットも黙って消えます",
            ),
        },
        PanelOperation::Publish => OperationHelp {
            title: "発行 (push --set-upstream)",
            explanation: "このブランチをリモートに初めて送り、以後のプッシュ / プルの相手として結び付けます",
            caution: None,
        },
        PanelOperation::Republish => OperationHelp {
            title: "再発行 (push --set-upstream)",
            explanation: "リモートで消えたブランチを、もう一度リモートに送って結び付け直します",
            caution: None,
        },
        PanelOperation::Merge => OperationHelp {
            title: "ブランチをマージ (merge)",
            explanation: "選んだブランチの変更を今のブランチに取り込みます。両方の履歴が残ります。今のブランチにも新しいコミットがあれば、合流したコミット (マージコミット) ができます",
            caution: Some(CONFLICT_CAUTION),
        },
        PanelOperation::ResetToRemote => OperationHelp {
            title: "リモートに強制的に合わせる (reset --hard)",
            explanation: "リモートをフェッチしてから、今のブランチをリモートのブランチと同じ内容にします",
            caution: Some(
                "未コミットの変更と、リモートに無い自分のコミットは捨てられ、元に戻せません",
            ),
        },
        PanelOperation::AddRemote => OperationHelp {
            title: "リモートを追加 (remote add)",
            explanation: "送り先 / 取得元のサーバー (リモート) を、名前と URL で登録します",
            caution: None,
        },
        PanelOperation::RemoveRemote => OperationHelp {
            title: "リモートを削除 (remote remove)",
            explanation: "登録したリモートを外します。手元にある、そのリモートのブランチの記録も消えます。サーバー上のデータは消えません",
            caution: None,
        },
        PanelOperation::StageFile => OperationHelp {
            title: "ファイルをステージ (stage)",
            explanation: "このファイルの変更に、次のコミットに入れる印を付けます。ファイルの中身は変わりません",
            caution: None,
        },
        PanelOperation::UnstageFile => OperationHelp {
            title: "ファイルをアンステージ (unstage)",
            explanation: "このファイルのステージの印を外します。変更はファイルに残ります",
            caution: None,
        },
        PanelOperation::DiscardFileChanges => OperationHelp {
            title: "変更を破棄 (restore)",
            explanation: "このファイルの変更を破棄し、最後のコミットの内容に戻します",
            caution: Some(UNDO_UNAVAILABLE_CAUTION),
        },
        PanelOperation::TrashNewFile => OperationHelp {
            title: "ファイルをゴミ箱へ",
            explanation: "まだ一度もコミットしていない新しいファイルを、OS のゴミ箱へ移します。ゴミ箱からは戻せます",
            caution: Some("Git の履歴には無いので、ゴミ箱を空にすると戻せません"),
        },
        PanelOperation::RestoreDeletedFile => OperationHelp {
            title: "ファイルを元に戻す (restore)",
            explanation: "削除したファイルを、最後のコミットの内容で復元します",
            caution: None,
        },
        PanelOperation::ViewUnstagedChanges => OperationHelp {
            title: "未ステージの変更",
            explanation: "まだステージしていない (次のコミットに入らない) 変更だけを差分で表示します",
            caution: None,
        },
        PanelOperation::ViewStagedChanges => OperationHelp {
            title: "ステージ済みの変更",
            explanation: "ステージした (次のコミットに入る) 変更だけを差分で表示します",
            caution: None,
        },
        PanelOperation::AddToGitignore => OperationHelp {
            title: ".gitignore に追加",
            explanation: "このファイルを、リポジトリの一番上にある無視リスト (.gitignore) に書き足し、変更の一覧に出ないようにします。.gitignore の変更もコミットすれば、他の人にも効きます",
            caution: None,
        },
        PanelOperation::AddToGitInfoExclude => OperationHelp {
            title: ".git/info/exclude に追加",
            explanation: "このファイルを自分の手元だけの無視リスト (.git/info/exclude) に書き足します。コミットされず、他の人には効きません",
            caution: None,
        },
        PanelOperation::OpenDiff => OperationHelp {
            title: "差分を開く",
            explanation: "変更をまとめて見るタブで、このファイルの変更前と変更後の違い (差分) を開きます",
            caution: None,
        },
        PanelOperation::OpenFileDiff => OperationHelp {
            title: "ファイルの差分を開く",
            explanation: "このファイルだけの差分 (変更前と変更後の違い) を別のタブで開きます",
            caution: None,
        },
        PanelOperation::ViewFile => OperationHelp {
            title: "ファイルを表示",
            explanation: "差分ではなく、今のファイルそのものをエディターで開きます",
            caution: None,
        },
        PanelOperation::FileHistory => OperationHelp {
            title: "ファイルの履歴 (log)",
            explanation: "このファイルを変えたコミットの一覧を、新しい順に開きます",
            caution: None,
        },
        PanelOperation::StageFolder => OperationHelp {
            title: "フォルダーをステージ (stage)",
            explanation: "このフォルダーの中の変更を全部、次のコミットに入れる印を付けます",
            caution: None,
        },
        PanelOperation::UnstageFolder => OperationHelp {
            title: "フォルダーをアンステージ (unstage)",
            explanation: "このフォルダーの中のステージの印を全部外します。変更はファイルに残ります",
            caution: None,
        },
        PanelOperation::StageSection => OperationHelp {
            title: "見出しの中をすべてステージ (stage)",
            explanation: "この見出しの下にある変更を全部、次のコミットに入れる印を付けます",
            caution: None,
        },
        PanelOperation::UnstageSection => OperationHelp {
            title: "見出しの中をすべてアンステージ (unstage)",
            explanation: "この見出しの下にある変更のステージの印を全部外します。変更はファイルに残ります",
            caution: None,
        },
        PanelOperation::ShowDiff => OperationHelp {
            title: "差分を表示 (diff)",
            explanation: "未コミットの変更の差分 (変更前と変更後の違い) を、1 つのタブにまとめて表示します",
            caution: None,
        },
        PanelOperation::TrustDirectory => OperationHelp {
            title: "ディレクトリを信頼 (safe.directory)",
            explanation: "持ち主が別のユーザーのフォルダーでも、Git で扱ってよい場所として登録します",
            caution: Some("自分のユーザーの Git 設定全体 (--global) に書き込みます"),
        },
        PanelOperation::SwitchBranch => OperationHelp {
            title: "ブランチを切り替え (switch)",
            explanation: "作業するブランチ (作業の枝) を選び替えます。ファイルがそのブランチの内容になります",
            caution: None,
        },
        PanelOperation::SequencerContinue => OperationHelp {
            title: "続行 (--continue)",
            explanation: "コンフリクトを解決してステージした状態で、途中で止まった操作の残りを進めます",
            caution: None,
        },
        PanelOperation::SequencerSkip => OperationHelp {
            title: "スキップ (--skip)",
            explanation: "今止まっているコミットを飛ばして、次のコミットへ進みます",
            caution: Some("飛ばしたコミットの変更は取り込まれません"),
        },
        PanelOperation::SequencerAbort => OperationHelp {
            title: "中止 (--abort)",
            explanation: "操作をやめて、始める前の状態に戻します",
            caution: Some("途中で解決した内容は捨てられます"),
        },
        PanelOperation::ToggleHunkStaged => OperationHelp {
            title: "ステージ状態を切り替え",
            explanation: "選んだ変更箇所 (ハンク: ひとまとまりの変更) のステージの印を、付けたり外したりします",
            caution: None,
        },
        PanelOperation::StageHunk => OperationHelp {
            title: "変更箇所をステージ (stage)",
            explanation: "この変更箇所 (ハンク: ひとまとまりの変更) だけに、次のコミットに入れる印を付けます。ファイルの中身は変わりません",
            caution: None,
        },
        PanelOperation::UnstageHunk => OperationHelp {
            title: "変更箇所をアンステージ (unstage)",
            explanation: "この変更箇所 (ハンク) のステージの印を外します。変更はファイルに残ります",
            caution: None,
        },
        PanelOperation::RestoreHunk => OperationHelp {
            title: "変更箇所を元に戻す (restore)",
            explanation: "この変更箇所 (ハンク) の編集を取り消し、ファイルをステージ済みの内容に戻します",
            caution: Some(DISCARD_UNSTAGED_CAUTION),
        },
        PanelOperation::RestoreAllUnstaged => OperationHelp {
            title: "すべて元に戻す (restore)",
            explanation: "ステージしていない変更を全部取り消し、ファイルをステージ済みの内容に戻します。新しく作ったファイルは残ります",
            caution: Some(DISCARD_UNSTAGED_CAUTION),
        },
        PanelOperation::ViewStashEntry => OperationHelp {
            title: "スタッシュを表示",
            explanation: "棚 (stash) に退避したこの変更の中身を、差分で表示します。取り出しはしません",
            caution: None,
        },
        PanelOperation::PopStashEntry => OperationHelp {
            title: "スタッシュを取り出す (stash pop)",
            explanation: "棚 (stash) に退避したこの変更をファイルに戻し、棚からは消します",
            caution: Some(STASH_POP_CAUTION),
        },
        PanelOperation::DropStashEntry => OperationHelp {
            title: "スタッシュを破棄 (stash drop)",
            explanation: "棚 (stash) に退避したこの変更を、ファイルに戻さずに捨てます",
            caution: Some(UNDO_UNAVAILABLE_CAUTION),
        },
        PanelOperation::FilterBranches => OperationHelp {
            title: "ブランチを絞り込み",
            explanation: "一覧に出すブランチを、すべて / 手元 (ローカル) / サーバー側 (リモート) から選びます",
            caution: None,
        },
        PanelOperation::ShowAllBranches => OperationHelp {
            title: "すべてのブランチ",
            explanation: "手元のブランチ (ローカル) と、サーバーにあるブランチの写し (リモート) を両方出します",
            caution: None,
        },
        PanelOperation::ShowLocalBranches => OperationHelp {
            title: "ローカルブランチ",
            explanation: "手元のリポジトリにあるブランチだけを出します。コミットできるのはこちらです",
            caution: None,
        },
        PanelOperation::ShowRemoteBranches => OperationHelp {
            title: "リモートブランチ",
            explanation: "最後のフェッチで取ってきた、サーバー側のブランチの写し (origin/... など) だけを出します",
            caution: None,
        },
        PanelOperation::DeleteBranch => OperationHelp {
            title: "ブランチを削除 (branch -d)",
            explanation: "手元のブランチを消します。今のブランチ (または追跡先) に取り込まれていないコミットがあると止まり、強制削除するかを尋ねます。リモートブランチなら手元の写しだけが消え、サーバーのブランチは残ります",
            caution: None,
        },
        PanelOperation::ForceDeleteBranch => OperationHelp {
            title: "ブランチを強制削除 (branch -D)",
            explanation: "取り込んでいないコミットがあっても、手元のブランチを消します",
            caution: Some("そのブランチにしか無いコミットは見えなくなります"),
        },
        PanelOperation::CreateBranchFromDefault => OperationHelp {
            title: "既定のブランチから新規作成",
            explanation: "今のブランチではなく、既定のブランチ (main など) を出発点に新しいブランチを作ります",
            caution: None,
        },
        PanelOperation::UseOurs => OperationHelp {
            title: "上側を使用 (ours)",
            explanation: "競合した箇所を、印の上側 (<<<<<<< の側、ours) の内容だけにします。下側の内容は捨てます。マージ中は今いるブランチ側、リベース中は付け直し先のブランチ側です",
            caution: None,
        },
        PanelOperation::UseTheirs => OperationHelp {
            title: "下側を使用 (theirs)",
            explanation: "競合した箇所を、印の下側 (>>>>>>> の側、theirs) の内容だけにします。上側の内容は捨てます。マージ中は取り込もうとしている相手側、リベース中は付け直している自分のコミット側です",
            caution: None,
        },
        PanelOperation::UseBoth => OperationHelp {
            title: "両方を使用",
            explanation: "競合した箇所に、上側 (ours) と下側 (theirs) の内容を両方、上下に並べて残します",
            caution: None,
        },
    }
}

/// Why a greyed-out item of the Git panel's menus cannot be chosen.
pub(crate) const NOTHING_TO_STAGE_REFUSAL: &str = "ステージする変更がありません";
pub(crate) const NOTHING_TO_UNSTAGE_REFUSAL: &str = "ステージ済みの変更がありません";
pub(crate) const NO_TRACKED_CHANGES_REFUSAL: &str = "追跡中のファイルに変更がありません";
pub(crate) const NO_STAGED_TRACKED_CHANGES_REFUSAL: &str =
    "ステージ済みの、追跡中のファイルの変更がありません";
pub(crate) const NOTHING_TO_STASH_REFUSAL: &str = "退避する変更がありません";
pub(crate) const NO_STASH_REFUSAL: &str = "棚 (stash) に退避した変更がありません";
pub(crate) const NO_UNTRACKED_FILES_REFUSAL: &str = "未追跡のファイルがありません";
pub(crate) const ONLY_FOR_UNTRACKED_REFUSAL: &str =
    "まだ Git が管理していない新しいファイル (未追跡) だけに使えます";

/// The side of a Git panel menu that its side notes go on. The panel's menus open inside the
/// panel, so a note on the side of the dock's window edge would run off the window. In the
/// bottom dock the panel spans the window, and its menu buttons sit at its right end.
pub(crate) fn panel_aside_side(cx: &App) -> DocumentationSide {
    match GitPanelSettings::get_global(cx).dock {
        DockPosition::Right | DockPosition::Bottom => DocumentationSide::Left,
        DockPosition::Left => DocumentationSide::Right,
    }
}

/// A menu item that runs `action` like `ContextMenu::action` does, explained on the side. It is
/// greyed out exactly when there is a `refusal`, so that no greyed-out item goes unexplained.
pub(crate) fn help_action_entry(
    label: impl Into<SharedString>,
    action: Box<dyn Action>,
    focus_handle: Option<FocusHandle>,
    help: OperationHelp,
    refusal: Option<SharedString>,
    side: DocumentationSide,
) -> ContextMenuEntry {
    let disabled = refusal.is_some();
    ContextMenuEntry::new(label)
        .action(action.boxed_clone())
        .handler(move |window, cx| {
            // The action is handled by whatever held focus before the menu opened, as
            // `ContextMenu::action` does it.
            if let Some(focus_handle) = &focus_handle {
                window.focus(focus_handle, cx);
            }
            window.dispatch_action(action.boxed_clone(), cx);
        })
        .disabled(disabled)
        .documentation_aside(side, help_aside(help, refusal))
}

/// A button's tooltip that keeps the button's own title and explains the operation under it.
pub(crate) fn help_tooltip(
    title: impl Into<SharedString>,
    meta: impl Into<SharedString>,
    action: Option<Box<dyn Action>>,
    focus_handle: Option<FocusHandle>,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    let title = title.into();
    let meta = meta.into();
    move |_window, cx| match &focus_handle {
        Some(focus_handle) => Tooltip::with_meta_in(
            title.clone(),
            action.as_deref(),
            meta.clone(),
            focus_handle,
            cx,
        ),
        None => Tooltip::with_meta(title.clone(), action.as_deref(), meta.clone(), cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sentences_end_once_and_carry_the_caution() {
        let help = commit_operation_help(CommitMenuOperation::ResetHard);
        assert_eq!(
            help.sentence(),
            "ブランチを戻し、後の変更も未コミットの変更もまとめて捨てます。注意: 未コミットの変更は元に戻せません"
        );
        assert!(help.line().starts_with("戻す: hard: "));
        assert_eq!(
            commit_operation_help(CommitMenuOperation::ResetSoft).sentence(),
            "ブランチだけを戻します。後の変更はステージにも作業ツリーにもそのまま残ります。"
        );
    }

    /// Each operation is explained in its own words: an explanation that went to the wrong
    /// operation still reads well, so only the words that belong to it catch the mix-up.
    #[test]
    fn test_each_operation_explains_itself() {
        let expectations = [
            (
                CommitMenuOperation::CreateBranch,
                "ブランチ",
                "新しいブランチ",
            ),
            (
                CommitMenuOperation::Checkout,
                "チェックアウト",
                "detached HEAD",
            ),
            (CommitMenuOperation::CreateTag, "tag", "タグ"),
            (CommitMenuOperation::CherryPick, "cherry-pick", "コピー"),
            (CommitMenuOperation::Revert, "revert", "逆向き"),
            (CommitMenuOperation::Reword, "reword", "説明文"),
            (CommitMenuOperation::Fixup, "fixup", "未コミット"),
            (CommitMenuOperation::Drop, "drop", "取り除き"),
            (CommitMenuOperation::ResetSoft, "soft", "ブランチだけ"),
            (CommitMenuOperation::ResetMixed, "mixed", "ステージだけ解除"),
            (CommitMenuOperation::ResetHard, "hard", "捨て"),
            (CommitMenuOperation::CopyTagDistance, "タグ", "git describe"),
        ];
        for (operation, in_title, in_explanation) in expectations {
            let help = commit_operation_help(operation);
            assert!(help.title.contains(in_title), "{operation:?}: {help:?}");
            assert!(
                help.explanation.contains(in_explanation),
                "{operation:?}: {help:?}"
            );
        }
    }

    /// Each Git panel operation is explained in its own words, and none is left out: a new
    /// operation without a row here fails the coverage check below.
    #[test]
    fn test_each_panel_operation_explains_itself() {
        use strum::IntoEnumIterator as _;

        let expectations = [
            (PanelOperation::StageAll, "stage", "次のコミットに入れる印"),
            (PanelOperation::UnstageAll, "unstage", "印を全部外"),
            (
                PanelOperation::DiscardTrackedChanges,
                "破棄",
                "最後のコミットの内容",
            ),
            (PanelOperation::StashAll, "stash", "棚"),
            (PanelOperation::StashTracked, "stash", "追跡中"),
            (
                PanelOperation::StashStaged,
                "--staged",
                "ステージした変更だけ",
            ),
            (PanelOperation::StashPop, "pop", "棚からは消"),
            (PanelOperation::ViewStash, "stash list", "一覧"),
            (PanelOperation::CopyPath, "パス", "フルパス"),
            (
                PanelOperation::CopyRelativePath,
                "相対パス",
                "プロジェクトのフォルダー",
            ),
            (PanelOperation::TrashUntrackedFiles, "ゴミ箱", "未追跡"),
            (PanelOperation::CreatePatch, "パッチ", "書き出"),
            (PanelOperation::ApplyPatch, "apply", "当て"),
            (PanelOperation::InitRepository, "init", ".git フォルダー"),
            (PanelOperation::ShowGuide, "ガイド", "用語"),
            (PanelOperation::ViewOptions, "表示", "見せ方"),
            (PanelOperation::ListView, "リスト", "1 列"),
            (PanelOperation::TreeView, "ツリー", "階層"),
            (PanelOperation::SortByPath, "パス", "場所 (パス) の順"),
            (PanelOperation::SortByName, "名前", "ファイル名の順"),
            (PanelOperation::GroupByNone, "なし", "1 つの一覧"),
            (
                PanelOperation::GroupByStatus,
                "未追跡",
                "まだ管理していない",
            ),
            (
                PanelOperation::GroupByStaging,
                "未ステージ",
                "印を付けた変更",
            ),
            (PanelOperation::Commit, "commit", "1 つの記録"),
            (PanelOperation::Amend, "amend", "作り直し"),
            (PanelOperation::Signoff, "signoff", "Signed-off-by"),
            (PanelOperation::SkipHooks, "--no-verify", "フック"),
            (PanelOperation::Uncommit, "取り消す", "未コミットの状態"),
            (
                PanelOperation::CoAuthors,
                "Co-authored-by",
                "一緒に作業した人",
            ),
            (PanelOperation::Fetch, "fetch", "取ってくるだけ"),
            (PanelOperation::FetchFrom, "取得元", "1 つを選んで"),
            (PanelOperation::Pull, "pull", "取り込み"),
            (PanelOperation::PullRebase, "--rebase", "付け直し"),
            (PanelOperation::Push, "push", "リモートへ送り"),
            (PanelOperation::PushTo, "プッシュ先", "送り先"),
            (PanelOperation::ForcePush, "--force-with-lease", "上書き"),
            (
                PanelOperation::ForcePushWithoutLease,
                "--force",
                "確かめずに",
            ),
            (PanelOperation::Publish, "--set-upstream", "初めて送り"),
            (PanelOperation::Republish, "再発行", "消えたブランチ"),
            (PanelOperation::Merge, "merge", "合流"),
            (PanelOperation::ResetToRemote, "reset --hard", "同じ内容"),
            (PanelOperation::AddRemote, "remote add", "URL"),
            (PanelOperation::RemoveRemote, "remote remove", "外し"),
            (PanelOperation::StageFile, "ステージ", "このファイル"),
            (PanelOperation::UnstageFile, "アンステージ", "このファイル"),
            (
                PanelOperation::DiscardFileChanges,
                "破棄",
                "このファイルの変更",
            ),
            (PanelOperation::TrashNewFile, "ゴミ箱", "新しいファイル"),
            (
                PanelOperation::RestoreDeletedFile,
                "restore",
                "削除したファイル",
            ),
            (
                PanelOperation::ViewUnstagedChanges,
                "未ステージ",
                "次のコミットに入らない",
            ),
            (
                PanelOperation::ViewStagedChanges,
                "ステージ済み",
                "次のコミットに入る",
            ),
            (
                PanelOperation::AddToGitignore,
                ".gitignore",
                "他の人にも効き",
            ),
            (PanelOperation::AddToGitInfoExclude, "exclude", "手元だけ"),
            (PanelOperation::OpenDiff, "差分", "まとめて見るタブ"),
            (
                PanelOperation::OpenFileDiff,
                "ファイルの差分",
                "このファイルだけ",
            ),
            (PanelOperation::ViewFile, "ファイル", "ファイルそのもの"),
            (PanelOperation::FileHistory, "log", "コミットの一覧"),
            (PanelOperation::StageFolder, "フォルダー", "フォルダーの中"),
            (PanelOperation::UnstageFolder, "フォルダー", "印を全部外"),
            (PanelOperation::StageSection, "stage", "見出しの下"),
            (PanelOperation::UnstageSection, "unstage", "見出しの下"),
            (PanelOperation::ShowDiff, "diff", "1 つのタブ"),
            (PanelOperation::TrustDirectory, "safe.directory", "持ち主"),
            (PanelOperation::SwitchBranch, "switch", "作業の枝"),
            (
                PanelOperation::SequencerContinue,
                "--continue",
                "残りを進め",
            ),
            (PanelOperation::SequencerSkip, "--skip", "飛ばし"),
            (PanelOperation::SequencerAbort, "--abort", "始める前"),
            (
                PanelOperation::ToggleHunkStaged,
                "切り替え",
                "付けたり外したり",
            ),
            (PanelOperation::StageHunk, "stage", "ハンク"),
            (PanelOperation::UnstageHunk, "unstage", "ハンク"),
            (PanelOperation::RestoreHunk, "元に戻す", "編集を取り消し"),
            (
                PanelOperation::RestoreAllUnstaged,
                "すべて",
                "ステージしていない変更を全部",
            ),
            (PanelOperation::ViewStashEntry, "表示", "取り出しはしません"),
            (PanelOperation::PopStashEntry, "取り出す", "棚からは消"),
            (PanelOperation::DropStashEntry, "drop", "捨て"),
            (PanelOperation::FilterBranches, "絞り込み", "ローカル"),
            (PanelOperation::ShowAllBranches, "すべて", "両方"),
            (PanelOperation::ShowLocalBranches, "ローカル", "手元"),
            (PanelOperation::ShowRemoteBranches, "リモート", "サーバー側"),
            (PanelOperation::DeleteBranch, "branch -d", "止まり"),
            (PanelOperation::ForceDeleteBranch, "branch -D", "あっても"),
            (PanelOperation::CreateBranchFromDefault, "既定", "main"),
            (PanelOperation::UseOurs, "ours", "リベース中は付け直し先"),
            (
                PanelOperation::UseTheirs,
                "theirs",
                "リベース中は付け直している自分",
            ),
            (PanelOperation::UseBoth, "両方", "両方、上下に"),
        ];
        for (operation, in_title, in_explanation) in expectations {
            let help = panel_operation_help(operation);
            assert!(help.title.contains(in_title), "{operation:?}: {help:?}");
            assert!(
                help.explanation.contains(in_explanation),
                "{operation:?}: {help:?}"
            );
        }
        for operation in PanelOperation::iter() {
            assert!(
                expectations
                    .iter()
                    .any(|(expected, _, _)| *expected == operation),
                "{operation:?} has no expectation"
            );
        }
    }

    /// The operations that throw work away say so, and the harmless ones do not cry wolf.
    #[test]
    fn test_panel_operations_that_lose_work_carry_a_caution() {
        let losing_work = [
            (PanelOperation::DiscardTrackedChanges, "元に戻せません"),
            (PanelOperation::DiscardFileChanges, "元に戻せません"),
            (PanelOperation::DropStashEntry, "元に戻せません"),
            (PanelOperation::ResetToRemote, "元に戻せません"),
            (PanelOperation::ForcePushWithoutLease, "相手の変更を消す"),
            (PanelOperation::ForcePush, "消えます"),
            (PanelOperation::TrashUntrackedFiles, "ゴミ箱を空にすると"),
            (PanelOperation::Amend, "強制プッシュ"),
            (PanelOperation::Uncommit, "強制プッシュ"),
            (PanelOperation::SequencerAbort, "捨てられ"),
            (PanelOperation::SequencerSkip, "取り込まれません"),
            (PanelOperation::ForceDeleteBranch, "見えなくなり"),
            (PanelOperation::TrashNewFile, "ゴミ箱を空にすると"),
            (PanelOperation::RestoreHunk, "編集は消え"),
            (PanelOperation::RestoreAllUnstaged, "編集は消え"),
        ];
        for (operation, in_caution) in losing_work {
            let help = panel_operation_help(operation);
            assert!(
                help.caution
                    .is_some_and(|caution| caution.contains(in_caution)),
                "{operation:?}: {help:?}"
            );
        }
        for harmless in [
            PanelOperation::Fetch,
            PanelOperation::StageAll,
            PanelOperation::ViewStash,
            PanelOperation::ListView,
        ] {
            assert_eq!(panel_operation_help(harmless).caution, None);
        }
        assert_eq!(
            panel_operation_help(PanelOperation::Fetch).sentence_with_command("git fetch"),
            "リモート (共有先のサーバー) の最新を取ってくるだけです。自分のファイルやブランチは変わりません。 (git fetch)"
        );
    }

    /// A menu item runs its action after focusing what it was given, as `ContextMenu::action`
    /// does, and a greyed-out one runs nothing.
    #[gpui::test]
    fn test_help_action_entry_runs_its_action_unless_refused(cx: &mut gpui::TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        use ui::ContextMenu;

        let runs = Rc::new(Cell::new(0));
        cx.update(|cx| {
            let runs = runs.clone();
            cx.on_action(move |_: &git::Fetch, _| runs.set(runs.get() + 1));
        });
        let cx = cx.add_empty_window();
        for (refusal, expected_runs, expected_focus) in [
            (None, 1, true),
            (Some(SharedString::from("理由")), 1, false),
        ] {
            let target = cx.update(|_, cx| cx.focus_handle());
            let menu = cx.update(|window, cx| {
                let target = target.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(help_action_entry(
                        "フェッチ",
                        git::Fetch.boxed_clone(),
                        Some(target),
                        panel_operation_help(PanelOperation::Fetch),
                        refusal,
                        DocumentationSide::Left,
                    ))
                })
            });
            cx.update(|window, cx| {
                menu.update(cx, |menu, cx| {
                    menu.select_first(&menu::SelectFirst, window, cx);
                    menu.confirm(&menu::Confirm, window, cx);
                });
            });
            cx.run_until_parked();
            assert_eq!(runs.get(), expected_runs);
            cx.update(|window, _| assert_eq!(target.is_focused(window), expected_focus));
        }
    }
}
