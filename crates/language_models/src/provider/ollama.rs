use anyhow::{Result, anyhow};
use collections::HashMap;
use credentials_provider::CredentialsProvider;
use fs::Fs;
use futures::{FutureExt, StreamExt, future::BoxFuture};
use futures::{Stream, TryFutureExt, stream};
use gpui::{App, AsyncApp, Context, Entity, Task, TaskExt};
use http_client::{CustomHeaders, HttpClient};
use language_model::{
    ApiKeyState, AuthenticateError, DisabledReason, EnvVar, IconOrSvg, InlineDescription,
    LanguageModel, LanguageModelClient, LanguageModelCompletionError, LanguageModelCompletionEvent,
    LanguageModelCompletionStream, LanguageModelEffortLevel, LanguageModelId, LanguageModelName,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelProviderName,
    LanguageModelProviderState, LanguageModelRequest, LanguageModelRequestTool,
    LanguageModelToolUse, LanguageModelToolUseId, MessageContent, ModelRateLimiters,
    ProviderSettingsView, ReasoningEffort, Role, StopReason, SubPageProviderSettings, TokenUsage,
    env_var, unavailable_error,
};
use menu;
use ollama::{
    ChatMessage, ChatOptions, ChatRequest, ChatResponseDelta, OLLAMA_API_URL, OllamaFunctionCall,
    OllamaFunctionTool, OllamaToolCall, ThinkValue, get_models, show_model, stream_chat_completion,
};
pub use settings::OllamaAvailableModel as AvailableModel;
use settings::{Settings, SettingsStore, update_settings_file};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::LazyLock;
use ui::{
    ButtonLike, ButtonLink, ConfiguredApiCard, Divider, List, ListBulletItem, Tooltip, prelude::*,
};
use ui_input::InputField;

use crate::AllLanguageModelSettings;

const OLLAMA_DOWNLOAD_URL: &str = "https://ollama.com/download";
const OLLAMA_LIBRARY_URL: &str = "https://ollama.com/library";
const OLLAMA_SITE: &str = "https://ollama.com/";

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("ollama");
const PROVIDER_NAME: LanguageModelProviderName = LanguageModelProviderName::new("Ollama");

const API_KEY_ENV_VAR_NAME: &str = "OLLAMA_API_KEY";
static API_KEY_ENV_VAR: LazyLock<EnvVar> = env_var!(API_KEY_ENV_VAR_NAME);

#[derive(Default, Debug, Clone, PartialEq)]
pub struct OllamaSettings {
    pub api_url: String,
    pub auto_discover: bool,
    pub available_models: Vec<AvailableModel>,
    pub context_window: Option<u64>,
    pub custom_headers: CustomHeaders,
}

pub struct OllamaLanguageModelProvider {
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
    request_limiters: ModelRateLimiters,
}

pub struct State {
    api_key_state: ApiKeyState,
    credentials_provider: Arc<dyn CredentialsProvider>,
    http_client: Arc<dyn HttpClient>,
    fetched_models: Vec<ollama::Model>,
    fetch_model_task: Option<Task<Result<()>>>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        !self.fetched_models.is_empty()
    }

    fn set_api_key(&mut self, api_key: Option<String>, cx: &mut Context<Self>) -> Task<Result<()>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = OllamaLanguageModelProvider::api_url(cx);
        let task = self.api_key_state.store(
            api_url,
            api_key,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );

        self.fetched_models.clear();
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.restart_fetch_models_task(cx))
                .ok();
            result
        })
    }

    fn authenticate(&mut self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        let credentials_provider = self.credentials_provider.clone();
        let api_url = OllamaLanguageModelProvider::api_url(cx);
        let task = self.api_key_state.load_if_needed(
            api_url,
            |this| &mut this.api_key_state,
            credentials_provider,
            cx,
        );

        // Always try to fetch models - if no API key is needed (local Ollama), it will work
        // If API key is needed and provided, it will work
        // If API key is needed and not provided, it will fail gracefully
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| this.restart_fetch_models_task(cx))
                .ok();
            result
        })
    }

    fn fetch_models(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let http_client = Arc::clone(&self.http_client);
        let settings = OllamaLanguageModelProvider::settings(cx);
        let api_url = OllamaLanguageModelProvider::api_url(cx);
        let api_key = self.api_key_state.key(&api_url);
        let extra_headers = settings.custom_headers.clone();

        // zed-jp-enhance: the list is fetched at startup without the user asking,
        // so an endpoint off this machine is not contacted until the user has
        // accepted the external AI warning for it (ADR-F10).
        if !OllamaLanguageModelProvider::endpoint_is_local_or_acknowledged(&api_url, cx) {
            self.fetched_models.clear();
            return Task::ready(Err(anyhow!(
                "not fetching Ollama models from {api_url}: it is not on this machine and \
                 the external AI warning for Ollama has not been accepted"
            )));
        }

        // As a proxy for the server being "authenticated", we'll check if its up by fetching the models
        cx.spawn(async move |this, cx| {
            let models = get_models(
                http_client.as_ref(),
                &api_url,
                api_key.as_deref(),
                &extra_headers,
            )
            .await?;

            let tasks = models
                .into_iter()
                // Since there is no metadata from the Ollama API
                // indicating which models are embedding models,
                // simply filter out models with "-embed" in their name
                .filter(|model| !model.name.contains("-embed"))
                .map(|model| {
                    let http_client = Arc::clone(&http_client);
                    let api_url = api_url.clone();
                    let api_key = api_key.clone();
                    let extra_headers = extra_headers.clone();
                    async move {
                        let name = model.name.as_str();

                        show_model(
                            http_client.as_ref(),
                            &api_url,
                            api_key.as_deref(),
                            name,
                            &extra_headers,
                        )
                        .await
                        .map_or_else(
                            |error| {
                                ollama::Model::new_disabled(
                                    name,
                                    i18n::t!("Failed to fetch model from API: {error}"),
                                )
                            },
                            |model| {
                                ollama::Model::new(
                                    name,
                                    model.context_length,
                                    Some(model.supports_tools()),
                                    Some(model.supports_vision()),
                                    Some(model.supports_thinking()),
                                )
                            },
                        )
                    }
                });

            // Rate-limit capability fetches
            // since there is an arbitrary number of models available
            let mut ollama_models: Vec<_> = futures::stream::iter(tasks)
                .buffer_unordered(5)
                .collect()
                .await;

            ollama_models.sort_by(|a, b| a.name.cmp(&b.name));

            this.update(cx, |this, cx| {
                this.fetched_models = ollama_models;
                cx.notify();
            })
        })
    }

    fn restart_fetch_models_task(&mut self, cx: &mut Context<Self>) {
        let task = self.fetch_models(cx);
        self.fetch_model_task.replace(task);
    }
}

