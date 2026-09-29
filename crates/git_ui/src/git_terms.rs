//! What the graph's buttons, its toolbar and the commit menus say about each git operation, in
//! plain words: the tooltips, the line under the graph's buttons and the menus' side notes all
//! read from here so that they cannot drift apart.

use crate::commit_context_menu::CommitMenuOperation;
use ui::prelude::*;

/// An operation's words, already in the display language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OperationHelp {
    /// The operation's short name, with the git term in brackets where there is one. Titles are
    /// translated under the msgctxt "git operation": a translation may add the git term to a
    /// title ("Stage All") whose English is also a plain button label elsewhere.
    pub(crate) title: &'static str,
    pub(crate) explanation: &'static str,
    /// What the operation does that cannot be seen from the explanation and is easy to regret.
    pub(crate) caution: Option<&'static str>,
}

impl OperationHelp {
    /// The explanation and the caution as one sentence, for a tooltip or a side note.
    pub(crate) fn sentence(&self) -> String {
        let explanation = self.explanation;
        match self.caution {
            Some(caution) => i18n::t!("{explanation}. Caution: {caution}."),
            None => i18n::t!("{explanation}."),
        }
    }

    /// The sentence led by the operation's name, for a line that is not next to the button.
    pub(crate) fn line(&self) -> String {
        format!("{}: {}", self.title, self.sentence())
    }
}

/// What each reset leaves of the changes after the commit, short enough for a menu item's label.
/// Translate them with `i18n::tr` where they are shown.
pub(crate) const RESET_SOFT_SUMMARY: &str = i18n::msg!("keep the changes staged");
pub(crate) const RESET_MIXED_SUMMARY: &str = i18n::msg!("unstage only, keep the files");
pub(crate) const RESET_HARD_SUMMARY: &str = i18n::msg!("discard the changes too");

fn history_rewrite_caution() -> &'static str {
    i18n::t!("The SHAs of the later commits change")
}

pub(crate) fn amend_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Amend Last Commit"),
        explanation: i18n::t!("Rebuilds the last commit from your changes and message (Git panel)"),
        caution: Some(i18n::t!("This commit's SHA changes")),
    }
}

pub(crate) fn reset_menu_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Reset to This Commit"),
        explanation: i18n::t!(
            "Moves the current branch back to here. Choose soft / mixed / hard for the later changes"
        ),
        caution: None,
    }
}

pub(crate) fn merge_request_box_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "MR Box"),
        explanation: i18n::t!(
            "Collects commits in the MR box. From them you can create a branch squashed into one commit for the MR"
        ),
        caution: None,
    }
}

pub(crate) fn merge_request_exclude_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Exclude from MR"),
        explanation: i18n::t!(
            "Marks the commit as one you decided to keep out of the MR, so that the check for forgotten commits no longer counts it as \"unplaced\". If it is in the box, it is taken out too"
        ),
        caution: None,
    }
}

pub(crate) fn merge_request_restore_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Stop Excluding from MR"),
        explanation: i18n::t!("Removes the exclusion mark and makes the commit \"unplaced\" again"),
        caution: None,
    }
}

pub(crate) fn cherry_pick_selection_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Cherry-Pick Selected"),
        explanation: i18n::t!(
            "Copies the picked commits' changes onto the current branch, oldest first. The originals stay"
        ),
        caution: None,
    }
}

pub(crate) fn rewrite_selection_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Reorder / Squash / Drop"),
        explanation: i18n::t!("Reorders, squashes or drops the picked range"),
        caution: Some(history_rewrite_caution()),
    }
}

pub(crate) fn squash_selection_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Squash into One"),
        explanation: i18n::t!("Combines the picked consecutive commits into a single commit"),
        caution: Some(history_rewrite_caution()),
    }
}

pub(crate) fn more_menu_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "More"),
        explanation: i18n::t!(
            "Operations that did not fit in the row. Open it to see each one explained beside it"
        ),
        caution: None,
    }
}

pub(crate) fn log_source_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Show"),
        explanation: i18n::t!(
            "Chooses the commits the graph shows: all branches, the current branch or a chosen branch"
        ),
        caution: None,
    }
}

pub(crate) fn tag_list_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Tags"),
        explanation: i18n::t!(
            "Searches the tags (names given to commits) and jumps to the commit of the one you pick"
        ),
        caution: None,
    }
}

pub(crate) fn compare_refs_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Compare"),
        explanation: i18n::t!(
            "Compares the contents of two branches or tags. Useful to check that no commit was left behind"
        ),
        caution: None,
    }
}

pub(crate) fn merge_request_toolbar_help() -> OperationHelp {
    OperationHelp {
        title: i18n::tc!("git operation", "Create MR"),
        explanation: i18n::t!(
            "Collects commits in a box and creates a branch with them squashed into one commit for an MR (merge request)"
        ),
        caution: None,
    }
}

