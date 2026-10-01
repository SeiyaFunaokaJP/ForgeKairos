//! Tells the user that clangd went over `clangd.memory_limit_mb`, and offers
//! the one change that saves the most: light mode. Nothing is shown while
//! clangd stays under its limit.

use gpui::{App, AppContext as _, EntityId};
use project::{
    lsp_store::clangd_memory::{ClangdMemoryEvent, ClangdMemoryEvents},
    project_settings::ProjectSettings,
};
use settings::{ClangdIndexMode, Settings as _, SettingsStore};
use ui::prelude::*;
use workspace::{
    Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

struct ClangdMemoryNotification;

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, cx| {
        let lsp_store = workspace.project().read(cx).lsp_store().entity_id();
        let events = ClangdMemoryEvents::global(cx);
        cx.subscribe(
            &events,
            move |workspace, _, event: &ClangdMemoryEvent, cx| {
                show_notification(workspace, lsp_store, event, cx);
            },
        )
        .detach();
    })
    .detach();
}

/// Bytes as the language server menu shows them: "812.0 MB", "1.2 GB".
pub(crate) fn format_memory(bytes: u64) -> String {
    const MEGABYTE: f64 = 1024.0 * 1024.0;
    const GIGABYTE: f64 = 1024.0 * MEGABYTE;
    let bytes = bytes as f64;
    if bytes >= GIGABYTE {
        format!("{:.1} GB", bytes / GIGABYTE)
    } else {
        format!("{:.1} MB", bytes / MEGABYTE)
    }
}

fn show_notification(
    workspace: &mut Workspace,
    workspace_lsp_store: EntityId,
    event: &ClangdMemoryEvent,
    cx: &mut Context<Workspace>,
) {
    let ClangdMemoryEvent::LimitExceeded {
        lsp_store,
        resident_bytes,
        limit_bytes,
        restarted,
        ..
    } = event;
    if *lsp_store != workspace_lsp_store {
        return;
    }
    let used = format_memory(*resident_bytes);
    let limit = format_memory(*limit_bytes);
    let mut message = if *restarted {
        i18n::t!("clangd was using {used} of memory, over its limit of {limit}, and was restarted.")
    } else {
        i18n::t!(
            "clangd is using {used} of memory, over its limit of {limit}. It was restarted less than 10 minutes ago, so it is left running."
        )
    };
    // Light mode in the user's settings does nothing for a project whose own
    // settings choose full mode, so the button would do nothing either.
    let project_chooses_full = project_chooses_full_index(workspace, cx);
    let offer_light_mode = !project_chooses_full
        && ProjectSettings::get_global(cx).clangd.index_mode != ClangdIndexMode::Light;
    if offer_light_mode {
        message.push(' ');
        message.push_str(&i18n::t!(
            "Light mode saves the most memory by not indexing the whole project; references and renames then only cover the open files."
        ));
    }
    if project_chooses_full {
        message.push(' ');
        message.push_str(&i18n::t!(
            "This project's settings choose clangd's full mode, which your own settings do not change. To save memory with light mode, change `clangd.index_mode` in the project's settings."
        ));
    }
    let fs = workspace.app_state().fs.clone();
    workspace.show_notification(
        NotificationId::unique::<ClangdMemoryNotification>(),
        cx,
        move |cx| {
            cx.new(|cx| {
                let notification = MessageNotification::new(message, cx);
                if project_chooses_full {
                    notification
                        .primary_message(i18n::t!("Open Project Settings"))
                        .primary_on_click(|window, cx| {
                            window.dispatch_action(Box::new(zed_actions::OpenProjectSettings), cx);
                        })
                } else if offer_light_mode {
                    let fs = fs.clone();
                    notification
                        .primary_message(i18n::t!("Switch to Light Mode"))
                        .primary_on_click(move |_, cx| {
                            // The change of settings restarts clangd, here or
                            // on the remote host the settings are sent to.
                            settings::update_settings_file(fs.clone(), cx, |settings, _| {
                                settings.project.clangd.get_or_insert_default().index_mode =
                                    Some(ClangdIndexMode::Light);
                            });
                        })
                } else {
                    notification
                }
            })
        },
    );
}

/// Whether a `.zed/settings.json` of the project sets `clangd.index_mode` to
/// full.
fn project_chooses_full_index(workspace: &Workspace, cx: &App) -> bool {
    let settings = cx.global::<SettingsStore>();
    workspace.project().read(cx).worktrees(cx).any(|worktree| {
        settings
            .local_settings(worktree.read(cx).id())
            .any(|(_, content)| {
                content.clangd.as_ref().and_then(|clangd| clangd.index_mode)
                    == Some(ClangdIndexMode::Full)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_is_shown_in_megabytes_below_a_gigabyte() {
        assert_eq!(format_memory(812 * 1024 * 1024), "812.0 MB");
        assert_eq!(format_memory(1024 * 1024 * 1024 - 1), "1024.0 MB");
        assert_eq!(format_memory(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(format_memory(0), "0.0 MB");
        assert_eq!(format_memory(1288 * 1024 * 1024), "1.3 GB");
    }
}
