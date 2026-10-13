//! ForgeKairos (ADR-F93): the git panel's control for which git account the active repository
//! commits and pushes as.

use std::sync::Arc;

use gpui::{Anchor, AnyElement, App, Entity, SharedString, prelude::*};
use project::{
    Fs, Project,
    git_accounts::{
        GitAccountChoice, RemoteGitAccountStatus, account_choice_for_key,
        set_project_account_for_key,
    },
    git_store::Repository,
    project_settings::ProjectSettings,
};
use settings::Settings as _;
use ui::{
    ButtonLike, ContextMenu, ContextMenuEntry, IconPosition, PopoverMenu, Tooltip, prelude::*,
};

/// The settings window's path to the accounts page.
const GIT_ACCOUNTS_SETTINGS_PATH: &str = "git.accounts";

pub(crate) fn render_git_account_picker(
    repository: &Entity<Repository>,
    project: &Entity<Project>,
    fs: Arc<dyn Fs>,
    cx: &App,
) -> AnyElement {
    let Some(key) = project
        .read(cx)
        .git_store()
        .read(cx)
        .git_account_key(repository.read(cx))
    else {
        return disabled_picker(i18n::t!(
            "Git accounts don't apply to a project shared by a collaborator. It commits and pushes as git is configured on its host."
        ));
    };

    let settings = &ProjectSettings::get_global(cx).git;
    let choice = account_choice_for_key(settings, &key);
    let accounts = settings.accounts.clone();
    // ADR-F93: on a remote host, what its server made of the pick.
    let (not_applied_reason, commits_refused) =
        match repository.read(cx).remote_git_account_status() {
            Some(RemoteGitAccountStatus::NotApplied {
                reason,
                commits_refused,
            }) => (Some(reason.clone()), *commits_refused),
            _ => (None, false),
        };
    let ssh_key_unused_reason = repository.read(cx).ssh_key_unused_reason().cloned();
    if let (Some(reason), GitAccountChoice::GitConfig, false) =
        (&not_applied_reason, &choice, commits_refused)
    {
        return disabled_picker(i18n::t!(
            "This remote host's server does not support git accounts ({reason}); update it to pick one. The project commits and pushes as git is configured on the host.",
            reason = reason
        ));
    }

    let (label, color, tooltip): (SharedString, Color, SharedString) = match &choice {
        GitAccountChoice::GitConfig | GitAccountChoice::Unavailable { .. } if commits_refused => (
            SharedString::default(),
            Color::Warning,
            i18n::t!(
                "The remote host may still commit as the git account picked before, as it did not take the change ({reason}), so commits are refused there. Pick an account again, or reconnect to the remote server.",
                reason = not_applied_reason.as_deref().unwrap_or_default()
            )
            .into(),
        ),
        GitAccountChoice::GitConfig => (
            SharedString::default(),
            Color::Muted,
            i18n::t!("Commits and pushes as git is configured. Click to pick a git account.")
                .into(),
        ),
        GitAccountChoice::Account(account) if not_applied_reason.is_some() => (
            account.id.clone().into(),
            Color::Warning,
            i18n::t!(
                "The git account \"{id}\" is not in effect on the remote host ({reason}), so commits are refused there. Update the remote server, or pick Git's Own Configuration.",
                id = account.id,
                reason = not_applied_reason.as_deref().unwrap_or_default()
            )
            .into(),
        ),
        GitAccountChoice::Account(account) => match &ssh_key_unused_reason {
            Some(reason) => (
                account.id.clone().into(),
                Color::Warning,
                i18n::t!(
                    "Commits as {name} <{email}> (git account \"{id}\"). The account's SSH key is not used: {reason}",
                    name = account.name,
                    email = account.email,
                    id = account.id,
                    reason = reason
                )
                .into(),
            ),
            None => (
                account.id.clone().into(),
                Color::Default,
                i18n::t!(
                    "Commits as {name} <{email}> (git account \"{id}\")",
                    name = account.name,
                    email = account.email,
                    id = account.id
                )
                .into(),
            ),
        },
        GitAccountChoice::Unavailable { id, reason } => (
            id.clone().into(),
            Color::Warning,
            i18n::t!(
                "The git account \"{id}\" picked for this repository cannot be used ({reason}), so it commits and pushes as git is configured.",
                id = id,
                reason = reason
            )
            .into(),
        ),
    };

    let trigger = ButtonLike::new("git-account-picker").child(
        h_flex()
            .gap_1()
            .child(
                Icon::new(IconName::UserRoundPen)
                    .size(IconSize::Small)
                    .color(color),
            )
            .when(!label.is_empty(), |this| {
                this.child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .color(color)
                        .truncate(),
                )
            }),
    );

    PopoverMenu::new("git-account-picker-menu")
        .trigger_with_tooltip(trigger, Tooltip::text(tooltip))
        .anchor(Anchor::BottomRight)
        .menu(move |window, cx| {
            let key = key.clone();
            let fs = fs.clone();
            let accounts = accounts.clone();
            let choice = choice.clone();
            Some(ContextMenu::build(window, cx, move |menu, _, _| {
                let picked_id = match &choice {
                    GitAccountChoice::GitConfig => None,
                    GitAccountChoice::Account(account) => Some(account.id.clone()),
                    GitAccountChoice::Unavailable { id, .. } => Some(id.clone()),
                };
                let mut menu = menu.header(i18n::t!("Commit and Push As")).item(
                    ContextMenuEntry::new(i18n::t!("Git's Own Configuration"))
                        .toggleable(IconPosition::Start, picked_id.is_none())
                        .handler({
                            let key = key.clone();
                            let fs = fs.clone();
                            move |_, cx| {
                                set_project_account_for_key(key.clone(), None, fs.clone(), cx)
                            }
                        }),
                );
                for account in &accounts {
                    let id = account.id.clone();
                    let label = format!("{} — {} <{}>", account.id, account.name, account.email);
                    menu = menu.item(
                        ContextMenuEntry::new(label)
                            .toggleable(IconPosition::Start, picked_id.as_ref() == Some(&id))
                            .handler({
                                let key = key.clone();
                                let fs = fs.clone();
                                move |_, cx| {
                                    set_project_account_for_key(
                                        key.clone(),
                                        Some(id.clone()),
                                        fs.clone(),
                                        cx,
                                    )
                                }
                            }),
                    );
                }
                menu.separator().item(
                    ContextMenuEntry::new(i18n::t!("Manage Git Accounts…"))
                        .icon(IconName::Settings)
                        .handler(|window, cx| {
                            window.dispatch_action(
                                Box::new(zed_actions::OpenSettingsAt {
                                    path: GIT_ACCOUNTS_SETTINGS_PATH.to_string(),
                                    target: None,
                                }),
                                cx,
                            )
                        }),
                )
            }))
        })
        .into_any_element()
}

fn disabled_picker(tooltip: impl Into<SharedString>) -> AnyElement {
    IconButton::new("git-account-picker", IconName::UserRoundPen)
        .icon_size(IconSize::Small)
        .icon_color(Color::Disabled)
        .disabled(true)
        .tooltip(Tooltip::text(tooltip.into()))
        .into_any_element()
}