pub(crate) fn commit_operation_help(operation: CommitMenuOperation) -> OperationHelp {
    match operation {
        CommitMenuOperation::CreateBranch => OperationHelp {
            title: i18n::tc!("git operation", "Branch from Here"),
            explanation: i18n::t!(
                "Creates a new branch (a line of work) starting at this commit. Your current work stays as it is"
            ),
            caution: None,
        },
        CommitMenuOperation::Checkout => OperationHelp {
            title: i18n::tc!("git operation", "Check Out"),
            explanation: i18n::t!("Switches to this commit (detached HEAD: not on any branch)"),
            caution: Some(i18n::t!("To work here, create a branch first")),
        },
        CommitMenuOperation::CreateTag => OperationHelp {
            title: i18n::tc!("git operation", "Tag"),
            explanation: i18n::t!(
                "Gives this commit a marker name (a tag), for example to record a released version"
            ),
            caution: None,
        },
        CommitMenuOperation::CherryPick => OperationHelp {
            title: i18n::tc!("git operation", "Cherry-Pick"),
            explanation: i18n::t!(
                "Copies only this commit's changes onto the current branch as a new commit. The original stays"
            ),
            caution: None,
        },
        CommitMenuOperation::Revert => OperationHelp {
            title: i18n::tc!("git operation", "Revert"),
            explanation: i18n::t!(
                "Creates a new commit that applies this commit's changes in reverse. History is kept, so it is safe after sharing"
            ),
            caution: None,
        },
        CommitMenuOperation::Reword => OperationHelp {
            title: i18n::tc!("git operation", "Reword Message"),
            explanation: i18n::t!("Changes only this commit's message"),
            caution: Some(history_rewrite_caution()),
        },
        CommitMenuOperation::Fixup => OperationHelp {
            title: i18n::tc!("git operation", "Add Current Changes (fixup)"),
            explanation: i18n::t!("Adds your uncommitted changes into this commit"),
            caution: Some(history_rewrite_caution()),
        },
        CommitMenuOperation::Drop => OperationHelp {
            title: i18n::tc!("git operation", "Drop"),
            explanation: i18n::t!("Removes this commit and its changes from history"),
            caution: Some(history_rewrite_caution()),
        },
        CommitMenuOperation::ResetSoft => OperationHelp {
            title: i18n::tc!("git operation", "Reset: soft"),
            explanation: i18n::t!(
                "Moves only the branch back. The later changes stay staged and in the working tree"
            ),
            caution: None,
        },
        CommitMenuOperation::ResetMixed => OperationHelp {
            title: i18n::tc!("git operation", "Reset: mixed"),
            explanation: i18n::t!(
                "Moves the branch back and unstages. The later changes stay in the working tree's files"
            ),
            caution: None,
        },
        CommitMenuOperation::ResetHard => OperationHelp {
            title: i18n::tc!("git operation", "Reset: hard"),
            explanation: i18n::t!("Resets the branch, discarding later and uncommitted changes"),
            caution: Some(i18n::t!("Uncommitted changes cannot be recovered")),
        },
        CommitMenuOperation::CopyTagDistance => OperationHelp {
            title: i18n::tc!("git operation", "Distance from Tag"),
            explanation: i18n::t!(
                "Copies how many commits past the nearest tag this is (in git describe form)"
            ),
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
                    Label::new(i18n::t!("Caution: {caution}"))
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

fn undo_unavailable_caution() -> &'static str {
    i18n::t!("This cannot be undone")
}

fn pushed_commit_caution() -> &'static str {
    i18n::t!("If the commit was already pushed, you will need to force push")
}

fn conflict_caution() -> &'static str {
    i18n::t!("Changes to the same lines on both sides cause a conflict")
}

fn stash_pop_caution() -> &'static str {
    i18n::t!(
        "If your current changes touch the same files, it stops or ends in a conflict. On a conflict, the changes also stay in the stash"
    )
}

fn discard_unstaged_caution() -> &'static str {
    i18n::t!("Unstaged edits are lost")
}

