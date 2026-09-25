use anyhow::{Context as _, Result};
use collections::HashMap;
use context_server::{ContextServerCommand, ContextServerId};
use editor::{Editor, EditorElement, EditorStyle};

use extension_host::ExtensionStore;
use gpui::{
    AsyncWindowContext, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    Subscription, Task, TextStyle, TextStyleRefinement, UnderlineStyle, WeakEntity, prelude::*,
};
use language::{Language, LanguageRegistry};
use markdown::{Markdown, MarkdownElement, MarkdownStyle};
use notifications::status_toast::StatusToast;
use project::{
    context_server_store::{
        ContextServerStatus, ContextServerStore, ServerStatusChangedEvent,
        registry::ContextServerDescriptorRegistry, wait_for_context_server,
    },
    project_settings::{ContextServerSettings, OAuthClientSettings},
    worktree_store::WorktreeStore,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use settings::{Settings as _, SettingsStore, update_settings_file};
use std::sync::Arc;
use theme_settings::ThemeSettings;
use ui::{
    CommonAnimationExt, ContextMenu, DropdownMenu, DropdownStyle, IconPosition, KeyBinding, Modal,
    ModalFooter, ModalHeader, Section, Switch, ToggleState, Tooltip, WithScrollbar, prelude::*,
};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

enum ConfigurationTarget {
    Existing {
        id: ContextServerId,
        command: ContextServerCommand,
    },
    ExistingHttp {
        id: ContextServerId,
        url: String,
        headers: HashMap<String, String>,
        oauth: Option<OAuthClientSettings>,
    },

    Extension {
        id: ContextServerId,
        repository_url: Option<SharedString>,
        installation: Option<extension::ContextServerConfiguration>,
        /// What the user configured before, if anything: re-opening the dialog
        /// must not put the extension's placeholders back in its place.
        current_settings: Option<Value>,
        remote: bool,
    },
}

enum ExistingServerType {
    Local,
    Remote,
}

enum ConfigurationSource {
    Existing {
        editor: Entity<Editor>,
        server_type: ExistingServerType,
    },
    Extension {
        id: ContextServerId,
        editor: Option<Entity<Editor>>,
        repository_url: Option<SharedString>,
        installation_instructions: Option<Entity<markdown::Markdown>>,
        settings_validator: Option<jsonschema::Validator>,
        /// `None` when the extension's settings are not a flat object of
        /// scalars, which leaves the JSON editor as the only way to edit them.
        form: Option<ExtensionSettingsForm>,
        editing_json: bool,
        /// With no form to mask them, saved secrets would sit in the JSON in
        /// plain text; the editor then waits for an explicit 「表示」.
        json_revealed: bool,
        /// Carried through so that configuring a server that runs on the SSH
        /// host does not move it back to this machine.
        remote: bool,
    },
}

impl ConfigurationSource {
    fn has_configuration_options(&self) -> bool {
        !matches!(self, ConfigurationSource::Extension { editor: None, .. })
    }

    fn from_target(
        target: ConfigurationTarget,
        language_registry: Arc<LanguageRegistry>,
        jsonc_language: Option<Arc<Language>>,
        window: &mut Window,
        cx: &mut App,
    ) -> Self {
        fn create_editor(
            json: String,
            jsonc_language: Option<Arc<Language>>,
            window: &mut Window,
            cx: &mut App,
        ) -> Entity<Editor> {
            cx.new(|cx| {
                let mut editor = Editor::auto_height(4, 16, window, cx);
                editor.set_text(json, window, cx);
                editor.set_show_gutter(false, cx);
                editor.set_soft_wrap_mode(language::language_settings::SoftWrap::None, cx);
                if let Some(buffer) = editor.buffer().read(cx).as_singleton() {
                    buffer.update(cx, |buffer, cx| buffer.set_language(jsonc_language, cx))
                }
                editor
            })
        }

        match target {
            ConfigurationTarget::Existing { id, command } => ConfigurationSource::Existing {
                editor: create_editor(
                    context_server_input(Some((id, command))),
                    jsonc_language,
                    window,
                    cx,
                ),
                server_type: ExistingServerType::Local,
            },
            ConfigurationTarget::ExistingHttp {
                id,
                url,
                headers: auth,
                oauth,
            } => ConfigurationSource::Existing {
                editor: create_editor(
                    context_server_http_input(Some((id, url, auth, oauth))),
                    jsonc_language,
                    window,
                    cx,
                ),
                server_type: ExistingServerType::Remote,
            },

            ConfigurationTarget::Extension {
                id,
                repository_url,
                installation,
                current_settings,
                remote,
            } => {
                let settings_validator = installation.as_ref().and_then(|installation| {
                    jsonschema::validator_for(&installation.settings_schema)
                        .context("Failed to load JSON schema for context server settings")
                        .log_err()
                });
                let installation_instructions = installation.as_ref().map(|installation| {
                    cx.new(|cx| {
                        Markdown::new(
                            installation.installation_instructions.clone().into(),
                            Some(language_registry.clone()),
                            None,
                            cx,
                        )
                    })
                });
                let (editor, form, json_revealed) = match installation {
                    Some(installation) => {
                        let initial_text = current_settings
                            .and_then(|settings| {
                                serde_json::to_string_pretty(&settings)
                                    .context(
                                        "Failed to show the configured context server settings",
                                    )
                                    .log_err()
                            })
                            .unwrap_or(installation.default_settings);
                        let initial_settings =
                            serde_json_lenient::from_str::<Value>(&initial_text).ok();
                        let form =
                            schema_form_fields(&installation.settings_schema).and_then(|fields| {
                                let Some(Value::Object(settings)) = initial_settings.clone() else {
                                    return None;
                                };
                                ExtensionSettingsForm::new(fields, settings, window, cx)
                            });
                        let json_revealed = !initial_settings
                            .as_ref()
                            .is_some_and(contains_secret_values);
                        (
                            Some(create_editor(initial_text, jsonc_language, window, cx)),
                            form,
                            json_revealed,
                        )
                    }
                    None => (None, None, true),
                };
                ConfigurationSource::Extension {
                    id,
                    repository_url,
                    installation_instructions,
                    settings_validator,
                    editor,
                    form,
                    editing_json: false,
                    json_revealed,
                    remote,
                }
            }
        }
    }

    fn output(&self, cx: &mut App) -> Result<(ContextServerId, ContextServerSettings)> {
        match self {
            ConfigurationSource::Existing {
                editor,
                server_type,
            } => match *server_type {
                ExistingServerType::Remote => {
                    parse_http_input(&editor.read(cx).text(cx)).map(|(id, url, auth, oauth)| {
                        (
                            id,
                            ContextServerSettings::Http {
                                enabled: true,
                                url,
                                headers: auth,
                                timeout: None,
                                oauth,
                            },
                        )
                    })
                }
                ExistingServerType::Local => {
                    parse_input(&editor.read(cx).text(cx)).map(|(id, command)| {
                        (
                            id,
                            ContextServerSettings::Stdio {
                                enabled: true,
                                remote: false,
                                command,
                            },
                        )
                    })
                }
            },
            ConfigurationSource::Extension {
                id,
                editor,
                settings_validator,
                form,
                editing_json,
                remote,
                ..
            } => {
                let settings = match form {
                    Some(form) if !*editing_json => form
                        .settings(true, cx)
                        .map_err(|error| anyhow::anyhow!(error))?,
                    _ => {
                        let text = editor
                            .as_ref()
                            .context("設定を取得できませんでした。")?
                            .read(cx)
                            .text(cx);
                        serde_json_lenient::from_str::<serde_json::Value>(&text)?
                    }
                };
                if let Some(settings_validator) = settings_validator
                    && let Err(error) = settings_validator.validate(&settings)
                {
                    // The message quotes the offending value, which can be a
                    // token the form was masking.
                    let message = if contains_secret_values(&settings) {
                        error.masked().to_string()
                    } else {
                        error.to_string()
                    };
                    return Err(anyhow::anyhow!(message));
                }
                Ok((
                    id.clone(),
                    ContextServerSettings::Extension {
                        enabled: true,
                        remote: *remote,
                        settings,
                    },
                ))
            }
        }
    }
}

fn context_server_input(existing: Option<(ContextServerId, ContextServerCommand)>) -> String {
    let (name, command, args, env) = match existing {
        Some((id, cmd)) => {
            let args = serde_json::to_string(&cmd.args).unwrap();
            let env = serde_json::to_string(&cmd.env.unwrap_or_default()).unwrap();
            let cmd_path = serde_json::to_string(&cmd.path).unwrap();
            (id.0.to_string(), cmd_path, args, env)
        }
        None => (
            "some-mcp-server".to_string(),
            "".to_string(),
            "[]".to_string(),
            "{}".to_string(),
        ),
    };

    format!(
        r#"{{
  /// Configure an MCP server that runs locally via stdin/stdout
  ///
  /// The name of your MCP server
  "{name}": {{
    /// The command which runs the MCP server
    "command": {command},
    /// The arguments to pass to the MCP server
    "args": {args},
    /// The environment variables to set
    "env": {env}
  }}
}}"#
    )
}

