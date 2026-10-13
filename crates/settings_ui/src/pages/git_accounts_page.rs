//! ForgeKairos (ADR-F93): the settings window's page for git accounts. The accounts are written to
//! `git.accounts` in the user settings; a token goes to the operating system's credential store
//! and is never shown again once saved.

use editor::{Editor, EditorEvent};
use gpui::{Entity, ReadGlobal as _, ScrollHandle, Subscription, Task, prelude::*};
use project::git_accounts::{
    GitHttpsLogin, https_login_for_account, login_is_used, remove_account, remove_token,
    save_token, token_is_stored, upsert_account,
};
use settings::{GitAccountContent, SettingsStore, update_settings_file};
use ui::{Divider, Tooltip, prelude::*};
use util::ResultExt as _;

use super::mcp_servers_page::input_box;
use crate::SettingsWindow;

pub(crate) struct GitAccountForm {
    original_id: Option<String>,
    id: Entity<Editor>,
    name: Entity<Editor>,
    email: Entity<Editor>,
    host: Entity<Editor>,
    login: Entity<Editor>,
    token: Entity<Editor>,
    signing_key: Entity<Editor>,
    ssh_key: Entity<Editor>,
    /// The host and login the form's fields name now, which `token_stored` is about.
    token_login: Option<GitHttpsLogin>,
    /// Whether the credential store has a token for `token_login`; `None` until that has been
    /// read.
    token_stored: Option<bool>,
    error: Option<SharedString>,
    saving: bool,
    _task: Option<Task<()>>,
    _token_status_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

/// The accounts as the user settings file has them, so that editing one does not copy defaults
/// or other layers into it.
fn user_accounts(cx: &App) -> Vec<GitAccountContent> {
    SettingsStore::global(cx)
        .raw_user_settings()
        .and_then(|settings| settings.content.git.as_ref())
        .and_then(|git| git.accounts.clone())
        .unwrap_or_default()
}

pub(crate) fn render_git_accounts_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let content = match settings_window.git_account_form.as_ref() {
        Some(form) => render_form(form, window, cx).into_any_element(),
        None => render_account_list(cx).into_any_element(),
    };
    v_flex()
        .id("git-accounts-page")
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            v_flex()
                .gap_1()
                .child(Label::new(i18n::t!("Git Accounts")))
                .child(
                    Label::new(i18n::t!(
                        "Accounts a repository can commit and push as. Pick one for each repository with the account button next to Commit in the Git panel; a repository without one follows git's own configuration. Tokens are kept in {store}, never in settings.json.",
                        store = credential_store_name()
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .child(content)
        .into_any_element()
}

fn credential_store_name() -> &'static str {
    if cfg!(target_os = "windows") {
        i18n::t!("Windows Credential Manager")
    } else if cfg!(target_os = "macos") {
        i18n::t!("macOS Keychain")
    } else {
        i18n::t!("the system keyring (Secret Service)")
    }
}

fn render_account_list(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    let accounts = user_accounts(cx);
    v_flex()
        .gap_2()
        .when(accounts.is_empty(), |this| {
            this.child(
                Label::new(i18n::t!("No git accounts yet."))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
        .children(accounts.into_iter().enumerate().map(|(index, account)| {
            let sign_in = match (&account.host, &account.login) {
                (Some(host), Some(login)) if !host.trim().is_empty() => {
                    i18n::t!("Signs in to {host} as {login}", host = host, login = login)
                }
                _ => i18n::t!("Signs in as git is configured").to_string(),
            };
            let edit_account = account.clone();
            let remove_id = account.id.clone();
            v_flex()
                .gap_2()
                .when(index > 0, |this| this.child(Divider::horizontal()))
                .child(
                    h_flex()
                        .justify_between()
                        .gap_2()
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(Label::new(account.id.clone()))
                                .child(
                                    Label::new(format!("{} <{}>", account.name, account.email))
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                )
                                .child(
                                    Label::new(sign_in)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_1()
                                .child(
                                    Button::new(("git-account-edit", index), i18n::t!("Edit"))
                                        .label_size(LabelSize::Small)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            open_form(this, Some(edit_account.clone()), window, cx);
                                        })),
                                )
                                .child(
                                    Button::new(("git-account-remove", index), i18n::t!("Remove"))
                                        .label_size(LabelSize::Small)
                                        .tooltip(Tooltip::text(i18n::t!(
                                            "Remove the account, its token if no other account uses it, and every repository's pick of it"
                                        )))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            remove_git_account(&remove_id, cx);
                                        })),
                                ),
                        ),
                )
        }))
        .child(
            h_flex().pt_2().child(
                Button::new("git-account-add", i18n::t!("Add Account"))
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .start_icon(
                        Icon::new(IconName::Plus)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .on_click(cx.listener(|this, _, window, cx| {
                        open_form(this, None, window, cx);
                    })),
            ),
        )
}

