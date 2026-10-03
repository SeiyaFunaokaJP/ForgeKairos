# Git

[日本語](ja/git.md)

ForgeKairos adds a commit graph with history-editing operations, a box for building merge
request branches, an operation guide, and support for old versions of git. For Zed's Git
panel, see [Zed's Git documentation](https://zed.dev/docs/git).

## The Git menu

The menu bar has a **Git** menu: Open Git Graph, Create Branch…, Switch Branch…, Amend Last
Commit, Fixup into an Earlier Commit…, Create Merge Request, Compare Two Branches…, Fetch,
Pull, Push, Force Push with Lease…, Stash… and Git Guide.

## Commit graph

The graph shows ref chips for HEAD, local and remote branches and tags. You can show all
branches, the current branch or a chosen one, pick a tag from a searchable list, and select
several commits with Shift or Ctrl. The details of a commit show the latest tag before it.

Right-click a commit to:

- create a branch from it (and switch to it, or not)
- check it out (detached HEAD)
- tag it
- cherry-pick it into the current branch, or revert it
- reword its message
- reset to it: soft, mixed or hard (hard asks first)
- drop it

Operations that do not apply are shown greyed out with the reason.

### Rewriting history

- **Amend**: the Git panel's commit area has an "Amend" checkbox; it warns
  when that commit was already pushed.
- **Fixup**: puts the current changes (staged only, or all) into an earlier commit. It is
  refused before it starts when it cannot work: during a rebase or merge, with conflicts,
  on a detached HEAD, for a commit outside the current branch, or with a merge in the range.
  The success message offers **Undo…**.
- **Squash, reorder, drop**: a dialog lists the commits with keep, squash, drop and reword,
  and rows can be dragged to reorder.
- **Force push**: after a rewrite, force push is offered only "with lease", and only when
  the commit was already pushed. Force pushing always asks for confirmation.

Before any rewrite, ForgeKairos refuses to start if an ignored local file (such as `.env`)
would be overwritten. A rewrite that stops on a conflict shows a banner with continue, skip
and abort.

### Drag and drop

- Drag files in the Git panel onto the staged or unstaged section to stage or unstage them.
- Drag files onto a commit in the graph to fix them up into that commit.
- Drag a commit onto another commit to reorder.
- Drag a commit onto the HEAD chip to cherry-pick it.
- Drag the HEAD chip onto a commit to reset to it (soft, mixed or hard).

## Merge request box

**Create Merge Request** opens a box beside the graph. Choose your development branch and the
base, drag commits into the box, write the message and the branch name (default
`mr/<branch>`), and create the branch. Your current branch stays checked out.

After creating it, ForgeKairos compares the result and classifies each file, for example as
matching, changed by an excluded commit, or possibly missing (with the commits involved). It
warns when the merge request branch is not based on the latest base. Commits not yet placed
are marked in the graph, and you can step through them.

## Comparing two refs

**Compare Two Branches…** opens a tab with the changed files and read-only diffs between two
refs.

## Search

The graph's search accepts `author:`, `tag:`, `branch:`, `path:`, `file:`, `since:`,
`after:`, `until:`, `before:`, `change:`, `merge:` and `sha:`, combined with AND.

## Help in the UI

- **?** (`git: show guide`) in the graph toolbar, the Git panel header and the merge request
  box opens the Git Guide: amend, fixup, reword, squash, cherry-pick, merge requests,
  branches, undoing, reset modes, comparing, tags and search.
- A line under the button row explains the button under the mouse. Menus show a short
  explanation beside each item; disabled items give the reason; destructive items carry a
  caution.

## More operations

The Git panel and the command palette add: initialize a repository, add and remove remotes,
merge a branch, make the local branch match the remote (asks first), create a patch from
the working tree, the staged changes or the last commit, and apply a patch.

## Many refs

Staging and committing stay responsive with tens of thousands of tags or branches. When there
are 1,000 or more loose refs, a banner offers to pack them.

## Old git

Git integration also works with old git (for example on older Linux servers). Features that
need a newer git than the one installed are refused with a message that names the required
version. In projects you have not trusted, with git older than 2.12, operations that run
hooks and push, pull and fetch are refused. When `git status` fails, the Git panel shows the
error in a banner.
