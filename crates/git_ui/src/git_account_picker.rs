//! ForgeKairos (ADR-F93): the git panel's control for which git account the active repository
//! commits and pushes as.

use std::path::Path;
use std::sync::Arc;

use gpui::{Anchor, AnyElement, App, Entity, SharedString, prelude::*};
use project::{
    Fs, Project,
    git_accounts::{GitAccountChoice, account_choice, set_project_account},
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
    let applies = project.read(cx).git_store().read(cx).applies_git_accounts();
    if !applies {
        return IconButton::new("git-account-picker", IconName::UserRoundPen)
            .icon_size(IconSize::Small)
            .icon_color(Color::Disabled)
            .disabled(true)
            .tooltip(Tooltip::text(i18n::t!(
                "Git accounts apply to local projects only. This project commits and pushes as git is configured on its host."
            )))
            .into_any_element();
    }

    let work_directory: Arc<Path> = repository.read(cx).work_directory_abs_path.clone();
    let settings = &ProjectSettings::get_global(cx).git;
    let choice = account_choice(settings, &work_directory);
    let accounts = settings.accounts.clone();

    let (label, color, tooltip): (SharedString, Color, SharedString) = match &choice {
        GitAccountChoice::GitConfig => (
            SharedString::default(),
            Color::Muted,
            i18n::t!("Commits and pushes as git is configured. Click to pick a git account.")
                .into(),
        ),
        GitAccountChoice::Account(account) => (
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
            let work_directory = work_directory.clone();
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
                            let work_directory = work_directory.clone();
                            let fs = fs.clone();
                            move |_, cx| set_project_account(&work_directory, None, fs.clone(), cx)
                        }),
                );
                for account in &accounts {
                    let id = account.id.clone();
                    let label = format!("{} — {} <{}>", account.id, account.name, account.email);
                    menu = menu.item(
                        ContextMenuEntry::new(label)
                            .toggleable(IconPosition::Start, picked_id.as_ref() == Some(&id))
                            .handler({
                                let work_directory = work_directory.clone();
                                let fs = fs.clone();
                                move |_, cx| {
                                    set_project_account(
                                        &work_directory,
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