fn context_server_http_input(
    existing: Option<(
        ContextServerId,
        String,
        HashMap<String, String>,
        Option<OAuthClientSettings>,
    )>,
) -> String {
    let (name, url, headers, oauth) = match existing {
        Some((id, url, headers, oauth)) => {
            let headers = if headers.is_empty() {
                r#"// "Authorization": "Bearer <token>"#.to_string()
            } else {
                let json = serde_json::to_string_pretty(&headers).unwrap();
                let mut lines = json.split("\n").collect::<Vec<_>>();
                if lines.len() > 1 {
                    lines.remove(0);
                    lines.pop();
                }
                lines
                    .into_iter()
                    .map(|line| format!("  {}", line))
                    .collect::<String>()
            };
            (id.0.to_string(), url, headers, oauth)
        }
        None => (
            "some-remote-server".to_string(),
            "https://example.com/mcp".to_string(),
            r#"// "Authorization": "Bearer <token>"#.to_string(),
            None,
        ),
    };

    let oauth = oauth.map_or_else(
        || {
            r#"
    /// Uncomment to use a pre-registered OAuth client. You can include the client secret here as well, otherwise it will be prompted interactively and saved in the system keychain.
    // "oauth": {
    //   "client_id": "your-client-id",
    // },"#
                .to_string()
        },

        |oauth| {
            let mut lines = vec![
                String::from("\n    \"oauth\": {"),

                format!("      \"client_id\": {},", serde_json::to_string(&oauth.client_id).unwrap()),
            ];
            if let Some(client_secret) = oauth.client_secret {
                lines.push(format!(
                    "      \"client_secret\": {}",
                    serde_json::to_string(&client_secret).unwrap()
                ));
            } else {
                lines.push(String::from(
                    "      /// Optional client secret for confidential clients\n      // \"client_secret\": \"your-client-secret\"",
                ));
            }
            lines.push(String::from("    },"));

            lines.join("\n")
        },
    );

    format!(
        r#"{{
  /// Configure an MCP server that you connect to over HTTP
  ///
  /// The name of your remote MCP server
  "{name}": {{
    /// The URL of the remote MCP server
    "url": "{url}",{oauth}
    "headers": {{
     /// Any headers to send along
     {headers}
    }}
  }}
}}"#
    )
}

fn parse_http_input(
    text: &str,
) -> Result<(
    ContextServerId,
    String,
    HashMap<String, String>,
    Option<OAuthClientSettings>,
)> {
    #[derive(Deserialize)]
    struct Temp {
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        oauth: Option<OAuthClientSettings>,
    }
    let value: HashMap<String, Temp> = serde_json_lenient::from_str(text)?;
    if value.len() != 1 {
        anyhow::bail!("コンテキストサーバーの設定は 1 つだけ指定してください。");
    }

    let (key, value) = value.into_iter().next().unwrap();

    Ok((
        ContextServerId(key.into()),
        value.url,
        value.headers,
        value.oauth,
    ))
}

fn resolve_context_server_extension(
    id: ContextServerId,
    worktree_store: Entity<WorktreeStore>,
    current_settings: Option<Value>,
    remote: bool,
    cx: &mut App,
) -> Task<Option<ConfigurationTarget>> {
    let registry = ContextServerDescriptorRegistry::default_global(cx).read(cx);

    let Some(descriptor) = registry.context_server_descriptor(&id.0) else {
        return Task::ready(None);
    };

    let extension = ExtensionStore::global(cx)
        .read(cx)
        .installed_extensions()
        .iter()
        .find(|(_, entry)| entry.manifest.context_servers.contains_key(&id.0))
        .map(|(id, entry)| (id.clone(), entry.manifest.clone()));
    cx.spawn(async move |cx| {
        let installation = descriptor
            .configuration(worktree_store, cx)
            .await
            .context("Failed to resolve context server configuration")
            .log_err()
            .flatten();

        Some(ConfigurationTarget::Extension {
            id,
            repository_url: extension
                .and_then(|(_, manifest)| manifest.repository.clone().map(SharedString::from)),
            installation,
            current_settings,
            remote,
        })
    })
}

enum State {
    Idle,
    Waiting,
    AuthRequired {
        server_id: ContextServerId,
    },
    ClientSecretRequired {
        server_id: ContextServerId,
        error: Option<SharedString>,
    },
    Authenticating {
        server_id: ContextServerId,
    },
    Error(SharedString),
}

pub struct ConfigureContextServerModal {
    context_server_store: Entity<ContextServerStore>,
    workspace: WeakEntity<Workspace>,
    source: ConfigurationSource,
    state: State,
    original_server_id: Option<ContextServerId>,
    scroll_handle: ScrollHandle,
    secret_editor: Entity<Editor>,
    _auth_subscription: Option<Subscription>,
}

impl ConfigureContextServerModal {
    fn initial_state(
        context_server_store: &Entity<ContextServerStore>,
        target: &ConfigurationTarget,
        cx: &App,
    ) -> State {
        let server_id = match target {
            ConfigurationTarget::Existing { id, .. }
            | ConfigurationTarget::ExistingHttp { id, .. }
            | ConfigurationTarget::Extension { id, .. } => id,
        };

        match context_server_store.read(cx).status_for_server(server_id) {
            Some(ContextServerStatus::AuthRequired) => State::AuthRequired {
                server_id: server_id.clone(),
            },
            Some(ContextServerStatus::ClientSecretRequired { error }) => {
                State::ClientSecretRequired {
                    server_id: server_id.clone(),
                    error: error.map(SharedString::from),
                }
            }
            Some(ContextServerStatus::Authenticating) => State::Authenticating {
                server_id: server_id.clone(),
            },
            Some(ContextServerStatus::Error(error)) => State::Error(error.into()),

            Some(ContextServerStatus::Starting)
            | Some(ContextServerStatus::Running)
            | Some(ContextServerStatus::Stopped)
            | Some(ContextServerStatus::ApprovalRequired)
            | None => State::Idle,
        }
    }