impl OllamaLanguageModelProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        let this = Self {
            http_client: http_client.clone(),
            state: cx.new(|cx| {
                cx.observe_global::<SettingsStore>({
                    let mut last_settings = OllamaLanguageModelProvider::settings(cx).clone();
                    let mut last_reachable = Self::endpoint_is_local_or_acknowledged(
                        &OllamaLanguageModelProvider::api_url(cx),
                        cx,
                    );
                    move |this: &mut State, cx| {
                        // Accepting or revoking the warning changes no Ollama
                        // setting, yet decides whether the endpoint may be asked.
                        let reachable = Self::endpoint_is_local_or_acknowledged(
                            &OllamaLanguageModelProvider::api_url(cx),
                            cx,
                        );
                        let reachability_changed = reachable != last_reachable;
                        last_reachable = reachable;
                        if reachability_changed
                            && OllamaLanguageModelProvider::settings(cx) == &last_settings
                        {
                            this.restart_fetch_models_task(cx);
                            cx.notify();
                        }
                        let current_settings = OllamaLanguageModelProvider::settings(cx);
                        let settings_changed = current_settings != &last_settings;
                        if settings_changed {
                            let url_changed = last_settings.api_url != current_settings.api_url;
                            last_settings = current_settings.clone();
                            if url_changed {
                                let credentials_provider = this.credentials_provider.clone();
                                let api_url = Self::api_url(cx);
                                this.api_key_state.handle_url_change(
                                    api_url,
                                    |this| &mut this.api_key_state,
                                    credentials_provider,
                                    cx,
                                );
                                this.fetched_models.clear();
                                this.authenticate(cx).detach();
                            }
                            cx.notify();
                        }
                    }
                })
                .detach();

                State {
                    http_client,
                    fetched_models: Default::default(),
                    fetch_model_task: None,
                    api_key_state: ApiKeyState::new(Self::api_url(cx), (*API_KEY_ENV_VAR).clone()),
                    credentials_provider,
                }
            }),
            request_limiters: ModelRateLimiters::default(),
        };
        this
    }

    /// Every model this provider offers, keyed by id: the fetched list when
    /// auto-discovery is on, with settings entries merged over it.
    fn ollama_models(&self, cx: &App) -> HashMap<String, ollama::Model> {
        let mut models: HashMap<String, ollama::Model> = HashMap::default();
        let settings = OllamaLanguageModelProvider::settings(cx);

        if settings.auto_discover {
            for model in self.state.read(cx).fetched_models.iter() {
                let mut model = model.clone();
                if let Some(context_window) = settings.context_window {
                    model.max_tokens = context_window;
                }
                models.insert(model.name.clone(), model);
            }
        }

        merge_settings_into_models(
            &mut models,
            &settings.available_models,
            settings.context_window,
        );

        models
    }

    /// The current configuration of `model`, if this provider still offers it.
    fn config(
        &self,
        model: &LanguageModel,
        cx: &App,
    ) -> Result<ollama::Model, LanguageModelCompletionError> {
        self.ollama_models(cx)
            .remove(model.id.0.as_ref())
            .ok_or_else(|| unavailable_error(model))
    }

    fn to_ollama_request(
        config: &ollama::Model,
        request: LanguageModelRequest,
    ) -> Result<ChatRequest> {
        if request.contains_custom_tool_input() {
            anyhow::bail!("Ollama does not support custom tools");
        }

        let supports_vision = config.supports_vision.unwrap_or(false);

        let mut messages = Vec::with_capacity(request.messages.len());

        for mut msg in request.messages.into_iter() {
            let images = if supports_vision {
                msg.content
                    .iter()
                    .filter_map(|content| match content {
                        MessageContent::Image(image) => Some(image.source.to_string()),
                        _ => None,
                    })
                    .collect::<Vec<String>>()
            } else {
                vec![]
            };

            match msg.role {
                Role::User => {
                    for tool_result in msg
                        .content
                        .extract_if(.., |x| matches!(x, MessageContent::ToolResult(..)))
                    {
                        match tool_result {
                            MessageContent::ToolResult(tool_result) => {
                                let images = tool_result
                                    .images()
                                    .map(|image| image.source.to_string())
                                    .collect::<Vec<_>>();
                                messages.push(ChatMessage::Tool {
                                    tool_name: tool_result.tool_name.to_string(),
                                    content: tool_result.text_contents(),
                                    images: if images.is_empty() {
                                        None
                                    } else {
                                        Some(images)
                                    },
                                })
                            }
                            _ => unreachable!("Only tool result should be extracted"),
                        }
                    }
                    if !msg.content.is_empty() {
                        messages.push(ChatMessage::User {
                            content: msg.string_contents(),
                            images: if images.is_empty() {
                                None
                            } else {
                                Some(images)
                            },
                        })
                    }
                }
                Role::Assistant => {
                    let mut text_content = String::new();
                    let mut thinking = None;
                    let mut tool_calls = Vec::new();
                    for content in msg.content.into_iter() {
                        match content {
                            MessageContent::Text(text) => {
                                text_content.push_str(&text);
                            }
                            MessageContent::Thinking { text, .. } if !text.is_empty() => {
                                thinking = Some(text)
                            }
                            MessageContent::ToolUse(tool_use) => {
                                tool_calls.push(OllamaToolCall {
                                    id: tool_use.id.to_string(),
                                    function: OllamaFunctionCall {
                                        name: tool_use.name.to_string(),
                                        arguments: match tool_use.input {
                                            language_model::LanguageModelToolUseInput::Json(
                                                input,
                                            ) => input,
                                            language_model::LanguageModelToolUseInput::Text(_) => {
                                                return Err(anyhow::anyhow!(
                                                    "Ollama does not support custom tool calls"
                                                ));
                                            }
                                        },
                                    },
                                });
                            }
                            _ => (),
                        }
                    }
                    messages.push(ChatMessage::Assistant {
                        content: text_content,
                        tool_calls: Some(tool_calls),
                        images: if images.is_empty() {
                            None
                        } else {
                            Some(images)
                        },
                        thinking,
                    })
                }
                Role::System => messages.push(ChatMessage::System {
                    content: msg.string_contents(),
                }),
            }
        }
        Ok(ChatRequest {
            model: config.name.clone(),
            messages,
            keep_alive: config.keep_alive.clone().unwrap_or_default(),
            stream: true,
            options: Some(ChatOptions {
                num_ctx: Some(config.max_tokens),
                num_predict: request.max_output_tokens.map(isize::try_from).transpose()?,
                // Only send stop tokens if explicitly provided. When empty/None,
                // Ollama will use the model's default stop tokens from its Modelfile.
                // Sending an empty array would override and disable the defaults.
                stop: if request.stop.is_empty() {
                    None
                } else {
                    Some(request.stop)
                },
                temperature: request.temperature.or(Some(1.0)),
                ..Default::default()
            }),
            think: config.supports_thinking.map(|supports_thinking| {
                let thinking = supports_thinking && request.thinking_allowed;
                match selected_thinking_level(config, request.thinking_effort.as_deref())
                    .filter(|_| thinking)
                {
                    Some(level) => ThinkValue::Level(level),
                    None => ThinkValue::Enabled(thinking),
                }
            }),
            tools: if config.supports_tools.unwrap_or(false) {
                request
                    .tools
                    .into_iter()
                    .map(tool_into_ollama)
                    .collect::<Result<_>>()?
            } else {
                vec![]
            },
        })
    }

    fn settings(cx: &App) -> &OllamaSettings {
        &AllLanguageModelSettings::get_global(cx).ollama
    }

    fn api_url(cx: &App) -> SharedString {
        let api_url = &Self::settings(cx).api_url;
        if api_url.is_empty() {
            OLLAMA_API_URL.into()
        } else {
            SharedString::new(api_url.as_str())
        }
    }

    /// The same test the edit prediction menu applies to its self-hosted
    /// providers, under the consent key of this provider.
    fn endpoint_is_local_or_acknowledged(api_url: &str, cx: &App) -> bool {
        http_client::endpoint_is_loopback(api_url)
            || settings::external_ai_provider_is_acknowledged(PROVIDER_ID.0.as_ref(), cx)
    }

    fn has_custom_url(cx: &App) -> bool {
        Self::settings(cx).api_url != OLLAMA_API_URL
    }
}

