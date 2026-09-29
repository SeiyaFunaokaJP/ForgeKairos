//! A round-trippable editing model for Mermaid git graphs.
//!
//! The shape of this module mirrors [`crate::flowchart_model`]: [`parse_gitgraph`]
//! reads the body of a ```` ```mermaid ```` fence into [`GitGraphDocument`], and
//! [`emit_gitgraph`] writes it back. See [`crate::mermaid_model`] for the
//! invariants every diagram kind keeps.
//!
//! A git graph is a script: `commit` lands on whichever branch the last
//! `branch` or `checkout` moved to, and `merge` joins the heads the branches
//! have at that moment. The order of the statements is therefore the diagram,
//! and the model is one ordered list of them rather than `merman`'s commit
//! table, which cannot be turned back into the statements that built it.
//! Comments and accessibility statements keep their place inside that list.
//!
//! `merman`'s parser quietly skips a few things — the fields after a quoted
//! commit message, the keys a statement does not know, what follows a branch
//! name on a `checkout` line. Writing such a line back without them would drop
//! them for good, so [`parse_gitgraph`] refuses those lines instead.

use std::collections::HashMap;

use anyhow::{Result, anyhow, bail};
use merman::diagram::RenderSemanticModel;

use crate::mermaid_model::{INDENT, first_statement_line, join_lines, keyword_tail, scan_preamble};

/// The branch every git graph starts on unless its configuration names
/// another one.
pub const DEFAULT_MAIN_BRANCH: &str = "main";

/// Which way the branches run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitGraphDirection {
    LeftToRight,
    TopToBottom,
    BottomToTop,
}

impl GitGraphDirection {
    pub const ALL: [GitGraphDirection; 3] = [
        GitGraphDirection::LeftToRight,
        GitGraphDirection::TopToBottom,
        GitGraphDirection::BottomToTop,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::LeftToRight => "LR",
            Self::TopToBottom => "TB",
            Self::BottomToTop => "BT",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LeftToRight => i18n::t!("Left to right (LR)"),
            Self::TopToBottom => i18n::t!("Top to bottom (TB)"),
            Self::BottomToTop => i18n::t!("Bottom to top (BT)"),
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|direction| direction.keyword() == keyword)
    }
}

/// How a commit is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCommitType {
    Normal,
    Reverse,
    Highlight,
}

impl GitCommitType {
    pub const ALL: [GitCommitType; 3] = [
        GitCommitType::Normal,
        GitCommitType::Reverse,
        GitCommitType::Highlight,
    ];