fn new_input(
    placeholder: &str,
    initial: Option<&str>,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    let placeholder = placeholder.to_string();
    let initial = initial.map(str::to_string);
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder.as_str(), window, cx);
        editor.set_masked(masked, cx);
        if let Some(text) = initial {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

fn open_form(
    settings_window: &mut SettingsWindow,
    existing: Option<GitAccountContent>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let host = new_input(
        "github.com",
        existing
            .as_ref()
            .and_then(|account| non_empty(account.host.as_deref())),
        false,
        window,
        cx,
    );
    let login = new_input(
        "ada-work",
        existing
            .as_ref()
            .and_then(|account| non_empty(account.login.as_deref())),
        false,
        window,
        cx,
    );
    let subscriptions = [&host, &login]
        .into_iter()
        .map(|editor| {
            cx.subscribe(editor, |this, _, event: &EditorEvent, cx| {
                if matches!(event, EditorEvent::BufferEdited) {
                    refresh_token_status(this, cx);
                }
            })
        })
        .collect();
    let form = GitAccountForm {
        original_id: existing.as_ref().map(|account| account.id.clone()),
        id: new_input(
            "work",
            existing.as_ref().map(|account| account.id.as_str()),
            false,
            window,
            cx,
        ),
        name: new_input(
            "Ada Lovelace",
            existing.as_ref().map(|account| account.name.as_str()),
            false,
            window,
            cx,
        ),
        email: new_input(
            "ada@example.com",
            existing.as_ref().map(|account| account.email.as_str()),
            false,
            window,
            cx,
        ),
        host,
        login,
        token: new_input("", None, true, window, cx),
        signing_key: new_input(
            "",
            existing
                .as_ref()
                .and_then(|account| non_empty(account.signing_key.as_deref())),
            false,
            window,
            cx,
        ),
        ssh_key: new_input(
            if cfg!(target_os = "windows") {
                r"~\.ssh\id_ed25519"
            } else {
                "~/.ssh/id_ed25519"
            },
            existing
                .as_ref()
                .and_then(|account| non_empty(account.ssh_key.as_deref())),
            false,
            window,
            cx,
        ),
        token_login: None,
        token_stored: None,
        error: None,
        saving: false,
        _task: None,
        _token_status_task: None,
        _subscriptions: subscriptions,
    };
    settings_window.git_account_form = Some(form);
    refresh_token_status(settings_window, cx);
    cx.notify();
}

/// The login the form's host and login fields name, if they name a valid one.
fn form_login(form: &GitAccountForm, cx: &App) -> Option<GitHttpsLogin> {
    let host = text(&form.host, cx);
    let login = text(&form.login, cx);
    if host.is_empty() || login.is_empty() {
        return None;
    }
    GitHttpsLogin::new(&host, &login).ok()
}

/// Reads whether a token is stored for the login the fields name now, so that the form never
/// describes the token of a login it is no longer about.
fn refresh_token_status(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(form) = settings_window.git_account_form.as_mut() else {
        return;
    };
    let login = form_login(form, cx);
    if login == form.token_login && (login.is_none() || form.token_stored.is_some()) {
        return;
    }
    form.token_login = login.clone();
    form.token_stored = None;
    form._token_status_task = login.map(|login| {
        let stored = token_is_stored(&login, cx);
        cx.spawn(async move |this, cx| {
            let stored = stored.await.log_err().unwrap_or(false);
            this.update(cx, |this, cx| {
                if let Some(form) = this.git_account_form.as_mut()
                    && form.token_login.as_ref() == Some(&login)
                {
                    form.token_stored = Some(stored);
                    cx.notify();
                }
            })
            .log_err();
        })
    });
    cx.notify();
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

fn render_field(
    label: &'static str,
    description: Option<&'static str>,
    editor: &Entity<Editor>,
    cx: &App,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(Label::new(label))
        .when_some(description, |this, description| {
            this.child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
        .child(input_box(editor, cx))
}

fn render_form(
    form: &GitAccountForm,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let token_description = match form.token_stored {
        Some(true) => i18n::t!("A token is stored. Leave this empty to keep it."),
        Some(false) => {
            i18n::t!("No token is stored. Without one, git asks for the password as usual.")
        }
        None => i18n::t!(
            "A personal access token for the login. Without one, git asks for the password as usual."
        ),
    };
    v_flex()
        .gap_4()
        .child(
            Label::new(if form.original_id.is_some() {
                i18n::t!("Edit Git Account")
            } else {
                i18n::t!("Add Git Account")
            })
            .size(LabelSize::Large),
        )
        .child(render_field(
            i18n::t!("Account Name"),
            Some(i18n::t!("The name the account is picked by in the Git panel.")),
            &form.id,
            cx,
        ))
        .child(render_field(
            i18n::t!("User Name"),
            Some(i18n::t!("The author and committer name of commits (user.name).")),
            &form.name,
            cx,
        ))
        .child(render_field(
            i18n::t!("Email"),
            Some(i18n::t!("The author and committer email of commits (user.email).")),
            &form.email,
            cx,
        ))
        .child(render_field(
            i18n::t!("Host"),
            Some(i18n::t!(
                "Optional. The HTTPS host the token signs in to, such as github.com. Remotes on other hosts and SSH remotes sign in as git is configured."
            )),
            &form.host,
            cx,
        ))
        .child(render_field(
            i18n::t!("Login"),
            Some(i18n::t!("The user name on the host that goes with the token.")),
            &form.login,
            cx,
        ))
        .child(render_field(
            i18n::t!("Token"),
            Some(token_description),
            &form.token,
            cx,
        ))
        .when(form.token_stored == Some(true), |this| {
            this.child(
                h_flex().child(
                    Button::new("git-account-remove-token", i18n::t!("Remove Stored Token"))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| remove_form_token(this, cx))),
                ),
            )
        })
        .child(render_field(
            i18n::t!("Signing Key"),
            Some(i18n::t!(
                "Optional. The key commits are signed with when signing is on (user.signingkey)."
            )),
            &form.signing_key,
            cx,
        ))
        .child(
            v_flex()
                .gap_1()
                .child(Label::new(i18n::t!("SSH Key")))
                .child(
                    Label::new(i18n::t!(
                        "Optional. The private key file pushing, pulling and fetching reach SSH remotes with, offered alone. Local projects only. Not used when GIT_SSH_COMMAND, GIT_SSH or core.sshCommand is set, which Git for Windows does in its system configuration when installed to use the external OpenSSH. A passphrase is asked for each time."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().min_w_0().child(input_box(&form.ssh_key, cx)))
                        .child(
                            Button::new("git-account-ssh-key-browse", i18n::t!("Browse…"))
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    choose_ssh_key(this, window, cx)
                                })),
                        ),
                ),
        )
        .when_some(form.error.clone(), |this, error| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(
                        Icon::new(IconName::XCircle)
                            .size(IconSize::Small)
                            .color(Color::Error),
                    )
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error)),
            )
        })
        .child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("git-account-cancel", i18n::t!("Cancel"))
                        .style(ButtonStyle::Subtle)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.git_account_form = None;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("git-account-save", i18n::t!("Save"))
                        .style(ButtonStyle::Filled)
                        .disabled(form.saving)
                        .on_click(cx.listener(|this, _, _, cx| save_form(this, cx))),
                ),
        )
}