impl LanguageModelProviderState for OllamaLanguageModelProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for OllamaLanguageModelProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiOllama)
    }

    fn default_model(&self, _: &App) -> Option<LanguageModel> {
        // We shouldn't try to select default model, because it might lead to a load call for an unloaded model.
        // In a constrained environment where user might not have enough resources it'll be a bad UX to select something
        // to load by default.
        None
    }

    fn default_fast_model(&self, _: &App) -> Option<LanguageModel> {
        // See explanation for default_model.
        None
    }

    fn provided_models(&self, cx: &App) -> Vec<LanguageModel> {
        let mut models = self
            .ollama_models(cx)
            .values()
            .map(language_model)
            .collect::<Vec<_>>();
        models.sort_by_key(|model| model.name());
        models
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn settings_view(&self, _cx: &mut App) -> Option<ProviderSettingsView> {
        let state = self.state.clone();
        Some(ProviderSettingsView::SubPage(
            SubPageProviderSettings::new(move |window, cx| {
                cx.new(|cx| ConfigurationView::new(state.clone(), window, cx))
                    .into()
            })
            .description(InlineDescription::Text(
                i18n::t!("Run local models on your machine with Ollama.").into(),
            )),
        ))
    }
}

impl LanguageModelClient for OllamaLanguageModelProvider {
    fn stream_completion(
        &self,
        model: &LanguageModel,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<'static, Result<LanguageModelCompletionStream, LanguageModelCompletionError>>
    {
        let config = match cx.update(|cx| self.config(model, cx)) {
            Ok(config) => config,
            Err(error) => return async move { Err(error) }.boxed(),
        };
        let request_limiter = self.request_limiters.for_model(&model.id);
        let request = match Self::to_ollama_request(&config, request) {
            Ok(request) => request,
            Err(error) => return async move { Err(error.into()) }.boxed(),
        };

        let http_client = self.http_client.clone();
        let (api_key, api_url, extra_headers, reachable) = self.state.read_with(cx, |state, cx| {
            let api_url = OllamaLanguageModelProvider::api_url(cx);
            let extra_headers = OllamaLanguageModelProvider::settings(cx)
                .custom_headers
                .clone();
            // zed-jp-enhance: models listed in the settings can be chosen
            // without the list ever being fetched, so the request itself
            // is held to the same consent (ADR-F10).
            let reachable =
                OllamaLanguageModelProvider::endpoint_is_local_or_acknowledged(&api_url, cx);
            (
                state.api_key_state.key(&api_url),
                api_url,
                extra_headers,
                reachable,
            )
        });
        if !reachable {
            return async move {
                Err(anyhow!(
                    "not sending the request to Ollama at {api_url}: it is not on this machine \
                     and the external AI warning for Ollama has not been accepted"
                )
                .into())
            }
            .boxed();
        }

        let future = request_limiter.stream(async move {
            let stream = stream_chat_completion(
                http_client.as_ref(),
                &api_url,
                api_key.as_deref(),
                request,
                &extra_headers,
            )
            .await?;
            let stream = map_to_language_model_completion_events(stream);
            Ok(stream)
        });

        future.map_ok(|f| f.boxed()).boxed()
    }
}

/// Ollama で選べる思考の深さ。`think` に強度文字列を受け付けるモデルにだけ存在する。
/// 表示名は標準の [`ReasoningEffort`] と一致する値だけ日本語化され、それ以外
/// (ユーザーが `thinking_levels` に書いた独自の値) は原文のまま出る。
fn thinking_effort_levels(model: &ollama::Model) -> Vec<LanguageModelEffortLevel> {
    if !model.supports_thinking.unwrap_or(false) {
        return Vec::new();
    }

    let Some(levels) = model.thinking_levels.as_ref() else {
        return Vec::new();
    };

    let default_level = levels
        .iter()
        .find(|level| level.as_str() == ReasoningEffort::Medium.value())
        .or_else(|| levels.first());

    levels
        .iter()
        .map(|level| LanguageModelEffortLevel {
            name: ReasoningEffort::from_str(level)
                .map_or_else(|_| level.clone().into(), |effort| effort.label().into()),
            value: level.clone().into(),
            is_default: default_level.is_some_and(|default| default == level),
        })
        .collect()
}

fn selected_thinking_level(model: &ollama::Model, effort: Option<&str>) -> Option<String> {
    let effort = effort?;
    model
        .thinking_levels
        .as_ref()?
        .iter()
        .find(|level| level.as_str() == effort)
        .cloned()
}

fn language_model(model: &ollama::Model) -> LanguageModel {
    LanguageModel {
        supports_tools: model.supports_tools.unwrap_or(false),
        supports_images: model.supports_vision.unwrap_or(false),
        supports_thinking: model.supports_thinking.unwrap_or(false),
        supported_effort_levels: thinking_effort_levels(model).into(),
        disabled_reason: model.disabled.as_ref().map(|d| DisabledReason::new(d)),
        ..LanguageModel::new(
            LanguageModelId::from(model.name.clone()),
            LanguageModelName::from(model.display_name().to_string()),
            PROVIDER_ID,
            PROVIDER_NAME,
            format!("ollama/{}", model.id()),
            model.max_token_count(),
        )
    }
}

fn map_to_language_model_completion_events(
    stream: Pin<Box<dyn Stream<Item = anyhow::Result<ChatResponseDelta>> + Send>>,
) -> impl Stream<Item = Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
    struct State {
        stream: Pin<Box<dyn Stream<Item = anyhow::Result<ChatResponseDelta>> + Send>>,
        used_tools: bool,
    }