    pub fn keyword(self) -> &'static str {
        match self {
            Self::Normal => "NORMAL",
            Self::Reverse => "REVERSE",
            Self::Highlight => "HIGHLIGHT",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => i18n::tc!("gitGraph commit type", "Normal"),
            Self::Reverse => i18n::tc!("gitGraph commit type", "Reverse"),
            Self::Highlight => i18n::tc!("gitGraph commit type", "Highlight"),
        }
    }

    fn from_keyword(keyword: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|commit_type| commit_type.keyword() == keyword.trim())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitCommit {
    /// `None` lets Mermaid make one up, which nothing else can then refer to.
    pub id: Option<String>,
    pub message: Option<String>,
    /// Every `tag:` in the order written. An empty string is a `tag: ""`,
    /// which Mermaid draws as an empty tag, so it is kept rather than dropped.
    pub tags: Vec<String>,
    /// `None` is a commit without `type:`, drawn the same as `NORMAL` but
    /// written back without the key.
    pub commit_type: Option<GitCommitType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranch {
    pub name: String,
    /// The `order:` value as typed, so that a half-typed number can be shown
    /// in its field and refused by [`GitGraphDocument::structure_error`]
    /// instead of vanishing.
    pub order: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCheckout {
    pub branch: String,
    /// Whether the source spelled it `switch`, which means the same thing.
    pub uses_switch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitMerge {
    pub branch: String,
    pub id: Option<String>,
    pub tags: Vec<String>,
    pub commit_type: Option<GitCommitType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCherryPick {
    /// The id of the commit being picked. Only a commit with an id of its own
    /// can be picked.
    pub id: String,
    /// Which parent of a merge commit is picked. Required for a merge commit and
    /// refused for any other.
    pub parent: Option<String>,
    /// The tags written on the statement. Without any Mermaid labels the new
    /// commit `cherry-pick:<id>`; a single `tag: ""` suppresses that label, so
    /// empty strings are kept as written.
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitGraphItem {
    Commit(GitCommit),
    Branch(GitBranch),
    Checkout(GitCheckout),
    Merge(GitMerge),
    CherryPick(GitCherryPick),
    /// A comment, a directive or an accessibility statement, kept verbatim at
    /// the place it was found. A statement spanning lines is one entry with
    /// embedded newlines and its original indentation.
    Preserved(String),
}

/// What kind of statement an item is, for the editor's menus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitGraphItemKind {
    Commit,
    Branch,
    Checkout,
    Merge,
    CherryPick,
}

impl GitGraphItemKind {
    pub const ALL: [GitGraphItemKind; 5] = [
        GitGraphItemKind::Commit,
        GitGraphItemKind::Branch,
        GitGraphItemKind::Checkout,
        GitGraphItemKind::Merge,
        GitGraphItemKind::CherryPick,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Commit => i18n::tc!("gitGraph operation", "Commit"),
            Self::Branch => i18n::tc!("gitGraph operation", "Branch"),
            Self::Checkout => i18n::tc!("gitGraph operation", "Checkout"),
            Self::Merge => i18n::tc!("gitGraph operation", "Merge"),
            Self::CherryPick => i18n::tc!("gitGraph operation", "Cherry-pick"),
        }
    }
}

impl GitGraphItem {
    pub fn kind(&self) -> Option<GitGraphItemKind> {
        match self {
            Self::Commit(_) => Some(GitGraphItemKind::Commit),
            Self::Branch(_) => Some(GitGraphItemKind::Branch),
            Self::Checkout(_) => Some(GitGraphItemKind::Checkout),
            Self::Merge(_) => Some(GitGraphItemKind::Merge),
            Self::CherryPick(_) => Some(GitGraphItemKind::CherryPick),
            Self::Preserved(_) => None,
        }
    }

    /// The id this statement gives the commit it creates, when it names one.
    pub fn commit_id(&self) -> Option<&str> {
        match self {
            Self::Commit(commit) => commit.id.as_deref(),
            Self::Merge(merge) => merge.id.as_deref(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitGraphDocument {
    /// Frontmatter, `%%{init}%%` directives and comments before `gitGraph`,
    /// verbatim.
    pub preamble: Vec<String>,
    /// `None` is a header without a direction, which Mermaid draws left to
    /// right.
    pub direction: Option<GitGraphDirection>,
    /// Whether a header without a direction was spelled `gitGraph:`.
    pub header_colon: bool,
    /// The branch the graph starts on, as the configuration in the preamble
    /// names it. It is not written anywhere by this model: it only exists so
    /// that the checks here agree with what Mermaid will do.
    pub main_branch: String,
    pub items: Vec<GitGraphItem>,
}

impl GitGraphDocument {
    /// A blank git graph, used when the editor is opened on an empty fence.
    pub fn empty() -> Self {
        Self {
            preamble: Vec::new(),
            direction: None,
            header_colon: false,
            main_branch: DEFAULT_MAIN_BRANCH.to_string(),
            items: Vec::new(),
        }
    }

    /// The main branch followed by every branch the graph creates, in the order
    /// they are created.
    pub fn branch_names(&self) -> Vec<&str> {
        std::iter::once(self.main_branch.as_str())
            .chain(self.items.iter().filter_map(|item| match item {
                GitGraphItem::Branch(branch) => Some(branch.name.as_str()),
                _ => None,
            }))
            .collect()
    }

    /// Every id a commit or a merge names, in the order written.
    pub fn commit_ids(&self) -> Vec<&str> {
        self.items
            .iter()
            .filter_map(GitGraphItem::commit_id)
            .collect()
    }

    /// The branch the statement at `index` runs on: the one the last `branch`
    /// or `checkout` before it moved to.
    pub fn branch_at(&self, index: usize) -> &str {
        self.items
            .iter()
            .take(index)
            .rev()
            .find_map(|item| match item {
                GitGraphItem::Branch(branch) => Some(branch.name.as_str()),
                GitGraphItem::Checkout(checkout) => Some(checkout.branch.as_str()),
                _ => None,
            })
            .unwrap_or(self.main_branch.as_str())
    }

    /// Points every `checkout` and `merge` that named a branch in `renames` at
    /// its new name. The `branch` statements themselves are left alone: the
    /// caller has already written the new names there.
    pub fn retarget_branches(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let renamed = |name: &str| {
            renames
                .iter()
                .find(|(old, _)| old == name)
                .map(|(_, new)| new.clone())
        };
        for item in &mut self.items {
            let target = match item {
                GitGraphItem::Checkout(checkout) => &mut checkout.branch,
                GitGraphItem::Merge(merge) => &mut merge.branch,
                _ => continue,
            };
            if let Some(new) = renamed(target) {
                *target = new;
            }
        }
    }

    /// Points every `cherry-pick` that named a commit id in `renames` at its new
    /// id, for both the commit it picks and the parent it names.
    pub fn retarget_commits(&mut self, renames: &[(String, String)]) {
        if renames.is_empty() {
            return;
        }
        let renamed = |id: &str| {
            renames
                .iter()
                .find(|(old, _)| old == id)
                .map(|(_, new)| new.clone())
        };
        for item in &mut self.items {
            let GitGraphItem::CherryPick(pick) = item else {
                continue;
            };
            if let Some(new) = renamed(&pick.id) {
                pick.id = new;
            }
            if let Some(parent) = &mut pick.parent
                && let Some(new) = renamed(parent)
            {
                *parent = new;
            }
        }
    }

    /// Why the statement at `index` cannot be taken out as it stands, or `None`
    /// when it can.
    ///
    /// A `branch` that something checks out or merges would leave those
    /// statements naming a branch that is never created, and the commits
    /// written right after it would silently move onto the branch before it. A
    /// commit that a `cherry-pick` names would leave the pick pointing at
    /// nothing. In each case the user is told which statements are in the way,
    /// so that they can be dealt with first.
    pub fn removal_blocker(&self, index: usize) -> Option<String> {
        let item = self.items.get(index)?;
        if let GitGraphItem::Branch(branch) = item {
            let references = self
                .items
                .iter()
                .enumerate()
                .filter(|(position, item)| {
                    *position != index
                        && match item {
                            GitGraphItem::Checkout(checkout) => checkout.branch == branch.name,
                            GitGraphItem::Merge(merge) => merge.branch == branch.name,
                            _ => false,
                        }
                })
                .map(|(position, _)| (position + 1).to_string())
                .collect::<Vec<_>>();
            if !references.is_empty() {
                let name = &branch.name;
                let list = references.join(i18n::tc!("gitGraph operation list separator", ", "));
                return Some(i18n::tn!(
                    references.len(),
                    "Branch `{name}` is referenced by operation {list}. Delete it first, or point it at another branch.",
                    "Branch `{name}` is referenced by operations {list}. Delete them first, or point them at another branch."
                ));
            }
            let runs_on_it = self
                .items
                .iter()
                .skip(index + 1)
                .take_while(|item| {
                    !matches!(item, GitGraphItem::Branch(_) | GitGraphItem::Checkout(_))
                })
                .any(|item| {
                    matches!(
                        item,
                        GitGraphItem::Commit(_)
                            | GitGraphItem::Merge(_)
                            | GitGraphItem::CherryPick(_)
                    )
                });
            if runs_on_it {
                return Some(i18n::t!(
                    "The operations right after branch `{}` run on this branch. Delete them first, or move them to another branch.",
                    branch.name
                ));
            }
            return None;
        }

        let id = item.commit_id()?;
        let pickers = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| match item {
                GitGraphItem::CherryPick(pick) => {
                    pick.id == id || pick.parent.as_deref() == Some(id)
                }
                _ => false,
            })
            .map(|(position, _)| (position + 1).to_string())
            .collect::<Vec<_>>();
        if pickers.is_empty() {
            return None;
        }
        let list = pickers.join(i18n::tc!("gitGraph operation list separator", ", "));
        Some(i18n::tn!(
            pickers.len(),
            "Commit `{id}` is referenced by the cherry-pick at operation {list}. Delete it first.",
            "Commit `{id}` is referenced by the cherry-picks at operations {list}. Delete them first."
        ))
    }

    /// Takes the statement at `index` out, or says why it cannot be.
    pub fn remove_item(&mut self, index: usize) -> Result<GitGraphItem, String> {
        if index >= self.items.len() {
            return Err(i18n::t!("That operation no longer exists.").to_string());
        }
        if let Some(blocker) = self.removal_blocker(index) {
            return Err(blocker);
        }
        Ok(self.items.remove(index))
    }

    /// Turns the statement at `index` into one of `kind`, carrying over what the
    /// two kinds share, or says why the statement it replaces cannot go.
    pub fn change_kind(&mut self, index: usize, kind: GitGraphItemKind) -> Result<(), String> {
        let Some(item) = self.items.get(index) else {
            return Err(i18n::t!("That operation no longer exists.").to_string());
        };
        if item.kind() == Some(kind) {
            return Ok(());
        }
        let replacement = match (item, kind) {
            (GitGraphItem::Merge(merge), GitGraphItemKind::Commit) => {
                GitGraphItem::Commit(GitCommit {
                    id: merge.id.clone(),
                    message: None,
                    tags: merge.tags.clone(),
                    commit_type: merge.commit_type,
                })
            }
            (GitGraphItem::Commit(commit), GitGraphItemKind::Merge) => {
                GitGraphItem::Merge(GitMerge {
                    branch: self.default_merge_source(index),
                    id: commit.id.clone(),
                    tags: commit.tags.clone(),
                    commit_type: commit.commit_type,
                })
            }
            (GitGraphItem::Checkout(checkout), GitGraphItemKind::Merge) => {
                GitGraphItem::Merge(GitMerge {
                    branch: checkout.branch.clone(),
                    id: None,
                    tags: Vec::new(),
                    commit_type: None,
                })
            }
            (GitGraphItem::Merge(merge), GitGraphItemKind::Checkout) => {
                GitGraphItem::Checkout(GitCheckout {
                    branch: merge.branch.clone(),
                    uses_switch: false,
                })
            }
            _ => self.new_item(kind, index),
        };
        let keeps_id =
            replacement.commit_id().is_some() && replacement.commit_id() == item.commit_id();
        if !keeps_id && let Some(blocker) = self.removal_blocker(index) {
            return Err(blocker);
        }
        if let Some(slot) = self.items.get_mut(index) {
            *slot = replacement;
        }
        Ok(())
    }

    /// A fresh statement of `kind` for position `index`, filled in with the
    /// choice most likely to be meant there so that it draws straight away.
    pub fn new_item(&self, kind: GitGraphItemKind, index: usize) -> GitGraphItem {
        match kind {
            GitGraphItemKind::Commit => GitGraphItem::Commit(GitCommit::default()),
            GitGraphItemKind::Branch => GitGraphItem::Branch(GitBranch {
                name: self.unused_branch_name(),
                order: None,
            }),
            GitGraphItemKind::Checkout => {
                let current = self.branch_at(index);
                let branch = self
                    .branch_names()
                    .into_iter()
                    .find(|name| *name != current)
                    .unwrap_or(self.main_branch.as_str())
                    .to_string();
                GitGraphItem::Checkout(GitCheckout {
                    branch,
                    uses_switch: false,
                })
            }
            GitGraphItemKind::Merge => GitGraphItem::Merge(GitMerge {
                branch: self.default_merge_source(index),
                id: None,
                tags: Vec::new(),
                commit_type: None,
            }),
            GitGraphItemKind::CherryPick => {
                let current = self.branch_at(index);
                let id = self
                    .items
                    .iter()
                    .enumerate()
                    .take(index)
                    .rev()
                    .find_map(|(position, item)| match item {
                        GitGraphItem::Commit(commit) if self.branch_at(position) != current => {
                            commit.id.clone()
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                GitGraphItem::CherryPick(GitCherryPick {
                    id,
                    parent: None,
                    tags: Vec::new(),
                })
            }
        }
    }

    /// The branch a merge at `index` most likely means: the most recently
    /// created one that is not the branch the merge runs on.
    fn default_merge_source(&self, index: usize) -> String {
        let current = self.branch_at(index);
        self.branch_names()
            .into_iter()
            .rev()
            .find(|name| *name != current)
            .unwrap_or(self.main_branch.as_str())
            .to_string()
    }

    fn unused_branch_name(&self) -> String {
        let taken = self.branch_names();
        let mut number = 1;
        loop {
            let name = if number == 1 {
                "feature".to_string()
            } else {
                format!("feature-{number}")
            };
            if !taken.contains(&name.as_str()) {
                return name;
            }
            number += 1;
        }
    }

    /// Why the document cannot be written back as a git graph Mermaid draws, in
    /// the display language, or `None` when it can.
    ///
    /// The spelling of every value is checked first. The rest replays the
    /// statements the way Mermaid's git graph database does and reports the
    /// first one it would throw on, in the words of the operation the user sees
    /// in the form. Two commits with the same id are refused as well, although
    /// Mermaid only warns about them: a `cherry-pick` naming that id, and a
    /// rename following it, could not tell which of the two was meant.
    pub fn structure_error(&self) -> Option<String> {
        for (index, item) in self.items.iter().enumerate() {
            if let Some(problem) = item_spelling_problem(item) {
                return Some(i18n::t!("Operation {}: {problem}", index + 1));
            }
        }
        replay(self).err()
    }
}

fn item_spelling_problem(item: &GitGraphItem) -> Option<String> {
    let check_id = |id: &Option<String>| {
        id.as_ref()
            .filter(|id| !is_valid_gitgraph_text(id) || id.is_empty())
            .map(|id| i18n::t!("The ID `{id}` cannot be used (no `\"`, `%%` or line breaks)."))
    };
    let check_tags = |tags: &[String]| {
        tags.iter()
            .find(|tag| !is_valid_gitgraph_text(tag))
            .map(|tag| i18n::t!("The tag `{tag}` cannot be used (no `\"`, `%%` or line breaks)."))
    };
    match item {
        GitGraphItem::Commit(commit) => check_id(&commit.id)
            .or_else(|| check_tags(&commit.tags))
            .or_else(|| {
                commit
                    .message
                    .as_ref()
                    .filter(|message| !is_valid_gitgraph_text(message))
                    .map(|message| {
                        i18n::t!(
                            "The message `{message}` cannot be used (no `\"`, `%%` or line breaks)."
                        )
                    })
            }),
        GitGraphItem::Branch(branch) => {
            if !is_valid_gitgraph_branch_name(&branch.name) {
                return Some(branch_name_problem(&branch.name));
            }
            branch
                .order
                .as_ref()
                .filter(|order| !is_valid_gitgraph_order(order))
                .map(|order| i18n::t!("Write the order `{order}` as a whole number."))
        }
        GitGraphItem::Checkout(checkout) => (!is_valid_gitgraph_branch_name(&checkout.branch))
            .then(|| branch_name_problem(&checkout.branch)),
        GitGraphItem::Merge(merge) => {
            if !is_valid_gitgraph_branch_name(&merge.branch) {
                return Some(branch_name_problem(&merge.branch));
            }
            check_id(&merge.id).or_else(|| check_tags(&merge.tags))
        }
        GitGraphItem::CherryPick(pick) => {
            if pick.id.is_empty() {
                return Some(i18n::t!("Choose the commit to cherry-pick.").to_string());
            }
            check_id(&Some(pick.id.clone()))
                .or_else(|| check_id(&pick.parent))
                .or_else(|| check_tags(&pick.tags))
        }
        GitGraphItem::Preserved(_) => None,
    }
}

fn branch_name_problem(name: &str) -> String {
    if name.is_empty() {
        i18n::t!("The branch name is empty.").to_string()
    } else {
        i18n::t!("The branch name `{name}` cannot be used (no `\"`, `%%` or line breaks).")
    }
}

/// A commit as far as the checks need it. A commit without an id of its own is
/// told apart by its position, so that it can never be confused with one the
/// user named.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CommitKey {
    Named(String),
    Unnamed(usize),
}

struct ReplayedCommit {
    branch: String,
    is_merge: bool,
    parents: Vec<CommitKey>,
}

/// Runs the statements through the same rules as `merman`'s git graph database
/// (`GitGraphDb::branch`, `checkout`, `merge` and `cherry_pick`), in the same
/// order, and reports the first one they reject.
fn replay(document: &GitGraphDocument) -> Result<(), String> {
    let mut heads: Vec<(String, Option<CommitKey>)> = vec![(document.main_branch.clone(), None)];
    let mut commits: HashMap<CommitKey, ReplayedCommit> = HashMap::new();
    let mut current = document.main_branch.clone();
    let mut head: Option<CommitKey> = None;

    fn head_of<'a>(
        heads: &'a [(String, Option<CommitKey>)],
        name: &str,
    ) -> Option<&'a Option<CommitKey>> {
        heads
            .iter()
            .find(|(branch, _)| branch == name)
            .map(|(_, head)| head)
    }
    fn set_head(heads: &mut [(String, Option<CommitKey>)], name: &str, key: CommitKey) {
        if let Some((_, head)) = heads.iter_mut().find(|(branch, _)| branch == name) {
            *head = Some(key);
        }
    }

    for (index, item) in document.items.iter().enumerate() {
        let number = index + 1;
        match item {
            GitGraphItem::Commit(commit) => {
                let key = match &commit.id {
                    Some(id) => {
                        let key = CommitKey::Named(id.clone());
                        if commits.contains_key(&key) {
                            return Err(i18n::t!(
                                "Operation {number} (commit): the ID `{id}` is used more than once."
                            ));
                        }
                        key
                    }
                    None => CommitKey::Unnamed(index),
                };
                commits.insert(
                    key.clone(),
                    ReplayedCommit {
                        branch: current.clone(),
                        is_merge: false,
                        parents: head.iter().cloned().collect(),
                    },
                );
                set_head(&mut heads, &current, key.clone());
                head = Some(key);
            }
            GitGraphItem::Branch(branch) => {
                if head_of(&heads, &branch.name).is_some() {
                    return Err(i18n::t!(
                        "Operation {number} (branch): branch `{}` already exists (to switch to it, use a checkout).",
                        branch.name
                    ));
                }
                heads.push((branch.name.clone(), head.clone()));
                current = branch.name.clone();
            }
            GitGraphItem::Checkout(checkout) => {
                let Some(branch_head) = head_of(&heads, &checkout.branch) else {
                    return Err(i18n::t!(
                        "Operation {number} (checkout): branch `{}` has not been created yet (create it with a branch operation first).",
                        checkout.branch
                    ));
                };
                head = branch_head.clone();
                current = checkout.branch.clone();
            }
            GitGraphItem::Merge(merge) => {
                let other = &merge.branch;
                if *other == current {
                    return Err(i18n::t!(
                        "Operation {number} (merge): branch `{other}` cannot be merged into itself."
                    ));
                }
                let Some(current_head) = head_of(&heads, &current).cloned().flatten() else {
                    return Err(i18n::t!(
                        "Operation {number} (merge): the current branch `{current}` has no commits yet."
                    ));
                };
                let Some(other_head) = head_of(&heads, other) else {
                    return Err(i18n::t!(
                        "Operation {number} (merge): branch `{other}` has not been created yet."
                    ));
                };
                let Some(other_head) = other_head.clone() else {
                    return Err(i18n::t!(
                        "Operation {number} (merge): branch `{other}` has no commits yet."
                    ));
                };
                if commits
                    .get(&current_head)
                    .is_some_and(|commit| commit.branch == *other)
                {
                    return Err(i18n::t!(
                        "Operation {number} (merge): `{current}` has no commits since it branched off `{other}`, so it cannot be merged."
                    ));
                }
                if current_head == other_head {
                    return Err(i18n::t!(
                        "Operation {number} (merge): the heads of `{current}` and `{other}` are the same commit."
                    ));
                }
                let key = match &merge.id {
                    Some(id) => {
                        let key = CommitKey::Named(id.clone());
                        if commits.contains_key(&key) {
                            return Err(i18n::t!(
                                "Operation {number} (merge): the ID `{id}` is already in use."
                            ));
                        }
                        key
                    }
                    None => CommitKey::Unnamed(index),
                };
                commits.insert(
                    key.clone(),
                    ReplayedCommit {
                        branch: current.clone(),
                        is_merge: true,
                        parents: vec![current_head, other_head],
                    },
                );
                set_head(&mut heads, &current, key.clone());
                head = Some(key);
            }
            GitGraphItem::CherryPick(pick) => {
                let Some(source) = commits.get(&CommitKey::Named(pick.id.clone())) else {
                    return Err(i18n::t!(
                        "Operation {number} (cherry-pick): commit `{}` does not come before it.",
                        pick.id
                    ));
                };
                if let Some(parent) = &pick.parent
                    && !source.parents.contains(&CommitKey::Named(parent.clone()))
                {
                    return Err(i18n::t!(
                        "Operation {number} (cherry-pick): `{parent}` is not a direct parent of `{}`.",
                        pick.id
                    ));
                }
                if source.is_merge && pick.parent.is_none() {
                    return Err(i18n::t!(
                        "Operation {number} (cherry-pick): `{}` is a merge commit, so specify its parent.",
                        pick.id
                    ));
                }
                if source.branch == current {
                    return Err(i18n::t!(
                        "Operation {number} (cherry-pick): `{}` is already on the current branch `{current}`.",
                        pick.id
                    ));
                }
                if head_of(&heads, &current).cloned().flatten().is_none() {
                    return Err(i18n::t!(
                        "Operation {number} (cherry-pick): the current branch `{current}` has no commits yet."
                    ));
                }
                let source_key = CommitKey::Named(pick.id.clone());
                let key = CommitKey::Unnamed(index);
                commits.insert(
                    key.clone(),
                    ReplayedCommit {
                        branch: current.clone(),
                        is_merge: false,
                        parents: head.iter().cloned().chain([source_key]).collect(),
                    },
                );
                set_head(&mut heads, &current, key.clone());
                head = Some(key);
            }
            GitGraphItem::Preserved(_) => {}
        }
    }
    Ok(())
}

/// Whether `source` is a git graph. Cheap enough to call before parsing.
pub fn is_gitgraph_source(source: &str) -> bool {
    first_statement_line(source).is_some_and(|line| header_tail(line).is_some())
}

/// What follows `gitGraph` on the header line, or `None` when the line is not
/// a git graph header. `gitGraph:` counts as well as `gitGraph LR:`.
fn header_tail(line: &str) -> Option<&str> {
    if let Some(rest) = keyword_tail(line, "gitGraph") {
        return Some(rest);
    }
    line.strip_prefix("gitGraph")
        .filter(|rest| rest.starts_with(':'))
}

/// Reads `source` — the body of a ```` ```mermaid ```` fence — into an editable
/// document.
pub fn parse_gitgraph(source: &str) -> Result<GitGraphDocument> {
    if let Some(problem) = crate::mermaid_model::preprocessing_problem(source) {
        bail!("{problem}");
    }
    let mut main_branch = DEFAULT_MAIN_BRANCH.to_string();
    if !source.trim().is_empty() {
        let parsed = crate::parse_for_render_model(source)
            .map_err(|error| {
                anyhow!(i18n::t!(
                    "Mermaid could not parse this diagram ({error}). Edit it as text."
                ))
            })?
            .ok_or_else(|| anyhow!(i18n::t!("Mermaid could not read this as a diagram.")))?;
        if !matches!(parsed.model(), RenderSemanticModel::GitGraph(_)) {
            bail!(i18n::t!("This diagram is not a Git graph."));
        }
        // The main branch is named by the configuration, which can come from
        // the frontmatter, an init directive or both; `merman` has already
        // merged them the way Mermaid does, so its answer is taken as is.
        if let Some(name) = parsed
            .metadata()
            .effective_config
            .get_str("gitGraph.mainBranchName")
        {
            main_branch = name.to_string();
        }
    }

    let lines = source.lines().collect::<Vec<_>>();
    let preamble = scan_preamble(&lines);
    let mut document = GitGraphDocument {
        preamble: preamble
            .preserved
            .into_iter()
            .map(|line| line.text)
            .collect(),
        direction: None,
        header_colon: false,
        main_branch,
        items: Vec::new(),
    };

    let Some(header_index) = preamble.header else {
        return Ok(document);
    };
    let header = lines.get(header_index).copied().unwrap_or_default().trim();
    let Some(tail) = header_tail(header) else {
        bail!(i18n::t!(
            "This is not a Git graph (only diagrams that start with `gitGraph` can be edited in the structured editor)."
        ));
    };
    parse_header(tail, &mut document)?;

    let mut index = header_index + 1;
    while let Some(line) = lines.get(index).copied() {
        let trimmed = line.trim();
        let start = index;
        index += 1;
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with("%%") {
            // A directive that is never closed is only a comment line to
            // Mermaid, which goes on to draw the statements after it; taking
            // them into the preserved entry would hide them from the form.
            if trimmed.starts_with("%%{")
                && !trimmed.contains("}%%")
                && let Some(end) = lines
                    .iter()
                    .skip(index)
                    .position(|line| line.contains("}%%"))
            {
                index += end + 1;
            }
            // A one-line comment is re-indented on the way out like every
            // other statement, so it is held without its indentation.
            document
                .items
                .push(GitGraphItem::Preserved(if index == start + 1 {
                    trimmed.to_string()
                } else {
                    join_lines(lines.get(start..index).unwrap_or_default())
                }));
            continue;
        }
        // Mermaid and `merman` 0.8 read a `title` line as the diagram's title;
        // the form does not edit it, so it stays the line it is.
        if is_acc_statement(trimmed, "accTitle")
            || is_acc_statement(trimmed, "accDescr")
            || keyword_tail(trimmed, "title").is_some()
        {
            document
                .items
                .push(GitGraphItem::Preserved(trimmed.to_string()));
            continue;
        }
        if is_acc_block_start(trimmed) {
            // `merman` reads the block up to the next line that starts with
            // `}`, whatever the opening line holds after its `{`.
            while lines
                .get(index)
                .is_some_and(|line| !line.trim().starts_with('}'))
            {
                index += 1;
            }
            if index < lines.len() {
                index += 1;
            }
            document.items.push(GitGraphItem::Preserved(join_lines(
                lines.get(start..index).unwrap_or_default(),
            )));
            continue;
        }
        let item = parse_statement(trimmed).map_err(|reason| {
            anyhow!(i18n::t!(
                "`{trimmed}` is not supported by the structured editor ({reason}). Edit it as text."
            ))
        })?;
        document.items.push(item);
    }

    Ok(document)
}

/// Reads what follows `gitGraph`: nothing, `:`, or a direction ended by `:`.
fn parse_header(tail: &str, document: &mut GitGraphDocument) -> Result<()> {
    let tail = tail.trim();
    if tail.is_empty() {
        return Ok(());
    }
    if tail == ":" {
        document.header_colon = true;
        return Ok(());
    }
    let word = tail.trim_end_matches(':').trim();
    match GitGraphDirection::from_keyword(word) {
        Some(direction) => {
            document.direction = Some(direction);
            Ok(())
        }
        None => bail!(i18n::t!(
            "`{tail}` on the `gitGraph` line is not supported by the structured editor (the direction is one of LR / TB / BT). Edit it as text."
        )),
    }
}

/// `merman` takes `accTitle` and inline `accDescr` only when a `:` follows the
/// keyword, spelled with this exact case.
fn is_acc_statement(line: &str, keyword: &str) -> bool {
    line.strip_prefix(keyword)
        .is_some_and(|rest| rest.trim_start().starts_with(':'))
}

fn is_acc_block_start(line: &str) -> bool {
    line.strip_prefix("accDescr")
        .is_some_and(|rest| rest.trim_start().starts_with('{'))
}

/// A statement's words, read the way `merman`'s `LineParser` reads them.
struct Cursor<'a> {
    input: &'a str,
    position: usize,
}

impl<'a> Cursor<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }

    fn rest(&self) -> &'a str {
        self.input.get(self.position..).unwrap_or_default()
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn advance(&mut self) {
        if let Some(character) = self.peek() {
            self.position += character.len_utf8();
        }
    }

    fn skip_whitespace(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.advance();
        }
    }

    fn is_done(&mut self) -> bool {
        self.skip_whitespace();
        self.rest().is_empty()
    }

    fn word(&mut self) -> Option<&'a str> {
        self.skip_whitespace();
        let start = self.position;
        while self
            .peek()
            .is_some_and(|character| !character.is_whitespace() && character != ':')
        {
            self.advance();
        }
        (self.position > start).then(|| self.input.get(start..self.position).unwrap_or_default())
    }

    fn quoted(&mut self) -> Result<String, String> {
        self.skip_whitespace();
        if self.peek() != Some('"') {
            return Err(i18n::t!("`\"` is required").to_string());
        }
        self.advance();
        let start = self.position;
        while self.peek().is_some_and(|character| character != '"') {
            self.advance();
        }
        if self.peek() != Some('"') {
            return Err(i18n::t!("the quotes are not closed").to_string());
        }
        let text = self.input.get(start..self.position).unwrap_or_default();
        self.advance();
        Ok(text.to_string())
    }

    fn name(&mut self) -> Result<String, String> {
        self.skip_whitespace();
        if self.peek() == Some('"') {
            return self.quoted();
        }
        let start = self.position;
        while self
            .peek()
            .is_some_and(|character| !character.is_whitespace())
        {
            self.advance();
        }
        if self.position == start {
            return Err(i18n::t!("the branch name is missing").to_string());
        }
        Ok(self
            .input
            .get(start..self.position)
            .unwrap_or_default()
            .to_string())
    }

    /// The `key: value` pairs up to the end of the statement. `merman` stops
    /// reading at the first thing that is not a key and ignores the rest of the
    /// line; that rest would be lost on the way back, so it is refused here.
    fn pairs(&mut self) -> Result<Vec<(&'a str, String)>, String> {
        let mut pairs = Vec::new();
        while !self.is_done() {
            let Some(key) = self.word() else {
                return Err(i18n::t!("cannot read `{}`", self.rest()));
            };
            self.skip_whitespace();
            if self.peek() != Some(':') {
                return Err(i18n::t!("there is no `:` after `{key}`"));
            }
            self.advance();
            self.skip_whitespace();
            let value = if self.peek() == Some('"') {
                self.quoted()?
            } else {
                self.word().unwrap_or_default().to_string()
            };
            pairs.push((key, value));
        }
        Ok(pairs)
    }
}

fn parse_statement(statement: &str) -> Result<GitGraphItem, String> {
    let mut cursor = Cursor::new(statement);
    let command = cursor.word().unwrap_or_default();
    match command {
        "commit" => parse_commit(&mut cursor).map(GitGraphItem::Commit),
        "branch" => {
            let name = cursor.name()?;
            check_branch_name(&name)?;
            let mut order = None;
            for (key, value) in cursor.pairs()? {
                match key {
                    "order" => {
                        let value = value.trim().to_string();
                        if !is_valid_gitgraph_order(&value) {
                            return Err(i18n::t!("the order `{value}` is not a whole number"));
                        }
                        set_once(&mut order, value, key)?;
                    }
                    other => return Err(i18n::t!("`branch` cannot take `{other}:`")),
                }
            }
            Ok(GitGraphItem::Branch(GitBranch { name, order }))
        }
        "checkout" | "switch" => {
            let branch = cursor.name()?;
            check_branch_name(&branch)?;
            if !cursor.is_done() {
                return Err(i18n::t!(
                    "`{}` after the branch name is ignored",
                    cursor.rest()
                ));
            }
            Ok(GitGraphItem::Checkout(GitCheckout {
                branch,
                uses_switch: command == "switch",
            }))
        }
        "merge" => {
            let branch = cursor.name()?;
            check_branch_name(&branch)?;
            let mut merge = GitMerge {
                branch,
                id: None,
                tags: Vec::new(),
                commit_type: None,
            };
            for (key, value) in cursor.pairs()? {
                match key {
                    "id" => {
                        check_text(&value)?;
                        if !value.is_empty() {
                            set_once(&mut merge.id, value, key)?;
                        }
                    }
                    "tag" => {
                        check_text(&value)?;
                        merge.tags.push(value);
                    }
                    "type" => set_once(&mut merge.commit_type, commit_type(&value)?, key)?,
                    other => return Err(i18n::t!("`merge` cannot take `{other}:`")),
                }
            }
            Ok(GitGraphItem::Merge(merge))
        }
        "cherry-pick" => {
            let mut id = None;
            let mut pick = GitCherryPick {
                id: String::new(),
                parent: None,
                tags: Vec::new(),
            };
            for (key, value) in cursor.pairs()? {
                check_text(&value)?;
                match key {
                    "id" => set_once(&mut id, value, key)?,
                    "parent" => {
                        if !value.is_empty() {
                            set_once(&mut pick.parent, value, key)?;
                        }
                    }
                    "tag" => pick.tags.push(value),
                    other => return Err(i18n::t!("`cherry-pick` cannot take `{other}:`")),
                }
            }
            pick.id = id.unwrap_or_default();
            if pick.id.is_empty() {
                return Err(i18n::t!("`id:` is missing").to_string());
            }
            Ok(GitGraphItem::CherryPick(pick))
        }
        other => Err(i18n::t!("`{other}` is not a Git graph statement")),
    }
}

fn parse_commit(cursor: &mut Cursor) -> Result<GitCommit, String> {
    let mut commit = GitCommit::default();
    cursor.skip_whitespace();
    if cursor.peek() == Some('"') {
        let message = cursor.quoted()?;
        check_text(&message)?;
        if !cursor.is_done() {
            return Err(i18n::t!(
                "`{}` after the quoted message is ignored",
                cursor.rest()
            ));
        }
        commit.message = Some(message);
        return Ok(commit);
    }
    for (key, value) in cursor.pairs()? {
        match key {
            "id" => {
                check_text(&value)?;
                if !value.is_empty() {
                    set_once(&mut commit.id, value, key)?;
                }
            }
            "msg" => {
                check_text(&value)?;
                set_once(&mut commit.message, value, key)?;
            }
            "tag" => {
                check_text(&value)?;
                commit.tags.push(value);
            }
            "type" => set_once(&mut commit.commit_type, commit_type(&value)?, key)?,
            other => return Err(i18n::t!("`commit` cannot take `{other}:`")),
        }
    }
    Ok(commit)
}

/// `merman` lets the last of two equal keys win, which would drop the first one
/// on the way back.
fn set_once<T>(slot: &mut Option<T>, value: T, key: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(i18n::t!("`{key}:` is written twice"));
    }
    *slot = Some(value);
    Ok(())
}

fn commit_type(value: &str) -> Result<GitCommitType, String> {
    GitCommitType::from_keyword(value)
        .ok_or_else(|| i18n::t!("the type `{value}` is not one of NORMAL / REVERSE / HIGHLIGHT"))
}

fn check_text(text: &str) -> Result<(), String> {
    if is_valid_gitgraph_text(text) {
        Ok(())
    } else {
        Err(i18n::t!("`{text}` cannot be written back"))
    }
}

fn check_branch_name(name: &str) -> Result<(), String> {
    if is_valid_gitgraph_branch_name(name) {
        Ok(())
    } else {
        Err(i18n::t!("the branch name `{name}` cannot be written back"))
    }
}

/// Whether `text` can be written as a quoted id, message or tag. The quote has
/// no escape, and `%%` would be read as the start of a comment or directive by
/// the preprocessor.
pub fn is_valid_gitgraph_text(text: &str) -> bool {
    !text.contains('"') && !text.contains("%%") && !text.contains(['\n', '\r'])
}

/// Whether `name` can be written as a branch name. One with spaces or a `:` in
/// it is written in quotes, so only what quotes cannot hold is ruled out.
pub fn is_valid_gitgraph_branch_name(name: &str) -> bool {
    !name.is_empty() && name.trim() == name && is_valid_gitgraph_text(name)
}

/// Whether `text` is an `order:` value `merman` reads: an integer.
pub fn is_valid_gitgraph_order(text: &str) -> bool {
    text.parse::<i64>().is_ok()
}

/// A branch name as a statement spells it: bare when the name is one word, in
/// quotes when it would otherwise be cut short or read as a key.
/// Words Mermaid's lexer takes for a keyword before it takes them for a branch
/// id, so that a branch named after one has to be quoted.
const GITGRAPH_KEYWORDS: [&str; 13] = [
    "commit",
    "branch",
    "checkout",
    "switch",
    "merge",
    "cherry-pick",
    "NORMAL",
    "REVERSE",
    "HIGHLIGHT",
    "LR",
    "TB",
    "BT",
    "gitGraph",
];

/// `name` as a statement writes it: bare when Mermaid's lexer reads it as one
/// branch id (ASCII word characters, with `-` `.` `/` inside and `-` at the
/// end, and not a keyword), in quotes otherwise — Japanese included, which
/// Mermaid and `merman` 0.8 refuse bare.
fn branch_token(name: &str) -> String {
    let is_word = |character: char| character.is_ascii_alphanumeric() || character == '_';
    let is_bare = !GITGRAPH_KEYWORDS.contains(&name)
        && !name.starts_with("title")
        && name.chars().next().is_some_and(is_word)
        && name
            .chars()
            .last()
            .is_some_and(|character| is_word(character) || character == '-')
        && name
            .chars()
            .all(|character| is_word(character) || matches!(character, '-' | '.' | '/'));
    if is_bare {
        name.to_string()
    } else {
        format!("\"{name}\"")
    }
}

fn push_pair(out: &mut String, key: &str, value: &str) {
    out.push(' ');
    out.push_str(key);
    out.push_str(": \"");
    out.push_str(value);
    out.push('"');
}

fn push_type(out: &mut String, commit_type: Option<GitCommitType>) {
    if let Some(commit_type) = commit_type {
        out.push_str(" type: ");
        out.push_str(commit_type.keyword());
    }
}

/// Writes `document` back as Mermaid source, without a trailing newline.
pub fn emit_gitgraph(document: &GitGraphDocument) -> String {
    let mut out = String::new();
    for text in &document.preamble {
        out.push_str(text);
        out.push('\n');
    }

    out.push_str("gitGraph");
    match document.direction {
        // Mermaid's grammar only takes a direction with the colon after it.
        Some(direction) => {
            out.push(' ');
            out.push_str(direction.keyword());
            out.push(':');
        }
        None if document.header_colon => out.push(':'),
        None => {}
    }
    out.push('\n');

    for item in &document.items {
        match item {
            GitGraphItem::Preserved(text) => {
                // A multi-line entry carries its own indentation; a single
                // line was trimmed when it was read.
                if !text.contains('\n') {
                    out.push_str(INDENT);
                }
                out.push_str(text);
            }
            GitGraphItem::Commit(commit) => {
                out.push_str(INDENT);
                out.push_str("commit");
                if let Some(id) = &commit.id {
                    push_pair(&mut out, "id", id);
                }
                if let Some(message) = &commit.message {
                    push_pair(&mut out, "msg", message);
                }
                for tag in &commit.tags {
                    push_pair(&mut out, "tag", tag);
                }
                push_type(&mut out, commit.commit_type);
            }
            GitGraphItem::Branch(branch) => {
                out.push_str(INDENT);
                out.push_str("branch ");
                out.push_str(&branch_token(&branch.name));
                if let Some(order) = &branch.order {
                    out.push_str(" order: ");
                    out.push_str(order);
                }
            }
            GitGraphItem::Checkout(checkout) => {
                out.push_str(INDENT);
                out.push_str(if checkout.uses_switch {
                    "switch "
                } else {
                    "checkout "
                });
                out.push_str(&branch_token(&checkout.branch));
            }
            GitGraphItem::Merge(merge) => {
                out.push_str(INDENT);
                out.push_str("merge ");
                out.push_str(&branch_token(&merge.branch));
                if let Some(id) = &merge.id {
                    push_pair(&mut out, "id", id);
                }
                for tag in &merge.tags {
                    push_pair(&mut out, "tag", tag);
                }
                push_type(&mut out, merge.commit_type);
            }
            GitGraphItem::CherryPick(pick) => {
                out.push_str(INDENT);
                out.push_str("cherry-pick");
                push_pair(&mut out, "id", &pick.id);
                if let Some(parent) = &pick.parent {
                    push_pair(&mut out, "parent", parent);
                }
                for tag in &pick.tags {
                    push_pair(&mut out, "tag", tag);
                }
            }
        }
        out.push('\n');
    }

    while out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MermaidTheme, render_to_svg};

    fn parse(source: &str) -> GitGraphDocument {
        parse_gitgraph(source).unwrap_or_else(|error| panic!("{source} should parse: {error}"))
    }

    fn refusal(source: &str) -> String {
        match parse_gitgraph(source) {
            Ok(document) => panic!("{source} should be refused, got {document:?}"),
            Err(error) => format!("{error}"),
        }
    }

    /// Emitting what was parsed must give text that parses to the same
    /// document and emits the same text again.
    fn assert_stable(source: &str) -> String {
        let first = parse(source);
        let first_text = emit_gitgraph(&first);
        let second = parse_gitgraph(&first_text)
            .unwrap_or_else(|error| panic!("emitted source should parse: {error}\n{first_text}"));
        let second_text = emit_gitgraph(&second);
        assert_eq!(
            first, second,
            "round trip changed the document:\n{first_text}"
        );
        assert_eq!(first_text, second_text, "emitting twice changed the source");
        first_text
    }

    fn assert_renders(source: &str) {
        let svg = render_to_svg(source, &MermaidTheme::default())
            .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"));
        assert!(
            !svg.contains("Syntax error in text"),
            "source rendered an error diagram:\n{source}"
        );
    }

    const TEMPLATE: &str = "gitGraph\n    commit id: \"初期化\"\n    branch feature\n    checkout feature\n    commit id: \"機能を追加\"\n    checkout main\n    merge feature";

    #[test]
    fn the_insert_template_round_trips_byte_for_byte() {
        let document = parse(TEMPLATE);
        assert_eq!(emit_gitgraph(&document), TEMPLATE);
        assert_eq!(document.structure_error(), None);
        assert_eq!(document.items.len(), 6);
        assert_renders(TEMPLATE);
    }

    #[test]
    fn every_spelling_merman_accepts_is_written_back_the_same_way_twice() {
        for source in [
            TEMPLATE,
            "gitGraph:\n  commit",
            "gitGraph TB:\n  commit\n  commit",
            "gitGraph BT:\n  commit id: \"a\" tag: \"v1\" tag: \"\" type: HIGHLIGHT",
            "gitGraph\n  commit \"メッセージだけ\"\n  commit msg: \"m\" id:\"x\"",
            "gitGraph\n  commit\n  branch \"my feature\" order: 2\n  commit\n  switch main\n  commit\n  merge \"my feature\" id: \"M\" tag: \"t\" type: REVERSE",
            "gitGraph\n  commit id: \"A\"\n  branch dev\n  commit id: \"B\"\n  checkout main\n  cherry-pick id: \"B\" tag: \"\"",
            "gitGraph\n  commit id: \"A\"\n  branch dev\n  commit id: \"B\"\n  checkout main\n  commit id: \"C\"\n  merge dev id: \"M\"\n  branch rel\n  commit\n  cherry-pick id: \"M\" parent: \"C\"",
            "gitGraph\n  title 題\n  commit\n  branch \"開発\"\n  commit",
        ] {
            let emitted = assert_stable(source);
            assert_renders(&emitted);
            assert_eq!(parse(&emitted).structure_error(), None, "{emitted}");
        }
        // A branch named after a keyword stays quoted: Mermaid reads a bare
        // `branch branch` as two keywords.
        let emitted = assert_stable(
            "gitGraph\n  commit\n  branch \"branch\"\n  checkout \"branch\"\n  commit\n  branch commit01",
        );
        assert!(
            emitted.contains("branch \"branch\"\n    checkout \"branch\""),
            "{emitted}"
        );
        assert!(emitted.contains("branch commit01"), "{emitted}");
    }

    #[test]
    fn comments_directives_and_accessibility_keep_their_place() {
        let source = "---\ntitle: 図\nconfig:\n  gitGraph:\n    showBranches: false\n---\n%%{init: {'gitGraph': {'rotateCommitLabel': true}}}%%\n%% 前置き\ngitGraph\n    accTitle: 概要\n    commit\n    %% 途中のメモ\n    branch dev\n    accDescr {\n      複数行の\n      説明\n    }\n    commit\n    %% 末尾";
        let document = parse(source);
        assert_eq!(document.preamble.len(), 3);
        let emitted = assert_stable(source);
        assert_eq!(emitted, source);
        assert!(matches!(
            document.items.get(2),
            Some(GitGraphItem::Preserved(text)) if text == "%% 途中のメモ"
        ));
        assert_renders(&emitted);
    }

    #[test]
    fn the_main_branch_name_comes_from_the_configuration() {
        let source = "%%{init: {'gitGraph': {'mainBranchName': 'trunk'}}}%%\ngitGraph\n    commit\n    branch dev\n    commit\n    checkout trunk\n    merge dev";
        let document = parse(source);
        assert_eq!(document.main_branch, "trunk");
        assert_eq!(document.structure_error(), None);
        assert_eq!(document.branch_names(), vec!["trunk", "dev"]);

        let frontmatter = "---\nconfig:\n  gitGraph:\n    mainBranchName: develop\n---\ngitGraph\n    commit\n    checkout develop";
        assert_eq!(parse(frontmatter).main_branch, "develop");
    }

    #[test]
    fn a_line_the_model_would_shorten_is_refused() {
        for (source, fragment) in [
            ("gitGraph\n  commit \"m\" id: \"x\"", "is ignored"),
            ("gitGraph\n  commit id: \"a\" id: \"b\"", "written twice"),
        ] {
            let message = refusal(source);
            assert!(message.contains(fragment), "{source}: {message}");
        }
    }

    #[test]
    fn a_diagram_merman_rejects_does_not_open() {
        // `merman` 0.8 refuses, as Mermaid does, what 0.7 let through: words
        // after a `checkout`, the `cherryPick` spelling, a direction without
        // its `:`, an unquoted value, and a branch name that is not an ASCII
        // word unless it is quoted.
        for source in [
            "gitGraph\n  commit;",
            "gitGraph\n  checkout nowhere",
            "gitGraph\n  commit\n  branch b\n  merge b",
            "gitGraph\n  commit\n  branch b\n  checkout b extra",
            "gitGraph\n  commit id: \"A\"\n  branch dev\n  commit id: \"B\"\n  checkout main\n  cherryPick id: \"B\"",
            "gitGraph LR\n  commit",
            "gitGraph\n  commit msg:",
            "gitGraph\n  commit id: a",
            "gitGraph\n  branch x order: \"2\"",
            "gitGraph\n  branch 日本",
            "gitGraph\n  branch a;b",
            "gitGraph\n  commit\n  branch b color: red",
            "gitGraph\n  commit\n  branch b\n  commit\n  checkout main\n  merge b foo: 1",
            "gitGraph\n  commit id: a:b",
            "gitGraph RL:\n  commit",
        ] {
            let message = refusal(source);
            assert!(
                message.contains("Mermaid could not parse"),
                "{source}: {message}"
            );
        }
    }

    fn document_of(items: Vec<GitGraphItem>) -> GitGraphDocument {
        GitGraphDocument {
            items,
            ..GitGraphDocument::empty()
        }
    }

    fn commit(id: Option<&str>) -> GitGraphItem {
        GitGraphItem::Commit(GitCommit {
            id: id.map(str::to_string),
            ..GitCommit::default()
        })
    }

    fn branch(name: &str) -> GitGraphItem {
        GitGraphItem::Branch(GitBranch {
            name: name.to_string(),
            order: None,
        })
    }

    fn checkout(name: &str) -> GitGraphItem {
        GitGraphItem::Checkout(GitCheckout {
            branch: name.to_string(),
            uses_switch: false,
        })
    }

    fn merge(name: &str) -> GitGraphItem {
        GitGraphItem::Merge(GitMerge {
            branch: name.to_string(),
            id: None,
            tags: Vec::new(),
            commit_type: None,
        })
    }

    fn pick(id: &str, parent: Option<&str>) -> GitGraphItem {
        GitGraphItem::CherryPick(GitCherryPick {
            id: id.to_string(),
            parent: parent.map(str::to_string),
            tags: Vec::new(),
        })
    }

    /// Every rule the replay enforces, each checked against `merman` itself so
    /// that the form never refuses what Mermaid draws or lets through what it
    /// cannot.
    #[test]
    fn the_structure_checks_agree_with_merman() {
        let cases: Vec<(Vec<GitGraphItem>, Option<&str>)> = vec![
            (vec![checkout("dev")], Some("has not been created yet")),
            (vec![commit(None), branch("main")], Some("already exists")),
            (
                vec![commit(None), branch("b"), merge("b")],
                Some("into itself"),
            ),
            (
                vec![branch("b"), commit(None), checkout("main"), merge("b")],
                Some("has no commits yet"),
            ),
            (
                vec![commit(None), merge("b")],
                Some("has not been created yet"),
            ),
            (
                vec![commit(None), branch("b"), checkout("main"), merge("b")],
                Some("are the same commit"),
            ),
            (
                vec![
                    commit(None),
                    branch("b"),
                    checkout("main"),
                    commit(None),
                    checkout("b"),
                    merge("main"),
                ],
                Some("since it branched off"),
            ),
            (
                vec![commit(Some("a")), commit(Some("a"))],
                Some("used more than once"),
            ),
            (
                vec![commit(Some("a")), pick("x", None)],
                Some("does not come before it"),
            ),
            (
                vec![commit(Some("a")), pick("a", None)],
                Some("is already on the current branch"),
            ),
            (
                vec![
                    commit(Some("a")),
                    branch("b"),
                    commit(Some("c")),
                    checkout("main"),
                    pick("c", Some("zzz")),
                ],
                Some("is not a direct parent"),
            ),
            (
                vec![
                    commit(Some("a")),
                    branch("b"),
                    commit(Some("c")),
                    checkout("main"),
                    commit(Some("d")),
                    merge("b"),
                    branch("x"),
                    commit(None),
                    pick("M", None),
                ],
                Some("does not come before it"),
            ),
            (
                vec![
                    commit(Some("a")),
                    branch("b"),
                    commit(Some("c")),
                    checkout("main"),
                    pick("c", None),
                ],
                None,
            ),
            (
                vec![
                    commit(None),
                    branch("b"),
                    commit(None),
                    checkout("main"),
                    merge("b"),
                ],
                None,
            ),
        ];
        for (items, expected) in cases {
            let document = document_of(items);
            let error = document.structure_error();
            let source = emit_gitgraph(&document);
            match expected {
                Some(fragment) => {
                    let error = error.unwrap_or_else(|| panic!("{source} should be refused"));
                    assert!(error.contains(fragment), "{source}: {error}");
                    // Mermaid's warning about a repeated id is the one rule
                    // stricter here than in `merman`.
                    if fragment != "used more than once" {
                        assert!(
                            parse_gitgraph(&source).is_err(),
                            "merman should refuse this as well:\n{source}"
                        );
                    }
                }
                None => {
                    assert_eq!(error, None, "{source}");
                    assert_renders(&source);
                }
            }
        }
    }

    #[test]
    fn a_merge_commit_is_picked_through_one_of_its_parents() {
        let mut document = parse(
            "gitGraph\n    commit id: \"A\"\n    branch dev\n    commit id: \"B\"\n    checkout main\n    commit id: \"C\"\n    merge dev id: \"M\"\n    branch rel\n    commit\n    cherry-pick id: \"M\" parent: \"C\"",
        );
        assert_eq!(document.structure_error(), None);
        if let Some(GitGraphItem::CherryPick(pick)) = document.items.last_mut() {
            pick.parent = None;
        }
        let error = document.structure_error().unwrap_or_default();
        assert!(error.contains("specify its parent"), "{error}");
    }

    #[test]
    fn spelling_problems_are_reported_before_the_replay() {
        let mut document = parse(TEMPLATE);
        if let Some(GitGraphItem::Commit(commit)) = document.items.first_mut() {
            commit.id = Some("a\"b".to_string());
        }
        assert!(
            document
                .structure_error()
                .unwrap_or_default()
                .contains("Operation 1:")
        );

        let mut document = parse(TEMPLATE);
        if let Some(GitGraphItem::Branch(branch)) = document.items.get_mut(1) {
            branch.order = Some("x".to_string());
        }
        assert!(
            document
                .structure_error()
                .unwrap_or_default()
                .contains("whole number")
        );
    }

    #[test]
    fn renames_follow_into_the_statements_that_name_them() {
        let mut document = parse(
            "gitGraph\n    commit id: \"A\"\n    branch dev\n    commit id: \"B\"\n    checkout main\n    merge dev\n    checkout dev\n    commit\n    checkout main\n    cherry-pick id: \"B\"",
        );
        let renames = vec![("dev".to_string(), "開発".to_string())];
        if let Some(GitGraphItem::Branch(branch)) = document.items.get_mut(1) {
            branch.name = "開発".to_string();
        }
        document.retarget_branches(&renames);
        if let Some(GitGraphItem::Commit(commit)) = document.items.get_mut(2) {
            commit.id = Some("B2".to_string());
        }
        document.retarget_commits(&[("B".to_string(), "B2".to_string())]);
        let emitted = emit_gitgraph(&document);
        assert_eq!(
            emitted,
            "gitGraph\n    commit id: \"A\"\n    branch \"開発\"\n    commit id: \"B2\"\n    checkout main\n    merge \"開発\"\n    checkout \"開発\"\n    commit\n    checkout main\n    cherry-pick id: \"B2\""
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);
    }

    /// A merge commit is picked through one of its parents, and renaming that
    /// parent has to reach the `parent:` of the pick as well as a rename of the
    /// merge reaches its `id:`.
    #[test]
    fn a_renamed_parent_follows_into_the_cherry_pick() {
        let mut document = parse(
            "gitGraph\n    commit id: \"A\"\n    branch dev\n    commit id: \"B\"\n    checkout main\n    commit id: \"C\"\n    merge dev id: \"M\"\n    branch rel\n    commit\n    cherry-pick id: \"M\" parent: \"C\"",
        );
        if let Some(GitGraphItem::Commit(commit)) = document.items.get_mut(4) {
            commit.id = Some("親".to_string());
        }
        if let Some(GitGraphItem::Merge(merge)) = document.items.get_mut(5) {
            merge.id = Some("併合".to_string());
        }
        document.retarget_commits(&[
            ("C".to_string(), "親".to_string()),
            ("M".to_string(), "併合".to_string()),
        ]);
        let emitted = emit_gitgraph(&document);
        assert!(
            emitted.ends_with("cherry-pick id: \"併合\" parent: \"親\""),
            "{emitted}"
        );
        assert_eq!(document.structure_error(), None);
        assert_renders(&emitted);
    }

    #[test]
    fn a_name_with_a_space_is_written_in_quotes() {
        let mut document = parse(TEMPLATE);
        let renames = vec![("feature".to_string(), "新しい 機能".to_string())];
        if let Some(GitGraphItem::Branch(branch)) = document.items.get_mut(1) {
            branch.name = "新しい 機能".to_string();
        }
        document.retarget_branches(&renames);
        let emitted = emit_gitgraph(&document);
        assert!(
            emitted.contains("branch \"新しい 機能\"\n    checkout \"新しい 機能\""),
            "{emitted}"
        );
        assert!(emitted.ends_with("merge \"新しい 機能\""), "{emitted}");
        assert_stable(&emitted);
        assert_renders(&emitted);
        // A `:` is written in quotes as well, so that it is never read as a key.
        let renames = vec![("新しい 機能".to_string(), "fix:1".to_string())];
        if let Some(GitGraphItem::Branch(branch)) = document.items.get_mut(1) {
            branch.name = "fix:1".to_string();
        }
        document.retarget_branches(&renames);
        let emitted = emit_gitgraph(&document);
        assert!(emitted.ends_with("merge \"fix:1\""), "{emitted}");
        assert_stable(&emitted);
        assert_renders(&emitted);
    }

    #[test]
    fn a_referenced_branch_or_commit_is_not_deleted() {
        let mut document = parse(TEMPLATE);
        let before = emit_gitgraph(&document);
        let error = document.remove_item(1).err().unwrap_or_default();
        assert!(error.contains("operations 3, 6."), "{error}");
        assert_eq!(
            emit_gitgraph(&document),
            before,
            "a refused removal changes nothing"
        );

        let error = document
            .change_kind(1, GitGraphItemKind::Commit)
            .err()
            .unwrap_or_default();
        assert!(error.contains("is referenced by"), "{error}");
        assert_eq!(emit_gitgraph(&document), before);

        let mut document =
            parse("gitGraph\n    commit\n    branch dev\n    commit\n    checkout main");
        let error = document.remove_item(1).err().unwrap_or_default();
        assert!(error.contains("operations right after"), "{error}");

        let mut document = parse(
            "gitGraph\n    commit id: \"A\"\n    branch dev\n    commit id: \"B\"\n    checkout main\n    cherry-pick id: \"B\"",
        );
        let error = document.remove_item(2).err().unwrap_or_default();
        assert!(error.contains("cherry-pick"), "{error}");
        assert!(document.change_kind(2, GitGraphItemKind::Merge).is_ok());
        assert_eq!(
            document.items.get(2).and_then(GitGraphItem::commit_id),
            Some("B")
        );

        // Once the references are gone, the branch can go too.
        let mut document = parse(TEMPLATE);
        for index in [5, 4, 3, 2] {
            document
                .remove_item(index)
                .unwrap_or_else(|error| panic!("{index}: {error}"));
        }
        document
            .remove_item(1)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            emit_gitgraph(&document),
            "gitGraph\n    commit id: \"初期化\""
        );
    }

    #[test]
    fn new_statements_draw_straight_away() {
        let mut document = parse(TEMPLATE);
        for kind in [
            GitGraphItemKind::Branch,
            GitGraphItemKind::Commit,
            GitGraphItemKind::Checkout,
            GitGraphItemKind::Commit,
            GitGraphItemKind::Merge,
        ] {
            let item = document.new_item(kind, document.items.len());
            document.items.push(item);
            assert_eq!(
                document.structure_error(),
                None,
                "{}",
                emit_gitgraph(&document)
            );
        }
        let emitted = emit_gitgraph(&document);
        assert!(
            emitted.ends_with(
                "branch feature-2\n    commit\n    checkout main\n    commit\n    merge feature-2"
            ),
            "{emitted}"
        );
        assert_renders(&emitted);
    }

    #[test]
    fn a_blank_graph_round_trips() {
        let document = GitGraphDocument::empty();
        assert_eq!(emit_gitgraph(&document), "gitGraph");
        assert_eq!(parse("gitGraph"), document);
        assert!(is_gitgraph_source("%% x\ngitGraph:\n commit"));
        assert!(is_gitgraph_source("gitGraph LR:"));
        assert!(!is_gitgraph_source("gitGraphs\n commit"));
    }
    /// Every normalization `emit_gitgraph` makes must draw exactly what the
    /// source drew. Mermaid numbers the SVG it makes, and a seed fixes the ids
    /// it makes up for commits without one, so the drawings compare as text.
    #[test]
    fn a_normalized_spelling_draws_the_same_diagram() {
        const SEEDED: &str = "%%{init: {'gitGraph': {'seed': 7}}}%%\n";
        fn without_svg_numbers(svg: String) -> String {
            let mut out = String::new();
            let mut rest = svg.as_str();
            while let Some(position) = rest.find("merman-") {
                let (before, after) = rest.split_at(position + "merman-".len());
                out.push_str(before);
                out.push('N');
                rest = after.trim_start_matches(|character: char| character.is_ascii_digit());
            }
            out.push_str(rest);
            out
        }
        let drawing = |source: &str| {
            render_to_svg(&format!("{SEEDED}{source}"), &MermaidTheme::default())
                .map(without_svg_numbers)
                .unwrap_or_else(|error| panic!("source should render: {error}\n{source}"))
        };
        assert_eq!(
            drawing("gitGraph\n  commit\n  commit"),
            drawing("gitGraph\n  commit\n  commit"),
            "the comparison itself must be deterministic"
        );

        for source in [
            "gitGraph\n  commit \"m\"\n  commit",
            "gitGraph\n  commit id: \"\"\n  commit",
            "gitGraph\n  commit\n  branch \"x\" order: 2\n  commit\n  checkout \"main\"\n  commit\n  merge \"x\" id: \"\"",
            "gitGraph\n  commit id: \"A\"\n  branch dev\n  commit id: \"B\"\n  checkout main\n  commit\n  cherry-pick id: \"B\" parent: \"\"",
            "gitGraph\n  commit id: \"A\"\n  branch dev\n  commit id: \"B\"\n  checkout main\n  commit id: \"C\"\n  cherry-pick id: \"B\" parent: \"A\"",
            "gitGraph\n  commit id:\"甲\"\n  branch \"開発\u{3000}ブランチ\"\n  commit id:\"乙 丙\"\n  checkout main\n  merge \"開発\u{3000}ブランチ\" tag: \"タグ\"",
        ] {
            let emitted = emit_gitgraph(&parse(source));
            assert_ne!(emitted, source, "this case should exercise a normalization");
            assert_eq!(
                drawing(source),
                drawing(&emitted),
                "{source}\n->\n{emitted}"
            );
        }
    }

    /// `merman` 0.7 read a directive that is never closed as a comment line
    /// and drew the statements after it; 0.8 cuts from it to the end of the
    /// text, as Mermaid does, so the diagram does not open.
    #[test]
    fn an_unclosed_directive_swallows_the_statements_after_it() {
        let source =
            "gitGraph\n    commit\n    %%{init: {'theme': 'dark'\n    branch dev\n    commit";
        let error = refusal(source);
        assert!(
            error.contains("There is a `%%{` that is never closed"),
            "{error}"
        );

        let closed = "gitGraph\n    commit\n    %%{init: {'theme':\n    'dark'}}%%\n    commit";
        let document = parse(closed);
        assert_eq!(emit_gitgraph(&document), closed);
        assert_eq!(document.items.len(), 3, "{:?}", document.items);
    }

    /// The replay in `structure_error` has to say yes exactly when `merman`
    /// does, for scripts nobody wrote by hand. Two commits sharing an id are
    /// left out: that is the one rule stricter here than in Mermaid.
    #[test]
    fn the_structure_checks_agree_with_merman_on_generated_scripts() {
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % bound as u64) as usize
        };
        let branches = ["main", "a", "b", "日本"];
        let ids = ["x", "y", "z", "甲"];
        let mut accepted = 0;
        for _ in 0..3000 {
            let length = 1 + next(9);
            let mut items = Vec::new();
            for _ in 0..length {
                let item = match next(6) {
                    0 | 1 => commit((next(2) == 1).then(|| ids[next(4)])),
                    2 => branch(branches[next(4)]),
                    3 => checkout(branches[next(4)]),
                    4 => {
                        let mut item = merge(branches[next(4)]);
                        if let GitGraphItem::Merge(merge) = &mut item
                            && next(3) == 0
                        {
                            merge.id = Some(ids[next(4)].to_string());
                        }
                        item
                    }
                    _ => pick(ids[next(4)], (next(2) == 1).then(|| ids[next(4)])),
                };
                items.push(item);
            }
            let document = document_of(items);
            let ours = document.structure_error();
            if ours
                .as_deref()
                .is_some_and(|error| error.contains("used more than once"))
            {
                continue;
            }
            let source = emit_gitgraph(&document);
            match (ours, parse_gitgraph(&source)) {
                (None, Ok(_)) => accepted += 1,
                (Some(_), Err(_)) => {}
                (Some(error), Ok(_)) => panic!("refused what merman draws: {error}\n{source}"),
                (None, Err(error)) => panic!("let through what merman refuses: {error}\n{source}"),
            }
        }
        assert!(
            accepted > 100,
            "too few drawable scripts to mean much: {accepted}"
        );
    }

    #[test]
    fn statements_merman_reads_around_a_directive_are_refused() {
        crate::mermaid_model::preprocessing_cases::assert_directive_rules(
            "gitGraph",
            "commit",
            "commit %%{init: {}}%% id: \"x\"",
        );
    }

    /// An id and a tag that are fine on their own, but written on one line
    /// let `merman`'s rewrite of `="…"` inside what looks like a tag close the
    /// id at the tag's opening quote.
    #[test]
    fn fields_whose_quotes_merman_would_rewrite_cannot_be_written() {
        let mut document = parse("gitGraph\n    commit");
        let GitGraphItem::Commit(commit) = &mut document.items[0] else {
            panic!("expected a commit: {:?}", document.items);
        };
        commit.id = Some("a<b=".to_string());
        commit.tags = vec!["c>".to_string()];
        assert!(is_valid_gitgraph_text("a<b="));
        assert!(is_valid_gitgraph_text("c>"));
        let whole = crate::mermaid_model::MermaidDocument::GitGraph(document.clone());
        assert!(whole.structure_error().is_some(), "{}", whole.emit());
        let drawn = crate::render_to_svg(&whole.emit(), &crate::MermaidTheme::default())
            .expect("merman draws the rewritten line");
        assert!(
            !drawn.contains("a&lt;b="),
            "the reason for the refusal no longer holds:
{}",
            whole.emit()
        );

        let GitGraphItem::Commit(commit) = &mut document.items[0] else {
            panic!("expected a commit");
        };
        commit.id = Some("a<b".to_string());
        let whole = crate::mermaid_model::MermaidDocument::GitGraph(document);
        assert_eq!(whole.structure_error(), None);
    }
}