fn text(editor: &Entity<Editor>, cx: &App) -> String {
    editor.read(cx).text(cx).trim().to_string()
}

fn optional(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn save_form(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(form) = settings_window.git_account_form.as_mut() else {
        return;
    };
    let account = GitAccountContent {
        id: text(&form.id, cx),
        name: text(&form.name, cx),
        email: text(&form.email, cx),
        host: optional(text(&form.host, cx)),
        login: optional(text(&form.login, cx)),
        signing_key: optional(text(&form.signing_key, cx)),
        ssh_key: optional(text(&form.ssh_key, cx)),
    };
    let original_id = form.original_id.clone();
    let token = form.token.read(cx).text(cx);
    let original_login = original_id.as_ref().and_then(|original_id| {
        user_accounts(cx)
            .into_iter()
            .find(|account| &account.id == original_id)
            .and_then(|account| https_login_for_account(&account).ok().flatten())
    });

    // Checked against the file as it is now, so that the write below does not fail unseen.
    let mut checked = settings::GitSettings {
        accounts: Some(user_accounts(cx)),
        ..Default::default()
    };
    let login = upsert_account(&mut checked, original_id.as_deref(), account.clone())
        .and_then(|()| https_login_for_account(&account));
    let login = match login {
        Ok(login) => login,
        Err(error) => {
            form.error = Some(format!("{error:#}").into());
            cx.notify();
            return;
        }
    };
    // The token of the login the account signed in with, once nothing signs in with it.
    let orphaned_login = original_login.filter(|original_login| {
        login.as_ref() != Some(original_login)
            && !login_is_used(
                checked.accounts.as_deref().unwrap_or_default(),
                original_login,
            )
    });
    let token_save = match (login, token.trim().is_empty()) {
        (_, true) => None,
        (Some(login), false) => match save_token(&login, &token, cx) {
            Ok(task) => Some(task),
            Err(error) => {
                form.error = Some(format!("{error:#}").into());
                cx.notify();
                return;
            }
        },
        (None, false) => {
            form.error = Some(
                i18n::t!("A token needs a host and a login to sign in with.")
                    .to_string()
                    .into(),
            );
            cx.notify();
            return;
        }
    };

    form.saving = true;
    form.error = None;
    form._task = Some(cx.spawn(async move |this, cx| {
        let saved = match token_save {
            Some(task) => task.await,
            None => Ok(()),
        };
        this.update(cx, |this, cx| {
            match saved {
                Ok(()) => {
                    let fs = <dyn fs::Fs>::global(cx);
                    update_settings_file(fs, cx, move |settings, _| {
                        upsert_account(
                            settings.git.get_or_insert_default(),
                            original_id.as_deref(),
                            account,
                        )
                        .log_err();
                    });
                    if let Some(orphaned_login) = orphaned_login {
                        remove_token(&orphaned_login, cx).detach_and_log_err(cx);
                    }
                    this.git_account_form = None;
                }
                Err(error) => {
                    if let Some(form) = this.git_account_form.as_mut() {
                        form.saving = false;
                        form.error = Some(format!("{error:#}").into());
                    }
                }
            }
            cx.notify();
        })
        .log_err();
    }));
    cx.notify();
}

fn choose_ssh_key(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    if settings_window.git_account_form.is_none() {
        return;
    }
    let starting_directory = paths::home_dir().join(".ssh");
    // Attached to the settings window: a dialog shown for the whole application goes to
    // whichever window happens to be active.
    let paths = window.prompt_for_paths(
        gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(i18n::t!("Select").into()),
            starting_directory: Some(starting_directory),
        },
        cx,
    );
    cx.spawn_in(window, async move |this, cx| {
        let picked = match paths.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            // Dismissed, or the dialog went away with the window.
            Ok(Ok(None)) | Err(_) => None,
            Ok(Err(error)) => {
                this.update(cx, |this, cx| {
                    if let Some(form) = this.git_account_form.as_mut() {
                        form.error = Some(i18n::t!("Couldn't choose the file: {error}").into());
                    }
                    cx.notify();
                })
                .log_err();
                None
            }
        };
        let Some(path) = picked else {
            return;
        };
        this.update_in(cx, |this, window, cx| {
            if let Some(form) = this.git_account_form.as_ref() {
                form.ssh_key.update(cx, |editor, cx| {
                    editor.set_text(path.to_string_lossy().to_string(), window, cx);
                });
            }
        })
        .log_err();
    })
    .detach();
}