    // We need to create a ToolUse and Stop event from a single
    // response from the original stream
    let stream = stream::unfold(
        State {
            stream,
            used_tools: false,
        },
        async move |mut state| {
            let response = state.stream.next().await?;

            let delta = match response {
                Ok(delta) => delta,
                Err(e) => {
                    let event = Err(LanguageModelCompletionError::from(anyhow!(e)));
                    return Some((vec![event], state));
                }
            };

            let mut events = Vec::new();

            match delta.message {
                ChatMessage::User { content, images: _ } => {
                    events.push(Ok(LanguageModelCompletionEvent::Text(content)));
                }
                ChatMessage::System { content } => {
                    events.push(Ok(LanguageModelCompletionEvent::Text(content)));
                }
                ChatMessage::Tool { content, .. } => {
                    events.push(Ok(LanguageModelCompletionEvent::Text(content)));
                }
                ChatMessage::Assistant {
                    content,
                    tool_calls,
                    images: _,
                    thinking,
                } => {
                    if let Some(text) = thinking {
                        events.push(Ok(LanguageModelCompletionEvent::Thinking {
                            text,
                            signature: None,
                        }));
                    }

                    if let Some(tool_call) = tool_calls.and_then(|v| v.into_iter().next()) {
                        let OllamaToolCall { id, function } = tool_call;
                        let event = LanguageModelCompletionEvent::ToolUse(LanguageModelToolUse {
                            id: LanguageModelToolUseId::from(id),
                            name: Arc::from(function.name),
                            raw_input: function.arguments.to_string(),
                            input: language_model::LanguageModelToolUseInput::Json(
                                function.arguments,
                            ),
                            is_input_complete: true,
                            thought_signature: None,
                        });
                        events.push(Ok(event));
                        state.used_tools = true;
                    } else if !content.is_empty() {
                        events.push(Ok(LanguageModelCompletionEvent::Text(content)));
                    }
                }
            };

            if delta.done {
                events.push(Ok(LanguageModelCompletionEvent::UsageUpdate(TokenUsage {
                    input_tokens: delta.prompt_eval_count.unwrap_or(0),
                    output_tokens: delta.eval_count.unwrap_or(0),
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                })));
                if state.used_tools {
                    state.used_tools = false;
                    events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::ToolUse)));
                } else {
                    events.push(Ok(LanguageModelCompletionEvent::Stop(StopReason::EndTurn)));
                }
            }

            Some((events, state))
        },
    );

    stream.flat_map(futures::stream::iter)
}

struct ConfigurationView {
    api_key_editor: Entity<InputField>,
    api_url_editor: Entity<InputField>,
    context_window_editor: Entity<InputField>,
    state: Entity<State>,
}

impl ConfigurationView {
    pub fn new(state: Entity<State>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let api_key_editor =
            cx.new(|cx| InputField::new(window, cx, "63e02e...").label(i18n::t!("API key")));

        let api_url_editor = cx.new(|cx| {
            let input = InputField::new(window, cx, OLLAMA_API_URL).label(i18n::t!("API URL"));
            input.set_text(&OllamaLanguageModelProvider::api_url(cx), window, cx);
            input
        });

        let context_window_editor = cx.new(|cx| {
            let input = InputField::new(window, cx, "8192").label(i18n::t!("Context Window"));
            if let Some(context_window) = OllamaLanguageModelProvider::settings(cx).context_window {
                input.set_text(&context_window.to_string(), window, cx);
            }
            input
        });

        cx.observe(&state, |_, _, cx| {
            cx.notify();
        })
        .detach();

        Self {
            api_key_editor,
            api_url_editor,
            context_window_editor,
            state,
        }
    }

    fn retry_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let has_api_url = OllamaLanguageModelProvider::has_custom_url(cx);
        let has_api_key = self
            .state
            .read_with(cx, |state, _| state.api_key_state.has_key());
        if !has_api_url {
            self.save_api_url(cx);
        }
        if !has_api_key {
            self.save_api_key(&Default::default(), window, cx);
        }