pub(crate) fn panel_operation_help(operation: PanelOperation) -> OperationHelp {
    match operation {
        PanelOperation::StageAll => OperationHelp {
            title: i18n::tc!("git operation", "Stage All"),
            explanation: i18n::t!(
                "Marks every changed file to go into the next commit (stages it). The files' contents do not change"
            ),
            caution: None,
        },
        PanelOperation::UnstageAll => OperationHelp {
            title: i18n::tc!("git operation", "Unstage All"),
            explanation: i18n::t!(
                "Removes every stage mark. The changes stay in the files and just no longer go into the next commit"
            ),
            caution: None,
        },
        PanelOperation::DiscardTrackedChanges => OperationHelp {
            title: i18n::tc!("git operation", "Discard Tracked Changes (restore)"),
            explanation: i18n::t!(
                "Returns each tracked file that has staged changes to its content in the last commit, unstaged changes included. Files with only unstaged changes and newly created files (untracked) are kept"
            ),
            caution: Some(undo_unavailable_caution()),
        },
        PanelOperation::StashAll => OperationHelp {
            title: i18n::tc!("git operation", "Stash All"),
            explanation: i18n::t!(
                "Puts all uncommitted changes, newly created files (untracked) included, away on a temporary shelf (the stash) and returns the files to the last commit. \"Stash Pop\" brings them back later"
            ),
            caution: None,
        },
        PanelOperation::StashTracked => OperationHelp {
            title: i18n::tc!("git operation", "Stash Tracked"),
            explanation: i18n::t!(
                "Puts only the changes to files Git manages (tracked) away on the shelf (the stash). Newly created files are kept"
            ),
            caution: None,
        },
        PanelOperation::StashStaged => OperationHelp {
            title: i18n::tc!("git operation", "Stash Staged (stash --staged)"),
            explanation: i18n::t!(
                "Puts only the staged changes away on the shelf (the stash). Unstaged changes stay in the files"
            ),
            caution: None,
        },
        PanelOperation::StashPop => OperationHelp {
            title: i18n::tc!("git operation", "Stash Pop"),
            explanation: i18n::t!(
                "Puts the changes last put on the shelf (the stash) back into the files and removes them from the shelf"
            ),
            caution: Some(stash_pop_caution()),
        },
        PanelOperation::ViewStash => OperationHelp {
            title: i18n::tc!("git operation", "View Stash List (stash list)"),
            explanation: i18n::t!(
                "Opens the list of changes put on the shelf (the stash), where you can look inside, pop or drop them"
            ),
            caution: None,
        },
        PanelOperation::CopyPath => OperationHelp {
            title: i18n::tc!("git operation", "Copy Path"),
            explanation: i18n::t!(
                "Copies the file's location as a full (absolute) path to the clipboard"
            ),
            caution: None,
        },
        PanelOperation::CopyRelativePath => OperationHelp {
            title: i18n::tc!("git operation", "Copy Relative Path"),
            explanation: i18n::t!(
                "Copies the file's location as a path relative to the project folder to the clipboard"
            ),
            caution: None,
        },
        PanelOperation::TrashUntrackedFiles => OperationHelp {
            title: i18n::tc!("git operation", "Move Untracked Files to Trash"),
            explanation: i18n::t!(
                "Moves new files Git does not manage yet (untracked) to the system trash. They can be restored from the trash"
            ),
            caution: Some(i18n::t!(
                "Git's history has no copy of them, so once the trash is emptied they cannot be restored"
            )),
        },
        PanelOperation::CreatePatch => OperationHelp {
            title: i18n::tc!("git operation", "Create Patch File (diff)"),
            explanation: i18n::t!(
                "Writes a text file (a patch) that contains only the changes. Choose the working tree, the staged changes or the last commit. The files themselves do not change"
            ),
            caution: None,
        },
        PanelOperation::ApplyPatch => OperationHelp {
            title: i18n::tc!("git operation", "Apply Patch File (apply)"),
            explanation: i18n::t!(
                "Applies the changes written in a patch file to your current files. Nothing is committed, so check the result before you commit"
            ),
            caution: None,
        },
        PanelOperation::InitRepository => OperationHelp {
            title: i18n::tc!("git operation", "Initialize Repository (init)"),
            explanation: i18n::t!(
                "Starts using Git in this folder (creates a .git folder). The existing files do not change"
            ),
            caution: None,
        },
        PanelOperation::ShowGuide => OperationHelp {
            title: i18n::tc!("git operation", "Git Guide"),
            explanation: i18n::t!(
                "Opens a guide that explains Git's terms and the steps of common operations"
            ),
            caution: None,
        },
        PanelOperation::ViewOptions => OperationHelp {
            title: i18n::tc!("git operation", "View Options"),
            explanation: i18n::t!(
                "Changes how the list of changes is shown (list or tree, sort order, grouping). Does nothing to the files"
            ),
            caution: None,
        },
        PanelOperation::ListView => OperationHelp {
            title: i18n::tc!("git operation", "List View"),
            explanation: i18n::t!(
                "Shows the changed files in a single column, without splitting them by folder"
            ),
            caution: None,
        },
        PanelOperation::TreeView => OperationHelp {
            title: i18n::tc!("git operation", "Tree View"),
            explanation: i18n::t!(
                "Groups the changed files by folder hierarchy, with folders you can expand and collapse"
            ),
            caution: None,
        },
        PanelOperation::SortByPath => OperationHelp {
            title: i18n::tc!("git operation", "Sort by Path"),
            explanation: i18n::t!(
                "Sorts by location, folders included (the path). Files in the same folder end up next to each other"
            ),
            caution: None,
        },
        PanelOperation::SortByName => OperationHelp {
            title: i18n::tc!("git operation", "Sort by Name"),
            explanation: i18n::t!("Sorts by file name, regardless of the folder"),
            caution: None,
        },
        PanelOperation::GroupByNone => OperationHelp {
            title: i18n::tc!("git operation", "No Grouping"),
            explanation: i18n::t!("Puts all changes together in one list, without splitting them"),
            caution: None,
        },
        PanelOperation::GroupByStatus => OperationHelp {
            title: i18n::tc!("git operation", "Group by Tracked and Untracked"),
            explanation: i18n::t!(
                "Separates the files Git manages (tracked) from the new files it does not manage yet (untracked)"
            ),
            caution: None,
        },
        PanelOperation::GroupByStaging => OperationHelp {
            title: i18n::tc!("git operation", "Group by Staged and Unstaged"),
            explanation: i18n::t!(
                "Separates the changes marked to go into the next commit (staged) from the changes not marked yet (unstaged)"
            ),
            caution: None,
        },
        PanelOperation::Commit => OperationHelp {
            title: i18n::tc!("git operation", "Commit"),
            explanation: i18n::t!(
                "Saves the staged changes as a single record with a message (a commit). With nothing staged, all tracked changes go in"
            ),
            caution: None,
        },
        PanelOperation::Amend => OperationHelp {
            title: i18n::tc!("git operation", "Amend Last Commit"),
            explanation: i18n::t!(
                "Instead of adding a new commit, the next commit rebuilds the last commit from the current changes and message"
            ),
            caution: Some(pushed_commit_caution()),
        },
        PanelOperation::Signoff => OperationHelp {
            title: i18n::tc!("git operation", "Signoff"),
            explanation: i18n::t!(
                "Adds a \"Signed-off-by: name <email>\" line at the end of the commit message. Use it when the project's rules ask for it"
            ),
            caution: None,
        },
        PanelOperation::SkipHooks => OperationHelp {
            title: i18n::tc!("git operation", "Skip Hooks (--no-verify)"),
            explanation: i18n::t!(
                "Commits without the checks that run automatically before a commit (hooks)"
            ),
            caution: Some(i18n::t!(
                "Problems the checks would have stopped can get into the commit"
            )),
        },
        PanelOperation::Uncommit => OperationHelp {
            title: i18n::tc!("git operation", "Uncommit (reset --soft)"),
            explanation: i18n::t!(
                "Undoes the last commit and returns its changes to the staged, uncommitted state. The files' contents do not change"
            ),
            caution: Some(pushed_commit_caution()),
        },
        PanelOperation::CoAuthors => OperationHelp {
            title: i18n::tc!("git operation", "Co-Authors (Co-authored-by)"),
            explanation: i18n::t!(
                "Records the names of the people you worked with in \"Co-authored-by\" lines of the commit message"
            ),
            caution: None,
        },
        PanelOperation::Fetch => OperationHelp {
            title: i18n::tc!("git operation", "Fetch"),
            explanation: i18n::t!(
                "Only downloads the latest from the remote (the shared server). Your files and branches do not change"
            ),
            caution: None,
        },
        PanelOperation::FetchFrom => OperationHelp {
            title: i18n::tc!("git operation", "Fetch From (fetch)"),
            explanation: i18n::t!(
                "Picks one of the registered remotes and only downloads its latest. The files do not change"
            ),
            caution: None,
        },
        PanelOperation::Pull => OperationHelp {
            title: i18n::tc!("git operation", "Pull"),
            explanation: i18n::t!(
                "Downloads the latest from the remote and brings it into the current branch (fetch + merge; when the git setting pull.rebase is set, it rebases as that says)"
            ),
            caution: Some(conflict_caution()),
        },
        PanelOperation::PullRebase => OperationHelp {
            title: i18n::tc!("git operation", "Pull with Rebase (pull --rebase)"),
            explanation: i18n::t!(
                "Brings in the latest from the remote and replays your commits that were not pushed yet on top of it. History stays in a straight line"
            ),
            caution: Some(i18n::t!("The replayed commits of yours get new SHAs")),
        },
        PanelOperation::Push => OperationHelp {
            title: i18n::tc!("git operation", "Push"),
            explanation: i18n::t!(
                "Sends your commits to the remote. Changes you have not committed are not sent"
            ),
            caution: None,
        },
        PanelOperation::PushTo => OperationHelp {
            title: i18n::tc!("git operation", "Push To (push)"),
            explanation: i18n::t!("Chooses the remote to send to, then sends your commits"),
            caution: None,
        },
        PanelOperation::ForcePush => OperationHelp {
            title: i18n::tc!("git operation", "Force Push (push --force-with-lease)"),
            explanation: i18n::t!(
                "Overwrites the remote branch with your history. Stops if someone else pushed commits after your last fetch"
            ),
            caution: Some(i18n::t!(
                "The overwritten commits disappear from the remote branch"
            )),
        },
        PanelOperation::ForcePushWithoutLease => OperationHelp {
            title: i18n::tc!("git operation", "Force Push Without Lease (push --force)"),
            explanation: i18n::t!(
                "Overwrites the remote branch with your history, without checking"
            ),
            caution: Some(i18n::t!(
                "A force push can erase other people's changes. Commits others pushed disappear without a word"
            )),
        },
        PanelOperation::Publish => OperationHelp {
            title: i18n::tc!("git operation", "Publish (push --set-upstream)"),
            explanation: i18n::t!(
                "Sends this branch to the remote for the first time and links it as the branch to push to and pull from from now on"
            ),
            caution: None,
        },
        PanelOperation::Republish => OperationHelp {
            title: i18n::tc!("git operation", "Republish (push --set-upstream)"),
            explanation: i18n::t!(
                "Sends a branch that disappeared from the remote there again and links it again"
            ),
            caution: None,
        },
        PanelOperation::Merge => OperationHelp {
            title: i18n::tc!("git operation", "Merge Branch"),
            explanation: i18n::t!(
                "Brings the chosen branch's changes into the current branch. Both histories are kept. If the current branch has new commits too, a commit that joins the two (a merge commit) is created"
            ),
            caution: Some(conflict_caution()),
        },
        PanelOperation::ResetToRemote => OperationHelp {
            title: i18n::tc!("git operation", "Reset to Remote (reset --hard)"),
            explanation: i18n::t!(
                "Fetches the remote, then makes the current branch the same as the remote branch"
            ),
            caution: Some(i18n::t!(
                "Uncommitted changes and your commits that are not on the remote are thrown away and cannot be undone"
            )),
        },
        PanelOperation::AddRemote => OperationHelp {
            title: i18n::tc!("git operation", "Add Remote (remote add)"),
            explanation: i18n::t!(
                "Registers a server to push to and fetch from (a remote) by name and URL"
            ),
            caution: None,
        },
        PanelOperation::RemoveRemote => OperationHelp {
            title: i18n::tc!("git operation", "Remove Remote (remote remove)"),
            explanation: i18n::t!(
                "Unregisters a remote. Your local record of that remote's branches goes too. The data on the server is not deleted"
            ),
            caution: None,
        },
        PanelOperation::StageFile => OperationHelp {
            title: i18n::tc!("git operation", "Stage File"),
            explanation: i18n::t!(
                "Marks this file's changes to go into the next commit. The file's contents do not change"
            ),
            caution: None,
        },
        PanelOperation::UnstageFile => OperationHelp {
            title: i18n::tc!("git operation", "Unstage File"),
            explanation: i18n::t!("Removes this file's stage mark. The changes stay in the file"),
            caution: None,
        },
        PanelOperation::DiscardFileChanges => OperationHelp {
            title: i18n::tc!("git operation", "Discard Changes (restore)"),
            explanation: i18n::t!(
                "Discards this file's changes and returns it to its content in the last commit"
            ),
            caution: Some(undo_unavailable_caution()),
        },
        PanelOperation::TrashNewFile => OperationHelp {
            title: i18n::tc!("git operation", "Move File to Trash"),
            explanation: i18n::t!(
                "Moves a new file that was never committed to the system trash. It can be restored from the trash"
            ),
            caution: Some(i18n::t!(
                "Git's history has no copy of it, so once the trash is emptied it cannot be restored"
            )),
        },
        PanelOperation::RestoreDeletedFile => OperationHelp {
            title: i18n::tc!("git operation", "Restore File (restore)"),
            explanation: i18n::t!("Brings a deleted file back with its content in the last commit"),
            caution: None,
        },
        PanelOperation::ViewUnstagedChanges => OperationHelp {
            title: i18n::tc!("git operation", "Unstaged Changes"),
            explanation: i18n::t!(
                "Shows a diff of only the changes not staged yet (which do not go into the next commit)"
            ),
            caution: None,
        },
        PanelOperation::ViewStagedChanges => OperationHelp {
            title: i18n::tc!("git operation", "Staged Changes"),
            explanation: i18n::t!(
                "Shows a diff of only the staged changes (which go into the next commit)"
            ),
            caution: None,
        },
        PanelOperation::AddToGitignore => OperationHelp {
            title: i18n::tc!("git operation", "Add to .gitignore"),
            explanation: i18n::t!(
                "Adds this file to the ignore list at the top of the repository (.gitignore), so that it no longer shows among the changes. Once the .gitignore change is committed, it applies to others too"
            ),
            caution: None,
        },
        PanelOperation::AddToGitInfoExclude => OperationHelp {
            title: i18n::tc!("git operation", "Add to .git/info/exclude"),
            explanation: i18n::t!(
                "Adds this file to your own local ignore list (.git/info/exclude). It is not committed and does not apply to others"
            ),
            caution: None,
        },
        PanelOperation::OpenDiff => OperationHelp {
            title: i18n::tc!("git operation", "Open Diff"),
            explanation: i18n::t!(
                "Opens the difference between this file before and after the change (the diff) in the tab that shows all changes together"
            ),
            caution: None,
        },
        PanelOperation::OpenFileDiff => OperationHelp {
            title: i18n::tc!("git operation", "Open File Diff"),
            explanation: i18n::t!(
                "Opens a diff of just this file (before and after the change) in a tab of its own"
            ),
            caution: None,
        },
        PanelOperation::ViewFile => OperationHelp {
            title: i18n::tc!("git operation", "View File"),
            explanation: i18n::t!(
                "Opens the current file itself in the editor, rather than the diff"
            ),
            caution: None,
        },
        PanelOperation::FileHistory => OperationHelp {
            title: i18n::tc!("git operation", "File History (log)"),
            explanation: i18n::t!("Opens the list of commits that changed this file, newest first"),
            caution: None,
        },
        PanelOperation::StageFolder => OperationHelp {
            title: i18n::tc!("git operation", "Stage Folder"),
            explanation: i18n::t!("Marks every change in this folder to go into the next commit"),
            caution: None,
        },
        PanelOperation::UnstageFolder => OperationHelp {
            title: i18n::tc!("git operation", "Unstage Folder"),
            explanation: i18n::t!(
                "Removes every stage mark in this folder. The changes stay in the files"
            ),
            caution: None,
        },
        PanelOperation::StageSection => OperationHelp {
            title: i18n::tc!("git operation", "Stage All in Section"),
            explanation: i18n::t!(
                "Marks every change under this heading to go into the next commit"
            ),
            caution: None,
        },
        PanelOperation::UnstageSection => OperationHelp {
            title: i18n::tc!("git operation", "Unstage All in Section"),
            explanation: i18n::t!(
                "Removes the stage marks of every change under this heading. The changes stay in the files"
            ),
            caution: None,
        },
        PanelOperation::ShowDiff => OperationHelp {
            title: i18n::tc!("git operation", "Show Diff"),
            explanation: i18n::t!(
                "Shows the diff (before and after) of the uncommitted changes together in one tab"
            ),
            caution: None,
        },
        PanelOperation::TrustDirectory => OperationHelp {
            title: i18n::tc!("git operation", "Trust Directory (safe.directory)"),
            explanation: i18n::t!(
                "Registers a folder owned by another user as a place Git may work in"
            ),
            caution: Some(i18n::t!(
                "Writes to your user's Git settings as a whole (--global)"
            )),
        },
        PanelOperation::SwitchBranch => OperationHelp {
            title: i18n::tc!("git operation", "Switch Branch"),
            explanation: i18n::t!(
                "Picks another branch (a line of work) to work on. The files change to that branch's content"
            ),
            caution: None,
        },
        PanelOperation::SequencerContinue => OperationHelp {
            title: i18n::tc!("git operation", "Continue (--continue)"),
            explanation: i18n::t!(
                "With the conflicts resolved and staged, carries on with the rest of the operation that stopped midway"
            ),
            caution: None,
        },
        PanelOperation::SequencerSkip => OperationHelp {
            title: i18n::tc!("git operation", "Skip (--skip)"),
            explanation: i18n::t!("Skips the commit it stopped at and moves on to the next commit"),
            caution: Some(i18n::t!("The skipped commit's changes are not brought in")),
        },
        PanelOperation::SequencerAbort => OperationHelp {
            title: i18n::tc!("git operation", "Abort (--abort)"),
            explanation: i18n::t!(
                "Gives up the operation and returns to the state before it started"
            ),
            caution: Some(i18n::t!(
                "Whatever you resolved along the way is thrown away"
            )),
        },
        PanelOperation::ToggleHunkStaged => OperationHelp {
            title: i18n::tc!("git operation", "Toggle Staged"),
            explanation: i18n::t!(
                "Adds or removes the stage mark of the chosen change (hunk: one block of changes)"
            ),
            caution: None,
        },
        PanelOperation::StageHunk => OperationHelp {
            title: i18n::tc!("git operation", "Stage Hunk"),
            explanation: i18n::t!(
                "Marks only this change (hunk: one block of changes) to go into the next commit. The file's contents do not change"
            ),
            caution: None,
        },
        PanelOperation::UnstageHunk => OperationHelp {
            title: i18n::tc!("git operation", "Unstage Hunk"),
            explanation: i18n::t!(
                "Removes the stage mark of this change (hunk). The change stays in the file"
            ),
            caution: None,
        },
        PanelOperation::RestoreHunk => OperationHelp {
            title: i18n::tc!("git operation", "Restore Hunk (restore)"),
            explanation: i18n::t!(
                "Undoes the edits in this change (hunk) and returns the file to its staged content"
            ),
            caution: Some(discard_unstaged_caution()),
        },
        PanelOperation::RestoreAllUnstaged => OperationHelp {
            title: i18n::tc!("git operation", "Restore All (restore)"),
            explanation: i18n::t!(
                "Undoes every unstaged change and returns the files to their staged content. Newly created files are kept"
            ),
            caution: Some(discard_unstaged_caution()),
        },
        PanelOperation::ViewStashEntry => OperationHelp {
            title: i18n::tc!("git operation", "View Stash"),
            explanation: i18n::t!(
                "Shows what these changes put on the shelf (the stash) contain, as a diff. Does not pop them"
            ),
            caution: None,
        },
        PanelOperation::PopStashEntry => OperationHelp {
            title: i18n::tc!("git operation", "Pop This Stash (stash pop)"),
            explanation: i18n::t!(
                "Puts these changes from the shelf (the stash) back into the files and removes them from the shelf"
            ),
            caution: Some(stash_pop_caution()),
        },
        PanelOperation::DropStashEntry => OperationHelp {
            title: i18n::tc!("git operation", "Drop Stash (stash drop)"),
            explanation: i18n::t!(
                "Throws away these changes on the shelf (the stash) without putting them back into the files"
            ),
            caution: Some(undo_unavailable_caution()),
        },
        PanelOperation::FilterBranches => OperationHelp {
            title: i18n::tc!("git operation", "Filter Branches"),
            explanation: i18n::t!(
                "Chooses the branches the list shows: all, yours (local) or the server's (remote)"
            ),
            caution: None,
        },
        PanelOperation::ShowAllBranches => OperationHelp {
            title: i18n::tc!("git operation", "All Branches"),
            explanation: i18n::t!(
                "Shows both your branches (local) and the copies of the server's branches (remote)"
            ),
            caution: None,
        },
        PanelOperation::ShowLocalBranches => OperationHelp {
            title: i18n::tc!("git operation", "Local Branches"),
            explanation: i18n::t!(
                "Shows only the branches in your repository. These are the ones you can commit to"
            ),
            caution: None,
        },
        PanelOperation::ShowRemoteBranches => OperationHelp {
            title: i18n::tc!("git operation", "Remote Branches"),
            explanation: i18n::t!(
                "Shows only the copies of the server's branches that the last fetch brought in (origin/… and so on)"
            ),
            caution: None,
        },
        PanelOperation::DeleteBranch => OperationHelp {
            title: i18n::tc!("git operation", "Delete Branch (branch -d)"),
            explanation: i18n::t!(
                "Deletes a local branch. If it has commits that are not in the current branch (or its upstream), it stops and asks whether to force delete. For a remote branch only your local copy goes; the branch on the server stays"
            ),
            caution: None,
        },
        PanelOperation::ForceDeleteBranch => OperationHelp {
            title: i18n::tc!("git operation", "Force Delete Branch (branch -D)"),
            explanation: i18n::t!(
                "Deletes a local branch even if it has commits that were not merged"
            ),
            caution: Some(i18n::t!(
                "Commits that are only on that branch can no longer be seen"
            )),
        },
        PanelOperation::CreateBranchFromDefault => OperationHelp {
            title: i18n::tc!("git operation", "Create from Default Branch"),
            explanation: i18n::t!(
                "Creates a new branch starting from the default branch (main, for example) instead of the current branch"
            ),
            caution: None,
        },
        PanelOperation::UseOurs => OperationHelp {
            title: i18n::tc!("git operation", "Use Ours"),
            explanation: i18n::t!(
                "Resolves the conflict with only the upper side (the <<<<<<< side, ours). The lower side is thrown away. During a merge that is the branch you are on; during a rebase, the branch being rebased onto"
            ),
            caution: None,
        },
        PanelOperation::UseTheirs => OperationHelp {
            title: i18n::tc!("git operation", "Use Theirs"),
            explanation: i18n::t!(
                "Resolves the conflict with only the lower side (the >>>>>>> side, theirs). The upper side is thrown away. During a merge that is the side being merged in; during a rebase, your commit being replayed"
            ),
            caution: None,
        },
        PanelOperation::UseBoth => OperationHelp {
            title: i18n::tc!("git operation", "Use Both"),
            explanation: i18n::t!(
                "Keeps both the upper side (ours) and the lower side (theirs) of the conflict, one above the other"
            ),
            caution: None,
        },
    }
}