    pub fn show_modal_for_existing_server(
        server_id: ContextServerId,
        language_registry: Arc<LanguageRegistry>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<()>> {
        // Only the user's own file: the merged settings also hold the SSH
        // host's server settings, which saving would copy into the user's file
        // as an entry that nobody approved.
        let Some(settings) = user_context_server_settings(&server_id, cx).or_else(|| {
            ContextServerDescriptorRegistry::default_global(cx)
                .read(cx)
                .context_server_descriptor(&server_id.0)
                .map(|_| ContextServerSettings::default_extension())
        }) else {
            return Task::ready(Err(anyhow::anyhow!("Context server not found")));
        };

        window.spawn(cx, async move |cx| {
            let target = match settings {
                ContextServerSettings::Stdio {
                    enabled: _,
                    command,
                    ..
                } => Some(ConfigurationTarget::Existing {
                    id: server_id,
                    command,
                }),
                ContextServerSettings::Http {
                    enabled: _,
                    url,
                    headers,
                    timeout: _,
                    oauth,
                } => Some(ConfigurationTarget::ExistingHttp {
                    id: server_id,
                    url,
                    headers,
                    oauth,
                }),

                ContextServerSettings::Extension {
                    settings, remote, ..
                } => {
                    // `{}` is what an extension server with no settings entry
                    // resolves to; the extension's own defaults are the better
                    // starting point then.
                    let current_settings = settings
                        .as_object()
                        .is_some_and(|settings| !settings.is_empty())
                        .then_some(settings);
                    match workspace
                        .update(cx, |workspace, cx| {
                            resolve_context_server_extension(
                                server_id,
                                workspace.project().read(cx).worktree_store(),
                                current_settings,
                                remote,
                                cx,
                            )
                        })
                        .ok()
                    {
                        Some(task) => task.await,
                        None => None,
                    }
                }
            };

            match target {
                Some(target) => Self::show_modal(target, language_registry, workspace, cx).await,
                None => Err(anyhow::anyhow!("Failed to resolve context server")),
            }
        })
    }

    fn show_modal(
        target: ConfigurationTarget,
        language_registry: Arc<LanguageRegistry>,
        workspace: WeakEntity<Workspace>,
        cx: &mut AsyncWindowContext,
    ) -> Task<Result<()>> {
        cx.spawn(async move |cx| {
            let jsonc_language = language_registry.language_for_name("jsonc").await.ok();
            workspace.update_in(cx, |workspace, window, cx| {
                let workspace_handle = cx.weak_entity();
                let context_server_store = workspace.project().read(cx).context_server_store();
                workspace.toggle_modal(window, cx, |window, cx| Self {
                    context_server_store: context_server_store.clone(),
                    workspace: workspace_handle,
                    state: Self::initial_state(&context_server_store, &target, cx),

                    original_server_id: Some(match &target {
                        ConfigurationTarget::Existing { id, .. }
                        | ConfigurationTarget::ExistingHttp { id, .. }
                        | ConfigurationTarget::Extension { id, .. } => id.clone(),
                    }),
                    source: ConfigurationSource::from_target(
                        target,
                        language_registry,
                        jsonc_language,
                        window,
                        cx,
                    ),
                    scroll_handle: ScrollHandle::new(),
                    secret_editor: cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_placeholder_text(
                            "クライアントシークレットを入力 (パブリッククライアントの場合は空欄)",
                            window,
                            cx,
                        );
                        editor.set_masked(true, cx);
                        editor
                    }),
                    _auth_subscription: None,
                })
            })
        })
    }

    fn set_error(&mut self, err: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.state = State::Error(err.into());
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, cx: &mut Context<Self>) {
        if matches!(self.state, State::Waiting | State::Authenticating { .. }) {
            return;
        }

        self._auth_subscription = None;

        self.state = State::Idle;
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let (id, settings) = match self.source.output(cx) {
            Ok(val) => val,
            Err(error) => {
                self.set_error(error.to_string(), cx);
                return;
            }
        };

        if defined_by_project(
            self.context_server_store.read(cx),
            &id,
            self.original_server_id.as_ref(),
            cx,
        ) {
            self.set_error(DEFINED_BY_PROJECT_MESSAGE, cx);
            return;
        }

        self.state = State::Waiting;

        let existing_server = self.context_server_store.read(cx).get_server(&id);
        if existing_server.is_some() {
            self.context_server_store.update(cx, |store, cx| {
                store.stop_server(&id, cx).log_err();
            });
        }

        let wait_for_context_server_task =
            wait_for_context_server(&self.context_server_store, id.clone(), cx);
        cx.spawn({
            let id = id.clone();
            async move |this, cx| {
                let result = wait_for_context_server_task.await;
                this.update(cx, |this, cx| match result {
                    Ok(ContextServerStatus::Running) => {
                        this.state = State::Idle;
                        this.show_configured_context_server_toast(id, cx);
                        cx.emit(DismissEvent);
                    }
                    Ok(ContextServerStatus::AuthRequired) => {
                        this.state = State::AuthRequired { server_id: id };
                        cx.notify();
                    }
                    Ok(ContextServerStatus::ClientSecretRequired { error }) => {
                        this.state = State::ClientSecretRequired {
                            server_id: id,
                            error: error.map(SharedString::from),
                        };
                        cx.notify();
                    }
                    Err(err) => {
                        this.set_error(err, cx);
                    }
                    Ok(_) => {}
                })
            }
        })
        .detach();

        let settings_changed = user_context_server_settings(&id, cx).as_ref() != Some(&settings);

        if settings_changed {
            // When we write the settings to the file, the context server will be restarted.
            workspace.update(cx, |workspace, cx| {
                let fs = workspace.app_state().fs.clone();
                let original_server_id = self.original_server_id.clone();
                update_settings_file(fs.clone(), cx, move |current, _| {
                    if let Some(original_id) = original_server_id {
                        if original_id != id {
                            current.project.context_servers.remove(&original_id.0);
                        }
                    }
                    current
                        .project
                        .context_servers
                        .insert(id.0, settings.into());
                });
            });
        } else if let Some(existing_server) = existing_server {
            self.context_server_store
                .update(cx, |store, cx| store.start_server(existing_server, cx));
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn reveal_json(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let ConfigurationSource::Extension { json_revealed, .. } = &mut self.source {
            *json_revealed = true;
        }
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn set_form_input(
        &mut self,
        index: usize,
        input: ExtensionSettingsInput,
        cx: &mut Context<Self>,
    ) {
        if let ConfigurationSource::Extension {
            form: Some(form), ..
        } = &mut self.source
            && let Some(slot) = form.inputs.get_mut(index)
        {
            *slot = input;
        }
        cx.notify();
    }

    fn toggle_json_editing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ConfigurationSource::Extension {
            editor: Some(editor),
            form: Some(form),
            editing_json,
            ..
        } = &mut self.source
        else {
            return;
        };

        let outcome = if *editing_json {
            match serde_json_lenient::from_str::<Value>(&editor.read(cx).text(cx)) {
                Ok(Value::Object(settings)) => {
                    match ExtensionSettingsForm::new(form.fields.clone(), settings, window, cx) {
                        Some(refilled) => {
                            *form = refilled;
                            *editing_json = false;
                            Ok(())
                        }
                        None => Err(
                            "この JSON にはフォームで表せない値が含まれています。JSON で編集を続けてください。"
                                .to_string(),
                        ),
                    }
                }
                Ok(_) => {
                    Err("拡張機能の設定は JSON オブジェクト ({ … }) にしてください。".to_string())
                }
                Err(error) => Err(format!(
                    "JSON を読み取れないため、フォームに切り替えられません: {error}"
                )),
            }
        } else {
            form.settings(false, cx).and_then(|settings| {
                // Rewriting only on a change keeps the comments the extension's
                // defaults came with.
                let unchanged = serde_json_lenient::from_str::<Value>(&editor.read(cx).text(cx))
                    .is_ok_and(|current| current == settings);
                if !unchanged {
                    let text = serde_json::to_string_pretty(&settings)
                        .map_err(|error| error.to_string())?;
                    editor.update(cx, |editor, cx| editor.set_text(text, window, cx));
                }
                *editing_json = true;
                Ok(())
            })
        };

        match outcome {
            Ok(()) => {
                if matches!(self.state, State::Error(_)) {
                    self.state = State::Idle;
                }
            }
            Err(error) => self.state = State::Error(error.into()),
        }
        self.focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    fn cancel_authentication(&mut self, server_id: &ContextServerId, cx: &mut Context<Self>) {
        self._auth_subscription = None;
        self.context_server_store.update(cx, |store, cx| {
            store.stop_server(server_id, cx).log_err();
        });
        self.state = State::Idle;
        cx.notify();
    }

    fn authenticate(&mut self, server_id: ContextServerId, cx: &mut Context<Self>) {
        self.context_server_store.update(cx, |store, cx| {
            store.authenticate_server(&server_id, cx).log_err();
        });
        self.await_auth_outcome(server_id, cx);
    }

    fn submit_client_secret(&mut self, server_id: ContextServerId, cx: &mut Context<Self>) {
        let secret = self.secret_editor.read(cx).text(cx);
        self.context_server_store.update(cx, |store, cx| {
            store.submit_client_secret(&server_id, secret, cx).log_err();
        });
        self.await_auth_outcome(server_id, cx);
    }

    fn await_auth_outcome(&mut self, server_id: ContextServerId, cx: &mut Context<Self>) {
        self.state = State::Authenticating {
            server_id: server_id.clone(),
        };

        self._auth_subscription = Some(cx.subscribe(
            &self.context_server_store,
            move |this, _, event: &ServerStatusChangedEvent, cx| {
                if event.server_id != server_id {
                    return;
                }
                match &event.status {
                    ContextServerStatus::Running => {
                        this._auth_subscription = None;
                        this.state = State::Idle;
                        this.show_configured_context_server_toast(event.server_id.clone(), cx);
                        cx.emit(DismissEvent);
                    }
                    ContextServerStatus::AuthRequired => {
                        this._auth_subscription = None;
                        this.state = State::AuthRequired {
                            server_id: event.server_id.clone(),
                        };
                        cx.notify();
                    }
                    ContextServerStatus::ClientSecretRequired { error } => {
                        this._auth_subscription = None;
                        this.state = State::ClientSecretRequired {
                            server_id: event.server_id.clone(),
                            error: error.clone().map(SharedString::from),
                        };
                        cx.notify();
                    }
                    ContextServerStatus::Error(error) => {
                        this._auth_subscription = None;
                        this.set_error(error.clone(), cx);
                    }
                    // Nothing more will happen until the project's definition
                    // is approved, so waiting on would never end.
                    ContextServerStatus::ApprovalRequired => {
                        this._auth_subscription = None;
                        this.set_error(
                            "このサーバーは承認待ちです。設定ウィンドウの「MCP サーバー」ページで確認してください。",
                            cx,
                        );
                    }
                    ContextServerStatus::Authenticating
                    | ContextServerStatus::Starting
                    | ContextServerStatus::Stopped => {}
                }
            },
        ));

        cx.notify();
    }

    fn show_configured_context_server_toast(&self, id: ContextServerId, cx: &mut App) {
        self.workspace
            .update(cx, {
                |workspace, cx| {
                    let status_toast = StatusToast::new(
                        format!("{} を構成しました。", id.0),
                        cx,
                        |this, _cx| {
                            this.icon(
                                Icon::new(IconName::ToolHammer)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .action("閉じる", |_, _| {})
                        },
                    );

                    workspace.toggle_status_toast(status_toast, cx);
                }
            })
            .log_err();
    }
}

/// This dialog writes the user's settings, where an entry for a name a
/// project defines is shadowed by the project's and would only leave a copy
/// behind that nothing runs.
fn defined_by_project(
    store: &ContextServerStore,
    id: &ContextServerId,
    original_id: Option<&ContextServerId>,
    cx: &App,
) -> bool {
    std::iter::once(id).chain(original_id).any(|id| {
        store
            .server_source(id, cx)
            .is_some_and(|source| source.is_project())
    })
}

const DEFINED_BY_PROJECT_MESSAGE: &str = "このサーバーはプロジェクトの設定ファイル (.zed/settings.json) で定義されています。ここで保存するとユーザー設定に書き込まれますが、プロジェクトの定義が優先されるため反映されません。設定ウィンドウの「MCP サーバー」ページから、定義しているファイルを編集してください。";

fn parse_input(text: &str) -> Result<(ContextServerId, ContextServerCommand)> {
    let value: serde_json::Value = serde_json_lenient::from_str(text)?;
    let object = value
        .as_object()
        .context("JSON オブジェクトを指定してください。")?;
    anyhow::ensure!(
        object.len() == 1,
        "キーと値のペアを 1 つだけ指定してください。"
    );
    let (context_server_name, value) = object.into_iter().next().unwrap();
    let command: ContextServerCommand = serde_json::from_value(value.clone())?;
    Ok((ContextServerId(context_server_name.clone().into()), command))
}

const UNSET_CHOICE_LABEL: &str = "(未設定)";

/// An extension's settings shown as a form built from its settings schema.
struct ExtensionSettingsForm {
    fields: Vec<SchemaFormField>,
    inputs: Vec<ExtensionSettingsInput>,
    /// What the inputs held when the form was filled, so that only what the
    /// user changed gets written: an untouched field leaves its key exactly as
    /// the settings had it, absent included.
    initial: Vec<SchemaFieldValue>,
    /// The settings the form was filled from. Keys it has no field for are
    /// written back as they were.
    base: Map<String, Value>,
    focus_handle: FocusHandle,
}

enum ExtensionSettingsInput {
    Text(Entity<Editor>),
    Toggle(bool),
    Choice(Option<String>),
}

impl ExtensionSettingsForm {
    /// `None` when a value in `base` does not fit its field, which the JSON
    /// editor can still show faithfully.
    fn new(
        fields: Vec<SchemaFormField>,
        base: Map<String, Value>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Self> {
        let initial = fields
            .iter()
            .map(|field| initial_field_value(field, base.get(&field.key)))
            .collect::<Option<Vec<_>>>()?;
        let inputs = fields
            .iter()
            .zip(&initial)
            .map(|(field, value)| match value {
                SchemaFieldValue::Text(text) => {
                    let text = text.clone();
                    let secret = field.secret;
                    ExtensionSettingsInput::Text(cx.new(|cx| {
                        let mut editor = Editor::single_line(window, cx);
                        editor.set_text(text, window, cx);
                        editor.set_masked(secret, cx);
                        editor
                    }))
                }
                SchemaFieldValue::Toggle(on) => ExtensionSettingsInput::Toggle(*on),
                SchemaFieldValue::Choice(choice) => ExtensionSettingsInput::Choice(choice.clone()),
            })
            .collect();
        Some(Self {
            fields,
            inputs,
            initial,
            base,
            focus_handle: cx.focus_handle(),
        })
    }

    fn values(&self, cx: &App) -> Vec<SchemaFieldValue> {
        self.inputs
            .iter()
            .map(|input| match input {
                ExtensionSettingsInput::Text(editor) => {
                    SchemaFieldValue::Text(editor.read(cx).text(cx))
                }
                ExtensionSettingsInput::Toggle(on) => SchemaFieldValue::Toggle(*on),
                ExtensionSettingsInput::Choice(choice) => SchemaFieldValue::Choice(choice.clone()),
            })
            .collect()
    }

    /// With `require` off, required fields may still be blank: switching to the
    /// JSON editor must not be blocked by a field the user has yet to fill in.
    fn settings(&self, require: bool, cx: &App) -> Result<Value, String> {
        settings_from_form(
            &self.fields,
            &self.initial,
            &self.values(cx),
            &self.base,
            require,
        )
        .map(Value::Object)
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.inputs
            .iter()
            .find_map(|input| match input {
                ExtensionSettingsInput::Text(editor) => Some(editor.focus_handle(cx)),
                ExtensionSettingsInput::Toggle(_) | ExtensionSettingsInput::Choice(_) => None,
            })
            .unwrap_or_else(|| self.focus_handle.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
struct SchemaFormField {
    key: String,
    title: String,
    description: Option<String>,
    kind: SchemaFieldKind,
    required: bool,
    secret: bool,
    default: Option<Value>,
}

#[derive(Clone, Debug, PartialEq)]
enum SchemaFieldKind {
    Text,
    Number,
    Integer,
    Boolean,
    Choice(Vec<String>),
}

#[derive(Clone, Debug, PartialEq)]
enum SchemaFieldValue {
    /// Also the value of number fields, which are parsed only when written.
    Text(String),
    Toggle(bool),
    Choice(Option<String>),
}

/// Keywords that make a settings object more than a fixed set of scalars.
const NON_FLAT_SCHEMA_KEYWORDS: &[&str] = &[
    "$ref",
    "allOf",
    "anyOf",
    "oneOf",
    "not",
    "if",
    "then",
    "else",
    "patternProperties",
    "propertyNames",
    "dependencies",
    "dependentSchemas",
    "unevaluatedProperties",
];

/// `None` unless the schema describes a flat object of strings, numbers,
/// booleans and string choices; anything else is left to the JSON editor.
fn schema_form_fields(schema: &Value) -> Option<Vec<SchemaFormField>> {
    let schema = schema.as_object()?;
    if NON_FLAT_SCHEMA_KEYWORDS
        .iter()
        .any(|keyword| schema.contains_key(*keyword))
    {
        return None;
    }
    if schema
        .get("type")
        .is_some_and(|kind| kind.as_str() != Some("object"))
    {
        return None;
    }
    if schema
        .get("additionalProperties")
        .is_some_and(|additional| !additional.is_boolean())
    {
        return None;
    }
    let properties = schema.get("properties")?.as_object()?;
    let required = match schema.get("required") {
        None => Vec::new(),
        Some(Value::Array(names)) => names
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?,
        Some(_) => return None,
    };

    properties
        .iter()
        .map(|(key, property)| {
            let property = property.as_object()?;
            Some(SchemaFormField {
                key: key.clone(),
                title: property
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| key.clone()),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                kind: property_kind(property)?,
                required: required.contains(&key.as_str()),
                secret: looks_secret(key),
                default: property.get("default").cloned(),
            })
        })
        .collect()
}

fn property_kind(property: &Map<String, Value>) -> Option<SchemaFieldKind> {
    if ["$ref", "allOf", "not", "if", "properties", "items"]
        .iter()
        .any(|keyword| property.contains_key(*keyword))
    {
        return None;
    }
    if let Some(choices) = property.get("enum") {
        return string_choices(choices.as_array()?).map(SchemaFieldKind::Choice);
    }
    for keyword in ["oneOf", "anyOf"] {
        if let Some(variants) = property.get(keyword) {
            return variants_kind(variants.as_array()?);
        }
    }
    match scalar_type(property.get("type")?)? {
        "string" => Some(SchemaFieldKind::Text),
        "number" => Some(SchemaFieldKind::Number),
        "integer" => Some(SchemaFieldKind::Integer),
        "boolean" => Some(SchemaFieldKind::Boolean),
        _ => None,
    }
}

/// `"string"`, or `["string", "null"]`, which is how an optional value is
/// spelled.
fn scalar_type(kind: &Value) -> Option<&str> {
    match kind {
        Value::String(name) => Some(name.as_str()),
        Value::Array(names) => {
            let mut non_null = names.iter().filter(|name| name.as_str() != Some("null"));
            let only = non_null.next()?.as_str()?;
            non_null.next().is_none().then_some(only)
        }
        _ => None,
    }
}

fn string_choices(values: &[Value]) -> Option<Vec<String>> {
    // `null` among the choices is how an optional choice is spelled, which
    // leaving the field unset already covers.
    let choices = values
        .iter()
        .filter(|value| !value.is_null())
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()?;
    (!choices.is_empty()).then_some(choices)
}

/// `oneOf` / `anyOf` as schema generators write them for an enum (one `const`
/// or single-value `enum` per variant) or for an optional value (the value's
/// schema next to `{ "type": "null" }`).
fn variants_kind(variants: &[Value]) -> Option<SchemaFieldKind> {
    let variants = variants
        .iter()
        .map(Value::as_object)
        .collect::<Option<Vec<_>>>()?;
    let non_null = variants
        .into_iter()
        .filter(|variant| !is_null_schema(variant))
        .collect::<Vec<_>>();

    let mut choices = Vec::new();
    for variant in &non_null {
        match (variant.get("const"), variant.get("enum")) {
            (Some(Value::String(constant)), _) => choices.push(constant.clone()),
            (None, Some(Value::Array(values))) => choices.extend(string_choices(values)?),
            _ => {
                return match non_null.as_slice() {
                    [only] => property_kind(only),
                    _ => None,
                };
            }
        }
    }
    (!choices.is_empty()).then_some(SchemaFieldKind::Choice(choices))
}

fn is_null_schema(variant: &Map<String, Value>) -> bool {
    variant.get("type").and_then(Value::as_str) == Some("null")
        && !variant.contains_key("const")
        && !variant.contains_key("enum")
}

fn user_context_server_settings(id: &ContextServerId, cx: &App) -> Option<ContextServerSettings> {
    cx.global::<SettingsStore>()
        .raw_user_settings()?
        .content
        .project
        .context_servers
        .get(&id.0)
        .cloned()
        .map(ContextServerSettings::from)
}

fn looks_secret(key: &str) -> bool {
    let lowercase = key.to_ascii_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "authorization",
        "credential",
        "cookie",
        "passphrase",
    ]
    .iter()
    .any(|word| lowercase.contains(word))
        // Only as a word of its own: as a substring it would catch `path`.
        || key_words(key).iter().any(|word| word == "pat")
}

/// `github_pat`, `GITHUB-PAT` and `githubPat` all split into `github`, `pat`.
fn key_words(key: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_was_lowercase = false;
    for character in key.chars() {
        if !character.is_ascii_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            previous_was_lowercase = false;
            continue;
        }
        if character.is_ascii_uppercase() && previous_was_lowercase && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        previous_was_lowercase = character.is_ascii_lowercase() || character.is_ascii_digit();
        current.push(character.to_ascii_lowercase());
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// Whether any key that looks like it names a secret holds a value, at any
/// depth.
fn contains_secret_values(value: &Value) -> bool {
    match value {
        Value::Object(entries) => entries.iter().any(|(key, value)| {
            let holds_value = match value {
                Value::Null => false,
                Value::String(text) => !text.is_empty(),
                _ => true,
            };
            (looks_secret(key) && holds_value) || contains_secret_values(value)
        }),
        Value::Array(items) => items.iter().any(contains_secret_values),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => false,
    }
}

/// `None` when the value does not fit the field (a string where a number is
/// expected, a choice that is not among the options).
fn initial_field_value(field: &SchemaFormField, value: Option<&Value>) -> Option<SchemaFieldValue> {
    let value = value
        .or(field.default.as_ref())
        .filter(|value| !value.is_null());
    match (&field.kind, value) {
        (SchemaFieldKind::Text | SchemaFieldKind::Number | SchemaFieldKind::Integer, None) => {
            Some(SchemaFieldValue::Text(String::new()))
        }
        (SchemaFieldKind::Text, Some(Value::String(text))) => {
            Some(SchemaFieldValue::Text(text.clone()))
        }
        (SchemaFieldKind::Number, Some(Value::Number(number))) => {
            Some(SchemaFieldValue::Text(number.to_string()))
        }
        (SchemaFieldKind::Integer, Some(Value::Number(number)))
            if number.is_i64() || number.is_u64() =>
        {
            Some(SchemaFieldValue::Text(number.to_string()))
        }
        (SchemaFieldKind::Boolean, None) => Some(SchemaFieldValue::Toggle(false)),
        (SchemaFieldKind::Boolean, Some(Value::Bool(on))) => Some(SchemaFieldValue::Toggle(*on)),
        (SchemaFieldKind::Choice(_), None) => Some(SchemaFieldValue::Choice(None)),
        (SchemaFieldKind::Choice(options), Some(Value::String(choice)))
            if options.contains(choice) =>
        {
            Some(SchemaFieldValue::Choice(Some(choice.clone())))
        }
        _ => None,
    }
}

fn settings_from_form(
    fields: &[SchemaFormField],
    initial: &[SchemaFieldValue],
    current: &[SchemaFieldValue],
    base: &Map<String, Value>,
    require: bool,
) -> Result<Map<String, Value>, String> {
    let mut settings = base.clone();
    for ((field, initial), current) in fields.iter().zip(initial).zip(current) {
        let blank = match current {
            SchemaFieldValue::Text(text) => text.trim().is_empty(),
            SchemaFieldValue::Toggle(_) => false,
            SchemaFieldValue::Choice(choice) => choice.is_none(),
        };
        if require && field.required && blank {
            return Err(format!("「{}」は必須です。", field.title));
        }
        if current == initial {
            continue;
        }

        let value = match current {
            SchemaFieldValue::Toggle(on) => Some(Value::Bool(*on)),
            SchemaFieldValue::Choice(choice) => choice.clone().map(Value::String),
            SchemaFieldValue::Text(text) => match &field.kind {
                SchemaFieldKind::Number => parse_number(text, &field.title)?,
                SchemaFieldKind::Integer => parse_integer(text, &field.title)?,
                // Removed rather than written as "", which the extension could
                // take for a value.
                _ if text.is_empty() && !field.required => None,
                _ => Some(Value::String(text.clone())),
            },
        };
        match value {
            Some(value) => {
                settings.insert(field.key.clone(), value);
            }
            None => {
                settings.shift_remove(&field.key);
            }
        }
    }
    Ok(settings)
}

fn parse_number(text: &str, title: &str) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    if let Ok(integer) = text.parse::<i64>() {
        return Ok(Some(Value::from(integer)));
    }
    text.parse::<f64>()
        .ok()
        .and_then(serde_json::Number::from_f64)
        .map(|number| Some(Value::Number(number)))
        .ok_or_else(|| format!("「{title}」には数値を入力してください。"))
}

fn parse_integer(text: &str, title: &str) -> Result<Option<Value>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<i64>()
        .map(|integer| Some(Value::from(integer)))
        .map_err(|_| format!("「{title}」には整数を入力してください。"))
}

impl ModalView for ConfigureContextServerModal {}

impl Focusable for ConfigureContextServerModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.source {
            ConfigurationSource::Existing { editor, .. } => editor.focus_handle(cx),
            ConfigurationSource::Extension {
                editor,
                form,
                editing_json,
                ..
            } => match form {
                Some(form) if !*editing_json => form.focus_handle(cx),
                _ => editor
                    .as_ref()
                    .map(|editor| editor.focus_handle(cx))
                    .unwrap_or_else(|| cx.focus_handle()),
            },
        }
    }
}

impl EventEmitter<DismissEvent> for ConfigureContextServerModal {}

impl ConfigureContextServerModal {
    fn render_modal_header(&self) -> ModalHeader {
        let text: SharedString = match &self.source {
            ConfigurationSource::Existing { .. } => "MCP サーバーを構成".into(),
            ConfigurationSource::Extension { id, .. } => format!("{} を構成", id.0).into(),
        };
        ModalHeader::new().headline(text)
    }

    fn render_modal_description(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        const MODAL_DESCRIPTION: &str =
            "必要な引数と環境変数についてはサーバーのドキュメントを確認してください。";

        if let ConfigurationSource::Extension {
            installation_instructions: Some(installation_instructions),
            ..
        } = &self.source
        {
            div()
                .pb_2()
                .text_sm()
                .child(MarkdownElement::new(
                    installation_instructions.clone(),
                    default_markdown_style(window, cx),
                ))
                .into_any_element()
        } else {
            Label::new(MODAL_DESCRIPTION)
                .color(Color::Muted)
                .into_any_element()
        }
    }

    fn render_modal_content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (editor, form, editing_json, json_revealed) = match &self.source {
            ConfigurationSource::Existing { editor, .. } => {
                return Self::render_json_editor(editor, cx);
            }
            ConfigurationSource::Extension { editor: None, .. } => {
                return div().into_any_element();
            }
            ConfigurationSource::Extension {
                editor: Some(editor),
                form,
                editing_json,
                json_revealed,
                ..
            } => (editor, form.as_ref(), *editing_json, *json_revealed),
        };

        let body = match form {
            Some(form) if !editing_json => Self::render_settings_form(form, window, cx),
            // Switching to JSON from the form is itself the explicit request.
            Some(_) => Self::render_json_editor(editor, cx),
            None if !json_revealed => Self::render_hidden_json(editor, cx),
            None => Self::render_json_editor(editor, cx),
        };

        v_flex()
            .w_full()
            .gap_2()
            .map(|this| match form {
                Some(_) => this.child(
                    h_flex().w_full().justify_end().child(
                        Button::new(
                            "toggle-json-editing",
                            if editing_json {
                                "フォームで編集"
                            } else {
                                "JSON で編集"
                            },
                        )
                        .style(ButtonStyle::Subtle)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            |this, _event, window, cx| {
                                this.toggle_json_editing(window, cx);
                            },
                        )),
                    ),
                ),
                None => this.child(
                    Label::new("この拡張機能の設定はフォームで表せないため、JSON で編集します。")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            })
            .child(body)
            .into_any_element()
    }

    fn render_settings_form(
        form: &ExtensionSettingsForm,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if form.fields.is_empty() {
            return Label::new("この拡張機能には設定する項目がありません。")
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element();
        }

        let border_focused = cx.theme().colors().border_focused;
        let modal = cx.weak_entity();
        let rows = form
            .fields
            .iter()
            .zip(&form.inputs)
            .enumerate()
            .map(|(index, (field, input))| {
                let control = match input {
                    ExtensionSettingsInput::Text(editor) => {
                        let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
                        ui::input_frame(false, cx)
                            .w_full()
                            .track_focus(&focus_handle)
                            .focus(|style| style.border_color(border_focused))
                            .child(editor.clone())
                            .into_any_element()
                    }
                    ExtensionSettingsInput::Toggle(on) => Switch::new(
                        ("extension-setting-toggle", index),
                        if *on {
                            ToggleState::Selected
                        } else {
                            ToggleState::Unselected
                        },
                    )
                    .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
                        this.set_form_input(
                            index,
                            ExtensionSettingsInput::Toggle(*state == ToggleState::Selected),
                            cx,
                        );
                    }))
                    .into_any_element(),
                    ExtensionSettingsInput::Choice(selected) => {
                        let options = match &field.kind {
                            SchemaFieldKind::Choice(options) => options.clone(),
                            _ => Vec::new(),
                        };
                        Self::render_choice(
                            index,
                            field.required,
                            selected.clone(),
                            options,
                            modal.clone(),
                            window,
                            cx,
                        )
                    }
                };

                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(Label::new(field.title.clone()).size(LabelSize::Small))
                            .when(field.required, |this| {
                                this.child(
                                    Label::new("必須")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Muted),
                                )
                            }),
                    )
                    .child(control)
                    .when_some(field.description.clone(), |this, description| {
                        this.child(
                            Label::new(description)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    })
            })
            .collect::<Vec<_>>();

        v_flex()
            .w_full()
            .gap_3()
            .track_focus(&form.focus_handle)
            .children(rows)
            .into_any_element()
    }

    fn render_choice(
        index: usize,
        required: bool,
        selected: Option<String>,
        options: Vec<String>,
        modal: WeakEntity<Self>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = selected
            .clone()
            .unwrap_or_else(|| UNSET_CHOICE_LABEL.to_string());
        let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
            if !required {
                let modal = modal.clone();
                menu = menu.toggleable_entry(
                    UNSET_CHOICE_LABEL,
                    selected.is_none(),
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        modal
                            .update(cx, |this, cx| {
                                this.set_form_input(index, ExtensionSettingsInput::Choice(None), cx)
                            })
                            .log_err();
                    },
                );
            }
            for option in options {
                let is_selected = selected.as_deref() == Some(option.as_str());
                let modal = modal.clone();
                menu = menu.toggleable_entry(
                    option.clone(),
                    is_selected,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        modal
                            .update(cx, |this, cx| {
                                this.set_form_input(
                                    index,
                                    ExtensionSettingsInput::Choice(Some(option.clone())),
                                    cx,
                                )
                            })
                            .log_err();
                    },
                );
            }
            menu
        });
        DropdownMenu::new(("extension-setting-choice", index), label, menu)
            .style(DropdownStyle::Outlined)
            .full_width(true)
            .into_any_element()
    }

    fn render_hidden_json(editor: &Entity<Editor>, cx: &mut Context<Self>) -> AnyElement {
        h_flex()
            .w_full()
            .gap_2()
            .justify_between()
            // Keeps keyboard focus inside the modal while the editor is not
            // on screen, so Escape and Ctrl+Enter still reach it.
            .track_focus(&editor.focus_handle(cx))
            .child(
                Label::new("保存済みの設定に秘密情報らしい値が含まれるため、JSON を隠しています。")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(
                Button::new("reveal-extension-settings", "表示")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.reveal_json(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_json_editor(editor: &Entity<Editor>, cx: &App) -> AnyElement {
        div()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().colors().border_variant)
            .bg(cx.theme().colors().editor_background)
            .child({
                let settings = ThemeSettings::get_global(cx);
                let text_style = TextStyle {
                    color: cx.theme().colors().text,
                    font_family: settings.buffer_font.family.clone(),
                    font_fallbacks: settings.buffer_font.fallbacks.clone(),
                    font_size: settings.buffer_font_size(cx).into(),
                    font_weight: settings.buffer_font.weight,
                    line_height: relative(settings.buffer_line_height.value()),
                    ..Default::default()
                };
                EditorElement::new(
                    editor,
                    EditorStyle {
                        background: cx.theme().colors().editor_background,
                        local_player: cx.theme().players().local(),
                        text: text_style,
                        syntax: cx.theme().syntax().clone(),
                        ..Default::default()
                    },
                )
            })
            .into_any_element()
    }

    fn render_modal_footer(&self, cx: &mut Context<Self>) -> ModalFooter {
        let focus_handle = self.focus_handle(cx);
        let is_busy = matches!(self.state, State::Waiting | State::Authenticating { .. });

        ModalFooter::new()
            .start_slot::<Button>(
                if let ConfigurationSource::Extension {
                    repository_url: Some(repository_url),
                    ..
                } = &self.source
                {
                    Some(
                        Button::new("open-repository", "リポジトリを開く")
                            .end_icon(
                                Icon::new(IconName::ArrowUpRight)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .tooltip({
                                let repository_url = repository_url.clone();
                                move |_window, cx| {
                                    Tooltip::with_meta(
                                        "リポジトリを開く",
                                        None,
                                        repository_url.clone(),
                                        cx,
                                    )
                                }
                            })
                            .on_click({
                                let repository_url = repository_url.clone();
                                move |_, _, cx| cx.open_url(&repository_url)
                            }),
                    )
                } else {
                    None
                },
            )
            .end_slot(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new(
                            "cancel",
                            if self.source.has_configuration_options() {
                                "キャンセル"
                            } else {
                                "閉じる"
                            },
                        )
                        .key_binding(
                            KeyBinding::for_action_in(&menu::Cancel, &focus_handle, cx)
                                .map(|kb| kb.size(rems_from_px(12_f32))),
                        )
                        .on_click(
                            cx.listener(|this, _event, _window, cx| this.cancel(&menu::Cancel, cx)),
                        ),
                    )
                    .children(self.source.has_configuration_options().then(|| {
                        Button::new("configure-server", "サーバーを構成")
                            .disabled(is_busy)
                            .key_binding(
                                KeyBinding::for_action_in(&menu::Confirm, &focus_handle, cx)
                                    .map(|kb| kb.size(rems_from_px(12_f32))),
                            )
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.confirm(&menu::Confirm, cx)
                            }))
                    })),
            )
    }

    fn render_loading(&self, label: impl Into<SharedString>) -> Div {
        h_flex()
            .h_8()
            .gap_1p5()
            .justify_center()
            .child(
                Icon::new(IconName::LoadCircle)
                    .size(IconSize::XSmall)
                    .color(Color::Muted)
                    .with_rotate_animation(3),
            )
            .child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
    }

    fn render_auth_required(&self, server_id: &ContextServerId, cx: &mut Context<Self>) -> Div {
        h_flex()
            .h_8()
            .min_w_0()
            .w_full()
            .gap_2()
            .justify_center()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new("このサーバーに接続するには認証が必要です")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Button::new("authenticate-server", "認証")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click({
                        let server_id = server_id.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.authenticate(server_id.clone(), cx);
                        })
                    }),
            )
    }

    fn render_client_secret_required(
        &self,
        server_id: &ContextServerId,
        error: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Div {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.buffer_font.family.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            font_size: settings.buffer_font_size(cx).into(),
            font_weight: settings.buffer_font.weight,
            line_height: relative(settings.buffer_line_height.value()),
            ..Default::default()
        };

        v_flex()
            .w_full()
            .gap_2()
            .when_some(error, |this, error| {
                this.child(Self::render_modal_error(error))
            })
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::Info)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(
                        Label::new(
                            "OAuth クライアントシークレットを入力してください。パブリッククライアントの場合は空欄のままにします",
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
            )
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .capture_action({
                        let server_id = server_id.clone();
                        cx.listener(move |this, _: &editor::actions::Newline, _window, cx| {
                            this.submit_client_secret(server_id.clone(), cx);
                        })
                    })
                    .child(div().flex_1().child(EditorElement::new(
                        &self.secret_editor,
                        EditorStyle {
                            background: cx.theme().colors().editor_background,
                            local_player: cx.theme().players().local(),
                            text: text_style,
                            syntax: cx.theme().syntax().clone(),
                            ..Default::default()
                        },
                    )))
                    .child(
                        Button::new("submit-client-secret", "送信")
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click({
                                let server_id = server_id.clone();
                                cx.listener(move |this, _event, _window, cx| {
                                    this.submit_client_secret(server_id.clone(), cx);
                                })
                            }),
                    ),
            )
    }

    fn render_authenticating(&self, server_id: &ContextServerId, cx: &mut Context<Self>) -> Div {
        h_flex()
            .h_8()
            .gap_2()
            .justify_center()
            .child(
                h_flex()
                    .gap_1p5()
                    .child(
                        Icon::new(IconName::LoadCircle)
                            .size(IconSize::XSmall)
                            .color(Color::Muted)
                            .with_rotate_animation(3),
                    )
                    .child(
                        Label::new("認証しています…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
            .child(
                Button::new("cancel-authentication", "キャンセル")
                    .style(ButtonStyle::Outlined)
                    .label_size(LabelSize::Small)
                    .on_click({
                        let server_id = server_id.clone();
                        cx.listener(move |this, _event, _window, cx| {
                            this.cancel_authentication(&server_id, cx);
                        })
                    }),
            )
    }

    fn render_modal_error(error: SharedString) -> Div {
        h_flex()
            .h_8()
            .gap_1p5()
            .justify_center()
            .child(
                Icon::new(IconName::Warning)
                    .size(IconSize::Small)
                    .color(Color::Warning),
            )
            .child(
                div()
                    .w_full()
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Muted)),
            )
    }
}