        self.state.update(cx, |state, cx| {
            state.restart_fetch_models_task(cx);
        });
    }

    fn save_api_key(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let api_key = self.api_key_editor.read(cx).text(cx).trim().to_string();
        if api_key.is_empty() {
            return;
        }

        // url changes can cause the editor to be displayed again
        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(Some(api_key), cx))
                .await
        })
        .detach_and_log_err(cx);
    }

    fn reset_api_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_key_editor
            .update(cx, |input, cx| input.set_text("", window, cx));

        let state = self.state.clone();
        cx.spawn_in(window, async move |_, cx| {
            state
                .update(cx, |state, cx| state.set_api_key(None, cx))
                .await
        })
        .detach_and_log_err(cx);

        cx.notify();
    }

    fn save_api_url(&self, cx: &mut Context<Self>) {
        let api_url = self.api_url_editor.read(cx).text(cx).trim().to_string();
        let current_url = OllamaLanguageModelProvider::api_url(cx);
        if !api_url.is_empty() && &api_url != &current_url {
            let fs = <dyn Fs>::global(cx);
            update_settings_file(fs, cx, move |settings, _| {
                settings
                    .language_models
                    .get_or_insert_default()
                    .ollama
                    .get_or_insert_default()
                    .api_url = Some(api_url);
            });
        }
    }

    fn reset_api_url(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.api_url_editor
            .update(cx, |input, cx| input.set_text("", window, cx));
        let fs = <dyn Fs>::global(cx);
        update_settings_file(fs, cx, |settings, _cx| {
            if let Some(settings) = settings
                .language_models
                .as_mut()
                .and_then(|models| models.ollama.as_mut())
            {
                settings.api_url = Some(OLLAMA_API_URL.into());
            }
        });
        cx.notify();
    }

    fn save_context_window(&mut self, cx: &mut Context<Self>) {
        let context_window_str = self
            .context_window_editor
            .read(cx)
            .text(cx)
            .trim()
            .to_string();
        let current_context_window = OllamaLanguageModelProvider::settings(cx).context_window;

        if let Ok(context_window) = context_window_str.parse::<u64>() {
            if Some(context_window) != current_context_window {
                let fs = <dyn Fs>::global(cx);
                update_settings_file(fs, cx, move |settings, _| {
                    settings
                        .language_models
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .context_window = Some(context_window);
                });
            }
        } else if context_window_str.is_empty() && current_context_window.is_some() {
            let fs = <dyn Fs>::global(cx);
            update_settings_file(fs, cx, move |settings, _| {
                settings
                    .language_models
                    .get_or_insert_default()
                    .ollama
                    .get_or_insert_default()
                    .context_window = None;
            });
        }
    }

    fn reset_context_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.context_window_editor
            .update(cx, |input, cx| input.set_text("", window, cx));
        let fs = <dyn Fs>::global(cx);
        update_settings_file(fs, cx, |settings, _cx| {
            if let Some(settings) = settings
                .language_models
                .as_mut()
                .and_then(|models| models.ollama.as_mut())
            {
                settings.context_window = None;
            }
        });
        cx.notify();
    }

    fn render_instructions(cx: &App) -> Div {
        v_flex()
            .gap_2()
            .child(
                Label::new(i18n::t!(
                    "Run LLMs locally on your machine with Ollama, or connect to an Ollama server. \
                Can provide access to Llama, Mistral, Gemma, and hundreds of other models."
                ))
                .color(Color::Muted),
            )
            .child(Label::new(i18n::t!("To use local Ollama:")).color(Color::Muted))
            .child(
                List::new()
                    .child(
                        ListBulletItem::new("")
                            .child(
                                Label::new(i18n::t!("Download and install Ollama from"))
                                    .color(Color::Muted),
                            )
                            .child(ButtonLink::new("ollama.com", "https://ollama.com/download")),
                    )
                    .child(
                        ListBulletItem::new("")
                            .child(
                                Label::new(i18n::t!("Start Ollama and download a model:"))
                                    .color(Color::Muted),
                            )
                            .child(Label::new("ollama run gpt-oss:20b").inline_code(cx)),
                    )
                    .child(
                        ListBulletItem::new(i18n::t!(
                            "Click 'Connect' below to start using Ollama in Zed"
                        ))
                        .label_color(Color::Muted),
                    ),
            )
            .child(
                Label::new(i18n::t!(
                    "Alternatively, you can connect to an Ollama server by specifying its \
                URL and API key (may not be required):"
                ))
                .color(Color::Muted),
            )
    }

    fn render_api_key_editor(&self, cx: &Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let env_var_set = state.api_key_state.is_from_env_var();
        let configured_card_label = if env_var_set {
            i18n::t!(
                "API key set in {env_var_name} environment variable",
                env_var_name = API_KEY_ENV_VAR_NAME
            )
        } else {
            i18n::t!("API key configured").to_string()
        };

        let api_key_control = if !state.api_key_state.has_key() {
            self.api_key_editor.clone().into_any_element()
        } else {
            ConfiguredApiCard::new("ollama-reset-key", configured_card_label)
                .disabled(env_var_set)
                .on_click(cx.listener(|this, _, window, cx| this.reset_api_key(window, cx)))
                .when(env_var_set, |this| {
                    this.tooltip_label(i18n::t!(
                        "To reset your API key, unset the {env_var_name} environment variable.",
                        env_var_name = API_KEY_ENV_VAR_NAME
                    ))
                })
                .into_any_element()
        };

        v_flex()
            .on_action(cx.listener(Self::save_api_key))
            .child(api_key_control)
            .gap_1p5()
            .mb_2()
            .child(
                Label::new(i18n::t!(
                    "You can also set the {env_var_name} environment variable and restart Zed.",
                    env_var_name = API_KEY_ENV_VAR_NAME
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
    }

    fn render_context_window_editor(&self, cx: &Context<Self>) -> Div {
        let settings = OllamaLanguageModelProvider::settings(cx);
        let custom_context_window_set = settings.context_window.is_some();

        if custom_context_window_set {
            h_flex()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().background.opacity(0.5))
                .child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(Label::new(i18n::t!(
                            "Context Window: {}",
                            settings.context_window.unwrap()
                        ))),
                )
                .child(
                    Button::new("reset-context-window", i18n::t!("Reset"))
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
                        .on_click(
                            cx.listener(|this, _, window, cx| {
                                this.reset_context_window(window, cx)
                            }),
                        ),
                )
        } else {
            v_flex()
                .on_action(
                    cx.listener(|this, _: &menu::Confirm, _window, cx| {
                        this.save_context_window(cx)
                    }),
                )
                .child(self.context_window_editor.clone())
                .gap_1p5()
                .child(
                    Label::new(i18n::t!("Default: Model specific"))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
        }
    }

    fn render_api_url_editor(&self, cx: &Context<Self>) -> Div {
        let api_url = OllamaLanguageModelProvider::api_url(cx);
        let custom_api_url_set = api_url != OLLAMA_API_URL;

        if custom_api_url_set {
            h_flex()
                .p_1()
                .justify_between()
                .rounded_md()
                .border_1()
                .border_color(cx.theme().colors().border_variant)
                .bg(cx.theme().colors().background.opacity(0.5))
                .child(
                    h_flex()
                        .gap_1()
                        .child(Icon::new(IconName::Check).color(Color::Success))
                        .child(Label::new(api_url)),
                )
                .child(
                    Button::new("reset-api-url", i18n::t!("Reset API URL"))
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .start_icon(Icon::new(IconName::Undo).size(IconSize::Small))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.reset_api_url(window, cx)),
                        ),
                )
        } else {
            v_flex()
                .on_action(cx.listener(|this, _: &menu::Confirm, _window, cx| {
                    this.save_api_url(cx);
                    cx.notify();
                }))
                .gap_2()
                .child(self.api_url_editor.clone())
        }
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_authenticated = self.state.read(cx).is_authenticated();

        v_flex()
            .gap_2()
            .child(Headline::new("Ollama").size(HeadlineSize::Small))
            .child(Self::render_instructions(cx))
            .child(self.render_api_url_editor(cx))
            .child(self.render_context_window_editor(cx))
            .child(self.render_api_key_editor(cx))
            .child(Divider::horizontal())
            .child(
                h_flex()
                    .pt_2()
                    .w_full()
                    .justify_between()
                    .gap_1()
                    .child(
                        h_flex()
                            .w_full()
                            .gap_2()
                            .map(|this| {
                                if is_authenticated {
                                    this.child(
                                        Button::new("ollama-site", "Ollama")
                                            .style(ButtonStyle::OutlinedGhost)
                                            .size(ButtonSize::Medium)
                                            .end_icon(
                                                Icon::new(IconName::ArrowUpRight)
                                                    .size(IconSize::XSmall)
                                                    .color(Color::Muted),
                                            )
                                            .on_click(move |_, _, cx| cx.open_url(OLLAMA_SITE))
                                            .into_any_element(),
                                    )
                                } else {
                                    this.child(
                                        Button::new(
                                            "download_ollama_button",
                                            i18n::t!("Download Ollama"),
                                        )
                                        .style(ButtonStyle::OutlinedGhost)
                                        .size(ButtonSize::Medium)
                                        .end_icon(
                                            Icon::new(IconName::ArrowUpRight)
                                                .size(IconSize::XSmall)
                                                .color(Color::Muted),
                                        )
                                        .on_click(move |_, _, cx| cx.open_url(OLLAMA_DOWNLOAD_URL))
                                        .into_any_element(),
                                    )
                                }
                            })
                            .child(
                                Button::new("view-models", i18n::t!("View All Models"))
                                    .style(ButtonStyle::OutlinedGhost)
                                    .size(ButtonSize::Medium)
                                    .end_icon(
                                        Icon::new(IconName::ArrowUpRight)
                                            .size(IconSize::XSmall)
                                            .color(Color::Muted),
                                    )
                                    .on_click(move |_, _, cx| cx.open_url(OLLAMA_LIBRARY_URL)),
                            ),
                    )
                    .map(|this| {
                        if is_authenticated {
                            this.child(
                                ButtonLike::new("connected")
                                    .size(ButtonSize::Medium)
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .child(Icon::new(IconName::Check).color(Color::Success))
                                            .child(Label::new(i18n::t!("Connected"))),
                                    )
                                    .child(
                                        IconButton::new("refresh-models", IconName::RotateCcw)
                                            .icon_size(IconSize::Small)
                                            .tooltip(Tooltip::text(i18n::t!("Refresh Models")))
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.state.update(cx, |state, _| {
                                                    state.fetched_models.clear();
                                                });
                                                this.retry_connection(window, cx);
                                            })),
                                    ),
                            )
                        } else {
                            this.child(
                                Button::new("retry_ollama_models", i18n::t!("Connect"))
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Medium)
                                    .start_icon(
                                        Icon::new(IconName::PlayOutlined).size(IconSize::XSmall),
                                    )
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.retry_connection(window, cx)
                                    })),
                            )
                        }
                    }),
            )
    }
}