/// Why a greyed-out item of the Git panel's menus cannot be chosen. Translate them with
/// `i18n::tr` where they are shown.
pub(crate) const NOTHING_TO_STAGE_REFUSAL: &str = i18n::msg!("There are no changes to stage");
pub(crate) const NOTHING_TO_UNSTAGE_REFUSAL: &str = i18n::msg!("There are no staged changes");
pub(crate) const NO_TRACKED_CHANGES_REFUSAL: &str = i18n::msg!("No tracked file has changes");
pub(crate) const NO_STAGED_TRACKED_CHANGES_REFUSAL: &str =
    i18n::msg!("No tracked file has staged changes");
pub(crate) const NOTHING_TO_STASH_REFUSAL: &str = i18n::msg!("There are no changes to stash");
pub(crate) const NO_STASH_REFUSAL: &str =
    i18n::msg!("Nothing has been put away on the shelf (the stash)");
pub(crate) const NO_UNTRACKED_FILES_REFUSAL: &str = i18n::msg!("There are no untracked files");
pub(crate) const ONLY_FOR_UNTRACKED_REFUSAL: &str =
    i18n::msg!("Only for new files Git does not manage yet (untracked)");

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
            "Resets the branch, discarding later and uncommitted changes. Caution: Uncommitted changes cannot be recovered."
        );
        assert!(help.line().starts_with("Reset: hard: "));
        assert_eq!(
            commit_operation_help(CommitMenuOperation::ResetSoft).sentence(),
            "Moves only the branch back. The later changes stay staged and in the working tree."
        );
    }

    /// The Japanese catalog puts the sentences together the way Japanese writes them: a full
    /// stop of its own, and none after the caution.
    #[test]
    fn test_sentences_in_japanese() {
        let _japanese = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        let help = commit_operation_help(CommitMenuOperation::ResetHard);
        assert_eq!(
            help.sentence(),
            "ブランチを戻し、後の変更も未コミットの変更もまとめて捨てます。注意: 未コミットの変更は元に戻せません"
        );
        assert!(help.line().starts_with("戻す: hard: "), "{}", help.line());
        assert_eq!(
            commit_operation_help(CommitMenuOperation::ResetSoft).sentence(),
            "ブランチだけを戻します。後の変更はステージにも作業ツリーにもそのまま残ります。"
        );
        assert_eq!(
            panel_operation_help(PanelOperation::Fetch).sentence_with_command("git fetch"),
            "リモート (共有先のサーバー) の最新を取ってくるだけです。自分のファイルやブランチは変わりません。 (git fetch)"
        );
        assert_eq!(
            panel_operation_help(PanelOperation::StageAll).title,
            "すべてステージ (stage)"
        );
    }

    /// Each operation is explained in its own words: an explanation that went to the wrong
    /// operation still reads well, so only the words that belong to it catch the mix-up.
    #[test]
    fn test_each_operation_explains_itself() {
        let expectations = [
            (CommitMenuOperation::CreateBranch, "Branch", "new branch"),
            (CommitMenuOperation::Checkout, "Check Out", "detached HEAD"),
            (CommitMenuOperation::CreateTag, "Tag", "a tag"),
            (CommitMenuOperation::CherryPick, "Cherry-Pick", "Copies"),
            (CommitMenuOperation::Revert, "Revert", "in reverse"),
            (CommitMenuOperation::Reword, "Reword", "message"),
            (CommitMenuOperation::Fixup, "fixup", "uncommitted"),
            (CommitMenuOperation::Drop, "Drop", "Removes this commit"),
            (CommitMenuOperation::ResetSoft, "soft", "only the branch"),
            (CommitMenuOperation::ResetMixed, "mixed", "unstages"),
            (CommitMenuOperation::ResetHard, "hard", "discarding"),
            (CommitMenuOperation::CopyTagDistance, "Tag", "git describe"),
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
            (
                PanelOperation::StageAll,
                "Stage",
                "to go into the next commit",
            ),
            (PanelOperation::UnstageAll, "Unstage", "every stage mark"),
            (
                PanelOperation::DiscardTrackedChanges,
                "Discard",
                "content in the last commit",
            ),
            (PanelOperation::StashAll, "Stash", "shelf"),
            (PanelOperation::StashTracked, "Stash", "tracked"),
            (
                PanelOperation::StashStaged,
                "--staged",
                "only the staged changes",
            ),
            (
                PanelOperation::StashPop,
                "Pop",
                "removes them from the shelf",
            ),
            (PanelOperation::ViewStash, "stash list", "list"),
            (PanelOperation::CopyPath, "Path", "full (absolute) path"),
            (
                PanelOperation::CopyRelativePath,
                "Relative Path",
                "project folder",
            ),
            (PanelOperation::TrashUntrackedFiles, "Trash", "untracked"),
            (PanelOperation::CreatePatch, "Patch", "Writes"),
            (PanelOperation::ApplyPatch, "apply", "Applies"),
            (PanelOperation::InitRepository, "init", ".git folder"),
            (PanelOperation::ShowGuide, "Guide", "terms"),
            (PanelOperation::ViewOptions, "View", "how the list"),
            (PanelOperation::ListView, "List", "single column"),
            (PanelOperation::TreeView, "Tree", "hierarchy"),
            (PanelOperation::SortByPath, "Path", "(the path)"),
            (PanelOperation::SortByName, "Name", "file name"),
            (PanelOperation::GroupByNone, "No Grouping", "one list"),
            (
                PanelOperation::GroupByStatus,
                "Untracked",
                "does not manage yet",
            ),
            (PanelOperation::GroupByStaging, "Unstaged", "changes marked"),
            (PanelOperation::Commit, "Commit", "single record"),
            (PanelOperation::Amend, "Amend", "rebuilds"),
            (PanelOperation::Signoff, "Signoff", "Signed-off-by"),
            (PanelOperation::SkipHooks, "--no-verify", "hooks"),
            (PanelOperation::Uncommit, "Uncommit", "uncommitted state"),
            (
                PanelOperation::CoAuthors,
                "Co-authored-by",
                "people you worked with",
            ),
            (PanelOperation::Fetch, "Fetch", "Only downloads"),
            (PanelOperation::FetchFrom, "From", "Picks one"),
            (PanelOperation::Pull, "Pull", "brings it into"),
            (PanelOperation::PullRebase, "--rebase", "replays"),
            (PanelOperation::Push, "Push", "Sends your commits"),
            (PanelOperation::PushTo, "Push To", "remote to send to"),
            (
                PanelOperation::ForcePush,
                "--force-with-lease",
                "Overwrites",
            ),
            (
                PanelOperation::ForcePushWithoutLease,
                "--force",
                "without checking",
            ),
            (
                PanelOperation::Publish,
                "--set-upstream",
                "for the first time",
            ),
            (PanelOperation::Republish, "Republish", "disappeared"),
            (PanelOperation::Merge, "Merge", "joins"),
            (PanelOperation::ResetToRemote, "reset --hard", "the same as"),
            (PanelOperation::AddRemote, "remote add", "URL"),
            (PanelOperation::RemoveRemote, "remote remove", "Unregisters"),
            (PanelOperation::StageFile, "Stage", "this file"),
            (PanelOperation::UnstageFile, "Unstage", "this file"),
            (
                PanelOperation::DiscardFileChanges,
                "Discard",
                "this file's changes",
            ),
            (PanelOperation::TrashNewFile, "Trash", "new file"),
            (
                PanelOperation::RestoreDeletedFile,
                "restore",
                "deleted file",
            ),
            (
                PanelOperation::ViewUnstagedChanges,
                "Unstaged",
                "do not go into the next commit",
            ),
            (
                PanelOperation::ViewStagedChanges,
                "Staged",
                "go into the next commit",
            ),
            (
                PanelOperation::AddToGitignore,
                ".gitignore",
                "applies to others too",
            ),
            (
                PanelOperation::AddToGitInfoExclude,
                "exclude",
                "your own local",
            ),
            (
                PanelOperation::OpenDiff,
                "Diff",
                "shows all changes together",
            ),
            (PanelOperation::OpenFileDiff, "File Diff", "just this file"),
            (PanelOperation::ViewFile, "File", "file itself"),
            (PanelOperation::FileHistory, "log", "list of commits"),
            (PanelOperation::StageFolder, "Folder", "in this folder"),
            (PanelOperation::UnstageFolder, "Folder", "every stage mark"),
            (PanelOperation::StageSection, "Stage", "under this heading"),
            (
                PanelOperation::UnstageSection,
                "Unstage",
                "under this heading",
            ),
            (PanelOperation::ShowDiff, "Diff", "in one tab"),
            (PanelOperation::TrustDirectory, "safe.directory", "owned by"),
            (PanelOperation::SwitchBranch, "Switch", "line of work"),
            (
                PanelOperation::SequencerContinue,
                "--continue",
                "carries on with the rest",
            ),
            (PanelOperation::SequencerSkip, "--skip", "Skips"),
            (
                PanelOperation::SequencerAbort,
                "--abort",
                "before it started",
            ),
            (
                PanelOperation::ToggleHunkStaged,
                "Toggle",
                "Adds or removes",
            ),
            (PanelOperation::StageHunk, "Stage", "hunk"),
            (PanelOperation::UnstageHunk, "Unstage", "hunk"),
            (PanelOperation::RestoreHunk, "Restore", "Undoes the edits"),
            (
                PanelOperation::RestoreAllUnstaged,
                "All",
                "every unstaged change",
            ),
            (PanelOperation::ViewStashEntry, "View", "Does not pop"),
            (
                PanelOperation::PopStashEntry,
                "Pop",
                "removes them from the shelf",
            ),
            (PanelOperation::DropStashEntry, "drop", "Throws away"),
            (PanelOperation::FilterBranches, "Filter", "local"),
            (PanelOperation::ShowAllBranches, "All", "both"),
            (
                PanelOperation::ShowLocalBranches,
                "Local",
                "your repository",
            ),
            (PanelOperation::ShowRemoteBranches, "Remote", "server's"),
            (PanelOperation::DeleteBranch, "branch -d", "it stops"),
            (PanelOperation::ForceDeleteBranch, "branch -D", "even if"),
            (PanelOperation::CreateBranchFromDefault, "Default", "main"),
            (
                PanelOperation::UseOurs,
                "Ours",
                "during a rebase, the branch being rebased onto",
            ),
            (
                PanelOperation::UseTheirs,
                "Theirs",
                "during a rebase, your commit being replayed",
            ),
            (PanelOperation::UseBoth, "Both", "one above the other"),
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
            (PanelOperation::DiscardTrackedChanges, "cannot be undone"),
            (PanelOperation::DiscardFileChanges, "cannot be undone"),
            (PanelOperation::DropStashEntry, "cannot be undone"),
            (PanelOperation::ResetToRemote, "cannot be undone"),
            (
                PanelOperation::ForcePushWithoutLease,
                "erase other people's changes",
            ),
            (PanelOperation::ForcePush, "disappear"),
            (PanelOperation::TrashUntrackedFiles, "trash is emptied"),
            (PanelOperation::Amend, "force push"),
            (PanelOperation::Uncommit, "force push"),
            (PanelOperation::SequencerAbort, "thrown away"),
            (PanelOperation::SequencerSkip, "not brought in"),
            (PanelOperation::ForceDeleteBranch, "can no longer be seen"),
            (PanelOperation::TrashNewFile, "trash is emptied"),
            (PanelOperation::RestoreHunk, "edits are lost"),
            (PanelOperation::RestoreAllUnstaged, "edits are lost"),
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
            "Only downloads the latest from the remote (the shared server). Your files and branches do not change. (git fetch)"
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
            (Some(SharedString::from("Reason")), 1, false),
        ] {
            let target = cx.update(|_, cx| cx.focus_handle());
            let menu = cx.update(|window, cx| {
                let target = target.clone();
                ContextMenu::build(window, cx, move |menu, _, _| {
                    menu.item(help_action_entry(
                        "Fetch",
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