impl Render for ConfigureContextServerModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .elevation_3(cx)
            .w(rems(40.))
            .key_context("ConfigureContextServerModal")
            .on_action(
                cx.listener(|this, _: &menu::Cancel, _window, cx| this.cancel(&menu::Cancel, cx)),
            )
            .on_action(
                cx.listener(|this, _: &menu::Confirm, _window, cx| {
                    this.confirm(&menu::Confirm, cx)
                }),
            )
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window, cx);
            }))
            .child(
                Modal::new("configure-context-server", None)
                    .header(self.render_modal_header())
                    .section(
                        Section::new().child(
                            div()
                                .size_full()
                                .child(
                                    div()
                                        .id("modal-content")
                                        .max_h(vh(0.7, window))
                                        .overflow_y_scroll()
                                        .track_scroll(&self.scroll_handle)
                                        .child(self.render_modal_description(window, cx))
                                        .child(self.render_modal_content(window, cx))
                                        .child(match &self.state {
                                            State::Idle => div(),
                                            State::Waiting => {
                                                self.render_loading("サーバーに接続しています…")
                                            }
                                            State::AuthRequired { server_id } => {
                                                self.render_auth_required(&server_id.clone(), cx)
                                            }
                                            State::ClientSecretRequired { server_id, error } => {
                                                self.render_client_secret_required(
                                                    &server_id.clone(),
                                                    error.clone(),
                                                    cx,
                                                )
                                            }
                                            State::Authenticating { server_id } => {
                                                self.render_authenticating(&server_id.clone(), cx)
                                            }
                                            State::Error(error) => {
                                                Self::render_modal_error(error.clone())
                                            }
                                        }),
                                )
                                .vertical_scrollbar_for(&self.scroll_handle, window, cx),
                        ),
                    )
                    .footer(self.render_modal_footer(cx)),
            )
    }
}