fn merge_settings_into_models(
    models: &mut HashMap<String, ollama::Model>,
    available_models: &[AvailableModel],
    context_window: Option<u64>,
) {
    for setting_model in available_models {
        if let Some(model) = models.get_mut(&setting_model.name) {
            if context_window.is_none() {
                model.max_tokens = setting_model.max_tokens;
            }
            model.display_name = setting_model.display_name.clone();
            model.keep_alive = setting_model.keep_alive.clone();
            model.supports_tools = setting_model.supports_tools;
            model.supports_vision = setting_model.supports_images;
            model.supports_thinking = setting_model.supports_thinking;
            model.thinking_levels = setting_model
                .thinking_levels
                .clone()
                .or_else(|| ollama::default_thinking_levels(&setting_model.name));
        } else {
            models.insert(
                setting_model.name.clone(),
                ollama::Model {
                    name: setting_model.name.clone(),
                    display_name: setting_model.display_name.clone(),
                    max_tokens: context_window.unwrap_or(setting_model.max_tokens),
                    keep_alive: setting_model.keep_alive.clone(),
                    supports_tools: setting_model.supports_tools,
                    supports_vision: setting_model.supports_images,
                    supports_thinking: setting_model.supports_thinking,
                    thinking_levels: setting_model
                        .thinking_levels
                        .clone()
                        .or_else(|| ollama::default_thinking_levels(&setting_model.name)),
                    disabled: None,
                },
            );
        }
    }
}

