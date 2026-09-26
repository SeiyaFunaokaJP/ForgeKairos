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
}
