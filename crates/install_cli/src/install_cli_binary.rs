use super::register_zed_scheme;
use anyhow::Result;
use gpui::{AppContext as _, AsyncApp, Context, PromptLevel, Window, actions};
use release_channel::ReleaseChannel;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use util::ResultExt;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{DetachAndPromptErr, NotificationId};
use workspace::{Toast, Workspace};

actions!(
    cli,
    [
        /// Installs the Zed CLI tool to the system PATH.
        InstallCliBinary,
    ]
);

const CANT_INSTALL_DOCS_URL: &str = "https://zed.dev/docs/macos#cant-install-cli";

/// Attempts to install the CLI symlink. Returns the installed path on success,
/// or `None` if the user dismissed the macOS administrator authentication
/// prompt. Returns an error if the install could not be completed, most
/// commonly because the user is not an admin.
async fn install_script(cx: &AsyncApp) -> Result<Option<PathBuf>> {
    let cli_path = cx.update(|cx| cx.path_for_auxiliary_executable("cli"))?;
    // zed-jp-enhance: named after this app's launcher so that an official
    // Zed's `zed` link is not replaced.
    let link_path = Path::new(branding::concat_const!(
        "/usr/local/bin/",
        branding::CURRENT.cli_name
    ));
    let bin_dir_path = link_path.parent().unwrap();

    // Don't re-create symlink if it points to the same CLI binary.
    if smol::fs::read_link(link_path).await.ok().as_ref() == Some(&cli_path) {
        return Ok(Some(link_path.into()));
    }

    // If the symlink is not there or is outdated, first try replacing it
    // without escalating.
    smol::fs::remove_file(link_path).await.log_err();
    if smol::fs::unix::symlink(&cli_path, link_path)
        .await
        .log_err()
        .is_some()
    {
        return Ok(Some(link_path.into()));
    }

    // The symlink could not be created without escalating, so use osascript
    // with admin privileges to create it.
    let output = smol::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \" \
                    mkdir -p \'{}\' && \
                    ln -sf \'{}\' \'{}\' \
                \" with administrator privileges",
                bin_dir_path.to_string_lossy(),
                cli_path.to_string_lossy(),
                link_path.to_string_lossy(),
            ),
        ])
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(link_path.into()));
    }

    // osascript reports "User canceled." (error -128) when the administrator
    // prompt is dismissed. Treat that as a cancellation rather than a failure
    // so we don't show an error the user already chose to avoid.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("User canceled") || stderr.contains("-128") {
        return Ok(None);
    }

    // The privileged write failed, most commonly because the user is not an
    // admin.
    anyhow::bail!("error running osascript: {}", stderr.trim());
}

pub fn install_cli_binary(window: &mut Window, cx: &mut Context<Workspace>) {
    let linux_prompt_detail = i18n::t!(
        "If you installed Zed from our official release add ~/.local/bin to your PATH.\n\nIf you installed Zed from a different source like your package manager, then you may need to create an alias/symlink to `{cli_name}` manually.",
        cli_name = branding::CURRENT.cli_name
    );

    cx.spawn_in(window, async move |workspace, cx| {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            let prompt = cx.prompt(
                PromptLevel::Warning,
                i18n::t!("CLI should already be installed"),
                Some(linux_prompt_detail.as_str()),
                &["OK"],
            );
            cx.background_spawn(prompt).detach();
            return Ok(());
        }
        let path = match install_script(cx.deref()).await {
            Ok(Some(path)) => path,
            // The user dismissed the administrator prompt; nothing to do.
            Ok(None) => return Ok(()),
            Err(error) => {
                log::error!("failed to install zed CLI: {error:#}");
                workspace.update(cx, |workspace, cx| {
                    struct CliInstallFailed;

                    workspace.show_notification(
                        NotificationId::unique::<CliInstallFailed>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    i18n::t!(
                                        "You can add `{cli_name}` to your PATH manually.",
                                        cli_name = branding::CURRENT.cli_name
                                    ),
                                    cx,
                                )
                                .with_title(i18n::t!("Couldn't install the Zed CLI"))
                                .more_info_message(i18n::t!("Show me how"))
                                .more_info_url(CANT_INSTALL_DOCS_URL)
                            })
                        },
                    );
                })?;
                return Ok(());
            }
        };

        workspace.update_in(cx, |workspace, _, cx| {
            struct InstalledZedCli;

            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<InstalledZedCli>(),
                    i18n::t!(
                        "Installed `{cli_name}` to {path}. You can launch {app} from your terminal.",
                        path = path.to_string_lossy(),
                        app = ReleaseChannel::global(cx).display_name(),
                        cli_name = branding::CURRENT.cli_name
                    ),
                ),
                cx,
            )
        })?;
        register_zed_scheme(cx).await.log_err();
        Ok(())
    })
    .detach_and_prompt_err(
        i18n::t!("Cannot install the Zed CLI"),
        window,
        cx,
        |_, _, _| None,
    );
}