fn tool_into_ollama(tool: LanguageModelRequestTool) -> Result<ollama::OllamaTool> {
    let input_schema = match tool.input {
        language_model::LanguageModelRequestToolInput::Function { input_schema, .. } => {
            input_schema
        }
        language_model::LanguageModelRequestToolInput::Custom { .. } => {
            anyhow::bail!("Ollama does not support custom tools");
        }
    };
    Ok(ollama::OllamaTool::Function {
        function: OllamaFunctionTool {
            name: tool.name,
            description: Some(tool.description),
            parameters: Some(input_schema),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;

    #[test]
    fn test_merge_settings_preserves_display_names_for_similar_models() {
        // Regression test for https://github.com/zed-industries/zed/issues/43646
        // When multiple models share the same base name (e.g., qwen2.5-coder:1.5b and qwen2.5-coder:3b),
        // each model should get its own display_name from settings, not a random one.

        let mut models: HashMap<String, ollama::Model> = HashMap::default();
        models.insert(
            "qwen2.5-coder:1.5b".to_string(),
            ollama::Model {
                name: "qwen2.5-coder:1.5b".to_string(),
                display_name: None,
                max_tokens: 4096,
                keep_alive: None,
                supports_tools: None,
                supports_vision: None,
                supports_thinking: None,
                thinking_levels: None,
                disabled: None,
            },
        );
        models.insert(
            "qwen2.5-coder:3b".to_string(),
            ollama::Model {
                name: "qwen2.5-coder:3b".to_string(),
                display_name: None,
                max_tokens: 4096,
                keep_alive: None,
                supports_tools: None,
                supports_vision: None,
                supports_thinking: None,
                thinking_levels: None,
                disabled: None,
            },
        );

        let available_models = vec![
            AvailableModel {
                name: "qwen2.5-coder:1.5b".to_string(),
                display_name: Some("QWEN2.5 Coder 1.5B".to_string()),
                max_tokens: 5000,
                keep_alive: None,
                supports_tools: Some(true),
                supports_images: None,
                supports_thinking: None,
                thinking_levels: None,
            },
            AvailableModel {
                name: "qwen2.5-coder:3b".to_string(),
                display_name: Some("QWEN2.5 Coder 3B".to_string()),
                max_tokens: 6000,
                keep_alive: None,
                supports_tools: Some(true),
                supports_images: None,
                supports_thinking: None,
                thinking_levels: None,
            },
        ];

        merge_settings_into_models(&mut models, &available_models, None);

        let model_1_5b = models
            .get("qwen2.5-coder:1.5b")
            .expect("1.5b model missing");
        let model_3b = models.get("qwen2.5-coder:3b").expect("3b model missing");

        assert_eq!(
            model_1_5b.display_name,
            Some("QWEN2.5 Coder 1.5B".to_string()),
            "1.5b model should have its own display_name"
        );
        assert_eq!(model_1_5b.max_tokens, 5000);

        assert_eq!(
            model_3b.display_name,
            Some("QWEN2.5 Coder 3B".to_string()),
            "3b model should have its own display_name"
        );
        assert_eq!(model_3b.max_tokens, 6000);
    }

    /// API から発見したモデル (設定で上書きしていない状態)。
    fn discovered_model(name: &str) -> ollama::Model {
        ollama::Model::new(name, None, None, None, Some(true))
    }

    /// `thinking_levels` を設定で明示したモデル。
    fn model_with_levels(name: &str, levels: &[&str]) -> ollama::Model {
        ollama::Model {
            thinking_levels: Some(levels.iter().map(|level| (*level).to_owned()).collect()),
            ..discovered_model(name)
        }
    }

    #[test]
    fn test_effort_levels_for_thinking_level_models() {
        let model = discovered_model("gpt-oss:20b");
        let levels = thinking_effort_levels(&model);
        assert_eq!(
            levels
                .iter()
                .map(|level| level.value.as_ref())
                .collect::<Vec<_>>(),
            vec!["low", "medium", "high"]
        );
        assert_eq!(
            levels
                .iter()
                .map(|level| level.display_name())
                .collect::<Vec<_>>(),
            vec!["Low", "Medium", "High"]
        );
        assert_eq!(
            levels
                .iter()
                .find(|level| level.is_default)
                .map(|level| level.value.clone()),
            Some("medium".into())
        );
    }

    #[test]
    fn test_effort_levels_from_settings_and_off_switches() {
        let explicit = model_with_levels("qwen3:8b", &["low", "ultra"]);
        let levels = thinking_effort_levels(&explicit);
        assert_eq!(
            levels
                .iter()
                .map(|level| (level.value.as_ref(), level.display_name(), level.is_default))
                .collect::<Vec<_>>(),
            vec![
                ("low", "Low".into(), true),
                ("ultra", "ultra".into(), false)
            ],
            "レベルに medium が無ければ先頭が既定になり、独自レベルの表示名は原文のまま"
        );

        assert!(
            thinking_effort_levels(&discovered_model("qwen3:8b")).is_empty(),
            "レベル非対応のモデルはオン/オフのみ"
        );

        let not_thinking = ollama::Model {
            supports_thinking: Some(false),
            ..discovered_model("gpt-oss:20b")
        };
        assert!(
            thinking_effort_levels(&not_thinking).is_empty(),
            "思考自体が無効なモデルにレベルを出さない"
        );
    }

    #[test]
    fn test_settings_thinking_levels_reach_the_model() {
        let mut models: HashMap<String, ollama::Model> = HashMap::default();
        let available_models = vec![
            AvailableModel {
                name: "gpt-oss:20b".to_string(),
                display_name: None,
                max_tokens: 4096,
                keep_alive: None,
                supports_tools: Some(true),
                supports_images: None,
                supports_thinking: Some(true),
                thinking_levels: None,
            },
            AvailableModel {
                name: "qwen3:8b".to_string(),
                display_name: None,
                max_tokens: 4096,
                keep_alive: None,
                supports_tools: Some(true),
                supports_images: None,
                supports_thinking: Some(true),
                thinking_levels: Some(vec!["low".to_string(), "high".to_string()]),
            },
        ];

        merge_settings_into_models(&mut models, &available_models, None);

        assert_eq!(
            models
                .get("gpt-oss:20b")
                .and_then(|model| model.thinking_levels.clone()),
            ollama::default_thinking_levels("gpt-oss:20b"),
            "gpt-oss は設定が無くても自動検出する"
        );
        assert_eq!(
            models
                .get("qwen3:8b")
                .and_then(|model| model.thinking_levels.clone()),
            Some(vec!["low".to_string(), "high".to_string()])
        );
    }

    #[test]
    fn test_selected_thinking_level_ignores_unsupported_values() {
        let model = discovered_model("gpt-oss:20b");
        assert_eq!(
            selected_thinking_level(&model, Some("high")),
            Some("high".to_string())
        );
        assert_eq!(selected_thinking_level(&model, Some("xhigh")), None);
        assert_eq!(selected_thinking_level(&model, None), None);
        assert_eq!(
            selected_thinking_level(&discovered_model("qwen3:8b"), Some("high")),
            None
        );
    }

    struct NoCredentials;

    impl CredentialsProvider for NoCredentials {
        fn read_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<Option<(String, Vec<u8>)>>> + 'a>> {
            Box::pin(async { Ok(None) })
        }

        fn write_credentials<'a>(
            &'a self,
            _url: &'a str,
            _username: &'a str,
            _password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_credentials<'a>(
            &'a self,
            _url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = Result<()>> + 'a>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// Builds the provider against `api_url` and returns the hosts of every
    /// request it sends.
    fn provider_recording_requests(
        api_url: &str,
        cx: &mut gpui::TestAppContext,
    ) -> (
        OllamaLanguageModelProvider,
        Arc<parking_lot::Mutex<Vec<String>>>,
    ) {
        provider_recording_requests_through(api_url, None, cx)
    }

    /// Like [`provider_recording_requests`], with the requests going through
    /// the egress allowlist when `egress_policy` is given, as they do in Zed.
    /// The settings also list one model, which can be chosen without the list
    /// ever being fetched.
    fn provider_recording_requests_through(
        api_url: &str,
        egress_policy: Option<http_client::EgressPolicyHandle>,
        cx: &mut gpui::TestAppContext,
    ) -> (
        OllamaLanguageModelProvider,
        Arc<parking_lot::Mutex<Vec<String>>>,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        set_user_settings(cx, {
            let api_url = api_url.to_string();
            move |settings| {
                settings
                    .language_models
                    .get_or_insert_default()
                    .ollama
                    .get_or_insert_default();
                let ollama = settings
                    .language_models
                    .get_or_insert_default()
                    .ollama
                    .get_or_insert_default();
                ollama.api_url = Some(api_url);
                ollama.available_models = Some(vec![settings::OllamaAvailableModel {
                    name: "listed-in-settings".to_string(),
                    display_name: None,
                    max_tokens: 4096,
                    keep_alive: None,
                    supports_tools: None,
                    supports_images: None,
                    supports_thinking: None,
                    thinking_levels: None,
                }]);
            }
        });
        let requested_hosts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let http_client = http_client::FakeHttpClient::create({
            let requested_hosts = requested_hosts.clone();
            move |request| {
                requested_hosts
                    .lock()
                    .push(request.uri().host().unwrap_or_default().to_string());
                async move {
                    Ok(http_client::Response::builder()
                        .status(404)
                        .body(http_client::AsyncBody::default())?)
                }
            }
        });
        let http_client: Arc<dyn HttpClient> = match egress_policy {
            Some(policy) => Arc::new(http_client::AllowlistedHttpClient::new(http_client, policy)),
            None => http_client,
        };
        let provider = cx.update(|cx| {
            OllamaLanguageModelProvider::new(http_client, Arc::new(NoCredentials), cx)
        });
        (provider, requested_hosts)
    }

    fn set_user_settings(
        cx: &mut gpui::TestAppContext,
        update: impl FnOnce(&mut settings::SettingsContent),
    ) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, update);
            });
        });
    }

    #[gpui::test]
    async fn test_remote_model_list_waits_for_consent(cx: &mut gpui::TestAppContext) {
        let (provider, requested_hosts) =
            provider_recording_requests("http://ollama.example.com:11434", cx);

        cx.update(|cx| provider.authenticate(cx)).await.ok();
        cx.run_until_parked();
        assert_eq!(
            *requested_hosts.lock(),
            Vec::<String>::new(),
            "a remote endpoint must not be asked for its models before consent"
        );

        set_user_settings(cx, |settings| {
            settings.acknowledged_external_ai_providers = Some(vec!["ollama".to_string()]);
        });
        cx.run_until_parked();
        assert_eq!(
            *requested_hosts.lock(),
            vec!["ollama.example.com".to_string()],
            "accepting the warning should fetch the list without another action"
        );
    }

    #[gpui::test]
    async fn test_consent_for_another_provider_does_not_open_ollama(cx: &mut gpui::TestAppContext) {
        let (provider, requested_hosts) =
            provider_recording_requests("http://ollama.example.com:11434", cx);
        set_user_settings(cx, |settings| {
            settings.acknowledged_external_ai_providers =
                Some(vec!["edit_prediction:ollama".to_string()]);
        });

        cx.update(|cx| provider.authenticate(cx)).await.ok();
        cx.run_until_parked();
        assert_eq!(*requested_hosts.lock(), Vec::<String>::new());
    }

    #[gpui::test]
    async fn test_local_model_list_needs_no_consent(cx: &mut gpui::TestAppContext) {
        let (provider, requested_hosts) = provider_recording_requests("http://127.0.0.1:11434", cx);

        cx.update(|cx| provider.authenticate(cx)).await.ok();
        cx.run_until_parked();
        assert_eq!(*requested_hosts.lock(), vec!["127.0.0.1".to_string()]);
    }

    async fn send_to_listed_model(
        provider: &OllamaLanguageModelProvider,
        cx: &mut gpui::TestAppContext,
    ) -> Result<(), LanguageModelCompletionError> {
        let model = cx
            .update(|cx| provider.provided_models(cx))
            .into_iter()
            .find(|model| model.id.0.as_ref() == "listed-in-settings")
            .expect("the model listed in the settings should be offered");
        let response =
            provider.stream_completion(&model, LanguageModelRequest::default(), &cx.to_async());
        let result = response.await.map(|_| ());
        cx.run_until_parked();
        result
    }

    #[gpui::test]
    async fn test_remote_completion_waits_for_consent(cx: &mut gpui::TestAppContext) {
        let (provider, requested_hosts) =
            provider_recording_requests("http://ollama.example.com:11434", cx);
        cx.run_until_parked();

        let error = send_to_listed_model(&provider, cx)
            .await
            .expect_err("a remote endpoint must not be sent a request before consent");
        assert!(
            error.to_string().contains("external AI warning"),
            "unexpected error: {error}"
        );
        assert_eq!(*requested_hosts.lock(), Vec::<String>::new());

        set_user_settings(cx, |settings| {
            settings.acknowledged_external_ai_providers = Some(vec!["ollama".to_string()]);
        });
        cx.run_until_parked();
        requested_hosts.lock().clear();
        send_to_listed_model(&provider, cx).await.ok();
        assert_eq!(
            *requested_hosts.lock(),
            vec!["ollama.example.com".to_string()],
            "after consent the request goes out"
        );
    }

    /// Consent is a warning the user reads, not the network control: with it
    /// given but the host not in the allowlist, nothing reaches the server.
    #[gpui::test]
    async fn test_egress_allowlist_still_stops_remote_ollama_after_consent(
        cx: &mut gpui::TestAppContext,
    ) {
        let empty_policy = http_client::EgressPolicyHandle::new(
            http_client::EgressPolicy::from_host_strings(Vec::<String>::new()).0,
        );
        let (provider, requested_hosts) = provider_recording_requests_through(
            "http://ollama.example.com:11434",
            Some(empty_policy),
            cx,
        );
        set_user_settings(cx, |settings| {
            settings.acknowledged_external_ai_providers = Some(vec!["ollama".to_string()]);
        });
        cx.update(|cx| provider.authenticate(cx)).await.ok();
        cx.run_until_parked();

        let error = send_to_listed_model(&provider, cx)
            .await
            .expect_err("the egress allowlist should refuse the host");
        let error = format!("{error:?}");
        assert!(
            error.contains("`ollama.example.com` is not on the network allowlist"),
            "unexpected error: {error}"
        );
        assert_eq!(
            *requested_hosts.lock(),
            Vec::<String>::new(),
            "neither the model list nor the completion should reach the server"
        );
    }

    #[gpui::test]
    async fn test_egress_allowlist_lets_local_ollama_through(cx: &mut gpui::TestAppContext) {
        let empty_policy = http_client::EgressPolicyHandle::new(
            http_client::EgressPolicy::from_host_strings(Vec::<String>::new()).0,
        );
        let (provider, requested_hosts) =
            provider_recording_requests_through("http://127.0.0.1:11434", Some(empty_policy), cx);
        cx.update(|cx| provider.authenticate(cx)).await.ok();
        cx.run_until_parked();
        send_to_listed_model(&provider, cx).await.ok();
        assert_eq!(
            *requested_hosts.lock(),
            vec!["127.0.0.1".to_string(), "127.0.0.1".to_string()],
            "the model list and the completion both go to the local server"
        );
    }
}