fn remove_form_token(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(form) = settings_window.git_account_form.as_mut() else {
        return;
    };
    let Some(login) = form.token_login.clone() else {
        return;
    };
    let removal = remove_token(&login, cx);
    form._task = Some(cx.spawn(async move |this, cx| {
        let removed = removal.await;
        this.update(cx, |this, cx| {
            if let Some(form) = this.git_account_form.as_mut() {
                match removed {
                    Ok(()) => {
                        if form.token_login.as_ref() == Some(&login) {
                            form.token_stored = Some(false);
                        }
                    }
                    Err(error) => form.error = Some(format!("{error:#}").into()),
                }
            }
            cx.notify();
        })
        .log_err();
    }));
}

fn remove_git_account(id: &str, cx: &mut Context<SettingsWindow>) {
    let accounts = user_accounts(cx);
    let login = accounts
        .iter()
        .find(|account| account.id == id)
        .and_then(|account| https_login_for_account(account).ok().flatten());
    let remaining: Vec<_> = accounts
        .into_iter()
        .filter(|account| account.id != id)
        .collect();
    if let Some(login) = login
        && !login_is_used(&remaining, &login)
    {
        remove_token(&login, cx).detach_and_log_err(cx);
    }
    let id = id.to_string();
    let fs = <dyn fs::Fs>::global(cx);
    update_settings_file(fs, cx, move |settings, _| {
        remove_account(settings.git.get_or_insert_default(), &id);
    });
}