pub(crate) fn default_markdown_style(window: &Window, cx: &App) -> MarkdownStyle {
    let theme_settings = ThemeSettings::get_global(cx);
    let colors = cx.theme().colors();
    let mut text_style = window.text_style();
    text_style.refine(&TextStyleRefinement {
        font_family: Some(theme_settings.ui_font.family.clone()),
        font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
        font_features: Some(theme_settings.ui_font.features.clone()),
        font_size: Some(TextSize::XSmall.rems(cx).into()),
        color: Some(colors.text_muted),
        ..Default::default()
    });

    MarkdownStyle {
        base_text_style: text_style.clone(),
        selection_background_color: colors.element_selection_background,
        link: TextStyleRefinement {
            background_color: Some(colors.editor_foreground.opacity(0.025)),
            underline: Some(UnderlineStyle {
                color: Some(colors.text_accent.opacity(0.5)),
                thickness: px(1.),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, Project};
    use settings::ContextServerSettingsContent;
    use util::path;

    /// The dialog writes the user's settings, and a project's entry of the
    /// same name wins over the user's: saving there would change nothing that
    /// runs and leave a stray copy in the user's file.
    #[gpui::test]
    async fn test_a_server_the_project_defines_is_not_saved_to_the_user_settings(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.context_servers.insert(
                        "shared".into(),
                        ContextServerSettingsContent::Stdio {
                            // Off for the same reason as the project's entry below: the store
                            // starts the user's entry before the project's settings are read.
                            enabled: false,
                            remote: false,
                            command: ContextServerCommand {
                                path: "user-server".into(),
                                args: Vec::new(),
                                env: None,
                                timeout: None,
                            },
                        },
                    );
                    settings.project.context_servers.insert(
                        "mine".into(),
                        ContextServerSettingsContent::Stdio {
                            enabled: false,
                            remote: false,
                            command: ContextServerCommand {
                                path: "my-server".into(),
                                args: Vec::new(),
                                env: None,
                                timeout: None,
                            },
                        },
                    );
                });
            });
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            serde_json::json!({
                ".zed": {
                    // Disabled so that nothing sets out to launch a real process.
                    "settings.json": r#"{ "context_servers": { "shared": { "enabled": false, "command": "project-server" } } }"#
                },
                "src": {}
            }),
        )
        .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        cx.run_until_parked();

        let shared = ContextServerId("shared".into());
        let other = ContextServerId("other".into());
        project.read_with(cx, |project, cx| {
            let store = project.context_server_store();
            let store = store.read(cx);
            assert!(defined_by_project(store, &shared, None, cx));
            assert!(
                defined_by_project(store, &other, Some(&shared), cx),
                "renaming it does not take it out of the project's hands"
            );
            assert!(defined_by_project(store, &shared, Some(&other), cx));
            assert!(!defined_by_project(store, &other, None, cx));
            let mine = ContextServerId("mine".into());
            assert!(
                store.server_source(&mine, cx).is_some(),
                "the store knows the user's own server"
            );
            assert!(
                !defined_by_project(store, &mine, None, cx),
                "a server only the user's settings define is saved as before"
            );
            assert!(!defined_by_project(store, &mine, Some(&other), cx));
        });
    }

    #[test]
    fn parse_http_input_reads_oauth_settings() {
        let (id, url, headers, oauth) = parse_http_input(
            r#"{
  "figma": {
    "url": "https://mcp.figma.com/mcp",
    "oauth": {
      "client_id": "client-id",
      "client_secret": "client-secret"
    },
    "headers": {
      "X-Test": "test"
    }
  }
}"#,
        )
        .unwrap();

        assert_eq!(id, ContextServerId("figma".into()));
        assert_eq!(url, "https://mcp.figma.com/mcp");
        assert_eq!(headers.get("X-Test"), Some(&String::from("test")));
        let oauth = oauth.expect("oauth should be present");
        assert_eq!(oauth.client_id, "client-id");
        assert_eq!(oauth.client_secret.as_deref(), Some("client-secret"));
    }

    #[test]
    fn context_server_http_input_preserves_existing_oauth_settings() {
        let text = context_server_http_input(Some((
            ContextServerId("figma".into()),
            String::from("https://mcp.figma.com/mcp"),
            HashMap::default(),
            Some(OAuthClientSettings {
                client_id: String::from("client-id"),
                client_secret: Some(String::from("client-secret")),
            }),
        )));

        let (_, _, _, oauth) = parse_http_input(&text).unwrap();
        let oauth = oauth.expect("oauth should be present");
        assert_eq!(oauth.client_id, "client-id");
        assert_eq!(oauth.client_secret.as_deref(), Some("client-secret"));
    }

    use serde_json::json;

    /// What `schemars` 0.8 generates for the GitHub extension's settings.
    #[test]
    fn maps_a_flat_extension_schema_to_form_fields() {
        let schema = json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "GitHubContextServerSettings",
            "type": "object",
            "required": ["github_personal_access_token"],
            "properties": {
                "github_personal_access_token": { "type": "string" }
            }
        });
        assert_eq!(
            schema_form_fields(&schema),
            Some(vec![SchemaFormField {
                key: "github_personal_access_token".into(),
                title: "github_personal_access_token".into(),
                description: None,
                kind: SchemaFieldKind::Text,
                required: true,
                secret: true,
                default: None,
            }])
        );
    }

    fn mixed_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Settings",
            "type": "object",
            "properties": {
                "database_url": { "description": "Postgres connection string", "type": "string" },
                "api_key": { "type": ["string", "null"] },
                "port": { "title": "Port", "type": "integer", "format": "uint16", "minimum": 0, "default": 5432 },
                "ratio": { "type": "number" },
                "read_only": { "type": "boolean", "default": false },
                "mode": { "type": "string", "enum": ["fast", "safe"] },
                "level": { "oneOf": [
                    { "const": "low", "description": "Fewer requests" },
                    { "const": "high" }
                ] },
                "region": { "anyOf": [
                    { "type": "string", "enum": ["us", "eu"] },
                    { "type": "null" }
                ] },
                "label": { "anyOf": [{ "type": "string" }, { "type": "null" }] },
                "admin_password": { "type": "string" }
            },
            "required": ["database_url"],
            "additionalProperties": false
        })
    }

    fn field<'a>(fields: &'a [SchemaFormField], key: &str) -> &'a SchemaFormField {
        fields
            .iter()
            .find(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field {key}"))
    }

    #[test]
    fn maps_every_scalar_kind_and_every_enum_spelling() {
        let fields = schema_form_fields(&mixed_schema()).expect("a flat schema");
        let summary = fields
            .iter()
            .map(|field| {
                (
                    field.key.as_str(),
                    field.kind.clone(),
                    field.required,
                    field.secret,
                )
            })
            .collect::<Vec<_>>();
        let choice = |options: &[&str]| {
            SchemaFieldKind::Choice(options.iter().map(|option| option.to_string()).collect())
        };
        assert_eq!(
            summary,
            vec![
                ("database_url", SchemaFieldKind::Text, true, false),
                ("api_key", SchemaFieldKind::Text, false, true),
                ("port", SchemaFieldKind::Integer, false, false),
                ("ratio", SchemaFieldKind::Number, false, false),
                ("read_only", SchemaFieldKind::Boolean, false, false),
                ("mode", choice(&["fast", "safe"]), false, false),
                ("level", choice(&["low", "high"]), false, false),
                ("region", choice(&["us", "eu"]), false, false),
                ("label", SchemaFieldKind::Text, false, false),
                ("admin_password", SchemaFieldKind::Text, false, true),
            ]
        );
        assert_eq!(
            field(&fields, "database_url").description.as_deref(),
            Some("Postgres connection string")
        );
        assert_eq!(field(&fields, "port").title, "Port");
        assert_eq!(field(&fields, "port").default, Some(json!(5432)));
    }

    #[test]
    fn leaves_anything_nested_to_the_json_editor() {
        for schema in [
            json!({ "type": "object", "properties": { "nested": { "type": "object", "properties": {} } } }),
            json!({ "type": "object", "properties": { "list": { "type": "array", "items": { "type": "string" } } } }),
            json!({
                "type": "object",
                "properties": { "mode": { "$ref": "#/definitions/Mode" } },
                "definitions": { "Mode": { "enum": ["a"] } }
            }),
            json!({ "type": "object", "properties": { "either": { "anyOf": [{ "type": "string" }, { "type": "integer" }] } } }),
            json!({ "type": "object", "properties": { "a": { "type": "string" } }, "oneOf": [{ "required": ["a"] }] }),
            json!({ "type": "object", "properties": {}, "additionalProperties": { "type": "string" } }),
            json!({ "type": "object" }),
            json!({ "type": "array", "items": { "type": "string" } }),
            json!("not a schema"),
        ] {
            assert_eq!(schema_form_fields(&schema), None, "{schema}");
        }
    }

    #[test]
    fn a_value_that_does_not_fit_its_field_is_left_to_the_json_editor() {
        let fields = schema_form_fields(&mixed_schema()).expect("a flat schema");
        let port = field(&fields, "port");
        assert_eq!(initial_field_value(port, Some(&json!("5432"))), None);
        assert_eq!(initial_field_value(port, Some(&json!(1.5))), None);
        assert_eq!(
            initial_field_value(port, Some(&json!(8080))),
            Some(SchemaFieldValue::Text("8080".into()))
        );
        // Absent: the schema's default is shown.
        assert_eq!(
            initial_field_value(port, None),
            Some(SchemaFieldValue::Text("5432".into()))
        );
        assert_eq!(
            initial_field_value(field(&fields, "mode"), Some(&json!("turbo"))),
            None
        );
        assert_eq!(
            initial_field_value(field(&fields, "read_only"), Some(&json!("yes"))),
            None
        );
    }

    fn initial_values(
        fields: &[SchemaFormField],
        base: &Map<String, Value>,
    ) -> Vec<SchemaFieldValue> {
        fields
            .iter()
            .map(|field| {
                initial_field_value(field, base.get(&field.key))
                    .unwrap_or_else(|| panic!("{} should fit", field.key))
            })
            .collect()
    }

    fn set(
        fields: &[SchemaFormField],
        values: &mut [SchemaFieldValue],
        key: &str,
        value: SchemaFieldValue,
    ) {
        let index = fields
            .iter()
            .position(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field {key}"));
        values[index] = value;
    }

    #[test]
    fn an_untouched_form_writes_back_exactly_what_it_was_filled_with() {
        let fields = schema_form_fields(&mixed_schema()).expect("a flat schema");
        let base = json!({
            "database_url": "postgres://localhost/db",
            "extra": { "kept": [1, 2] },
            "read_only": true,
            "label": ""
        })
        .as_object()
        .cloned()
        .expect("an object");
        let initial = initial_values(&fields, &base);

        let settings = settings_from_form(&fields, &initial, &initial, &base, true)
            .expect("nothing to complain about");
        assert_eq!(settings, base);
        assert_eq!(
            settings.keys().collect::<Vec<_>>(),
            base.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn writes_only_what_changed_and_checks_it_first() {
        let fields = schema_form_fields(&mixed_schema()).expect("a flat schema");
        let base = json!({ "database_url": "postgres://localhost/db", "label": "old" })
            .as_object()
            .cloned()
            .expect("an object");
        let initial = initial_values(&fields, &base);

        let mut current = initial.clone();
        set(
            &fields,
            &mut current,
            "port",
            SchemaFieldValue::Text("8080".into()),
        );
        set(
            &fields,
            &mut current,
            "ratio",
            SchemaFieldValue::Text("0.5".into()),
        );
        set(
            &fields,
            &mut current,
            "read_only",
            SchemaFieldValue::Toggle(true),
        );
        set(
            &fields,
            &mut current,
            "mode",
            SchemaFieldValue::Choice(Some("safe".into())),
        );
        set(
            &fields,
            &mut current,
            "label",
            SchemaFieldValue::Text(String::new()),
        );
        let settings =
            settings_from_form(&fields, &initial, &current, &base, true).expect("valid values");
        assert_eq!(
            Value::Object(settings),
            json!({
                "database_url": "postgres://localhost/db",
                "port": 8080,
                "ratio": 0.5,
                "read_only": true,
                "mode": "safe"
            })
        );

        let mut missing = initial.clone();
        set(
            &fields,
            &mut missing,
            "database_url",
            SchemaFieldValue::Text("  ".into()),
        );
        assert_eq!(
            settings_from_form(&fields, &initial, &missing, &base, true),
            Err("「database_url」は必須です。".to_string())
        );
        // Switching to the JSON editor does not insist on required fields.
        assert!(settings_from_form(&fields, &initial, &missing, &base, false).is_ok());

        let mut not_an_integer = initial.clone();
        set(
            &fields,
            &mut not_an_integer,
            "port",
            SchemaFieldValue::Text("80.5".into()),
        );
        assert_eq!(
            settings_from_form(&fields, &initial, &not_an_integer, &base, true),
            Err("「Port」には整数を入力してください。".to_string())
        );

        let mut not_a_number = initial.clone();
        set(
            &fields,
            &mut not_a_number,
            "ratio",
            SchemaFieldValue::Text("half".into()),
        );
        assert_eq!(
            settings_from_form(&fields, &initial, &not_a_number, &base, true),
            Err("「ratio」には数値を入力してください。".to_string())
        );
    }

    #[test]
    fn masks_fields_whose_names_look_secret() {
        for key in [
            "GITHUB_TOKEN",
            "apiKey",
            "client_secret",
            "password",
            "brave_api_key",
            "Authorization",
            "aws_credentials",
            "session_cookie",
            "ssh_passphrase",
            "github_pat",
            "GITHUB-PAT",
            "githubPat",
        ] {
            assert!(looks_secret(key), "{key}");
        }
        for key in [
            "database_url",
            "region",
            "read_only",
            "path",
            "pattern",
            "workspace_path",
        ] {
            assert!(!looks_secret(key), "{key}");
        }
    }

    #[test]
    fn finds_secret_values_at_any_depth() {
        assert!(!contains_secret_values(
            &json!({ "region": "eu", "api_key": "" })
        ));
        assert!(!contains_secret_values(
            &json!({ "api_key": null, "port": 5432 })
        ));
        assert!(contains_secret_values(&json!({ "api_key": "sk-123" })));
        assert!(contains_secret_values(
            &json!({ "servers": [{ "name": "a", "auth": { "token": "t" } }] })
        ));
    }
}
